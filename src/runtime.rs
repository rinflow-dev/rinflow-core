mod data;

use crate::clipboard::{Clipboard, ClipboardAssembler};
use crate::files::Offer;
use crate::input::{PlatformEntry, PlatformInputQueue, RemoteInputState, SequenceStatus};
use crate::layout::{self, Display, Offset, Side, validate_displays};
use crate::model::{
    Announcement, CAPABILITY_INPUT_V2, CoreConfig, DISCOVERY_GROUP, LcInputEvent, Peer,
    PlacementRecord, WireMessage, negotiate_protocol,
};
use crate::qos::TrafficClass;
use crate::remap::ShortcutRemap;
use crate::transport::{
    ActiveConnection, ConnectionEvent, connect_in_background, spawn_connection,
};
use data::{DataLink, Download};
use serde_json::{Value, json};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

/// Sessions (established or still handshaking) a core keeps at once. Bounds
/// what an unauthenticated device on the network can make us hold open.
const MAX_SESSIONS: usize = 8;
/// A session that has not answered a ping, or not finished its handshake,
/// within this time is closed.
const SESSION_TIMEOUT: Duration = Duration::from_secs(8);
const PEER_EXPIRY: Duration = Duration::from_secs(7);

#[derive(Clone)]
pub struct EventSink {
    event: Arc<dyn Fn(String) + Send + Sync>,
    input: Arc<dyn Fn(LcInputEvent) + Send + Sync>,
}

impl EventSink {
    pub fn new(
        event: impl Fn(String) + Send + Sync + 'static,
        input: impl Fn(LcInputEvent) + Send + Sync + 'static,
    ) -> Self {
        Self {
            event: Arc::new(event),
            input: Arc::new(input),
        }
    }

    fn emit_value(&self, value: serde_json::Value) {
        (self.event)(value.to_string());
    }

    fn emit_error(&self, message: impl Into<String>) {
        self.emit_value(json!({ "event": "error", "message": message.into() }));
    }
}

#[derive(Debug)]
pub enum Command {
    /// Opens a session with the peer (if none exists) and makes it the input target.
    Connect(String),
    /// Closes the session with the input target, or every session if there is none.
    Disconnect,
    DisconnectPeer(String),
    /// Chooses which peer receives `SendInput`; `None` drops input.
    SetInputTarget(Option<String>),
    SendInput(LcInputEvent),
    /// Releases everything the input target holds and tells it control ended.
    ResetInput,
    InputReady,
    /// This device's displays changed (logical points, see `layout.rs`).
    SetLocalDisplays(Vec<Display>),
    /// Places a peer at the given offset after snapping it to a valid spot,
    /// or removes its placement. Synced to the peer.
    SetPlacement {
        peer_id: String,
        offset: Option<Offset>,
    },
    /// Places a peer beside the outermost edge on one side.
    PlaceBeside {
        peer_id: String,
        side: Side,
    },
    /// The system clipboard changed; empty when it holds nothing shared.
    SetClipboard(Clipboard),
    /// Receive the offered files of this transfer (`files_offered`).
    AcceptFiles(u32),
    /// Stop receiving this transfer.
    CancelFiles(u32),
    Stop,
}

pub struct CoreThread {
    commands: mpsc::UnboundedSender<Command>,
    platform_input: PlatformInputQueue,
    join: Option<JoinHandle<()>>,
}

