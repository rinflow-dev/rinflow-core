//! Connections to peers: QUIC with TLS 1.3 (see `quic.rs` and `secure.rs`).
//! Each connection carries one bidirectional stream of length-prefixed
//! frames (see `protocol.rs`), opened by the side that dialed.

use crate::input::{OutboundInputQueue, PushOutcome};
use crate::model::{LcInputEvent, MAX_FRAME_SIZE, WireMessage};
use crate::protocol::{decode_message, encode_message};
use crate::remap::ShortcutRemap;
use crate::secure;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

/// `disconnected` reason when the peer closed the connection (it quit, was
/// told to disconnect, or dropped a duplicate connection): an orderly end,
/// unlike the `read failed: …` / `write failed: …` reasons.
pub const PEER_CLOSED: &str = "peer closed the connection";

/// `disconnected` reason when the peer's user declined pairing.
pub const PAIRING_DECLINED_BY_PEER: &str = "pairing declined on the other device";

/// `disconnected` reason when the peer's user (or shell) disconnected.
pub const PEER_DISCONNECTED: &str = "disconnected on the other device";

/// `disconnected` reason when nobody confirmed pairing in time, on either
/// side.
pub const PAIRING_TIMED_OUT: &str = "pairing timed out";

/// `disconnected` reason when the peer would not let this device in: it
/// is paired with another certificate under this device's ID, it now only
/// takes paired devices, or it forgot this one.
pub const PEER_REFUSED: &str = "refused by the other device";

/// Why this side closes a connection, sent as the application error code of
/// its QUIC CONNECTION_CLOSE frame. A message queued just before closing
/// would be dropped with the connection; the close frame always arrives.
/// The peer also learns from it whether to reconnect by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseCode {
    /// An orderly close (this device quit, or dropped a duplicate
    /// connection): the peer reports [`PEER_CLOSED`].
    Closed = 0,
    /// The user declined pairing: [`PAIRING_DECLINED_BY_PEER`].
    PairingDeclined = 1,
    /// The user disconnected: [`PEER_DISCONNECTED`].
    Disconnected = 2,
    /// This device would not let the peer in: [`PEER_REFUSED`].
    Refused = 3,
    /// Pairing was not confirmed in time: [`PAIRING_TIMED_OUT`].
    PairingTimedOut = 4,
}

impl CloseCode {
    /// The `disconnected` reason for a close the peer sent with `code`.
    fn reason(code: u64) -> &'static str {
        match code {
            1 => PAIRING_DECLINED_BY_PEER,
            2 => PEER_DISCONNECTED,
            3 => PEER_REFUSED,
            4 => PAIRING_TIMED_OUT,
            _ => PEER_CLOSED,
        }
    }
}

/// How long dialing (the QUIC handshake) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// A QUIC connection and its one bidirectional stream, encrypted with
/// TLS 1.3; the peer's certificate is known.
pub struct Link {
    pub connection: quinn::Connection,
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
}

impl Link {
    pub fn remote(&self) -> SocketAddr {
        self.connection.remote_address()
    }
}

impl std::fmt::Debug for Link {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Link({})", self.remote())
    }
}

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
        result: Result<Link, String>,
    },
    /// The data link a session asked for (see `DataInvite`).
    DataConnectResult {
        session: u64,
        token: u64,
        result: Result<Link, String>,
    },
    /// A connection that came in and opened its stream; `bulk` when it came
    /// to the bulk endpoint (a data link).
    Accepted {
        link: Link,
        bulk: bool,
    },
    /// The disk writer of a download (see `runtime/data.rs`) wrote every
    /// file queued to it, or failed.
    FilesWritten {
        writer: u64,
        outcome: Result<(), String>,
    },
    /// A manually added address was looked up (see `runtime/manual.rs`).
    ManualResolved {
        address: String,
        result: Result<Vec<SocketAddr>, String>,
    },
}

