use crate::input::{OutboundInputQueue, PushOutcome};
use crate::model::{LcInputEvent, MAX_FRAME_SIZE, WireMessage};
use crate::protocol::{decode_message, encode_message};
use crate::remap::ShortcutRemap;
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

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
}

pub struct ActiveConnection {
    pub peer_id: Option<String>,
    pub protocol_version: Option<u16>,
    pub capabilities: u64,
    /// Applied to input received from this peer; set once its platform is known.
    pub shortcut_remap: ShortcutRemap,
    control: mpsc::Sender<WireMessage>,
    input: OutboundInputQueue,
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

    pub fn abort(self) {
        self.read_task.abort();
        self.write_task.abort();
    }
}

pub fn spawn_connection(
    connection_id: u64,
    stream: TcpStream,
    hello: WireMessage,
    events: mpsc::UnboundedSender<ConnectionEvent>,
) -> io::Result<ActiveConnection> {
    stream.set_nodelay(true)?;
    let (reader, writer) = stream.into_split();
    let (control_tx, control_rx) = mpsc::channel(64);
    let input = OutboundInputQueue::new();

    let read_events = events.clone();
    let read_task = tokio::spawn(async move {
        if let Err(error) = read_loop(connection_id, reader, read_events.clone()).await {
            let _ = read_events.send(ConnectionEvent::Closed {
                connection_id,
                reason: format!("read failed: {error}"),
            });
        }
    });

    let write_events = events;
    let write_input = input.clone();
    let write_task = tokio::spawn(async move {
        if let Err(error) = write_loop(writer, control_rx, write_input, hello).await {
            let _ = write_events.send(ConnectionEvent::Closed {
                connection_id,
                reason: format!("write failed: {error}"),
            });
        }
    });

    Ok(ActiveConnection {
        peer_id: None,
        protocol_version: None,
        capabilities: 0,
        shortcut_remap: ShortcutRemap::None,
        control: control_tx,
        input,
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
        let length = reader.read_u32().await? as usize;
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

        tokio::select! {
            biased;
            message = controls.recv() => {
                let Some(message) = message else { break };
                write_frame(&mut writer, &message).await?;
            }
            _ = input.notified() => {}
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
