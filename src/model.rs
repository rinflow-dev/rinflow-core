use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Instant;

pub const PROTOCOL_MIN_VERSION: u16 = 2;
pub const PROTOCOL_MAX_VERSION: u16 = 2;
pub const DEFAULT_DISCOVERY_PORT: u16 = 42_420;
pub const DEFAULT_CONTROL_PORT: u16 = 42_421;
pub const DISCOVERY_GROUP: [u8; 4] = [239, 255, 42, 99];
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_INPUT_BATCH_SIZE: usize = 64;

pub const CAPABILITY_INPUT_V2: u64 = 1 << 0;
pub const CAPABILITY_RELATIVE_POINTER: u64 = 1 << 1;
pub const CAPABILITY_HIGH_RESOLUTION_SCROLL: u64 = 1 << 3;
pub const CAPABILITY_HID_KEYBOARD: u64 = 1 << 4;
pub const LOCAL_CAPABILITIES: u64 = CAPABILITY_INPUT_V2
    | CAPABILITY_RELATIVE_POINTER
    | CAPABILITY_HIGH_RESOLUTION_SCROLL
    | CAPABILITY_HID_KEYBOARD;

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
    pub control_port: u16,
    pub discovery_port: u16,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            device_name: "LinkCursor Mac".to_owned(),
            control_port: DEFAULT_CONTROL_PORT,
            discovery_port: DEFAULT_DISCOVERY_PORT,
        }
    }
}

impl CoreConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.device_id.trim().is_empty() {
            return Err("device_id must not be empty".to_owned());
        }
        if self.device_name.trim().is_empty() {
            return Err("device_name must not be empty".to_owned());
        }
        if self.device_id.len() > 1024 || self.device_name.len() > 1024 {
            return Err("device identity fields must not exceed 1024 bytes".to_owned());
        }
        if self.control_port == 0 || self.discovery_port == 0 {
            return Err("ports must be non-zero".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Announcement {
    pub product: String,
    pub min_protocol_version: u16,
    pub max_protocol_version: u16,
    pub capabilities: u64,
    pub device_id: String,
    pub device_name: String,
    pub control_port: u16,
}

impl Announcement {
    pub fn from_config(config: &CoreConfig) -> Self {
        Self {
            product: "linkcursor".to_owned(),
            min_protocol_version: PROTOCOL_MIN_VERSION,
            max_protocol_version: PROTOCOL_MAX_VERSION,
            capabilities: LOCAL_CAPABILITIES,
            device_id: config.device_id.clone(),
            device_name: config.device_name.clone(),
            control_port: config.control_port,
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
    pub address: String,
    pub port: u16,
    pub capabilities: u64,
    #[serde(skip)]
    pub socket_address: SocketAddr,
    #[serde(skip)]
    pub last_seen: Instant,
}

/// Platform-neutral input event passed over the C ABI and the wire protocol.
///
/// `code` is a USB HID usage for keyboard events and a one-based button number
/// for pointer button events. Rust assigns `sequence` immediately before an
/// event is written to the network; platform callers should leave it as zero.
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
}

impl WireMessage {
    pub fn hello(config: &CoreConfig) -> Self {
        Self::Hello {
            min_protocol_version: PROTOCOL_MIN_VERSION,
            max_protocol_version: PROTOCOL_MAX_VERSION,
            capabilities: LOCAL_CAPABILITIES,
            device_id: config.device_id.clone(),
            device_name: config.device_name.clone(),
            control_port: config.control_port,
        }
    }
}
