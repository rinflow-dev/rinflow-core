use crate::clipboard::{CLIPBOARD_CHUNK_SIZE, MAX_CLIPBOARD_SIZE};
use crate::files::FILE_CHUNK_SIZE;
use crate::layout::{Display, MAX_DISPLAYS, Offset, validate_displays};
use crate::model::{LcInputEvent, MAX_INPUT_BATCH_SIZE, WireMessage};
use std::io;

const WIRE_MAGIC: [u8; 2] = *b"LC";
const WIRE_FORMAT_VERSION: u8 = 1;
const WIRE_HEADER_SIZE: usize = 8;
const MAX_IDENTITY_LENGTH: usize = 1024;

const MESSAGE_HELLO: u8 = 1;
const MESSAGE_INPUT_BATCH: u8 = 2;
const MESSAGE_INPUT_RESET: u8 = 3;
const MESSAGE_PING: u8 = 4;
const MESSAGE_PONG: u8 = 5;
const MESSAGE_SCREEN_INFO: u8 = 6;
const MESSAGE_PLACEMENT: u8 = 7;
const MESSAGE_CLIPBOARD_CHUNK: u8 = 8;
const MESSAGE_DATA_INVITE: u8 = 9;
const MESSAGE_DATA_HELLO: u8 = 10;
const MESSAGE_FILE_REQUEST: u8 = 11;
const MESSAGE_FILE_CHUNK: u8 = 12;
const MESSAGE_FILE_CANCEL: u8 = 13;
const MESSAGE_CONTROL_REVOKE: u8 = 14;
const MESSAGE_PAIR_REQUEST: u8 = 15;
const MESSAGE_PAIR_ACCEPT: u8 = 16;
const MESSAGE_USER_ACTIVE: u8 = 17;
const MESSAGE_LOCK_SCREEN: u8 = 18;
const MESSAGE_POINTER_LOCK: u8 = 19;
const MAX_REASON_LENGTH: usize = 256;