impl CoreThread {
    pub fn spawn(config: CoreConfig, sink: EventSink) -> Result<Self, String> {
        config.validate()?;
        let (commands, receiver) = mpsc::unbounded_channel();
        let platform_input = PlatformInputQueue::new();
        let runtime_input = platform_input.clone();
        let join = std::thread::Builder::new()
            .name("linkcursor-core".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .thread_name("linkcursor-io")
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        sink.emit_error(format!("failed to create async runtime: {error}"));
                        return;
                    }
                };
                runtime.block_on(run(config, receiver, runtime_input, sink));
            })
            .map_err(|error| error.to_string())?;

        Ok(Self {
            commands,
            platform_input,
            join: Some(join),
        })
    }

    pub fn send(&self, command: Command) -> Result<(), String> {
        match command {
            Command::SendInput(event) => {
                let (_, should_notify) = self.platform_input.push(event).map_err(str::to_owned)?;
                self.notify_input(should_notify)
            }
            // Target changes and resets are ordered with input (see
            // `PlatformEntry`), so they go through the input queue too.
            Command::SetInputTarget(peer_id) => {
                let should_notify = self
                    .platform_input
                    .push_control(PlatformEntry::SetTarget(peer_id));
                self.notify_input(should_notify)
            }
            Command::ResetInput => {
                let should_notify = self.platform_input.push_control(PlatformEntry::Reset);
                self.notify_input(should_notify)
            }
            Command::Connect(peer_id) => {
                let should_notify = self
                    .platform_input
                    .push_control(PlatformEntry::SetTarget(Some(peer_id.clone())));
                self.notify_input(should_notify)?;
                self.send_command(Command::Connect(peer_id))
            }
            command => self.send_command(command),
        }
    }

    fn notify_input(&self, should_notify: bool) -> Result<(), String> {
        if should_notify {
            self.send_command(Command::InputReady)?;
        }
        Ok(())
    }

    fn send_command(&self, command: Command) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "LinkCursor core is not running".to_owned())
    }

    pub fn stop_and_join(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for CoreThread {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

async fn run(
    config: CoreConfig,
    mut commands: mpsc::UnboundedReceiver<Command>,
    platform_input: PlatformInputQueue,
    sink: EventSink,
) {
    let udp = match create_discovery_socket(config.discovery_port) {
        Ok(socket) => socket,
        Err(error) => {
            sink.emit_error(format!("discovery socket failed: {error}"));
            sink.emit_value(json!({ "event": "stopped" }));
            return;
        }
    };
    let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, config.control_port)).await {
        Ok(listener) => listener,
        Err(error) => {
            sink.emit_error(format!("control listener failed: {error}"));
            sink.emit_value(json!({ "event": "stopped" }));
            return;
        }
    };

    let multicast = SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::from(DISCOVERY_GROUP),
        config.discovery_port,
    ));
    let broadcast = SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::BROADCAST,
        config.discovery_port,
    ));
    let (connection_events_tx, mut connection_events_rx) = mpsc::unbounded_channel();
    let mut announce_timer = tokio::time::interval(Duration::from_secs(2));
    let mut maintenance_timer = tokio::time::interval(Duration::from_secs(1));
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut state = Runtime {
        config: &config,
        sink: &sink,
        connection_events: connection_events_tx,
        peers: HashMap::new(),
        sessions: HashMap::new(),
        dialing: HashSet::new(),
        input_target: None,
        next_connection_id: 1,
        ping_sequence: 0,
        announcement: serde_json::to_vec(&Announcement::from_config(&config, &config.displays))
            .unwrap_or_default(),
        local_displays: config.displays.clone(),
        placements: config
            .placements
            .iter()
            .map(|record| (record.peer_id.clone(), record.clone()))
            .collect(),
        peer_displays: HashMap::new(),
        pending_disconnects: Vec::new(),
        portals: Vec::new(),
        clipboard: None,
        next_clipboard_id: 0,
        controlling: None,
        data_links: HashMap::new(),
        offer: None,
        next_transfer: 0,
        download: None,
        download_failures: Vec::new(),
        uploads: HashMap::new(),
    };

    sink.emit_value(json!({
        "event": "started",
        "device_id": config.device_id,
        "control_port": config.control_port,
        "discovery_port": config.discovery_port
    }));
    state.publish_portals();

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(Command::Connect(peer_id)) => state.connect(peer_id),
                    Some(Command::Disconnect) => state.disconnect_target(),
                    Some(Command::DisconnectPeer(peer_id)) => state.disconnect_peer(&peer_id),
                    Some(Command::InputReady) => state.platform_input(platform_input.drain()),
                    Some(Command::SetLocalDisplays(displays)) => state.set_local_displays(displays),
                    Some(Command::SetPlacement { peer_id, offset }) => state.set_placement(&peer_id, offset),
                    Some(Command::PlaceBeside { peer_id, side }) => state.place_beside(&peer_id, side),
                    Some(Command::SetClipboard(clipboard)) => state.set_clipboard(clipboard),
                    Some(Command::AcceptFiles(transfer)) => state.accept_files(transfer),
                    Some(Command::CancelFiles(transfer)) => state.cancel_files(transfer),
                    Some(Command::SendInput(_) | Command::SetInputTarget(_) | Command::ResetInput) => {
                        unreachable!("input commands are queued by CoreThread")
                    }
                    Some(Command::Stop) | None => break,
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _address)) => state.accept(stream),
                    Err(error) => sink.emit_error(format!("accept failed: {error}")),
                }
            }
            received = udp.recv_from(&mut buffer) => {
                if let Ok((length, source)) = received {
                    state.announcement(&buffer[..length], source);
                }
            }
            connection_event = connection_events_rx.recv() => {
                if let Some(event) = connection_event {
                    state.connection_event(event);
                }
            }
            _ = announce_timer.tick() => {
                let announcement = state.announcement.clone();
                let _ = udp.send_to(&announcement, multicast).await;
                // Broadcast is a compatibility fallback for networks or VPN setups that
                // suppress multicast. Receivers deduplicate announcements by device ID.
                let _ = udp.send_to(&announcement, broadcast).await;
            }
            _ = maintenance_timer.tick() => state.maintenance(),
        }
    }

    state.close_all("core stopped");
    sink.emit_value(json!({ "event": "stopped" }));
}

/// One connection to a peer. It becomes established once the peer's
/// `Hello` has been accepted.
struct Session {
    connection: ActiveConnection,
    /// We dialed this connection (as opposed to accepting it).
    outgoing: bool,
    /// What the peer holds down on this device, released when it goes away.
    remote_input: RemoteInputState,
    /// Last sign of life: creation, handshake or pong.
    last_seen: Instant,
    /// The clipboard last exchanged with this peer, either way.
    clipboard_synced: Option<u64>,
    clipboard_in: ClipboardAssembler,
    /// A clipboard sync waits for the data link.
    clipboard_pending: bool,
    /// Connection ID of the session's data link (see `runtime/data.rs`).
    data: Option<u64>,
    /// The token this side invited the peer with, and when.
    data_token: Option<u64>,
    data_invited: Option<Instant>,
}

