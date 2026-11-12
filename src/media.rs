// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A device's picture, sent to the peers that watch it.
//!
//! A device with a picture (a KVM: the screen of the computer behind it) is
//! a **source** (`CoreConfig::media_source`); a device that can show one
//! **watches** (`CoreConfig::media`). The picture is a sequence of encoded
//! frames the shells make and take; the core carries them and does not look
//! inside. Each frame is whatever the source's encoder put out for one
//! picture (for H.264, the NAL units of one access unit with start codes),
//! and a **key frame** is one a decoder can start from.
//!
//! **The media link.** Pictures go over a QUIC connection of their own, to
//! a third endpoint (see `quic.rs`), for the reasons the data link has its
//! own: what crosses it must never wait in front of input, and the network
//! queues by socket (`qos.rs`: video, between input and bulk).
//!
//! 1. Once a session is established, a source sends `MediaOffer { token,
//!    port }` on the control connection to a peer that can watch.
//! 2. When its shell asks (`Core::watch`), that peer dials the port and
//!    opens one bidirectional stream with `token` and its device ID.
//! 3. The source takes the link only for a token it gave out, from the
//!    device it gave it to, showing the certificate of that session; the
//!    watching side likewise checks the source's certificate against the
//!    session's. It answers one byte, and frames follow.
//! 4. The watching side may write `K` on that stream: it needs a key frame
//!    (its decoder lost its place). Closing the connection ends watching.
//!
//! **Frames.** Each frame travels on a unidirectional stream of its own: a
//! header ([`HEADER_LENGTH`] bytes: flags, sequence, time stamp), then the
//! frame to the end of the stream. A stream is reliable and ordered within
//! itself and independent of the others, so a lost packet delays the frame
//! it belongs to and no other.
//!
//! **Late is useless.** A link that carries less than the encoder puts out
//! must not grow a queue: the picture would fall behind for good. So each
//! watching peer has a short queue ([`MAX_WAITING_FRAMES`]); when it is
//! full everything waiting is dropped and frames are skipped up to the next
//! key frame, which the shell is asked for (`media_key_frame_wanted`). A
//! decoder never sees a frame whose predecessor it did not get.

use crate::secure::peer_fingerprint;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Flags, sequence, time stamp.
pub const HEADER_LENGTH: usize = 1 + 8 + 8;
const FLAG_KEY_FRAME: u8 = 1;
/// A frame larger than this is not a frame: the stream is refused.
pub const MAX_FRAME_BYTES: usize = 8 << 20;
/// Frames that may wait for one watching peer's link: about a tenth of a
/// second at thirty frames a second.
const MAX_WAITING_FRAMES: usize = 3;
/// The first message on a media link is at most this long.
const MAX_HELLO_BYTES: usize = 8 + 2 + 256;
const ACCEPTED: u8 = 1;
const KEY_FRAME_PLEASE: u8 = b'K';

/// One encoded picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaFrame {
    /// A decoder can start from this frame.
    pub key_frame: bool,
    /// Counts the source's frames: a gap means frames were skipped.
    pub sequence: u64,
    /// The source's clock, in microseconds; only differences mean anything.
    pub timestamp_micros: u64,
    pub data: Vec<u8>,
}

impl MediaFrame {
    fn header(&self) -> [u8; HEADER_LENGTH] {
        let mut header = [0u8; HEADER_LENGTH];
        header[0] = if self.key_frame { FLAG_KEY_FRAME } else { 0 };
        header[1..9].copy_from_slice(&self.sequence.to_le_bytes());
        header[9..17].copy_from_slice(&self.timestamp_micros.to_le_bytes());
        header
    }