pub fn encode_message(message: &WireMessage) -> io::Result<Vec<u8>> {
    let message_type = match message {
        WireMessage::Hello { .. } => MESSAGE_HELLO,
        WireMessage::InputBatch { .. } => MESSAGE_INPUT_BATCH,
        WireMessage::InputReset { .. } => MESSAGE_INPUT_RESET,
        WireMessage::Ping { .. } => MESSAGE_PING,
        WireMessage::Pong { .. } => MESSAGE_PONG,
        WireMessage::ScreenInfo { .. } => MESSAGE_SCREEN_INFO,
        WireMessage::Placement { .. } => MESSAGE_PLACEMENT,
        WireMessage::ClipboardChunk { .. } => MESSAGE_CLIPBOARD_CHUNK,
        WireMessage::DataInvite { .. } => MESSAGE_DATA_INVITE,
        WireMessage::DataHello { .. } => MESSAGE_DATA_HELLO,
        WireMessage::FileRequest { .. } => MESSAGE_FILE_REQUEST,
        WireMessage::FileChunk { .. } => MESSAGE_FILE_CHUNK,
        WireMessage::FileCancel { .. } => MESSAGE_FILE_CANCEL,
        WireMessage::ControlRevoke { .. } => MESSAGE_CONTROL_REVOKE,
        WireMessage::PairRequest => MESSAGE_PAIR_REQUEST,
        WireMessage::PairAccept => MESSAGE_PAIR_ACCEPT,
        WireMessage::UserActive => MESSAGE_USER_ACTIVE,
        WireMessage::LockScreen => MESSAGE_LOCK_SCREEN,
        WireMessage::PointerLock { .. } => MESSAGE_POINTER_LOCK,
    };
    let mut encoder = Encoder::with_header(message_type);
    match message {
        WireMessage::Hello {
            min_protocol_version,
            max_protocol_version,
            capabilities,
            device_id,
            device_name,
            control_port,
        } => {
            if min_protocol_version > max_protocol_version {
                return Err(invalid_data("invalid protocol version range"));
            }
            encoder.u16(*min_protocol_version);
            encoder.u16(*max_protocol_version);
            encoder.u16(*control_port);
            encoder.u64(*capabilities);
            encoder.string(device_id)?;
            encoder.string(device_name)?;
        }
        WireMessage::InputBatch {
            first_sequence,
            events,
        } => {
            if *first_sequence == 0 {
                return Err(invalid_data("input sequence must be non-zero"));
            }
            if events.is_empty() || events.len() > MAX_INPUT_BATCH_SIZE {
                return Err(invalid_data("invalid input batch size"));
            }
            encoder.u64(*first_sequence);
            encoder.u16(events.len() as u16);
            for event in events {
                event.validate().map_err(invalid_data)?;
                encoder.input_event(event);
            }
        }
        WireMessage::InputReset { sequence } => {
            if *sequence == 0 {
                return Err(invalid_data("input reset sequence must be non-zero"));
            }
            encoder.u64(*sequence);
        }
        WireMessage::Ping { sequence } | WireMessage::Pong { sequence } => {
            encoder.u64(*sequence);
        }
        WireMessage::ScreenInfo { displays } => {
            validate_displays(displays).map_err(invalid_data)?;
            encoder.u16(displays.len() as u16);
            for display in displays {
                for value in [
                    display.x,
                    display.y,
                    display.width,
                    display.height,
                    display.scale,
                ] {
                    encoder.f64(value);
                }
            }
        }
        WireMessage::Placement { offset, updated_ms } => {
            match offset {
                Some(offset) if offset.is_valid() => {
                    encoder.u8(1);
                    encoder.f64(offset.x);
                    encoder.f64(offset.y);
                }
                Some(_) => return Err(invalid_data("placement offset must be finite")),
                None => encoder.u8(0),
            }
            encoder.u64(*updated_ms);
        }
        WireMessage::ClipboardChunk {
            id,
            total,
            offset,
            data,
        } => {
            check_clipboard_chunk(*total, *offset, data.len())?;
            encoder.u32(*id);
            encoder.u32(*total);
            encoder.u32(*offset);
            encoder.u32(data.len() as u32);
            encoder.bytes.extend_from_slice(data);
        }
        WireMessage::DataInvite { token, port } => {
            encoder.u64(*token);
            encoder.u16(*port);
        }
        WireMessage::DataHello { device_id, token } => {
            encoder.string(device_id)?;
            encoder.u64(*token);
        }
        WireMessage::FileRequest {
            transfer,
            entry,
            offset,
        } => {
            encoder.u32(*transfer);
            encoder.u32(*entry);
            encoder.u64(*offset);
        }
        WireMessage::FileChunk {
            transfer,
            entry,
            offset,
            data,
        } => {
            if data.len() > FILE_CHUNK_SIZE {
                return Err(invalid_data("file chunk too large"));
            }
            encoder.u32(*transfer);
            encoder.u32(*entry);
            encoder.u64(*offset);
            encoder.u32(data.len() as u32);
            encoder.bytes.extend_from_slice(data);
        }
        WireMessage::FileCancel { transfer, reason } => {
            let reason: String = reason.chars().take(MAX_REASON_LENGTH / 4).collect();
            encoder.u32(*transfer);
            encoder.u16(reason.len() as u16);
            encoder.bytes.extend_from_slice(reason.as_bytes());
        }
        WireMessage::ControlRevoke { handoff } => encoder.u8(u8::from(*handoff)),
        WireMessage::PairRequest
        | WireMessage::PairAccept
        | WireMessage::UserActive
        | WireMessage::LockScreen => {}
        WireMessage::PointerLock { locked } => encoder.u8(u8::from(*locked)),
    }
    Ok(encoder.bytes)
}

