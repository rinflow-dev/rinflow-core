use crate::input::{OutboundInputQueue, PushOutcome};
use crate::model::{LcInputEvent, MAX_FRAME_SIZE, WireMessage};
use crate::protocol::{decode_message, encode_message};
use crate::qos::{Qos, TrafficClass};
use crate::remap::ShortcutRemap;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

/// `disconnected` reason when the peer closed the connection (it quit, was
/// told to disconnect, or dropped a duplicate connection): an orderly end,
/// unlike the `read failed: …` / `write failed: …` reasons.
pub const PEER_CLOSED: &str = "peer closed the connection";

#[derive(Debug)]
pub enum ConnectionEvent {
    Message {
        connection_id: u64,
        message: WireMessage,
    },
    Closed {
        connection_id: u64,
        reason: String,
    },
    ConnectResult {
        peer_id: String,
        result: Result<TcpStream, String>,
    },
    /// The data link a session asked for (see `DataInvite`).
    DataConnectResult {
        session: u64,
        token: u64,
        result: Result<TcpStream, String>,
    },
    /// The disk writer of a download (see `runtime/data.rs`) wrote every
    /// file queued to it, or failed.
    FilesWritten {
        writer: u64,
        outcome: Result<(), String>,
    },
}

/// Bytes the system may hold for a connection before a write waits. Small on
/// purpose: what sits in the socket buffer is sent in order whatever this
/// side's priorities are, so a few megabytes of file data there would hold
/// input up for a second on Wi-Fi. 128 KiB still fills a LAN.
const SEND_BUFFER_SIZE: usize = 128 * 1024;

pub struct ActiveConnection {
    pub peer_id: Option<String>,
    /// Where the connection goes; a data link is dialed to the same place.
    pub remote: Option<SocketAddr>,
    pub protocol_version: Option<u16>,
    pub capabilities: u64,
    /// Applied to input received from this peer; set once its platform is known.
    pub shortcut_remap: ShortcutRemap,
    control: mpsc::Sender<WireMessage>,
    input: OutboundInputQueue,
    bulk: BulkQueue,
    files: mpsc::Sender<WireMessage>,
    qos: Qos,
    read_task: JoinHandle<()>,
    write_task: JoinHandle<()>,
}

impl ActiveConnection {
    /// What the connection carries, for the network's queues (see `qos.rs`).
    /// An accepted connection turns out to be a data link only once its
    /// `DataHello` arrives.
    pub fn set_traffic_class(&self, class: TrafficClass) {
        self.qos.set(class);
    }

    pub fn send_control(&self, message: WireMessage) -> Result<(), &'static str> {
        self.control
            .try_send(message)
            .map_err(|_| "outgoing control queue is full")
    }

    pub fn send_input(&self, event: LcInputEvent) -> Result<PushOutcome, &'static str> {
        self.input.push(event)
    }

    pub fn reset_input(&self) {
        self.input.reset();
    }

    /// Queues a bulk transfer (clipboard chunks) behind input, replacing one
    /// still queued: newer contents make the rest of older ones pointless.
    pub fn send_bulk(&self, messages: Vec<WireMessage>) {
        self.bulk.replace(messages);
    }

    /// For tasks that send on their own: small messages ahead of bulk data.
    pub fn control_sender(&self) -> mpsc::Sender<WireMessage> {
        self.control.clone()
    }

    /// File chunks, sent last. Bounded: a sender waits while the connection
    /// is busy, so a file is read only as fast as it goes out.
    pub fn file_sender(&self) -> mpsc::Sender<WireMessage> {
        self.files.clone()
    }

    pub fn abort(self) {
        self.read_task.abort();
        self.write_task.abort();
    }
}

/// Frames sent only when no control message or input is waiting, so a large
/// transfer delays input by at most one frame.
#[derive(Clone, Default)]
struct BulkQueue {
    shared: Arc<(Mutex<VecDeque<WireMessage>>, Notify)>,
}

impl BulkQueue {
    fn replace(&self, messages: Vec<WireMessage>) {
        let mut queue = self.shared.0.lock().expect("bulk queue poisoned");
        queue.clear();
        queue.extend(messages);
        self.shared.1.notify_one();
    }

    fn pop(&self) -> Option<WireMessage> {
        self.shared
            .0
            .lock()
            .expect("bulk queue poisoned")
            .pop_front()
    }

    async fn notified(&self) {
        self.shared.1.notified().await;
    }
}