    /// A frame as it came off its stream.
    fn decode(mut bytes: Vec<u8>) -> Option<Self> {
        let header: [u8; HEADER_LENGTH] = bytes.get(..HEADER_LENGTH)?.try_into().ok()?;
        if header[0] & !FLAG_KEY_FRAME != 0 {
            return None;
        }
        bytes.drain(..HEADER_LENGTH);
        Some(Self {
            key_frame: header[0] & FLAG_KEY_FRAME != 0,
            sequence: u64::from_le_bytes(header[1..9].try_into().ok()?),
            timestamp_micros: u64::from_le_bytes(header[9..17].try_into().ok()?),
            data: bytes,
        })
    }
}

/// Called with the peer's device ID for every frame of a watched picture,
/// on the core's threads, in order.
pub type FrameSink = Arc<dyn Fn(&str, MediaFrame) + Send + Sync>;
type EventSink = Arc<dyn Fn(Value) + Send + Sync>;

/// The frames waiting for one watching peer's link.
struct Queue {
    frames: VecDeque<Arc<MediaFrame>>,
    /// Frames are skipped until a key frame comes.
    wants_key_frame: bool,
    skipped: u64,
    closed: bool,
}

/// What became of a frame offered to a queue.
#[derive(Debug, PartialEq, Eq)]
enum Offered {
    Queued,
    /// Skipped: the queue waits for a key frame.
    Skipped,
    /// The queue was full: it was emptied and now waits for a key frame.
    Overflowed,
}

impl Queue {
    fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            // A decoder can only start from a key frame.
            wants_key_frame: true,
            skipped: 0,
            closed: false,
        }
    }

    fn offer(&mut self, frame: &Arc<MediaFrame>) -> Offered {
        if frame.key_frame {
            // Everything before a key frame is of no use once it is here.
            self.skipped += self.frames.len() as u64;
            self.frames.clear();
            self.wants_key_frame = false;
            self.frames.push_back(frame.clone());
            return Offered::Queued;
        }
        if self.wants_key_frame {
            self.skipped += 1;
            return Offered::Skipped;
        }
        if self.frames.len() >= MAX_WAITING_FRAMES {
            self.skipped += self.frames.len() as u64 + 1;
            self.frames.clear();
            self.wants_key_frame = true;
            return Offered::Overflowed;
        }
        self.frames.push_back(frame.clone());
        Offered::Queued
    }
}

/// A peer watching this device's picture.
struct Viewer {
    peer_id: String,
    connection: quinn::Connection,
    queue: Mutex<Queue>,
    wake: Notify,
}

/// A peer that may watch: the token it was offered, and the certificate its
/// session showed.
struct Offer {
    peer_id: String,
    fingerprint: String,
}

/// The link to a picture being watched.
struct Watching {
    connection: quinn::Connection,
    /// Woken to ask the source for a key frame.
    key_frame: Arc<Notify>,
}

/// A peer whose picture can be watched.
struct Source {
    address: SocketAddr,
    token: u64,
    fingerprint: String,
}

#[derive(Default)]
struct State {
    /// By token: who this device offered its picture to.
    offers: HashMap<u64, Offer>,
    viewers: Vec<Arc<Viewer>>,
    sequence: u64,
    /// By device ID: whose picture this device may watch.
    sources: HashMap<String, Source>,
    /// By device ID: the links to the pictures being watched.
    watching: HashMap<String, Watching>,
    /// By device ID: links being dialed, not yet in `watching`. A second
    /// `watch` meanwhile must not dial again.
    connecting: HashSet<String>,
    frame_sink: Option<FrameSink>,
    /// The core's runtime and this device's endpoint, while the core runs.
    running: Option<(tokio::runtime::Handle, quinn::Endpoint, String)>,
}

/// Everything about pictures a core holds. Shared by the core's loop (which
/// reports sessions and offers) and the shell's threads (which send frames
/// and ask to watch), so sending a frame does not pass through the loop.
pub struct MediaHub {
    state: Mutex<State>,
    events: EventSink,
}

