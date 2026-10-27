use crate::layout::{Display, Offset, validate_displays};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Instant;

/// Version 3 added display descriptions and synchronised placements.
/// Version 4 runs only over QUIC with TLS 1.3 (no more plain TCP), pairs
/// devices (`PairRequest` / `PairAccept`), and `DataInvite` names the port
/// of the data link. It does not talk to version 3.
pub const PROTOCOL_MIN_VERSION: u16 = 4;
pub const PROTOCOL_MAX_VERSION: u16 = 4;
pub const DEFAULT_DISCOVERY_PORT: u16 = 42_420;
pub const DEFAULT_CONTROL_PORT: u16 = 42_421;
pub const DISCOVERY_GROUP: [u8; 4] = [239, 255, 42, 99];
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_INPUT_BATCH_SIZE: usize = 64;

pub const CAPABILITY_INPUT_V2: u64 = 1 << 0;
pub const CAPABILITY_RELATIVE_POINTER: u64 = 1 << 1;
pub const CAPABILITY_HIGH_RESOLUTION_SCROLL: u64 = 1 << 3;
pub const CAPABILITY_HID_KEYBOARD: u64 = 1 << 4;
/// Receives copied files over the data link (`FileRequest` / `FileChunk`);
/// advertised when the shell enables `CoreConfig::files`.
pub const CAPABILITY_FILES: u64 = 1 << 6;
/// Exchanges clipboards (`ClipboardChunk` on the data link); advertised when
/// the shell enables `CoreConfig::clipboard`. Bit 5 meant an earlier
/// clipboard on the control connection and is retired. Data-link messages go
/// only to peers sharing one of these bits: an older peer closes the
/// connection on a message type it does not know.
pub const CAPABILITY_CLIPBOARD: u64 = 1 << 7;
/// Features carried by the data link (see `transport.rs`).
pub const DATA_LINK_CAPABILITIES: u64 = CAPABILITY_CLIPBOARD | CAPABILITY_FILES;
/// Gives control back when told to (`ControlRevoke`). Sent only to peers
/// with this bit, for the same reason as the data-link messages.
pub const CAPABILITY_CONTROL_REVOKE: u64 = 1 << 8;
/// Understands `UserActive` and `LockScreen`: devices stay awake while
/// another one is in use, and can lock together. Sent only to peers with
/// this bit.
pub const CAPABILITY_PRESENCE: u64 = 1 << 9;
/// Understands `PointerLock`: a driven device whose pointer a game or a
/// pointer-lock page holds says so, and the driving device stops crossing
/// at its edges meanwhile. Sent only to peers with this bit.
pub const CAPABILITY_POINTER_LOCK: u64 = 1 << 10;
/// Pairs with a committed code (`PairCommit` / `PairNonce` / `PairReveal`,
/// see `runtime/trust.rs`): each side fixes its random part of the code
/// before it learns the other's, so a device in the middle cannot pick
/// certificates that give both sides the same code. New pairings need it
/// on both sides; devices paired before connect without it.
pub const CAPABILITY_PAIRING_V2: u64 = 1 << 11;
/// Bounded, seekable file reads from a pinned clipboard offer. Never sent
/// to older peers. Receiving shells opt in with `on_demand_files`.
pub const CAPABILITY_FILE_STREAMS: u64 = 1 << 12;
/// Files up to this size in all are received without asking.
pub const DEFAULT_MAX_AUTO_RECEIVE_BYTES: u64 = 512 * 1024 * 1024;
pub const LOCAL_CAPABILITIES: u64 = CAPABILITY_INPUT_V2
    | CAPABILITY_RELATIVE_POINTER
    | CAPABILITY_HIGH_RESOLUTION_SCROLL
    | CAPABILITY_HID_KEYBOARD
    | CAPABILITY_CONTROL_REVOKE
    | CAPABILITY_PRESENCE
    | CAPABILITY_POINTER_LOCK
    | CAPABILITY_PAIRING_V2;
/// Addresses the user added by hand (`CoreConfig::manual_addresses`), at most.
pub const MAX_MANUAL_ADDRESSES: usize = 32;

pub const LC_INPUT_MOUSE_MOVE: u32 = 1;
pub const LC_INPUT_MOUSE_BUTTON_DOWN: u32 = 2;
pub const LC_INPUT_MOUSE_BUTTON_UP: u32 = 3;
pub const LC_INPUT_SCROLL: u32 = 4;
pub const LC_INPUT_KEY_DOWN: u32 = 5;
pub const LC_INPUT_KEY_UP: u32 = 6;

