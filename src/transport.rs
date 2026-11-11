// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Connections to peers: QUIC with TLS 1.3 (see `quic.rs` and `secure.rs`).
//! Each connection carries one bidirectional stream of length-prefixed
//! frames (see `protocol.rs`), opened by the side that dialed.

use crate::input::{OutboundInputQueue, PushOutcome};
use crate::model::{LcInputEvent, MAX_FRAME_SIZE, WireMessage};
use crate::priority::FineTimers;
use crate::protocol::{decode_message, encode_message};
use crate::remap::ModifierMap;
use crate::secure;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
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
    FileStreamServed {
        session: u64,
        lease: u64,
        entry: usize,
        offset: u64,
        length: usize,
    },
    ClipboardPrepared {
        generation: u64,
        clipboard: crate::clipboard::Clipboard,
        offer: Result<crate::files::Offer, String>,
    },
    Message {
        connection_id: u64,
        message: WireMessage,
        budget: Option<tokio::sync::OwnedSemaphorePermit>,
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
    pub modifier_map: ModifierMap,
    /// SHA-256 of the certificate the peer presented (64 hex digits).
    pub fingerprint: String,
    control: mpsc::Sender<WireMessage>,
    input: OutboundInputQueue,
    bulk: BulkQueue,
    files: mpsc::Sender<WireMessage>,
    paced: Arc<AtomicBool>,
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

    /// Paces motion to this peer, or sends it as it comes (see
    /// `MotionPacing` in `input.rs`).
    pub fn set_motion_paced(&self, paced: bool) {
        self.paced.store(paced, Ordering::Relaxed);
    }

    /// The round trip, and the packets sent and lost so far.
    pub fn path_stats(&self) -> quinn::PathStats {
        self.connection.stats().path
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
    let paced = Arc::new(AtomicBool::new(false));
    let queues = Queues {
        controls: control_rx,
        input: input.clone(),
        bulk: bulk.clone(),
        files: files_rx,
        paced: paced.clone(),
    };
    let (read_task, write_task) =
        spawn_loops(connection_id, link.recv, link.send, queues, hello, events);

    Ok(ActiveConnection {
        peer_id: None,
        remote,
        protocol_version: None,
        capabilities: 0,
        modifier_map: ModifierMap::KEEP,
        fingerprint,
        control: control_tx,
        input,
        bulk,
        files: files_tx,
        paced,
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
    /// Motion is paced now (see `MOTION_INTERVAL`); the runtime decides.
    paced: Arc<AtomicBool>,
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
    // Held through runtime dispatch and (for legacy file chunks) disk write.
    // A slow disk stops this data connection without blocking input sockets.
    let budget = Arc::new(tokio::sync::Semaphore::new(4 * 1024 * 1024));
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

        let permit = budget
            .clone()
            .acquire_many_owned(length as u32)
            .await
            .map_err(|_| io::Error::other("receive budget closed"))?;
        let mut payload = vec![0_u8; length];
        reader.read_exact(&mut payload).await?;
        let message = decode_message(&payload)?;

        if events
            .send(ConnectionEvent::Message {
                connection_id,
                message,
                budget: Some(permit),
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
    // When input last went out.
    let mut input_sent: Option<tokio::time::Instant> = None;
    // Held while input goes out, so the slots are kept to the millisecond.
    let mut fine_timers: Option<FineTimers> = None;
    loop {
        // Check control between every input batch so keepalives and handshake
        // responses cannot be starved by a high-rate pointing device.
        if let Ok(message) = queues.controls.try_recv() {
            write_frame(&mut writer, &message).await?;
            continue;
        }
        // Motion alone waits for its slot (see `MOTION_INTERVAL`), merging
        // with what comes meanwhile; anything else goes at once, and the
        // motion queued before it with it.
        let paced = queues.paced.load(Ordering::Relaxed);
        let slot = if paced && queues.input.only_motion() {
            input_sent
                .map(|sent| sent + MOTION_INTERVAL)
                .filter(|slot| *slot > tokio::time::Instant::now())
        } else {
            None
        };
        if slot.is_none()
            && let Some(message) = queues.input.pop_message()
        {
            write_frame(&mut writer, &message).await?;
            input_sent = Some(tokio::time::Instant::now());
            if paced {
                fine_timers.get_or_insert_with(FineTimers::new);
            }
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
            _ = motion_slot(slot) => {}
            _ = motion_slot(input_sent.filter(|_| fine_timers.is_some()).map(|sent| sent + INPUT_IDLE)) => {
                fine_timers = None;
            }
            _ = queues.bulk.notified() => {}
            Some(message) = queues.files.recv() => write_frame(&mut writer, &message).await?,
        }
    }
    writer.shutdown().await
}

/// The least time between two frames of pointer motion or scrolling.
///
/// A mouse reports up to 1000 times a second (some 8000), and each report
/// sent by itself is a packet: far more than any display shows, and on Wi-Fi
/// every packet waits for the channel, so a flood of them delays itself and
/// the acknowledgements coming back. Reports within one interval merge into
/// one frame instead (`coalesce` in `input.rs`). The first motion after a
/// pause still goes at once, and so do keys and buttons: at most this much
/// is added to the rest, half a frame at 120 Hz.
const MOTION_INTERVAL: Duration = Duration::from_millis(4);

/// How long after the last input the finer timers are given up.
const INPUT_IDLE: Duration = Duration::from_secs(2);

/// Waits until `slot`; for ever if there is none.
async fn motion_slot(slot: Option<tokio::time::Instant>) {
    match slot {
        Some(slot) => tokio::time::sleep_until(slot).await,
        None => std::future::pending().await,
    }
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

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[tokio::test]
    async fn receive_budget_is_held_until_the_disk_consumer_releases_chunks() {
        let (mut sender, receiver) = tokio::io::duplex(64 * 1024);
        let (events, mut received) = mpsc::unbounded_channel();
        let reader = tokio::spawn(read_loop(1, receiver, events));
        let writer = tokio::spawn(async move {
            for index in 0..100 {
                write_frame(
                    &mut sender,
                    &WireMessage::FileChunk {
                        transfer: 1,
                        entry: 0,
                        offset: index * 65536,
                        data: vec![0; 65536],
                    },
                )
                .await
                .unwrap();
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut held = vec![];
        while let Ok(event) = received.try_recv() {
            held.push(event);
        }
        assert!(!held.is_empty());
        assert!(held.len() <= 64, "unbounded receive queue: {}", held.len());
        assert!(
            !writer.is_finished(),
            "producer ran past the receive budget"
        );
        let count = held.len();
        drop(held);
        for _ in count..100 {
            drop(received.recv().await.unwrap());
        }
        writer.await.unwrap();
        reader.await.unwrap().unwrap();
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::*;
    use crate::model::{LC_INPUT_MOUSE_BUTTON_DOWN, LC_INPUT_MOUSE_MOVE};

    /// Sends 300 reports a millisecond apart, then a click; returns the
    /// frames of input that went out, the motion in them, and how long the
    /// click took.
    async fn frames_for_fast_motion(paced: bool) -> (usize, f64, Duration) {
        let (writer, mut reader) = tokio::io::duplex(1024 * 1024);
        let (_controls_tx, controls) = mpsc::channel(4);
        let (_files_tx, files) = mpsc::channel(1);
        let input = OutboundInputQueue::new();
        let queues = Queues {
            controls,
            input: input.clone(),
            bulk: BulkQueue::default(),
            files,
            paced: Arc::new(AtomicBool::new(paced)),
        };
        tokio::spawn(write_loop(
            writer,
            queues,
            WireMessage::Ping { sequence: 1 },
        ));

        let reading = tokio::spawn(async move {
            let (mut frames, mut total) = (0, 0.0);
            loop {
                let length = reader.read_u32().await.unwrap() as usize;
                let mut payload = vec![0; length];
                reader.read_exact(&mut payload).await.unwrap();
                let WireMessage::InputBatch { events, .. } = decode_message(&payload).unwrap()
                else {
                    continue;
                };
                frames += 1;
                total += events.iter().map(|event| event.delta_x).sum::<f64>();
                if events
                    .iter()
                    .any(|event| event.kind == LC_INPUT_MOUSE_BUTTON_DOWN)
                {
                    return (frames, total, std::time::Instant::now());
                }
            }
        });

        let start = std::time::Instant::now();
        for report in 0..300u64 {
            input
                .push(LcInputEvent {
                    kind: LC_INPUT_MOUSE_MOVE,
                    delta_x: 1.0,
                    ..LcInputEvent::default()
                })
                .unwrap();
            while start.elapsed() < Duration::from_millis(report + 1) {
                tokio::task::yield_now().await;
            }
        }
        let clicked = std::time::Instant::now();
        input
            .push(LcInputEvent {
                kind: LC_INPUT_MOUSE_BUTTON_DOWN,
                code: 1,
                buttons: 1,
                ..LcInputEvent::default()
            })
            .unwrap();
        let (frames, total, arrived) = reading.await.unwrap();
        (frames, total, arrived.saturating_duration_since(clicked))
    }

    /// Paced, a thousand reports a second go out as about 250 frames, and
    /// a click right behind them does not wait for a slot; not paced, every
    /// report goes as it comes. Either way no motion is lost.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn motion_is_merged_into_slots_only_while_paced() {
        let (frames, total, click) = frames_for_fast_motion(true).await;
        eprintln!("paced: 300 reports in 0.3 s went out as {frames} frames, click in {click:?}");
        assert_eq!(total, 300.0);
        // 75 slots of 4 ms in 300 ms, and the frame of the click.
        assert!((40..=110).contains(&frames), "{frames} frames");
        assert!(click < Duration::from_millis(5), "{click:?}");

        let (frames, total, click) = frames_for_fast_motion(false).await;
        eprintln!(
            "not paced: 300 reports in 0.3 s went out as {frames} frames, click in {click:?}"
        );
        assert_eq!(total, 300.0);
        assert!(frames > 200, "{frames} frames");
        assert!(click < Duration::from_millis(5), "{click:?}");
    }
}