impl MediaHub {
    pub fn new(events: impl Fn(Value) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            events: Arc::new(events),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The core is running: `endpoint` is its media endpoint, `device_id`
    /// its own. Call inside the core's Tokio runtime.
    pub fn start(&self, endpoint: quinn::Endpoint, device_id: &str) {
        self.state().running = Some((
            tokio::runtime::Handle::current(),
            endpoint,
            device_id.to_owned(),
        ));
    }

    /// The core stopped: every link ends.
    pub fn stop(&self) {
        let mut state = self.state();
        state.running = None;
        state.offers.clear();
        state.sources.clear();
        state.connecting.clear();
        for viewer in state.viewers.drain(..) {
            close_viewer(&viewer, "core stopped");
        }
        for (_, watching) in state.watching.drain() {
            watching.connection.close(0u32.into(), b"core stopped");
        }
    }

    pub fn set_frame_sink(&self, sink: FrameSink) {
        self.state().frame_sink = Some(sink);
    }

    // ---- Source ---------------------------------------------------------

    /// A session with a peer that can watch is established: a token for it.
    pub fn offer_to(&self, peer_id: &str, fingerprint: &str) -> Option<u64> {
        let token = random_token()?;
        let mut state = self.state();
        state.offers.retain(|_, offer| offer.peer_id != peer_id);
        state.offers.insert(
            token,
            Offer {
                peer_id: peer_id.to_owned(),
                fingerprint: fingerprint.to_owned(),
            },
        );
        Some(token)
    }

    /// One frame of this device's picture, for every peer watching.
    pub fn send_frame(&self, key_frame: bool, timestamp_micros: u64, data: Vec<u8>) {
        let (frame, viewers) = {
            let mut state = self.state();
            if state.viewers.is_empty() {
                return;
            }
            state.sequence += 1;
            let frame = Arc::new(MediaFrame {
                key_frame,
                sequence: state.sequence,
                timestamp_micros,
                data,
            });
            (frame, state.viewers.clone())
        };
        let mut wanted = false;
        for viewer in viewers {
            let offered = viewer
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .offer(&frame);
            match offered {
                Offered::Queued => viewer.wake.notify_one(),
                Offered::Skipped => {}
                Offered::Overflowed => wanted = true,
            }
        }
        if wanted {
            (self.events)(json!({ "event": "media_key_frame_wanted" }));
        }
    }

    /// A connection came in on the media endpoint.
    pub fn accept(self: &Arc<Self>, incoming: quinn::Incoming) {
        let hub = self.clone();
        tokio::spawn(async move {
            let Ok(connection) = incoming.await else {
                return;
            };
            if let Err(reason) = hub.clone().serve(connection.clone()).await {
                connection.close(1u32.into(), reason.as_bytes());
            }
        });
    }

    /// Checks who dialed, then sends them frames until the link ends.
    async fn serve(self: Arc<Self>, connection: quinn::Connection) -> Result<(), &'static str> {
        let (mut answer, mut hello) = tokio::time::timeout(HELLO_TIMEOUT, connection.accept_bi())
            .await
            .map_err(|_| "no hello")?
            .map_err(|_| "no hello")?;
        let mut head = [0u8; 10];
        tokio::time::timeout(HELLO_TIMEOUT, hello.read_exact(&mut head))
            .await
            .map_err(|_| "no hello")?
            .map_err(|_| "no hello")?;
        let token = u64::from_le_bytes(head[..8].try_into().map_err(|_| "bad hello")?);
        let length = usize::from(u16::from_le_bytes([head[8], head[9]]));
        if length > MAX_HELLO_BYTES - 10 {
            return Err("bad hello");
        }
        let mut device_id = vec![0u8; length];
        tokio::time::timeout(HELLO_TIMEOUT, hello.read_exact(&mut device_id))
            .await
            .map_err(|_| "no hello")?
            .map_err(|_| "no hello")?;
        let device_id = String::from_utf8(device_id).map_err(|_| "bad hello")?;

        let viewer = {
            let mut state = self.state();
            let offer = state.offers.get(&token).ok_or("not offered")?;
            if offer.peer_id != device_id
                || peer_fingerprint(&connection).as_deref() != Some(offer.fingerprint.as_str())
            {
                return Err("not offered");
            }
            let viewer = Arc::new(Viewer {
                peer_id: device_id,
                connection: connection.clone(),
                queue: Mutex::new(Queue::new()),
                wake: Notify::new(),
            });
            // A peer watches once: a second link replaces the first.
            state.viewers.retain(|other| {
                let same = other.peer_id == viewer.peer_id;
                if same {
                    close_viewer(other, "replaced");
                }
                !same
            });
            state.viewers.push(viewer.clone());
            viewer
        };
        answer.write_all(&[ACCEPTED]).await.map_err(|_| "closed")?;
        self.report_viewers();
        // It starts from a key frame.
        (self.events)(json!({ "event": "media_key_frame_wanted" }));

        let requests = async {
            let mut byte = [0u8; 1];
            while hello.read_exact(&mut byte).await.is_ok() {
                if byte[0] == KEY_FRAME_PLEASE {
                    viewer
                        .queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .wants_key_frame = true;
                    (self.events)(json!({ "event": "media_key_frame_wanted" }));
                }
            }
        };
        tokio::select! {
            _ = send_frames(&viewer) => {}
            _ = requests => {}
            _ = connection.closed() => {}
        }
        self.state()
            .viewers
            .retain(|other| !Arc::ptr_eq(other, &viewer));
        close_viewer(&viewer, "ended");
        self.report_viewers();
        Ok(())
    }

