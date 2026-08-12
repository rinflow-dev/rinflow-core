use crate::input::{PlatformInputQueue, RemoteInputState, SequenceStatus};
use crate::model::{
    Announcement, CAPABILITY_INPUT_V2, CoreConfig, DISCOVERY_GROUP, LcInputEvent, Peer,
    WireMessage, negotiate_protocol,
};
use crate::transport::{
    ActiveConnection, ConnectionEvent, connect_in_background, spawn_connection,
};
use serde_json::json;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;

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
    Connect(String),
    Disconnect,
    SendInput(LcInputEvent),
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
                if should_notify {
                    self.commands
                        .send(Command::InputReady)
                        .map_err(|_| "LinkCursor core is not running".to_owned())?;
                }
                Ok(())
            }
            command => self
                .commands
                .send(command)
                .map_err(|_| "LinkCursor core is not running".to_owned()),
        }
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
    let mut peers: HashMap<String, Peer> = HashMap::new();
    let mut active: Option<ActiveConnection> = None;
    let mut remote_input = RemoteInputState::default();
    let mut next_connection_id = 1_u64;
    let mut ping_sequence = 0_u64;
    let mut last_pong = Instant::now();
    let mut buffer = vec![0_u8; 64 * 1024];

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
                    Some(Command::Connect(peer_id)) => {
                        if let Some(peer) = peers.get(&peer_id) {
                            sink.emit_value(json!({ "event": "connecting", "peer_id": peer_id }));
                            connect_in_background(peer_id, peer.socket_address, connection_events_tx.clone());
                        } else {
                            sink.emit_error(format!("peer is no longer available: {peer_id}"));
                        }
                    }
                    Some(Command::Disconnect) => {
                        disconnect_active(&mut active, &mut remote_input, &sink, "local disconnect");
                        sink.emit_value(json!({ "event": "disconnected", "reason": "local request" }));
                    }
                    Some(Command::InputReady) => {
                        for event in platform_input.drain() {
                            let Some(connection) = active.as_ref().filter(|connection| connection.protocol_version.is_some()) else {
                                continue;
                            };
                            if let Err(error) = connection.send_input(event) {
                                sink.emit_error(error);
                            }
                        }
                    }
                    Some(Command::ResetInput) => {
                        // No stale platform input may be written after the reset.
                        let _ = platform_input.drain();
                        if let Some(connection) = active.as_ref().filter(|connection| connection.protocol_version.is_some()) {
                            connection.reset_input();
                        }
                    }
                    Some(Command::SendInput(_)) => unreachable!("input commands are intercepted by CoreThread"),
                    Some(Command::Stop) | None => break,
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _address)) => {
                        disconnect_active(&mut active, &mut remote_input, &sink, "connection replaced");
                        match spawn_connection(next_connection_id, stream, WireMessage::hello(&config), connection_events_tx.clone()) {
                            Ok(connection) => {
                                next_connection_id += 1;
                                last_pong = Instant::now();
                                active = Some(connection);
                            }
                            Err(error) => sink.emit_error(format!("incoming connection failed: {error}")),
                        }
                    }
                    Err(error) => sink.emit_error(format!("accept failed: {error}")),
                }
            }
            received = udp.recv_from(&mut buffer) => {
                if let Ok((length, source)) = received
                    && let Ok(message) = serde_json::from_slice::<Announcement>(&buffer[..length])
                    && message.is_compatible()
                    && message.device_id != config.device_id
                {
                    let socket_address = SocketAddr::new(source.ip(), message.control_port);
                    let peer = Peer {
                        id: message.device_id.clone(),
                        name: message.device_name,
                        address: source.ip().to_string(),
                        port: message.control_port,
                        capabilities: message.capabilities,
                        socket_address,
                        last_seen: Instant::now(),
                    };
                    let is_new = !peers.contains_key(&message.device_id);
                    peers.insert(message.device_id, peer.clone());
                    if is_new {
                        sink.emit_value(json!({ "event": "peer_up", "peer": peer }));
                    }
                }
            }
            connection_event = connection_events_rx.recv() => {
                if let Some(event) = connection_event {
                    handle_connection_event(event, ConnectionEventContext {
                        config: &config,
                        peers: &peers,
                        active: &mut active,
                        remote_input: &mut remote_input,
                        next_connection_id: &mut next_connection_id,
                        events: &connection_events_tx,
                        sink: &sink,
                        last_pong: &mut last_pong,
                    });
                }
            }
            _ = announce_timer.tick() => {
                let _ = udp.send_to(&announcement, multicast).await;
                // Broadcast is a compatibility fallback for networks or VPN setups that
                // suppress multicast. Receivers deduplicate announcements by device ID.
                let _ = udp.send_to(&announcement, broadcast).await;
            }
            _ = maintenance_timer.tick() => {
                let expired: Vec<String> = peers
                    .iter()
                    .filter(|(_, peer)| peer.last_seen.elapsed() > Duration::from_secs(7))
                    .map(|(id, _)| id.clone())
                    .collect();
                for peer_id in expired {
                    peers.remove(&peer_id);
                    sink.emit_value(json!({ "event": "peer_down", "peer_id": peer_id }));
                }

                if let Some(connection) = active.as_ref().filter(|connection| connection.protocol_version.is_some()) {
                    ping_sequence = ping_sequence.wrapping_add(1);
                    let _ = connection.send_control(WireMessage::Ping { sequence: ping_sequence });
                    if last_pong.elapsed() > Duration::from_secs(8) {
                        disconnect_active(&mut active, &mut remote_input, &sink, "heartbeat timeout");
                        sink.emit_value(json!({ "event": "disconnected", "reason": "heartbeat timeout" }));
                    }
                }
            }
        }
    }

    disconnect_active(&mut active, &mut remote_input, &sink, "core stopped");
    sink.emit_value(json!({ "event": "stopped" }));
}

