use crate::clipboard::Clipboard;
use crate::layout::{Display, Offset, Side};
use crate::model::{CoreConfig, LcInputEvent};
use crate::runtime::{Command, CoreThread, EventSink};

/// Safe Rust entry point for platform shells that link the crate directly
/// instead of going through the C ABI in `ffi.rs`.
///
/// Events are delivered as the same JSON strings the C ABI emits. Both
/// callbacks run on the core's internal threads. Dropping the value stops
/// the core and waits for its threads to exit.
pub struct Core {
    thread: CoreThread,
}

impl Core {
    pub fn start(
        config: CoreConfig,
        on_event: impl Fn(String) + Send + Sync + 'static,
        on_input: impl Fn(LcInputEvent) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let thread = CoreThread::spawn(config, EventSink::new(on_event, on_input))?;
        Ok(Self { thread })
    }

    /// Opens a session with the peer unless one exists, and makes it the
    /// input target. Sessions with other peers stay open.
    pub fn connect(&self, peer_id: &str) -> Result<(), String> {
        self.thread.send(Command::Connect(peer_id.to_owned()))
    }

    /// Closes the session with the input target (every session if no
    /// target is set).
    pub fn disconnect(&self) -> Result<(), String> {
        self.thread.send(Command::Disconnect)
    }

    pub fn disconnect_peer(&self, peer_id: &str) -> Result<(), String> {
        self.thread
            .send(Command::DisconnectPeer(peer_id.to_owned()))
    }

    /// Chooses the peer that `send_input` and `reset_input` go to. The old
    /// target first gets a reset, so nothing stays pressed there. `None`
    /// drops input until a target is set.
    pub fn set_input_target(&self, peer_id: Option<&str>) -> Result<(), String> {
        self.thread
            .send(Command::SetInputTarget(peer_id.map(str::to_owned)))
    }

    /// Describes this device's displays (logical points). Peers receive them
    /// at once, and placements that no longer touch the desktop are moved.
    pub fn set_local_displays(&self, displays: Vec<Display>) -> Result<(), String> {
        self.thread.send(Command::SetLocalDisplays(displays))
    }

    /// Places a peer near `offset` (snapped to a valid spot), or removes its
    /// placement with `None`. Reported as `placement_changed` and synced.
    pub fn set_placement(&self, peer_id: &str, offset: Option<Offset>) -> Result<(), String> {
        self.thread.send(Command::SetPlacement {
            peer_id: peer_id.to_owned(),
            offset,
        })
    }

    pub fn place_beside(&self, peer_id: &str, side: Side) -> Result<(), String> {
        self.thread.send(Command::PlaceBeside {
            peer_id: peer_id.to_owned(),
            side,
        })
    }

    /// The system clipboard after a change (empty when it holds nothing
    /// shared). Needs `CoreConfig::clipboard`; see `clipboard.rs`.
    pub fn set_clipboard(&self, clipboard: Clipboard) -> Result<(), String> {
        self.thread.send(Command::SetClipboard(clipboard))
    }

    pub fn send_input(&self, event: LcInputEvent) -> Result<(), String> {
        self.thread.send(Command::SendInput(event))
    }

    pub fn reset_input(&self) -> Result<(), String> {
        self.thread.send(Command::ResetInput)
    }
}
