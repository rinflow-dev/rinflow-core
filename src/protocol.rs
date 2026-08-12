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

pub fn encode_message(message: &WireMessage) -> io::Result<Vec<u8>> {
    let message_type = match message {
        WireMessage::Hello { .. } => MESSAGE_HELLO,
        WireMessage::InputBatch { .. } => MESSAGE_INPUT_BATCH,
        WireMessage::InputReset { .. } => MESSAGE_INPUT_RESET,
        WireMessage::Ping { .. } => MESSAGE_PING,
        WireMessage::Pong { .. } => MESSAGE_PONG,
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
        _ => return Err(invalid_data("unknown wire message type")),
    };
    decoder.finish()?;
    Ok(message)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
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