impl Session {
    fn established(&self) -> bool {
        self.connection.protocol_version.is_some()
    }

    fn peer_id(&self) -> Option<&str> {
        self.connection.peer_id.as_deref()
    }
}

struct Runtime<'a> {
    config: &'a CoreConfig,
    sink: &'a EventSink,
    connection_events: mpsc::UnboundedSender<ConnectionEvent>,
    peers: HashMap<String, Peer>,
    /// By connection ID.
    sessions: HashMap<u64, Session>,
    /// Peers with an outgoing TCP connect still in flight.
    dialing: HashSet<String>,
    input_target: Option<String>,
    next_connection_id: u64,
    ping_sequence: u64,
    /// The discovery datagram; rebuilt when the displays change.
    announcement: Vec<u8>,
    local_displays: Vec<Display>,
    /// By peer ID, in this device's space.
    placements: HashMap<String, PlacementRecord>,
    /// Latest displays each peer described, in its own space.
    peer_displays: HashMap<String, Vec<Display>>,
    pending_disconnects: Vec<PendingDisconnect>,
    /// The portals last reported in `portals_changed`.
    portals: Vec<Value>,
    /// This device's clipboard, encoded, with its digest.
    clipboard: Option<(u64, Arc<Vec<u8>>)>,
    next_clipboard_id: u32,
    /// The peer this device's input is driving, from the first event sent
    /// after a target change or reset until the next one.
    controlling: Option<String>,
    /// Data links by connection ID.
    data_links: HashMap<u64, DataLink>,
    /// Files on this device's clipboard.
    offer: Option<Arc<Offer>>,
    next_transfer: u32,
    /// Files being received.
    download: Option<Download>,
    /// `files_failed` events of stopped downloads, by disk writer: sent once
    /// that writer has removed its partial file.
    download_failures: Vec<(u64, Value)>,
    /// File uploads in progress, by data link.
    uploads: HashMap<u64, tokio::task::JoinHandle<()>>,
}

/// How long a disconnect may be held back while a duplicate connection that
/// could be the same peer finishes its handshake.
const DISCONNECT_GRACE: Duration = Duration::from_secs(1);

struct PendingDisconnect {
    peer_id: String,
    reason: String,
    deadline: Instant,
}