pub fn decode_message(bytes: &[u8]) -> io::Result<WireMessage> {
    if bytes.len() < WIRE_HEADER_SIZE {
        return Err(invalid_data("wire frame is shorter than its header"));
    }
    let mut decoder = Decoder::new(bytes);
    if decoder.take(2)? != WIRE_MAGIC {
        return Err(invalid_data("invalid wire magic"));
    }
    if decoder.u8()? != WIRE_FORMAT_VERSION {
        return Err(invalid_data("unsupported wire format"));
    }
    let message_type = decoder.u8()?;
    let flags = decoder.u16()?;
    let reserved = decoder.u16()?;
    if flags != 0 || reserved != 0 {
        return Err(invalid_data("unsupported wire header flags"));
    }

    let message = match message_type {
        MESSAGE_HELLO => {
            let min_protocol_version = decoder.u16()?;
            let max_protocol_version = decoder.u16()?;
            let control_port = decoder.u16()?;
            let capabilities = decoder.u64()?;
            let device_id = decoder.string()?;
            let device_name = decoder.string()?;
            if min_protocol_version > max_protocol_version
                || control_port == 0
                || device_id.trim().is_empty()
                || device_name.trim().is_empty()
            {
                return Err(invalid_data("invalid hello payload"));
            }
            WireMessage::Hello {
                min_protocol_version,
                max_protocol_version,
                capabilities,
                device_id,
                device_name,
                control_port,
            }
        }
        MESSAGE_INPUT_BATCH => {
            let first_sequence = decoder.u64()?;
            let count = decoder.u16()? as usize;
            if first_sequence == 0 || count == 0 || count > MAX_INPUT_BATCH_SIZE {
                return Err(invalid_data("invalid input batch header"));
            }
            let mut events = Vec::with_capacity(count);
            for index in 0..count {
                let mut event = decoder.input_event()?;
                event.sequence = first_sequence
                    .checked_add(index as u64)
                    .ok_or_else(|| invalid_data("input sequence overflow"))?;
                event.validate().map_err(invalid_data)?;
                events.push(event);
            }
            WireMessage::InputBatch {
                first_sequence,
                events,
            }
        }
        MESSAGE_INPUT_RESET => {
            let sequence = decoder.u64()?;
            if sequence == 0 {
                return Err(invalid_data("input reset sequence must be non-zero"));
            }
            WireMessage::InputReset { sequence }
        }
        MESSAGE_PING => WireMessage::Ping {
            sequence: decoder.u64()?,
        },
        MESSAGE_PONG => WireMessage::Pong {
            sequence: decoder.u64()?,
        },
        MESSAGE_SCREEN_INFO => {
            let count = decoder.u16()? as usize;
            if count > MAX_DISPLAYS {
                return Err(invalid_data("too many displays"));
            }
            let mut displays = Vec::with_capacity(count);
            for _ in 0..count {
                displays.push(Display {
                    x: decoder.f64()?,
                    y: decoder.f64()?,
                    width: decoder.f64()?,
                    height: decoder.f64()?,
                    scale: decoder.f64()?,
                });
            }
            validate_displays(&displays).map_err(invalid_data)?;
            WireMessage::ScreenInfo { displays }
        }
        MESSAGE_PLACEMENT => {
            let offset = match decoder.u8()? {
                0 => None,
                1 => {
                    let offset = Offset {
                        x: decoder.f64()?,
                        y: decoder.f64()?,
                    };
                    if !offset.is_valid() {
                        return Err(invalid_data("placement offset must be finite"));
                    }
                    Some(offset)
                }
                _ => return Err(invalid_data("invalid placement flag")),
            };
            WireMessage::Placement {
                offset,
                updated_ms: decoder.u64()?,
            }
        }
        MESSAGE_CLIPBOARD_CHUNK => {
            let id = decoder.u32()?;
            let total = decoder.u32()?;
            let offset = decoder.u32()?;
            let length = decoder.u32()? as usize;
            check_clipboard_chunk(total, offset, length)?;
            WireMessage::ClipboardChunk {
                id,
                total,
                offset,
                data: decoder.take(length)?.to_vec(),
            }
        }
        MESSAGE_DATA_INVITE => {
            let token = decoder.u64()?;
            let port = decoder.u16()?;
            if port == 0 {
                return Err(invalid_data("data link port must not be zero"));
            }
            WireMessage::DataInvite { token, port }
        }
        MESSAGE_DATA_HELLO => WireMessage::DataHello {
            device_id: decoder.string()?,
            token: decoder.u64()?,
        },
        MESSAGE_FILE_REQUEST => WireMessage::FileRequest {
            transfer: decoder.u32()?,
            entry: decoder.u32()?,
            offset: decoder.u64()?,
        },
        MESSAGE_FILE_CHUNK => {
            let transfer = decoder.u32()?;
            let entry = decoder.u32()?;
            let offset = decoder.u64()?;
            let length = decoder.u32()? as usize;
            if length > FILE_CHUNK_SIZE {
                return Err(invalid_data("file chunk too large"));
            }
            WireMessage::FileChunk {
                transfer,
                entry,
                offset,
                data: decoder.take(length)?.to_vec(),
            }
        }
        MESSAGE_FILE_CANCEL => {
            let transfer = decoder.u32()?;
            let length = decoder.u16()? as usize;
            if length > MAX_REASON_LENGTH {
                return Err(invalid_data("cancel reason too long"));
            }
            let reason = String::from_utf8(decoder.take(length)?.to_vec())
                .map_err(|_| invalid_data("cancel reason is not UTF-8"))?;
            WireMessage::FileCancel { transfer, reason }
        }
        MESSAGE_CONTROL_REVOKE => WireMessage::ControlRevoke {
            handoff: match decoder.u8()? {
                0 => false,
                1 => true,
                _ => return Err(invalid_data("invalid control revoke flag")),
            },
        },
        MESSAGE_PAIR_REQUEST => WireMessage::PairRequest,
        MESSAGE_PAIR_ACCEPT => WireMessage::PairAccept,
        MESSAGE_USER_ACTIVE => WireMessage::UserActive,
        MESSAGE_LOCK_SCREEN => WireMessage::LockScreen,
        MESSAGE_POINTER_LOCK => WireMessage::PointerLock {
            locked: match decoder.u8()? {
                0 => false,
                1 => true,
                _ => return Err(invalid_data("invalid pointer lock flag")),
            },
        },
        _ => return Err(invalid_data("unknown wire message type")),
    };
    decoder.finish()?;
    Ok(message)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn check_clipboard_chunk(total: u32, offset: u32, length: usize) -> io::Result<()> {
    let (total, offset) = (total as usize, offset as usize);
    if total > MAX_CLIPBOARD_SIZE || length > CLIPBOARD_CHUNK_SIZE || offset + length > total {
        return Err(invalid_data("invalid clipboard chunk"));
    }
    Ok(())
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn with_header(message_type: u8) -> Self {
        let mut encoder = Self {
            bytes: Vec::with_capacity(128),
        };
        encoder.bytes.extend_from_slice(&WIRE_MAGIC);
        encoder.u8(WIRE_FORMAT_VERSION);
        encoder.u8(message_type);
        encoder.u16(0);
        encoder.u16(0);
        encoder
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn f64(&mut self, value: f64) {
        self.u64(value.to_bits());
    }

    fn string(&mut self, value: &str) -> io::Result<()> {
        let length = value.len();
        if length == 0 || length > MAX_IDENTITY_LENGTH || length > u16::MAX as usize {
            return Err(invalid_data("invalid identity string length"));
        }
        self.u16(length as u16);
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn input_event(&mut self, event: &LcInputEvent) {
        self.u32(event.kind);
        self.u32(event.flags);
        self.u32(event.code);
        self.u32(event.key_symbol);
        self.u32(event.repeat_count);
        self.u32(event.modifiers);
        self.u32(event.buttons);
        self.u32(0);
        self.u64(event.timestamp_micros);
        self.f64(event.x);
        self.f64(event.y);
        self.f64(event.delta_x);
        self.f64(event.delta_y);
        self.f64(event.scroll_x);
        self.f64(event.scroll_y);
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| invalid_data("wire frame offset overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| invalid_data("truncated wire frame"))?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> io::Result<u16> {
        let bytes: [u8; 2] = self.take(2)?.try_into().expect("length checked");
        Ok(u16::from_be_bytes(bytes))
    }

    fn u32(&mut self) -> io::Result<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().expect("length checked");
        Ok(u32::from_be_bytes(bytes))
    }

    fn u64(&mut self) -> io::Result<u64> {
        let bytes: [u8; 8] = self.take(8)?.try_into().expect("length checked");
        Ok(u64::from_be_bytes(bytes))
    }

    fn f64(&mut self) -> io::Result<f64> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn string(&mut self) -> io::Result<String> {
        let length = self.u16()? as usize;
        if length == 0 || length > MAX_IDENTITY_LENGTH {
            return Err(invalid_data("invalid identity string length"));
        }
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| invalid_data("identity string is not UTF-8"))
    }

    fn input_event(&mut self) -> io::Result<LcInputEvent> {
        let kind = self.u32()?;
        let flags = self.u32()?;
        let code = self.u32()?;
        let key_symbol = self.u32()?;
        let repeat_count = self.u32()?;
        let modifiers = self.u32()?;
        let buttons = self.u32()?;
        let reserved = self.u32()?;
        if reserved != 0 {
            return Err(invalid_data("input event reserved field must be zero"));
        }
        Ok(LcInputEvent {
            kind,
            flags,
            code,
            key_symbol,
            repeat_count,
            modifiers,
            buttons,
            reserved: 0,
            sequence: 0,
            timestamp_micros: self.u64()?,
            x: self.f64()?,
            y: self.f64()?,
            delta_x: self.f64()?,
            delta_y: self.f64()?,
            scroll_x: self.f64()?,
            scroll_y: self.f64()?,
        })
    }

    fn finish(self) -> io::Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid_data("wire frame contains trailing bytes"))
        }
    }
}