pub const LC_INPUT_FLAG_KEY_REPEAT: u32 = 1 << 0;
pub const LC_INPUT_FLAG_PRECISE_SCROLL: u32 = 1 << 1;
pub const LC_INPUT_FLAG_ABSOLUTE_POSITION: u32 = 1 << 2;
const LC_INPUT_FLAGS_ALL: u32 =
    LC_INPUT_FLAG_KEY_REPEAT | LC_INPUT_FLAG_PRECISE_SCROLL | LC_INPUT_FLAG_ABSOLUTE_POSITION;

pub const LC_MODIFIER_SHIFT: u32 = 1 << 0;
pub const LC_MODIFIER_CONTROL: u32 = 1 << 1;
pub const LC_MODIFIER_OPTION: u32 = 1 << 2;
pub const LC_MODIFIER_COMMAND: u32 = 1 << 3;
pub const LC_MODIFIER_CAPS_LOCK: u32 = 1 << 4;
pub const LC_MODIFIER_LEFT_SHIFT: u32 = 1 << 8;
pub const LC_MODIFIER_RIGHT_SHIFT: u32 = 1 << 9;
pub const LC_MODIFIER_LEFT_CONTROL: u32 = 1 << 10;
pub const LC_MODIFIER_RIGHT_CONTROL: u32 = 1 << 11;
pub const LC_MODIFIER_LEFT_OPTION: u32 = 1 << 12;
pub const LC_MODIFIER_RIGHT_OPTION: u32 = 1 << 13;
pub const LC_MODIFIER_LEFT_COMMAND: u32 = 1 << 14;
pub const LC_MODIFIER_RIGHT_COMMAND: u32 = 1 << 15;
const LC_MODIFIERS_ALL: u32 = LC_MODIFIER_SHIFT
    | LC_MODIFIER_CONTROL
    | LC_MODIFIER_OPTION
    | LC_MODIFIER_COMMAND
    | LC_MODIFIER_CAPS_LOCK
    | LC_MODIFIER_LEFT_SHIFT
    | LC_MODIFIER_RIGHT_SHIFT
    | LC_MODIFIER_LEFT_CONTROL
    | LC_MODIFIER_RIGHT_CONTROL
    | LC_MODIFIER_LEFT_OPTION
    | LC_MODIFIER_RIGHT_OPTION
    | LC_MODIFIER_LEFT_COMMAND
    | LC_MODIFIER_RIGHT_COMMAND;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CoreConfig {
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub control_port: u16,
    pub discovery_port: u16,
    pub capabilities: u64,
    /// Translate Command and Control shortcuts when a controlling peer uses
    /// the other platform convention (see `remap.rs`). Off by default: keys
    /// keep their position, so a Mac's Command is the Windows key.
    pub shortcut_remap: bool,
    /// The shell reads and writes the system clipboard: `lc_core_set_clipboard`
    /// and the `clipboard` event (see `clipboard.rs`). Off by default.
    pub clipboard: bool,
    /// The shell reports copied files (`text/uri-list`) and takes received
    /// ones (see `files.rs`). Needs `clipboard` and `download_directory`.
    pub files: bool,
    /// Publish file offers to the shell without downloading their contents.
    /// The shell uses `Core::open_remote_files` to supply native file streams.
    pub on_demand_files: bool,
    /// Where received files go, e.g. `~/Downloads/LinkCursor`.
    pub download_directory: Option<String>,
    /// Larger transfers wait for `lc_core_accept_files`.
    pub max_auto_receive_bytes: u64,
    /// This device's displays in logical points (see `layout.rs`).
    pub displays: Vec<Display>,
    /// Placements saved by the shell from earlier `placement_changed` events.
    pub placements: Vec<PlacementRecord>,
    /// Where this device's key pair and certificate live (PEM), created on
    /// first start. Without it the core makes a new identity every start,
    /// so paired peers would no longer recognise it.
    pub identity_path: Option<String>,
    /// Peers the user paired, saved by the shell from `paired` events.
    pub trusted_peers: Vec<TrustedPeer>,
    /// Only paired peers may connect: others are asked to pair first.
    pub require_pairing: bool,
    /// Keep the user's devices connected without being asked: paired peers,
    /// and placed ones unless `require_pairing` (see `runtime/reconnect.rs`).
    /// On by default.
    pub auto_connect: bool,
    /// Devices to look for by address as well as by multicast, for networks
    /// that do not carry it (guest Wi-Fi, separate subnets, VPNs): host names
    /// or IPv4 addresses, optionally with the discovery port (`host:port`).
    /// See `runtime/manual.rs`.
    pub manual_addresses: Vec<String>,
}