/// Unpredictable enough for data-link tokens and file names (not for keys).
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::hash::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0),
    );
    hasher.finish()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl Runtime<'_> {
    fn established_session(&self, peer_id: &str) -> Option<u64> {
        self.sessions
            .iter()
            .find(|(_, session)| session.established() && session.peer_id() == Some(peer_id))
            .map(|(id, _)| *id)
    }

    /// The input target was already switched to `peer_id` through the input
    /// queue, ahead of this command.
    fn connect(&mut self, peer_id: String) {
        if let Some(id) = self.established_session(&peer_id) {
            // Already connected: repeat the event so a shell that lost track
            // of the session can resync.
            let session = &self.sessions[&id];
            let peer = self.peer_value(&peer_id, None, session.connection.capabilities);
            self.sink.emit_value(json!({
                "event": "connected",
                "peer": peer,
                "protocol_version": session.connection.protocol_version,
                "capabilities": session.connection.capabilities
            }));
            return;
        }
        let pending = self
            .sessions
            .values()
            .any(|session| session.outgoing && session.peer_id() == Some(peer_id.as_str()));
        if pending || self.dialing.contains(&peer_id) {
            return;
        }
        let Some(peer) = self.peers.get(&peer_id) else {
            self.sink
                .emit_error(format!("peer is no longer available: {peer_id}"));
            return;
        };
        let address = peer.socket_address;
        self.sink
            .emit_value(json!({ "event": "connecting", "peer_id": peer_id }));
        self.dialing.insert(peer_id.clone());
        connect_in_background(peer_id, address, self.connection_events.clone());
    }

    fn disconnect_target(&mut self) {
        match self.input_target.clone() {
            Some(peer_id) => self.disconnect_peer(&peer_id),
            None => {
                let peers: HashSet<String> = self
                    .sessions
                    .values()
                    .filter_map(|session| session.peer_id().map(str::to_owned))
                    .chain(self.dialing.iter().cloned())
                    .collect();
                if peers.is_empty() {
                    self.sink
                        .emit_value(json!({ "event": "disconnected", "reason": "local request" }));
                }
                for peer_id in peers {
                    self.disconnect_peer(&peer_id);
                }
            }
        }
    }

    fn disconnect_peer(&mut self, peer_id: &str) {
        // A connect still in flight is dropped when its result arrives.
        self.dialing.remove(peer_id);
        let ids: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.peer_id() == Some(peer_id))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.close_session(id, "local request", false);
        }
        if self.input_target.as_deref() == Some(peer_id) {
            self.input_target = None;
        }
        self.sink.emit_value(json!({
            "event": "disconnected",
            "peer_id": peer_id,
            "reason": "local request"
        }));
    }

    fn set_input_target(&mut self, peer_id: Option<String>) {
        if self.input_target == peer_id {
            return;
        }
        // Whatever the old target holds must not stay pressed there.
        self.reset_target_input();
        self.input_target = peer_id;
    }

    fn target_session(&self) -> Option<&Session> {
        let target = self.input_target.as_deref()?;
        let id = self.established_session(target)?;
        self.sessions.get(&id)
    }

    /// Applies the shell's input, target changes and resets in call order.
    fn platform_input(&mut self, entries: Vec<PlatformEntry>) {
        for entry in entries {
            match entry {
                PlatformEntry::Event(event) => {
                    let Some(target) = self.input_target.clone() else {
                        continue;
                    };
                    let Some(id) = self.established_session(&target) else {
                        continue;
                    };
                    if self.controlling.as_deref() != Some(target.as_str()) {
                        // Control moves to the peer, and the clipboard with it.
                        self.controlling = Some(target);
                        self.sync_clipboard(id);
                    }
                    if let Err(error) = self.sessions[&id].connection.send_input(event) {
                        self.sink.emit_error(error);
                    }
                }
                PlatformEntry::SetTarget(peer_id) => self.set_input_target(peer_id),
                PlatformEntry::Reset => self.reset_target_input(),
            }
        }
    }

    fn reset_target_input(&mut self) {
        self.controlling = None;
        if let Some(session) = self.target_session() {
            session.connection.reset_input();
        }
    }

    fn accept(&mut self, stream: TcpStream) {
        // A new connection never replaces an existing session; at the limit
        // it is simply refused.
        if self.sessions.len() >= MAX_SESSIONS {
            self.sink
                .emit_error("incoming connection refused: too many sessions");
            return;
        }
        self.start_session(stream, None);
    }

    fn start_session(&mut self, stream: TcpStream, peer_id: Option<String>) {
        let id = self.next_connection_id;
        // Accepted ones may turn out to be data links (see `attach_data_link`).
        match spawn_connection(
            id,
            stream,
            WireMessage::hello(self.config),
            self.connection_events.clone(),
            TrafficClass::Interactive,
        ) {
            Ok(mut connection) => {
                self.next_connection_id += 1;
                let outgoing = peer_id.is_some();
                connection.peer_id = peer_id;
                self.sessions.insert(
                    id,
                    Session {
                        connection,
                        outgoing,
                        remote_input: RemoteInputState::default(),
                        last_seen: Instant::now(),
                        clipboard_synced: None,
                        clipboard_in: ClipboardAssembler::default(),
                        clipboard_pending: false,
                        data: None,
                        data_token: None,
                        data_invited: None,
                    },
                );
            }
            Err(error) => self
                .sink
                .emit_error(format!("connection setup failed: {error}")),
        }
    }

    fn announcement(&mut self, data: &[u8], source: SocketAddr) {
        let Ok(message) = serde_json::from_slice::<Announcement>(data) else {
            return;
        };
        if !message.is_compatible()
            || message.device_id == self.config.device_id
            || validate_displays(&message.displays).is_err()
        {
            return;
        }
        let peer = Peer {
            id: message.device_id.clone(),
            name: message.device_name,
            platform: message.platform,
            address: source.ip().to_string(),
            port: message.control_port,
            capabilities: message.capabilities,
            displays: message.displays.clone(),
            socket_address: SocketAddr::new(source.ip(), message.control_port),
            last_seen: Instant::now(),
        };
        let is_new = !self.peers.contains_key(&message.device_id);
        self.peers.insert(message.device_id.clone(), peer.clone());
        if is_new {
            self.sink
                .emit_value(json!({ "event": "peer_up", "peer": peer }));
        }
        if !message.displays.is_empty() {
            self.update_peer_displays(&message.device_id, message.displays);
        }
    }

    // ---- Arrangement ----------------------------------------------------

    /// Records a peer's displays and reports a change.
    fn update_peer_displays(&mut self, peer_id: &str, displays: Vec<Display>) {
        if self.peer_displays.get(peer_id) == Some(&displays) {
            return;
        }
        self.sink.emit_value(json!({
            "event": "peer_displays",
            "peer_id": peer_id,
            "displays": displays
        }));
        self.peer_displays.insert(peer_id.to_owned(), displays);
        // A saved placement is checked once the peer's real size is known
        // (this device's displays may also have changed while it was off).
        self.refit_placement(peer_id);
        self.publish_portals();
    }

    /// Reports where the pointer crosses into each placed peer, whenever that
    /// changes: the segments of this device's display edges that touch a
    /// peer's displays, in this device's space. Shells decide edge crossings
    /// with these instead of computing the geometry themselves.
    fn publish_portals(&mut self) {
        let mut placed: Vec<(&String, Offset)> = self
            .placements
            .values()
            .filter_map(|record| record.offset.map(|offset| (&record.peer_id, offset)))
            .collect();
        placed.sort_by(|a, b| a.0.cmp(b.0));
        let mut portals = Vec::new();
        for (peer_id, offset) in placed {
            let peer = self.peer_layout(peer_id);
            for portal in layout::portals(&self.local_displays, &peer, offset) {
                portals.push(json!({
                    "peer_id": peer_id,
                    "side": portal.side,
                    "local_display": portal.local_display,
                    "peer_display": portal.peer_display,
                    "line": portal.line,
                    "from": portal.from,
                    "to": portal.to
                }));
            }
        }
        if portals == self.portals {
            return;
        }
        self.sink.emit_value(json!({
            "event": "portals_changed",
            "portals": portals
        }));
        self.portals = portals;
    }

    /// Moves a placement that no longer fits, because displays on either side
    /// changed, to the nearest spot where the two desktops touch. Waits until
    /// the peer has described its displays: judging by the default size could
    /// move a placement that is actually fine.
    fn refit_placement(&mut self, peer_id: &str) {
        if self.local_displays.is_empty() || !self.peer_displays.contains_key(peer_id) {
            return;
        }
        let Some(offset) = self
            .placements
            .get(peer_id)
            .and_then(|record| record.offset)
        else {
            return;
        };
        let peer = self.peer_layout(peer_id);
        let obstacles = self.obstacles(peer_id);
        if layout::is_valid_placement(&self.local_displays, &peer, offset, &obstacles) {
            return;
        }
        if let Some(snapped) = layout::snap(&self.local_displays, &peer, offset, &obstacles) {
            self.store_local_placement(peer_id, Some(snapped));
        }
    }

    fn set_local_displays(&mut self, displays: Vec<Display>) {
        if let Err(error) = validate_displays(&displays) {
            self.sink.emit_error(format!("invalid displays: {error}"));
            return;
        }
        if displays == self.local_displays {
            return;
        }
        self.local_displays = displays;
        self.announcement = serde_json::to_vec(&Announcement::from_config(
            self.config,
            &self.local_displays,
        ))
        .unwrap_or_default();
        let info = WireMessage::ScreenInfo {
            displays: self.local_displays.clone(),
        };
        for session in self
            .sessions
            .values()
            .filter(|session| session.established())
        {
            let _ = session.connection.send_control(info.clone());
        }
        // A monitor was unplugged or resized: move placements that no longer
        // touch this desktop to the nearest spot that does.
        let placed: Vec<String> = self
            .placements
            .values()
            .filter(|record| record.offset.is_some())
            .map(|record| record.peer_id.clone())
            .collect();
        for peer_id in placed {
            self.refit_placement(&peer_id);
        }
        self.publish_portals();
    }

    /// The peer's displays, or a default display until it describes them.
    fn peer_layout(&self, peer_id: &str) -> Vec<Display> {
        layout::peer_or_default(
            self.peer_displays
                .get(peer_id)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )
    }

    /// Displays of every other placed peer, in this device's space.
    fn obstacles(&self, except: &str) -> Vec<Display> {
        self.placements
            .values()
            .filter(|record| record.peer_id != except)
            .filter_map(|record| {
                record
                    .offset
                    .map(|offset| layout::placed(&self.peer_layout(&record.peer_id), offset))
            })
            .flatten()
            .collect()
    }

    fn set_placement(&mut self, peer_id: &str, proposed: Option<Offset>) {
        let offset = match proposed {
            None => None,
            Some(proposed) => {
                if self.local_displays.is_empty() {
                    self.sink
                        .emit_error("cannot place a peer before the local displays are known");
                    return;
                }
                if !proposed.is_valid() {
                    self.sink.emit_error("placement offset must be finite");
                    return;
                }
                let peer = self.peer_layout(peer_id);
                let obstacles = self.obstacles(peer_id);
                match layout::snap(&self.local_displays, &peer, proposed, &obstacles) {
                    Some(snapped) => Some(snapped),
                    None => {
                        self.sink.emit_error(format!("no room to place {peer_id}"));
                        return;
                    }
                }
            }
        };
        self.store_local_placement(peer_id, offset);
    }

    fn place_beside(&mut self, peer_id: &str, side: Side) {
        if self.local_displays.is_empty() {
            self.sink
                .emit_error("cannot place a peer before the local displays are known");
            return;
        }
        let peer = self.peer_layout(peer_id);
        let obstacles = self.obstacles(peer_id);
        match layout::place_beside(&self.local_displays, &peer, side, &obstacles) {
            Some(offset) => self.store_local_placement(peer_id, Some(offset)),
            None => self
                .sink
                .emit_error(format!("no room to place {peer_id} on that side")),
        }
    }

    /// A placement decided on this device. Its timestamp is later than the
    /// record it replaces even within the same millisecond: the user acted on
    /// what they saw, so their edit must win over it on both sides.
    fn store_local_placement(&mut self, peer_id: &str, offset: Option<Offset>) {
        let seen = self
            .placements
            .get(peer_id)
            .map_or(0, |record| record.updated_ms);
        let updated_ms = now_ms().max(seen + 1);
        self.store_placement(peer_id, offset, updated_ms, "local");
    }

    /// Saves a placement, tells the shell, and syncs it to the peer when the
    /// change was made here.
    fn store_placement(
        &mut self,
        peer_id: &str,
        offset: Option<Offset>,
        updated_ms: u64,
        source: &str,
    ) {
        let record = PlacementRecord {
            peer_id: peer_id.to_owned(),
            offset,
            updated_ms,
        };
        self.placements.insert(peer_id.to_owned(), record.clone());
        self.sink.emit_value(json!({
            "event": "placement_changed",
            "peer_id": peer_id,
            "offset": offset,
            "updated_ms": updated_ms,
            "source": source
        }));
        if source == "local" {
            self.send_placement(peer_id);
        }
        self.publish_portals();
    }

    fn send_placement(&self, peer_id: &str) {
        let (Some(record), Some(id)) = (
            self.placements.get(peer_id),
            self.established_session(peer_id),
        ) else {
            return;
        };
        let _ = self.sessions[&id]
            .connection
            .send_control(WireMessage::Placement {
                offset: record.offset,
                updated_ms: record.updated_ms,
            });
    }

    /// The peer placed this device in its space. Seen from here that is the
    /// mirrored offset; the more recent edit of the two sides wins.
    fn remote_placement(&mut self, peer_id: &str, offset: Option<Offset>, updated_ms: u64) {
        if let Some(record) = self.placements.get(peer_id) {
            // An exact tie (both sides edited in the same millisecond) goes
            // to the lower device ID, so both sides settle on the same edit.
            let theirs_wins = updated_ms > record.updated_ms
                || (updated_ms == record.updated_ms
                    && peer_id < self.config.device_id.as_str()
                    && record.offset != offset.map(Offset::mirrored));
            if !theirs_wins {
                return;
            }
        }
        let mirrored = offset.map(Offset::mirrored);
        // The peer computed it against our displays as it last knew them; if
        // they changed since, fit it again (and that newer fit syncs back).
        if let Some(mirrored) = mirrored
            && !self.local_displays.is_empty()
        {
            let peer = self.peer_layout(peer_id);
            let obstacles = self.obstacles(peer_id);
            if !layout::is_valid_placement(&self.local_displays, &peer, mirrored, &obstacles)
                && let Some(snapped) =
                    layout::snap(&self.local_displays, &peer, mirrored, &obstacles)
            {
                self.store_local_placement(peer_id, Some(snapped));
                return;
            }
        }
        self.store_placement(peer_id, mirrored, updated_ms, "peer");
    }

    fn connection_event(&mut self, event: ConnectionEvent) {
        match event {
            ConnectionEvent::ConnectResult { peer_id, result } => {
                if !self.dialing.remove(&peer_id) {
                    // Cancelled by a disconnect while the connect was in flight.
                    return;
                }
                match result {
                    Ok(stream) => {
                        if self.sessions.len() >= MAX_SESSIONS {
                            self.sink.emit_value(json!({
                                "event": "disconnected",
                                "peer_id": peer_id,
                                "reason": "too many sessions"
                            }));
                            return;
                        }
                        self.start_session(stream, Some(peer_id));
                    }
                    Err(error) => self.sink.emit_value(json!({
                        "event": "disconnected",
                        "peer_id": peer_id,
                        "reason": format!("connect failed: {error}")
                    })),
                }
            }
            ConnectionEvent::Message {
                connection_id,
                message,
            } => self.message(connection_id, message),
            ConnectionEvent::DataConnectResult {
                session,
                token,
                result,
            } => match result {
                Ok(stream) => self.data_connected(session, token, stream),
                Err(error) => self.sink.emit_error(format!("data link failed: {error}")),
            },
            ConnectionEvent::FilesWritten { writer, outcome } => {
                self.files_written(writer, outcome);
            }
            ConnectionEvent::Closed {
                connection_id,
                reason,
            } => {
                if self.data_links.contains_key(&connection_id) {
                    self.close_data_link(connection_id, &reason);
                } else if self.sessions.contains_key(&connection_id) {
                    self.close_session(connection_id, &reason, true);
                }
            }
        }
    }

    fn message(&mut self, id: u64, message: WireMessage) {
        if self.data_links.contains_key(&id) {
            return self.data_message(id, message);
        }
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if !session.established() {
            match message {
                WireMessage::Hello { .. } => {}
                // An accepted connection that is really a data link.
                WireMessage::DataHello { device_id, token } => {
                    return self.attach_data_link(id, device_id, token);
                }
                _ => return self.close_session(id, "message before hello", true),
            }
        }

        match message {
            WireMessage::Hello {
                min_protocol_version,
                max_protocol_version,
                capabilities,
                device_id,
                device_name,
                control_port,
            } => self.hello(
                id,
                Hello {
                    min_protocol_version,
                    max_protocol_version,
                    capabilities,
                    device_id,
                    device_name,
                    control_port,
                },
            ),
            WireMessage::InputBatch {
                first_sequence,
                events,
            } => {
                let peer_id = session.peer_id().map(str::to_owned);
                let sink = self.sink;
                match session
                    .remote_input
                    .accept_range(first_sequence, events.len())
                {
                    SequenceStatus::Duplicate => return,
                    SequenceStatus::Gap { expected, received } => {
                        emit_release_events(
                            session.remote_input.release_events(),
                            sink,
                            "input sequence gap",
                            peer_id.as_deref(),
                        );
                        sink.emit_value(json!({
                            "event": "input_sequence_gap",
                            "peer_id": peer_id,
                            "expected": expected,
                            "received": received
                        }));
                    }
                    SequenceStatus::InOrder => {}
                }
                if session.remote_input.begin_control() {
                    sink.emit_value(
                        json!({ "event": "remote_control_started", "peer_id": peer_id }),
                    );
                }
                let shortcut_remap = session.connection.shortcut_remap;
                for mut event in events {
                    // Translate before observing, so the synthetic key-ups
                    // for held keys use the same codes the shell saw.
                    shortcut_remap.apply(&mut event);
                    session.remote_input.observe(event);
                    (sink.input)(event);
                }
            }
            WireMessage::InputReset { sequence } => {
                if session.remote_input.accept_sequence(sequence) == SequenceStatus::Duplicate {
                    return;
                }
                // The controlling peer sends a reset when it gives control back.
                let peer_id = session.peer_id().map(str::to_owned);
                end_remote_control(
                    &mut session.remote_input,
                    self.sink,
                    "remote reset",
                    peer_id.as_deref(),
                );
                // Whatever was copied here while it drove this device goes
                // back with the control.
                self.sync_clipboard(id);
            }
            WireMessage::Ping { sequence } => {
                let _ = session
                    .connection
                    .send_control(WireMessage::Pong { sequence });
            }
            WireMessage::Pong { .. } => session.last_seen = Instant::now(),
            WireMessage::ScreenInfo { displays } => {
                if let Some(peer_id) = session.peer_id().map(str::to_owned) {
                    self.update_peer_displays(&peer_id, displays);
                }
            }
            WireMessage::Placement { offset, updated_ms } => {
                if let Some(peer_id) = session.peer_id().map(str::to_owned) {
                    self.remote_placement(&peer_id, offset, updated_ms);
                }
            }
            WireMessage::DataInvite { token } => self.data_invited(id, token),
            // Data-link messages have no business on a control connection.
            WireMessage::DataHello { .. }
            | WireMessage::ClipboardChunk { .. }
            | WireMessage::FileRequest { .. }
            | WireMessage::FileChunk { .. }
            | WireMessage::FileCancel { .. } => {}
        }
    }

    fn hello(&mut self, id: u64, hello: Hello) {
        let config = self.config;
        let session = &self.sessions[&id];
        let negotiated = negotiate_protocol(hello.min_protocol_version, hello.max_protocol_version);
        let incompatible = session.established()
            || negotiated.is_none()
            || hello.capabilities & CAPABILITY_INPUT_V2 == 0
            || hello.device_id == config.device_id
            || session
                .peer_id()
                .is_some_and(|expected| expected != hello.device_id);
        if incompatible {
            self.close_session(id, "incompatible peer", true);
            return;
        }

        // One session per peer. Two connections to the same peer appear when
        // the peer reconnects before the old one timed out, or when both
        // sides dial each other at once.
        if let Some(other) = self.established_session(&hello.device_id) {
            let other_outgoing = self.sessions[&other].outgoing;
            let keep_new = if other_outgoing == session.outgoing {
                // The same side dialed again: the old connection is stale.
                true
            } else {
                // Both sides dialed: keep the connection dialed by the lower
                // device ID. Both cores compute the same answer.
                let we_are_lower = config.device_id < hello.device_id;
                session.outgoing == we_are_lower
            };
            if keep_new {
                self.close_session(other, "replaced by a newer connection", false);
            } else {
                self.close_session(id, "duplicate connection", false);
                return;
            }
        }

        let protocol_version = negotiated.expect("checked above");
        let peer_platform = self
            .peers
            .get(&hello.device_id)
            .map(|peer| peer.platform.as_str())
            .unwrap_or("unknown");
        let shortcut_remap = if config.shortcut_remap {
            ShortcutRemap::between(peer_platform, &config.platform)
        } else {
            ShortcutRemap::None
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.connection.peer_id = Some(hello.device_id.clone());
        session.connection.protocol_version = Some(protocol_version);
        session.connection.capabilities = hello.capabilities;
        session.connection.shortcut_remap = shortcut_remap;
        session.last_seen = Instant::now();
        if self.input_target.is_none() {
            // With no explicit choice, input goes to the first connected peer,
            // as it did when the core held a single connection.
            self.input_target = Some(hello.device_id.clone());
        }
        let peer = self.peer_value(
            &hello.device_id,
            Some((&hello.device_name, hello.control_port)),
            hello.capabilities,
        );
        self.sink.emit_value(json!({
            "event": "connected",
            "peer": peer,
            "protocol_version": protocol_version,
            "capabilities": hello.capabilities
        }));

        // Describe our displays, then offer our placement of the peer. The
        // peer does the same; whichever placement is newer wins on both sides.
        if let Some(session) = self.sessions.get(&id) {
            let _ = session.connection.send_control(WireMessage::ScreenInfo {
                displays: self.local_displays.clone(),
            });
        }
        self.send_placement(&hello.device_id);
        self.invite_data_link(id);
        // A disconnect held back for this peer is moot now.
        self.flush_disconnects(false);
    }

    fn peer_value(
        &self,
        peer_id: &str,
        fallback: Option<(&str, u16)>,
        capabilities: u64,
    ) -> serde_json::Value {
        match self.peers.get(peer_id) {
            Some(peer) => json!(peer),
            None => {
                let (name, port) = fallback.unwrap_or((peer_id, 0));
                json!({
                    "id": peer_id,
                    "name": name,
                    "platform": "unknown",
                    "address": "incoming",
                    "port": port,
                    "capabilities": capabilities
                })
            }
        }
    }

    /// Closes a session and releases everything its peer held. `notify`
    /// reports `disconnected`; it is skipped for duplicates that another
    /// session with the same peer replaces, and for connections that never
    /// identified themselves.
    fn close_session(&mut self, id: u64, reason: &str, notify: bool) {
        let Some(mut session) = self.sessions.remove(&id) else {
            return;
        };
        let peer_id = session.peer_id().map(str::to_owned);
        session.connection.abort();
        if let Some(link) = session.data {
            self.close_data_link(link, reason);
        }
        if self.download_session() == Some(id) {
            self.fail_download(reason, false);
        }
        end_remote_control(
            &mut session.remote_input,
            self.sink,
            reason,
            peer_id.as_deref(),
        );
        if notify && let Some(peer_id) = peer_id {
            self.notify_disconnected(peer_id, reason);
        } else {
            // A connection that might have been a held-back peer is gone.
            self.flush_disconnects(false);
        }
    }

    /// Reports a lost session, unless another connection may still turn out
    /// to be the same peer: a duplicate that is still handshaking (incoming
    /// connections are anonymous until their `Hello`) replaces it without the
    /// shell ever seeing it go away. Held back at most [`DISCONNECT_GRACE`].
    fn notify_disconnected(&mut self, peer_id: String, reason: &str) {
        self.pending_disconnects.push(PendingDisconnect {
            peer_id,
            reason: reason.to_owned(),
            deadline: Instant::now() + DISCONNECT_GRACE,
        });
        self.flush_disconnects(false);
    }

    /// Emits held-back disconnects that are now certain and drops the ones a
    /// replacement session made moot. `all` emits everything still pending.
    fn flush_disconnects(&mut self, all: bool) {
        let pending = std::mem::take(&mut self.pending_disconnects);
        for entry in pending {
            if self.established_session(&entry.peer_id).is_some() {
                continue;
            }
            let may_reconnect = self.sessions.values().any(|session| {
                session
                    .peer_id()
                    .is_none_or(|peer_id| peer_id == entry.peer_id)
            });
            if all || !may_reconnect || Instant::now() >= entry.deadline {
                self.sink.emit_value(json!({
                    "event": "disconnected",
                    "peer_id": entry.peer_id,
                    "reason": entry.reason
                }));
            } else {
                self.pending_disconnects.push(entry);
            }
        }
    }

    fn close_all(&mut self, reason: &str) {
        let ids: Vec<u64> = self.sessions.keys().copied().collect();
        for id in ids {
            self.close_session(id, reason, true);
        }
        self.flush_disconnects(true);
    }

    fn maintenance(&mut self) {
        self.flush_disconnects(false);
        let expired: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.last_seen.elapsed() > PEER_EXPIRY)
            .map(|(id, _)| id.clone())
            .collect();
        for peer_id in expired {
            self.peers.remove(&peer_id);
            self.sink
                .emit_value(json!({ "event": "peer_down", "peer_id": peer_id }));
        }

        self.ping_sequence = self.ping_sequence.wrapping_add(1);
        let mut stale = Vec::new();
        for (id, session) in &self.sessions {
            if session.last_seen.elapsed() > SESSION_TIMEOUT {
                stale.push((
                    *id,
                    if session.established() {
                        "heartbeat timeout"
                    } else {
                        "handshake timeout"
                    },
                ));
            } else if session.established() {
                let _ = session.connection.send_control(WireMessage::Ping {
                    sequence: self.ping_sequence,
                });
            }
        }
        for (id, reason) in stale {
            self.close_session(id, reason, true);
        }
        self.reinvite_data_links();
    }
}