    fn report_viewers(&self) {
        let peers: Vec<String> = self
            .state()
            .viewers
            .iter()
            .map(|viewer| viewer.peer_id.clone())
            .collect();
        (self.events)(json!({ "event": "media_viewers", "peer_ids": peers }));
    }

    // ---- Watching -------------------------------------------------------

    /// `MediaOffer` from a peer: its picture is at `address`.
    pub fn offered(&self, peer_id: &str, address: SocketAddr, token: u64, fingerprint: &str) {
        self.state().sources.insert(
            peer_id.to_owned(),
            Source {
                address,
                token,
                fingerprint: fingerprint.to_owned(),
            },
        );
        (self.events)(json!({ "event": "media_available", "peer_id": peer_id }));
    }

    /// Starts watching a peer's picture. Its frames go to the frame sink.
    pub fn watch(self: &Arc<Self>, peer_id: &str) -> Result<(), String> {
        let (handle, endpoint, device_id, source) = {
            let mut state = self.state();
            if state.watching.contains_key(peer_id) || state.connecting.contains(peer_id) {
                return Ok(());
            }
            let (handle, endpoint, device_id) = state
                .running
                .clone()
                .ok_or("this device does not watch pictures (CoreConfig::media)")?;
            let source = state
                .sources
                .get(peer_id)
                .ok_or("that device offers no picture")?;
            let source = (source.address, source.token, source.fingerprint.clone());
            state.connecting.insert(peer_id.to_owned());
            (handle, endpoint, device_id, source)
        };
        let (hub, peer_id) = (self.clone(), peer_id.to_owned());
        handle.spawn(async move {
            let reason = match hub.receive(&endpoint, &device_id, &peer_id, source).await {
                Ok(()) => "ended",
                Err(reason) => reason,
            };
            // `receive` took its own link out of `watching`; a link that
            // replaced it in the meantime stays.
            hub.state().connecting.remove(&peer_id);
            (hub.events)(json!({
                "event": "media_ended",
                "peer_id": peer_id,
                "reason": reason
            }));
        });
        Ok(())
    }

    pub fn unwatch(&self, peer_id: &str) {
        if let Some(watching) = self.state().watching.get(peer_id) {
            watching.connection.close(0u32.into(), b"stopped watching");
        }
    }

    /// Asks the peer being watched for a key frame: the decoder lost its
    /// place.
    pub fn request_key_frame(&self, peer_id: &str) -> Result<(), String> {
        self.state()
            .watching
            .get(peer_id)
            .ok_or("that device is not being watched")?
            .key_frame
            .notify_one();
        Ok(())
    }