pub struct ActiveConnection {
    pub peer_id: Option<String>,
    /// Where the connection goes; a data link is dialed to the same address.
    pub remote: SocketAddr,
    pub protocol_version: Option<u16>,
    pub capabilities: u64,
    /// Applied to input received from this peer; set once its platform is known.
    pub shortcut_remap: ShortcutRemap,
    /// SHA-256 of the certificate the peer presented (64 hex digits).
    pub fingerprint: String,
    control: mpsc::Sender<WireMessage>,
    input: OutboundInputQueue,
    bulk: BulkQueue,
    files: mpsc::Sender<WireMessage>,
    connection: quinn::Connection,
    read_task: JoinHandle<()>,
    write_task: JoinHandle<()>,
}

impl ActiveConnection {
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

    /// Closes the connection now, telling the peer why. The connection is
    /// still to be aborted; its close then changes nothing.
    pub fn close_with(&self, code: CloseCode) {
        self.connection.close((code as u32).into(), b"");
    }

    pub fn abort(self) {
        self.read_task.abort();
        self.write_task.abort();
        // The peer sees an orderly close (`PEER_CLOSED`), unless `close_with`
        // gave another reason first.
        self.close_with(CloseCode::Closed);
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

/// Starts reading and writing `link`; `hello` is the first frame sent.
pub fn spawn_connection(
    connection_id: u64,
    link: Link,
    hello: WireMessage,
    events: mpsc::UnboundedSender<ConnectionEvent>,
) -> Result<ActiveConnection, String> {
    // Both sides must show a certificate (see `secure.rs`), so this only
    // fails if the handshake somehow let one through without.
    let fingerprint =
        secure::peer_fingerprint(&link.connection).ok_or("the peer showed no certificate")?;
    let remote = link.remote();
    let (control_tx, control_rx) = mpsc::channel(64);
    let input = OutboundInputQueue::new();
    let bulk = BulkQueue::default();
    let (files_tx, files_rx) = mpsc::channel(4);
    let queues = Queues {
        controls: control_rx,
        input: input.clone(),
        bulk: bulk.clone(),
        files: files_rx,
    };
    let (read_task, write_task) =
        spawn_loops(connection_id, link.recv, link.send, queues, hello, events);

    Ok(ActiveConnection {
        peer_id: None,
        remote,
        protocol_version: None,
        capabilities: 0,
        shortcut_remap: ShortcutRemap::None,
        fingerprint,
        control: control_tx,
        input,
        bulk,
        files: files_tx,
        connection: link.connection,
        read_task,
        write_task,
    })
}

/// What the write loop sends, in priority order.
struct Queues {
    controls: mpsc::Receiver<WireMessage>,
    input: OutboundInputQueue,
    bulk: BulkQueue,
    files: mpsc::Receiver<WireMessage>,
}

fn spawn_loops<R, W>(
    connection_id: u64,
    reader: R,
    writer: W,
    queues: Queues,
    hello: WireMessage,
    events: mpsc::UnboundedSender<ConnectionEvent>,
) -> (JoinHandle<()>, JoinHandle<()>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let read_events = events.clone();
    let read_task = tokio::spawn(async move {
        let reason = match read_loop(connection_id, reader, read_events.clone()).await {
            Ok(()) => PEER_CLOSED.to_owned(),
            Err(error) => match closed_by_peer(&error) {
                Some(reason) => reason.to_owned(),
                None => format!("read failed: {error}"),
            },
        };
        let _ = read_events.send(ConnectionEvent::Closed {
            connection_id,
            reason,
        });
    });

    let write_task = tokio::spawn(async move {
        if let Err(error) = write_loop(writer, queues, hello).await {
            let reason = match closed_by_peer(&error) {
                Some(reason) => reason.to_owned(),
                None => format!("write failed: {error}"),
            };
            let _ = events.send(ConnectionEvent::Closed {
                connection_id,
                reason,
            });
        }
    });
    (read_task, write_task)
}

/// The peer closed the connection (as `ActiveConnection::abort` does): the
/// `disconnected` reason for the code it closed with.
fn closed_by_peer(error: &io::Error) -> Option<&'static str> {
    let inner = error.get_ref()?;
    let lost = inner
        .downcast_ref::<quinn::ReadError>()
        .and_then(|error| match error {
            quinn::ReadError::ConnectionLost(lost) => Some(lost),
            _ => None,
        })
        .or_else(|| {
            inner
                .downcast_ref::<quinn::WriteError>()
                .and_then(|error| match error {
                    quinn::WriteError::ConnectionLost(lost) => Some(lost),
                    _ => None,
                })
        });
    match lost? {
        quinn::ConnectionError::ApplicationClosed(close) => {
            Some(CloseCode::reason(close.error_code.into_inner()))
        }
        _ => None,
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    connection_id: u64,
    mut reader: R,
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

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut queues: Queues,
    hello: WireMessage,
) -> io::Result<()> {
    write_frame(&mut writer, &hello).await?;
    loop {
        // Check control between every input batch so keepalives and handshake
        // responses cannot be starved by a high-rate pointing device.
        if let Ok(message) = queues.controls.try_recv() {
            write_frame(&mut writer, &message).await?;
            continue;
        }
        if let Some(message) = queues.input.pop_message() {
            write_frame(&mut writer, &message).await?;
            continue;
        }
        // Bulk data only when nothing else waits, one frame at a time:
        // clipboards before files.
        if let Some(message) = queues.bulk.pop() {
            write_frame(&mut writer, &message).await?;
            continue;
        }
        if let Ok(message) = queues.files.try_recv() {
            write_frame(&mut writer, &message).await?;
            continue;
        }

        tokio::select! {
            biased;
            message = queues.controls.recv() => {
                let Some(message) = message else { break };
                write_frame(&mut writer, &message).await?;
            }
            _ = queues.input.notified() => {}
            _ = queues.bulk.notified() => {}
            Some(message) = queues.files.recv() => write_frame(&mut writer, &message).await?,
        }
    }
    writer.shutdown().await
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &WireMessage,
) -> io::Result<()> {
    let payload = encode_message(message)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "outgoing frame exceeds maximum size",
        ));
    }
    // One write per frame: a QUIC stream sends each write as soon as it can.
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// Dials `address` through `endpoint` and opens the connection's stream.
async fn dial(endpoint: quinn::Endpoint, address: SocketAddr) -> Result<Link, String> {
    let connecting = endpoint
        .connect(address, secure::SERVER_NAME)
        .map_err(|error| error.to_string())?;
    let connection = tokio::time::timeout(CONNECT_TIMEOUT, connecting)
        .await
        .map_err(|_| "connection timed out".to_owned())?
        .map_err(|error| error.to_string())?;
    // The stream reaches the peer with its first frame (our hello).
    let (send, recv) = connection
        .open_bi()
        .await
        .map_err(|error| error.to_string())?;
    Ok(Link {
        connection,
        send,
        recv,
    })
}