struct Hello {
    min_protocol_version: u16,
    max_protocol_version: u16,
    capabilities: u64,
    device_id: String,
    device_name: String,
    control_port: u16,
}

/// Releases everything the peer still holds, then reports that remote
/// control ended. The releases go out first, so a shell that stops playing
/// input on `remote_control_ended` has already lifted every key and button.
fn end_remote_control(
    remote_input: &mut RemoteInputState,
    sink: &EventSink,
    reason: &str,
    peer_id: Option<&str>,
) {
    emit_release_events(remote_input.release_events(), sink, reason, peer_id);
    if remote_input.end_control() {
        sink.emit_value(json!({
            "event": "remote_control_ended",
            "peer_id": peer_id,
            "reason": reason
        }));
    }
}

fn emit_release_events(
    events: Vec<LcInputEvent>,
    sink: &EventSink,
    reason: &str,
    peer_id: Option<&str>,
) {
    if events.is_empty() {
        return;
    }
    let count = events.len();
    for event in events {
        (sink.input)(event);
    }
    sink.emit_value(json!({
        "event": "input_reset",
        "peer_id": peer_id,
        "reason": reason,
        "released_event_count": count
    }));
}

fn create_discovery_socket(port: u16) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_broadcast(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    socket.join_multicast_v4(&Ipv4Addr::from(DISCOVERY_GROUP), &Ipv4Addr::UNSPECIFIED)?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into())
}