    async fn receive(
        &self,
        endpoint: &quinn::Endpoint,
        device_id: &str,
        peer_id: &str,
        (address, token, fingerprint): (SocketAddr, u64, String),
    ) -> Result<(), &'static str> {
        let connection = endpoint
            .connect(address, crate::secure::SERVER_NAME)
            .map_err(|_| "cannot dial")?
            .await
            .map_err(|_| "cannot connect")?;
        if peer_fingerprint(&connection).as_deref() != Some(fingerprint.as_str()) {
            connection.close(1u32.into(), b"wrong device");
            return Err("wrong device");
        }
        let (mut hello, mut answer) = connection.open_bi().await.map_err(|_| "closed")?;
        let mut message = token.to_le_bytes().to_vec();
        message.extend_from_slice(&(device_id.len() as u16).to_le_bytes());
        message.extend_from_slice(device_id.as_bytes());
        hello.write_all(&message).await.map_err(|_| "closed")?;
        let mut accepted = [0u8; 1];
        tokio::time::timeout(HELLO_TIMEOUT, answer.read_exact(&mut accepted))
            .await
            .map_err(|_| "refused")?
            .map_err(|_| "refused")?;
        if accepted[0] != ACCEPTED {
            return Err("refused");
        }
        let key_frame = Arc::new(Notify::new());
        let sink = {
            let mut state = self.state();
            state.connecting.remove(peer_id);
            state.watching.insert(
                peer_id.to_owned(),
                Watching {
                    connection: connection.clone(),
                    key_frame: key_frame.clone(),
                },
            );
            state.frame_sink.clone()
        };
        (self.events)(json!({ "event": "media_started", "peer_id": peer_id }));

        // Frames are read in the order their streams were opened, which is
        // the order they were made in.
        let mut last = 0u64;
        let result = loop {
            let mut stream = tokio::select! {
                stream = connection.accept_uni() => match stream {
                    Ok(stream) => stream,
                    Err(_) => break Ok(()),
                },
                _ = key_frame.notified() => {
                    if hello.write_all(&[KEY_FRAME_PLEASE]).await.is_err() {
                        break Ok(());
                    }
                    continue;
                }
            };
            let Ok(bytes) = stream.read_to_end(HEADER_LENGTH + MAX_FRAME_BYTES).await else {
                // Reset by the source, or too large to be a frame.
                continue;
            };
            let Some(frame) = MediaFrame::decode(bytes) else {
                connection.close(1u32.into(), b"bad frame");
                break Err("bad frame");
            };
            // Frames skipped by the source resume at a key frame; anything
            // else out of order would only confuse a decoder.
            if frame.sequence <= last || (frame.sequence != last + 1 && !frame.key_frame) {
                continue;
            }
            last = frame.sequence;
            if let Some(sink) = &sink {
                sink(peer_id, frame);
            }
        };
        // This link's own entry only: a second `watch` may have replaced it.
        let mut state = self.state();
        if state
            .watching
            .get(peer_id)
            .is_some_and(|watching| watching.connection.stable_id() == connection.stable_id())
        {
            state.watching.remove(peer_id);
        }
        result
    }

    // ---- Both -----------------------------------------------------------

    /// The session with a peer ended: so does everything about pictures
    /// between the two.
    pub fn session_ended(&self, peer_id: &str) {
        let mut state = self.state();
        state.offers.retain(|_, offer| offer.peer_id != peer_id);
        let had_source = state.sources.remove(peer_id).is_some();
        if let Some(watching) = state.watching.get(peer_id) {
            watching.connection.close(0u32.into(), b"session ended");
        }
        let mut viewers_changed = false;
        state.viewers.retain(|viewer| {
            let gone = viewer.peer_id == peer_id;
            if gone {
                close_viewer(viewer, "session ended");
                viewers_changed = true;
            }
            !gone
        });
        drop(state);
        if had_source {
            (self.events)(json!({ "event": "media_unavailable", "peer_id": peer_id }));
        }
        if viewers_changed {
            self.report_viewers();
        }
    }
}