/// Dials the data link for session `session` (see `DataInvite`).
pub fn connect_data_in_background(
    session: u64,
    token: u64,
    endpoint: quinn::Endpoint,
    address: SocketAddr,
    events: mpsc::UnboundedSender<ConnectionEvent>,
) {
    tokio::spawn(async move {
        let result = dial(endpoint, address).await;
        let _ = events.send(ConnectionEvent::DataConnectResult {
            session,
            token,
            result,
        });
    });
}

pub fn connect_in_background(
    peer_id: String,
    endpoint: quinn::Endpoint,
    address: SocketAddr,
    events: mpsc::UnboundedSender<ConnectionEvent>,
) {
    tokio::spawn(async move {
        let result = dial(endpoint, address).await;
        let _ = events.send(ConnectionEvent::ConnectResult { peer_id, result });
    });
}

/// Completes a connection that arrived at an endpoint and waits for the
/// stream its dialer opens, then hands both to the runtime.
pub fn accept_in_background(
    incoming: quinn::Incoming,
    bulk: bool,
    events: mpsc::UnboundedSender<ConnectionEvent>,
) {
    tokio::spawn(async move {
        let accepted = async {
            let connection = incoming.await.ok()?;
            let (send, recv) = connection.accept_bi().await.ok()?;
            Some(Link {
                connection,
                send,
                recv,
            })
        };
        if let Ok(Some(link)) = tokio::time::timeout(CONNECT_TIMEOUT * 2, accepted).await {
            let _ = events.send(ConnectionEvent::Accepted { link, bulk });
        }
    });
}