pub fn spawn_connection(
    connection_id: u64,
    stream: TcpStream,
    hello: WireMessage,
    events: mpsc::UnboundedSender<ConnectionEvent>,
    class: TrafficClass,
) -> io::Result<ActiveConnection> {
    stream.set_nodelay(true)?;
    socket2::SockRef::from(&stream).set_send_buffer_size(SEND_BUFFER_SIZE)?;
    let qos = Qos::new(&stream);
    qos.set(class);
    let remote = stream.peer_addr().ok();
    let (reader, writer) = stream.into_split();
    let (control_tx, control_rx) = mpsc::channel(64);
    let input = OutboundInputQueue::new();
    let bulk = BulkQueue::default();
    let (files_tx, files_rx) = mpsc::channel(4);

    let read_events = events.clone();
    let read_task = tokio::spawn(async move {
        let reason = match read_loop(connection_id, reader, read_events.clone()).await {
            Ok(()) => PEER_CLOSED.to_owned(),
            Err(error) => format!("read failed: {error}"),
        };
        let _ = read_events.send(ConnectionEvent::Closed {
            connection_id,
            reason,
        });
    });

    let write_events = events;
    let write_input = input.clone();
    let write_bulk = bulk.clone();
    let write_task = tokio::spawn(async move {
        if let Err(error) =
            write_loop(writer, control_rx, write_input, write_bulk, files_rx, hello).await
        {
            let _ = write_events.send(ConnectionEvent::Closed {
                connection_id,
                reason: format!("write failed: {error}"),
            });
        }
    });

    Ok(ActiveConnection {
        peer_id: None,
        remote,
        protocol_version: None,
        capabilities: 0,
        shortcut_remap: ShortcutRemap::None,
        control: control_tx,
        input,
        bulk,
        files: files_tx,
        qos,
        read_task,
        write_task,
    })
}

async fn read_loop(
    connection_id: u64,
    mut reader: OwnedReadHalf,
    events: mpsc::UnboundedSender<ConnectionEvent>,
) -> io::Result<()> {
    loop {
        let length = match reader.read_u32().await {
            Ok(length) => length as usize,
            // The stream ended between frames: the peer closed the connection.
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        if length == 0 || length > MAX_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid frame length: {length}"),
            ));
        }

        let mut payload = vec![0_u8; length];
        reader.read_exact(&mut payload).await?;
        let message = decode_message(&payload)?;

        if events
            .send(ConnectionEvent::Message {
                connection_id,
                message,
            })
            .is_err()
        {
            return Ok(());
        }
    }
}

async fn write_loop(
    mut writer: OwnedWriteHalf,
    mut controls: mpsc::Receiver<WireMessage>,
    input: OutboundInputQueue,
    bulk: BulkQueue,
    mut files: mpsc::Receiver<WireMessage>,
    hello: WireMessage,
) -> io::Result<()> {
    write_frame(&mut writer, &hello).await?;
    loop {
        // Check control between every input batch so keepalives and handshake
        // responses cannot be starved by a high-rate pointing device.
        if let Ok(message) = controls.try_recv() {
            write_frame(&mut writer, &message).await?;
            continue;
        }
        if let Some(message) = input.pop_message() {
            write_frame(&mut writer, &message).await?;
            continue;
        }
        // Bulk data only when nothing else waits, one frame at a time:
        // clipboards before files.
        if let Some(message) = bulk.pop() {
            write_frame(&mut writer, &message).await?;
            continue;
        }
        if let Ok(message) = files.try_recv() {
            write_frame(&mut writer, &message).await?;
            continue;
        }

        tokio::select! {
            biased;
            message = controls.recv() => {
                let Some(message) = message else { break };
                write_frame(&mut writer, &message).await?;
            }
            _ = input.notified() => {}
            _ = bulk.notified() => {}
            Some(message) = files.recv() => write_frame(&mut writer, &message).await?,
        }
    }
    writer.shutdown().await
}

async fn write_frame(writer: &mut OwnedWriteHalf, message: &WireMessage) -> io::Result<()> {
    let payload = encode_message(message)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "outgoing frame exceeds maximum size",
        ));
    }
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

/// Dials the data link for session `session` (see `DataInvite`).
pub fn connect_data_in_background(
    session: u64,
    token: u64,
    address: SocketAddr,
    events: mpsc::UnboundedSender<ConnectionEvent>,
) {
    tokio::spawn(async move {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            TcpStream::connect(address),
        )
        .await
        .map_err(|_| "connection timed out".to_owned())
        .and_then(|result| result.map_err(|error| error.to_string()));
        let _ = events.send(ConnectionEvent::DataConnectResult {
            session,
            token,
            result,
        });
    });
}

pub fn connect_in_background(
    peer_id: String,
    address: SocketAddr,
    events: mpsc::UnboundedSender<ConnectionEvent>,
) {
    tokio::spawn(async move {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            TcpStream::connect(address),
        )
        .await
        .map_err(|_| "connection timed out".to_owned())
        .and_then(|result| result.map_err(|error| error.to_string()));
        let _ = events.send(ConnectionEvent::ConnectResult { peer_id, result });
    });
}
