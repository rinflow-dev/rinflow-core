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

    /// Receives the transfer announced by `files_offered`.
    pub fn accept_files(&self, transfer: u32) -> Result<(), String> {
        self.thread.send(Command::AcceptFiles(transfer))
    }

    /// Stops receiving a transfer (offered or under way).
    pub fn cancel_files(&self, transfer: u32) -> Result<(), String> {
        self.thread.send(Command::CancelFiles(transfer))
    }

    /// Lets peers drive this device (the default), or stops them: a peer
    /// that drives it, or starts to, is told to give control back and none
    /// of its input is played. This device can still drive peers.
    pub fn set_remote_control_allowed(&self, allowed: bool) -> Result<(), String> {
        self.thread.send(Command::SetRemoteControlAllowed(allowed))
    }

    /// The user checked the code of `pairing_requested` and accepts.
    pub fn accept_pairing(&self, peer_id: &str) -> Result<(), String> {
        self.thread.send(Command::AcceptPairing(peer_id.to_owned()))
    }

    /// The user declines pairing; the connection closes.
    pub fn reject_pairing(&self, peer_id: &str) -> Result<(), String> {
        self.thread.send(Command::RejectPairing(peer_id.to_owned()))
    }

    /// Forgets a paired peer (`unpaired`); the shell drops it from
    /// `CoreConfig::trusted_peers`.
    pub fn unpair(&self, peer_id: &str) -> Result<(), String> {
        self.thread.send(Command::Unpair(peer_id.to_owned()))
    }

    /// Only paired peers may connect (`CoreConfig::require_pairing`).
    pub fn set_require_pairing(&self, required: bool) -> Result<(), String> {
        self.thread.send(Command::SetRequirePairing(required))
    }

    /// The system is about to sleep. Sessions end now, in order, so peers
    /// get control back and release held keys at once instead of timing
    /// out; for a few seconds no connections are made or taken.
    pub fn system_will_sleep(&self) -> Result<(), String> {
        self.thread.send(Command::WillSleep)
    }

    /// The system woke up, or the network changed (an address came or
    /// went): announce this device, dial the user's devices and drop the
    /// sessions that did not survive, now rather than after timeouts.
    pub fn network_changed(&self) -> Result<(), String> {
        self.thread.send(Command::NetworkChanged)
    }

    /// Looks for devices at these addresses too (host names or IPv4, with an
    /// optional discovery port), replacing the earlier list. Reported as
    /// `manual_addresses`; see `CoreConfig::manual_addresses`.
    pub fn set_manual_addresses(&self, addresses: Vec<String>) -> Result<(), String> {
        self.thread.send(Command::SetManualAddresses(addresses))
    }

    /// Translates Command and Control shortcuts from peers that use the other
    /// platform convention (`CoreConfig::shortcut_remap`).
    pub fn set_shortcut_remap(&self, enabled: bool) -> Result<(), String> {
        self.thread.send(Command::SetShortcutRemap(enabled))
    }

    /// Someone used this device's own keyboard or mouse. Call it while that
    /// goes on (every few seconds is plenty): peers then report
    /// `peer_active` and can stay awake.
    pub fn report_user_activity(&self) -> Result<(), String> {
        self.thread.send(Command::ReportUserActivity)
    }

    /// The user locked this device: peers report `lock_requested`.
    pub fn lock_peers(&self) -> Result<(), String> {
        self.thread.send(Command::LockPeers)
    }

    /// A game or a pointer-lock page holds this device's pointer (or no
    /// longer does): peers driving it report `peer_pointer_lock`.
    pub fn set_pointer_locked(&self, locked: bool) -> Result<(), String> {
        self.thread.send(Command::SetPointerLocked(locked))
    }

    pub fn send_input(&self, event: LcInputEvent) -> Result<(), String> {
        self.thread.send(Command::SendInput(event))
    }

    pub fn reset_input(&self) -> Result<(), String> {
        self.thread.send(Command::ResetInput)
    }
}