/// A paired peer: its device ID and the fingerprint of its certificate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub peer_id: String,
    pub fingerprint: String,
}

/// Where a peer sits in this device's space. `offset: None` records that the
/// placement was removed, so the removal also wins over older placements.
/// `updated_ms` (Unix time) decides which side's edit wins when the two
/// devices sync: the more recent one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementRecord {
    pub peer_id: String,
    pub offset: Option<Offset>,
    pub updated_ms: u64,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            device_name: "LinkCursor".to_owned(),
            platform: "unknown".to_owned(),
            control_port: DEFAULT_CONTROL_PORT,
            discovery_port: DEFAULT_DISCOVERY_PORT,
            capabilities: LOCAL_CAPABILITIES,
            shortcut_remap: false,
            clipboard: false,
            files: false,
            on_demand_files: false,
            download_directory: None,
            max_auto_receive_bytes: DEFAULT_MAX_AUTO_RECEIVE_BYTES,
            displays: Vec::new(),
            placements: Vec::new(),
            identity_path: None,
            trusted_peers: Vec::new(),
            require_pairing: false,
            auto_connect: true,
            manual_addresses: Vec::new(),
        }
    }
}

impl CoreConfig {
    /// What this device announces and offers in `Hello`.
    pub fn advertised_capabilities(&self) -> u64 {
        let mut capabilities = self.capabilities;
        if self.clipboard {
            capabilities |= CAPABILITY_CLIPBOARD;
        }
        if self.files {
            capabilities |= CAPABILITY_FILES | CAPABILITY_FILE_STREAMS;
        }
        capabilities
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.on_demand_files && !self.files {
            return Err("on_demand_files needs files".to_owned());
        }
        if self.files && (!self.clipboard || self.download_directory.is_none()) {
            return Err("files needs clipboard and download_directory".to_owned());
        }
        if self.device_id.trim().is_empty() {
            return Err("device_id must not be empty".to_owned());
        }
        if self.device_name.trim().is_empty() {
            return Err("device_name must not be empty".to_owned());
        }
        if self.device_id.len() > 1024 || self.device_name.len() > 1024 {
            return Err("device identity fields must not exceed 1024 bytes".to_owned());
        }
        if !matches!(
            self.platform.as_str(),
            "android" | "macos" | "windows" | "linux" | "unknown"
        ) {
            return Err("platform must be android, macos, windows, linux, or unknown".to_owned());
        }
        if self.control_port == 0 || self.discovery_port == 0 {
            return Err("ports must be non-zero".to_owned());
        }
        validate_displays(&self.displays).map_err(str::to_owned)?;
        if self
            .placements
            .iter()
            .any(|record| record.offset.is_some_and(|offset| !offset.is_valid()))
        {
            return Err("placement offsets must be finite".to_owned());
        }
        if self.trusted_peers.iter().any(|peer| {
            peer.fingerprint.len() != 64 || !peer.fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
        }) {
            return Err("trusted peer fingerprints must be 64 hex digits".to_owned());
        }
        if self.manual_addresses.len() > MAX_MANUAL_ADDRESSES {
            return Err(format!(
                "at most {MAX_MANUAL_ADDRESSES} manual addresses are allowed"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Announcement {
    pub product: String,
    pub min_protocol_version: u16,
    pub max_protocol_version: u16,
    pub capabilities: u64,
    pub device_id: String,
    pub device_name: String,
    #[serde(default = "unknown_platform")]
    pub platform: String,
    pub control_port: u16,
    /// Lets devices that have not connected yet draw this one to scale.
    #[serde(default)]
    pub displays: Vec<Display>,
    /// SHA-256 of this device's certificate. Only a hint (anyone can
    /// announce anything): connections check the certificate itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Sent straight to an address the user added by hand: answer with an
    /// announcement to the sender, which multicast may not reach. Older
    /// cores ignore the field (and do not answer).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reply: bool,
}

impl Announcement {
    pub fn from_config(
        config: &CoreConfig,
        displays: &[Display],
        capabilities: u64,
        fingerprint: &str,
    ) -> Self {
        Self {
            product: "linkcursor".to_owned(),
            min_protocol_version: PROTOCOL_MIN_VERSION,
            max_protocol_version: PROTOCOL_MAX_VERSION,
            capabilities,
            device_id: config.device_id.clone(),
            device_name: config.device_name.clone(),
            platform: config.platform.clone(),
            control_port: config.control_port,
            displays: displays.to_vec(),
            fingerprint: Some(fingerprint.to_owned()),
            reply: false,
        }
    }

    pub fn negotiated_version(&self) -> Option<u16> {
        (self.product == "linkcursor")
            .then(|| negotiate_protocol(self.min_protocol_version, self.max_protocol_version))
            .flatten()
    }

    pub fn is_compatible(&self) -> bool {
        self.negotiated_version().is_some() && self.capabilities & CAPABILITY_INPUT_V2 != 0
    }
}

pub fn negotiate_protocol(remote_min: u16, remote_max: u16) -> Option<u16> {
    let minimum = PROTOCOL_MIN_VERSION.max(remote_min);
    let maximum = PROTOCOL_MAX_VERSION.min(remote_max);
    (minimum <= maximum).then_some(maximum)
}

#[derive(Debug, Clone, Serialize)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub address: String,
    pub port: u16,
    pub capabilities: u64,
    /// As last described by the peer, in its own logical space.
    pub displays: Vec<Display>,
    /// The certificate fingerprint it announces, unverified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip)]
    pub socket_address: SocketAddr,
    #[serde(skip)]
    pub last_seen: Instant,
}

fn unknown_platform() -> String {
    "unknown".to_owned()
}

/// Platform-neutral input event passed over the C ABI and the wire protocol.
///
/// - `code` is a USB HID usage (keyboard page) for keyboard events and a
///   one-based button number for pointer button events: 1 left, 2 right,
///   3 middle, 4 back, 5 forward. `buttons` has bit `n - 1` set while button
///   `n` is held.
/// - `delta_x`/`delta_y` are logical points (pixels at 100 % scale); the
///   receiver multiplies them by its own display scale.
/// - `scroll_x`/`scroll_y` are logical points. Positive `scroll_y` scrolls
///   toward the top of the content (wheel turned away from the user) and
///   positive `scroll_x` toward the left, as on macOS. With
///   `LC_INPUT_FLAG_PRECISE_SCROLL` they come from a trackpad or smooth
///   wheel; without it, from a notched wheel at 10 points per line.
///
/// Rust assigns `sequence` immediately before an event is written to the
/// network; platform callers should leave it as zero.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LcInputEvent {
    pub kind: u32,
    pub flags: u32,
    pub code: u32,
    pub key_symbol: u32,
    pub repeat_count: u32,
    pub modifiers: u32,
    pub buttons: u32,
    pub reserved: u32,
    pub sequence: u64,
    pub timestamp_micros: u64,
    pub x: f64,
    pub y: f64,
    pub delta_x: f64,
    pub delta_y: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
}

impl LcInputEvent {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self.kind {
            LC_INPUT_MOUSE_MOVE | LC_INPUT_SCROLL => {}
            LC_INPUT_MOUSE_BUTTON_DOWN | LC_INPUT_MOUSE_BUTTON_UP => {
                if !(1..=32).contains(&self.code) {
                    return Err("pointer button must be in the range 1...32");
                }
            }
            LC_INPUT_KEY_DOWN | LC_INPUT_KEY_UP => {
                if self.code == 0 || self.code > u16::MAX as u32 {
                    return Err("keyboard event must contain a USB HID usage");
                }
                if self.key_symbol != 0 && char::from_u32(self.key_symbol).is_none() {
                    return Err("keyboard symbol is not a valid Unicode scalar");
                }
            }
            _ => return Err("unknown input event kind"),
        }
        if self.flags & !LC_INPUT_FLAGS_ALL != 0 {
            return Err("input event contains unknown flags");
        }
        if self.modifiers & !LC_MODIFIERS_ALL != 0 {
            return Err("input event contains unknown modifiers");
        }
        if self.repeat_count > u16::MAX as u32 {
            return Err("input repeat count is too large");
        }
        if ![
            self.x,
            self.y,
            self.delta_x,
            self.delta_y,
            self.scroll_x,
            self.scroll_y,
        ]
        .into_iter()
        .all(f64::is_finite)
        {
            return Err("input event contains non-finite coordinates");
        }
        Ok(())
    }

    pub fn is_motion(&self) -> bool {
        self.kind == LC_INPUT_MOUSE_MOVE
    }

    pub fn is_scroll(&self) -> bool {
        self.kind == LC_INPUT_SCROLL
    }

    pub fn is_coalescible(&self) -> bool {
        self.is_motion() || self.is_scroll()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WireMessage {
    Hello {
        min_protocol_version: u16,
        max_protocol_version: u16,
        capabilities: u64,
        device_id: String,
        device_name: String,
        control_port: u16,
    },
    InputBatch {
        first_sequence: u64,
        events: Vec<LcInputEvent>,
    },
    InputReset {
        sequence: u64,
    },
    Ping {
        sequence: u64,
    },
    Pong {
        sequence: u64,
    },
    /// The sender's displays; sent after the handshake and when they change.
    ScreenInfo {
        displays: Vec<Display>,
    },
    /// Where the receiver sits in the sender's space (`None`: not placed).
    Placement {
        offset: Option<Offset>,
        updated_ms: u64,
    },
    /// Part of the sender's clipboard (see `clipboard.rs`): bytes
    /// `offset..offset + data.len()` of transfer `id`, `total` bytes long.
    /// Data link only.
    ClipboardChunk {
        id: u32,
        total: u32,
        offset: u32,
        data: Vec<u8>,
    },
    /// Control connection, from the side that accepted it: open the data
    /// link to `port` (the inviting side's bulk endpoint, UDP) with this
    /// token. Only to peers sharing a data-link capability.
    DataInvite {
        token: u64,
        port: u16,
    },
    /// First frame of a data link, from the side that dialed it.
    DataHello {
        device_id: String,
        token: u64,
    },
    /// Data link: send entry `entry` of transfer `transfer` from `offset`.
    FileRequest {
        transfer: u32,
        entry: u32,
        offset: u64,
    },
    /// Data link: bytes of a requested entry.
    FileChunk {
        transfer: u32,
        entry: u32,
        offset: u64,
        data: Vec<u8>,
    },
    /// Data link, either way: the transfer stops (`reason` for people).
    FileCancel {
        transfer: u32,
        reason: String,
    },
    FileStreamOpen {
        transfer: u32,
        lease: u64,
    },
    FileStreamOpened {
        lease: u64,
        reason: String,
    },
    FileStreamRead {
        lease: u64,
        request: u64,
        entry: u32,
        offset: u64,
        length: u32,
    },
    FileStreamData {
        request: u64,
        data: Vec<u8>,
        reason: String,
    },
    FileStreamClose {
        lease: u64,
        from_source: bool,
        reason: String,
    },
    FileOfferRevoke {
        transfer: u32,
    },
    /// Control connection: stop driving the sender, which someone else now
    /// controls. The receiver gives control back with an `InputReset`. With
    /// `handoff` the sender itself drives the receiver next.
    ControlRevoke {
        handoff: bool,
    },
    /// After `Hello`: the sender does not know the receiver's certificate
    /// and asks to pair. Both users compare the code their devices show and
    /// accept.
    PairRequest,
    /// The sender's user checked the code and accepted pairing. Paired once
    /// both sides have sent it. A user who declines closes the connection
    /// with `CloseCode::PairingDeclined` instead.
    PairAccept,
    /// Pairing, from the side that dialed the connection: a hash of its
    /// random part of the code, sent before it sees the other side's part.
    /// Only to peers with `CAPABILITY_PAIRING_V2`.
    PairCommit {
        commitment: [u8; 32],
    },
    /// Pairing, from the side that accepted the connection: its random part
    /// of the code, sent once it holds the dialer's commitment.
    PairNonce {
        nonce: [u8; 32],
    },
    /// Pairing, from the dialer: the random part it committed to. The
    /// receiver checks it against the commitment.
    PairReveal {
        nonce: [u8; 32],
    },
    /// Someone uses the sender's keyboard or mouse (at most every few
    /// seconds): the receiver stays awake meanwhile if its user wants that.
    /// Only to peers with `CAPABILITY_PRESENCE`.
    UserActive,
    /// The sender's user locked it: the receiver locks too if its user wants
    /// that. Only to peers with `CAPABILITY_PRESENCE`.
    LockScreen,
    /// From a driven device: a game or a pointer-lock page holds its pointer
    /// (`locked`), or no longer does. The driving device keeps the pointer
    /// there meanwhile instead of crossing at the edges. Only to peers with
    /// `CAPABILITY_POINTER_LOCK`.
    PointerLock {
        locked: bool,
    },
}

impl WireMessage {
    pub fn hello(config: &CoreConfig, capabilities: u64) -> Self {
        Self::Hello {
            min_protocol_version: PROTOCOL_MIN_VERSION,
            max_protocol_version: PROTOCOL_MAX_VERSION,
            capabilities,
            device_id: config.device_id.clone(),
            device_name: config.device_name.clone(),
            control_port: config.control_port,
        }
    }
}
