use crate::input::{PlatformEntry, PlatformInputQueue, RemoteInputState, SequenceStatus};
use crate::model::{
    Announcement, CAPABILITY_INPUT_V2, CoreConfig, DISCOVERY_GROUP, LcInputEvent, Peer,
    WireMessage, negotiate_protocol,
};
use crate::remap::ShortcutRemap;
use crate::transport::{
    ActiveConnection, ConnectionEvent, connect_in_background, spawn_connection,
};
use serde_json::json;
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
    let announcement = serde_json::to_vec(&Announcement::from_config(&config)).unwrap_or_default();
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
    };

    sink.emit_value(json!({
        "event": "started",
        "device_id": config.device_id,
        "control_port": config.control_port,
        "discovery_port": config.discovery_port
    }));

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(Command::Connect(peer_id)) => state.connect(peer_id),
                    Some(Command::Disconnect) => state.disconnect_target(),
                    Some(Command::DisconnectPeer(peer_id)) => state.disconnect_peer(&peer_id),
                    Some(Command::InputReady) => state.platform_input(platform_input.drain()),
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
                    if let Some(session) = self.target_session()
                        && let Err(error) = session.connection.send_input(event)
                    {
                        self.sink.emit_error(error);
                    }
                }
                PlatformEntry::SetTarget(peer_id) => self.set_input_target(peer_id),
                PlatformEntry::Reset => self.reset_target_input(),
            }
        }
    }

    fn reset_target_input(&self) {
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
        match spawn_connection(
            id,
            stream,
            WireMessage::hello(self.config),
            self.connection_events.clone(),
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
        if !message.is_compatible() || message.device_id == self.config.device_id {
            return;
        }
        let peer = Peer {
            id: message.device_id.clone(),
            name: message.device_name,
            platform: message.platform,
            address: source.ip().to_string(),
            port: message.control_port,
            capabilities: message.capabilities,
            socket_address: SocketAddr::new(source.ip(), message.control_port),
            last_seen: Instant::now(),
        };
        let is_new = !self.peers.contains_key(&message.device_id);
        self.peers.insert(message.device_id, peer.clone());
        if is_new {
            self.sink
                .emit_value(json!({ "event": "peer_up", "peer": peer }));
        }
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
            ConnectionEvent::Closed {
                connection_id,
                reason,
            } => {
                if self.sessions.contains_key(&connection_id) {
                    self.close_session(connection_id, &reason, true);
                }
            }
        }
    }

    fn message(&mut self, id: u64, message: WireMessage) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if !matches!(&message, WireMessage::Hello { .. }) && !session.established() {
            self.close_session(id, "message before hello", true);
            return;
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
            }
            WireMessage::Ping { sequence } => {
                let _ = session
                    .connection
                    .send_control(WireMessage::Pong { sequence });
            }
            WireMessage::Pong { .. } => session.last_seen = Instant::now(),
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
        end_remote_control(
            &mut session.remote_input,
            self.sink,
            reason,
            peer_id.as_deref(),
        );
        if notify && let Some(peer_id) = peer_id {
            self.sink.emit_value(json!({
                "event": "disconnected",
                "peer_id": peer_id,
                "reason": reason
            }));
        }
    }

    fn close_all(&mut self, reason: &str) {
        let ids: Vec<u64> = self.sessions.keys().copied().collect();
        for id in ids {
            self.close_session(id, reason, true);
        }
    }

    fn maintenance(&mut self) {
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