const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn close_viewer(viewer: &Viewer, reason: &str) {
    viewer
        .queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .closed = true;
    viewer.wake.notify_one();
    viewer.connection.close(0u32.into(), reason.as_bytes());
}

/// Writes a watching peer's frames, each on its own stream, one after the
/// other: a frame waits here while the link is busy with the one before,
/// which is what fills the queue behind it.
async fn send_frames(viewer: &Viewer) {
    loop {
        let frame = {
            let mut queue = viewer
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if queue.closed {
                return;
            }
            queue.frames.pop_front()
        };
        let Some(frame) = frame else {
            viewer.wake.notified().await;
            continue;
        };
        let Ok(mut stream) = viewer.connection.open_uni().await else {
            return;
        };
        if stream.write_all(&frame.header()).await.is_err()
            || stream.write_all(&frame.data).await.is_err()
            || stream.finish().is_err()
        {
            return;
        }
    }
}

/// From the system's random source: a token names who may watch.
fn random_token() -> Option<u64> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 8];
    ring::rand::SystemRandom::new().fill(&mut bytes).ok()?;
    Some(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(sequence: u64, key_frame: bool) -> Arc<MediaFrame> {
        Arc::new(MediaFrame {
            key_frame,
            sequence,
            timestamp_micros: sequence * 33_333,
            data: vec![sequence as u8; 4],
        })
    }

    #[test]
    fn a_frame_survives_its_stream() {
        let original = MediaFrame {
            key_frame: true,
            sequence: 7,
            timestamp_micros: 1_234_567,
            data: vec![0, 0, 0, 1, 0x67],
        };
        let mut bytes = original.header().to_vec();
        bytes.extend_from_slice(&original.data);
        assert_eq!(MediaFrame::decode(bytes), Some(original));
        // Too short to hold a header, or flags this version does not know.
        assert_eq!(MediaFrame::decode(vec![0; HEADER_LENGTH - 1]), None);
        assert_eq!(MediaFrame::decode(vec![0x80; HEADER_LENGTH]), None);
    }

    #[test]
    fn a_new_queue_waits_for_a_key_frame() {
        let mut queue = Queue::new();
        assert_eq!(queue.offer(&frame(1, false)), Offered::Skipped);
        assert_eq!(queue.offer(&frame(2, true)), Offered::Queued);
        assert_eq!(queue.offer(&frame(3, false)), Offered::Queued);
        let waiting: Vec<u64> = queue.frames.iter().map(|frame| frame.sequence).collect();
        assert_eq!(waiting, [2, 3]);
    }

    #[test]
    fn a_full_queue_empties_and_skips_to_the_next_key_frame() {
        let mut queue = Queue::new();
        assert_eq!(queue.offer(&frame(1, true)), Offered::Queued);
        assert_eq!(queue.offer(&frame(2, false)), Offered::Queued);
        assert_eq!(queue.offer(&frame(3, false)), Offered::Queued);
        // The link took nothing meanwhile.
        assert_eq!(queue.offer(&frame(4, false)), Offered::Overflowed);
        assert!(queue.frames.is_empty());
        assert_eq!(queue.offer(&frame(5, false)), Offered::Skipped);
        assert_eq!(queue.offer(&frame(6, true)), Offered::Queued);
        assert_eq!(queue.skipped, 5);
        assert_eq!(queue.frames.front().map(|frame| frame.sequence), Some(6));
    }

    #[test]
    fn a_key_frame_replaces_what_waits() {
        let mut queue = Queue::new();
        queue.offer(&frame(1, true));
        queue.offer(&frame(2, false));
        assert_eq!(queue.offer(&frame(3, true)), Offered::Queued);
        let waiting: Vec<u64> = queue.frames.iter().map(|frame| frame.sequence).collect();
        assert_eq!(waiting, [3]);
    }
}