struct ConnectionEventContext<'a> {
    config: &'a CoreConfig,
    peers: &'a HashMap<String, Peer>,
    active: &'a mut Option<ActiveConnection>,
    remote_input: &'a mut RemoteInputState,
    next_connection_id: &'a mut u64,
    events: &'a mpsc::UnboundedSender<ConnectionEvent>,
    sink: &'a EventSink,
    last_pong: &'a mut Instant,
}

fn handle_connection_event(event: ConnectionEvent, context: ConnectionEventContext<'_>) {
    let ConnectionEventContext {
        config,
        peers,
        active,
        remote_input,
        next_connection_id,
        events,
        sink,
        last_pong,
    } = context;
    match event {
        ConnectionEvent::ConnectResult { peer_id, result } => match result {
            Ok(stream) => {
                disconnect_active(active, remote_input, sink, "connection replaced");
                match spawn_connection(
                    *next_connection_id,
                    stream,
                    WireMessage::hello(config),
                    events.clone(),
                ) {
                    Ok(mut connection) => {
                        connection.peer_id = Some(peer_id);
                        *next_connection_id += 1;
                        *last_pong = Instant::now();
                        *active = Some(connection);
                    }
                    Err(error) => sink.emit_error(format!("connection setup failed: {error}")),
                }
            }
            Err(error) => sink.emit_value(json!({
                "event": "disconnected",
                "reason": format!("connect failed: {error}")
            })),
        },
        ConnectionEvent::Message {
            connection_id,
            message,
        } => {
            if active.as_ref().map(|connection| connection.id) != Some(connection_id) {
                return;
            }
            if !matches!(&message, WireMessage::Hello { .. })
                && active
                    .as_ref()
                    .and_then(|connection| connection.protocol_version)
                    .is_none()
            {
                disconnect_active(active, remote_input, sink, "message before hello");
                sink.emit_value(
                    json!({ "event": "disconnected", "reason": "message before hello" }),
                );
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
                } => {
                    let expected_peer = active
                        .as_ref()
                        .and_then(|connection| connection.peer_id.as_deref());
                    let negotiated = negotiate_protocol(min_protocol_version, max_protocol_version);
                    let incompatible = active
                        .as_ref()
                        .is_some_and(|connection| connection.protocol_version.is_some())
                        || negotiated.is_none()
                        || capabilities & CAPABILITY_INPUT_V2 == 0
                        || device_id == config.device_id
                        || expected_peer.is_some_and(|expected| expected != device_id);
                    if incompatible {
                        disconnect_active(active, remote_input, sink, "incompatible peer");
                        sink.emit_value(
                            json!({ "event": "disconnected", "reason": "incompatible peer" }),
                        );
                        return;
                    }
                    let protocol_version = negotiated.expect("checked above");
                    if let Some(connection) = active.as_mut() {
                        connection.peer_id = Some(device_id.clone());
                        connection.protocol_version = Some(protocol_version);
                        connection.capabilities = capabilities;
                    }
                    emit_release_events(remote_input.reset_for_connection(), sink, "new session");
                    *last_pong = Instant::now();
                    let peer_value = peers
                        .get(&device_id)
                        .map(|peer| json!(peer))
                        .unwrap_or_else(|| {
                            json!({
                                "id": device_id,
                                "name": device_name,
                                "address": "incoming",
                                "port": control_port,
                                "capabilities": capabilities
                            })
                        });
                    sink.emit_value(json!({
                        "event": "connected",
                        "peer": peer_value,
                        "protocol_version": protocol_version,
                        "capabilities": capabilities
                    }));
                }
                WireMessage::InputBatch {
                    first_sequence,
                    events,
                } => {
                    match remote_input.accept_range(first_sequence, events.len()) {
                        SequenceStatus::Duplicate => return,
                        SequenceStatus::Gap { expected, received } => {
                            emit_release_events(
                                remote_input.release_events(),
                                sink,
                                "input sequence gap",
                            );
                            sink.emit_value(json!({
                                "event": "input_sequence_gap",
                                "expected": expected,
                                "received": received
                            }));
                        }
                        SequenceStatus::InOrder => {}
                    }
                    for event in events {
                        remote_input.observe(event);
                        (sink.input)(event);
                    }
                }
                WireMessage::InputReset { sequence } => {
                    if remote_input.accept_sequence(sequence) == SequenceStatus::Duplicate {
                        return;
                    }
                    emit_release_events(remote_input.release_events(), sink, "remote reset");
                }
                WireMessage::Ping { sequence } => {
                    if let Some(connection) = active.as_ref() {
                        let _ = connection.send_control(WireMessage::Pong { sequence });
                    }
                }
                WireMessage::Pong { .. } => *last_pong = Instant::now(),
            }
        }
        ConnectionEvent::Closed {
            connection_id,
            reason,
        } => {
            if active.as_ref().map(|connection| connection.id) == Some(connection_id) {
                disconnect_active(active, remote_input, sink, "connection closed");
                sink.emit_value(json!({ "event": "disconnected", "reason": reason }));
            }
        }
    }
}

fn disconnect_active(
    active: &mut Option<ActiveConnection>,
    remote_input: &mut RemoteInputState,
    sink: &EventSink,
    reason: &str,
) {
    if let Some(connection) = active.take() {
        connection.abort();
    }
    emit_release_events(remote_input.release_events(), sink, reason);
}

fn emit_release_events(events: Vec<LcInputEvent>, sink: &EventSink, reason: &str) {
    if events.is_empty() {
        return;
    }
    let count = events.len();
    for event in events {
        (sink.input)(event);
    }
    sink.emit_value(json!({
        "event": "input_reset",
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
