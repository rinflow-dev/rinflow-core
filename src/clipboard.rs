//! Clipboard contents exchanged between peers.
//!
//! The clipboard follows the pointer, as in Deskflow: when this device starts
//! driving a peer it hands the peer its clipboard, and when a peer gives
//! control back the peer hands back its own. Only contents that changed since
//! the last exchange with that peer travel, which also keeps a clipboard from
//! echoing back to where it came from.
//!
//! A clipboard is a list of representations of the same content, most
//! specific first. Shells exchange these MIME types:
//!
//! - [`MIME_TEXT`]: UTF-8 text.
//! - [`MIME_HTML`]: UTF-8 HTML (a fragment or a document).
//! - [`MIME_PNG`]: a PNG image.
//!
//! Receivers ignore types they do not know. Contents travel in chunks on the
//! connection's bulk lane, behind input, so a large image never holds up the
//! pointer for more than one chunk.

use crate::model::WireMessage;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hasher};

pub const MIME_TEXT: &str = "text/plain;charset=utf-8";
pub const MIME_HTML: &str = "text/html;charset=utf-8";
pub const MIME_PNG: &str = "image/png";

/// Largest clipboard (all representations together) sent to a peer.
pub const MAX_CLIPBOARD_SIZE: usize = 8 * 1024 * 1024;
/// Bytes per `ClipboardChunk`: input waits behind at most one of these.
pub const CLIPBOARD_CHUNK_SIZE: usize = 64 * 1024;
const MAX_FORMATS: usize = 16;
const MAX_MIME_LENGTH: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardFormat {
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Clipboard {
    pub formats: Vec<ClipboardFormat>,
}

impl Clipboard {
    pub fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.formats.len() > MAX_FORMATS {
            return Err("too many clipboard formats");
        }
        for format in &self.formats {
            let mime = format.mime.as_str();
            if mime.is_empty() || mime.len() > MAX_MIME_LENGTH || !mime.is_ascii() {
                return Err("invalid clipboard MIME type");
            }
        }
        if self.encoded_size() > MAX_CLIPBOARD_SIZE {
            return Err("clipboard is larger than 8 MiB");
        }
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        2 + self
            .formats
            .iter()
            .map(|format| 2 + format.mime.len() + 4 + format.data.len())
            .sum::<usize>()
    }

    /// `u16` count, then per format `u16` + MIME bytes and `u32` + data, big
    /// endian. Call [`Clipboard::validate`] first.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.encoded_size());
        bytes.extend_from_slice(&(self.formats.len() as u16).to_be_bytes());
        for format in &self.formats {
            bytes.extend_from_slice(&(format.mime.len() as u16).to_be_bytes());
            bytes.extend_from_slice(format.mime.as_bytes());
            bytes.extend_from_slice(&(format.data.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&format.data);
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        fn take<'a>(rest: &mut &'a [u8], length: usize) -> Result<&'a [u8], &'static str> {
            if rest.len() < length {
                return Err("truncated clipboard");
            }
            let (head, tail) = rest.split_at(length);
            *rest = tail;
            Ok(head)
        }
        let mut rest = bytes;
        let count = u16::from_be_bytes(take(&mut rest, 2)?.try_into().expect("two bytes")) as usize;
        if count > MAX_FORMATS {
            return Err("too many clipboard formats");
        }
        let mut formats = Vec::with_capacity(count);
        for _ in 0..count {
            let length =
                u16::from_be_bytes(take(&mut rest, 2)?.try_into().expect("two bytes")) as usize;
            let mime = std::str::from_utf8(take(&mut rest, length)?)
                .map_err(|_| "clipboard MIME type is not UTF-8")?
                .to_owned();
            let length =
                u32::from_be_bytes(take(&mut rest, 4)?.try_into().expect("four bytes")) as usize;
            let data = take(&mut rest, length)?.to_vec();
            formats.push(ClipboardFormat { mime, data });
        }
        if !rest.is_empty() {
            return Err("clipboard has trailing bytes");
        }
        let clipboard = Self { formats };
        clipboard.validate()?;
        Ok(clipboard)
    }

    /// The `formats` array of the `clipboard` event and of
    /// `lc_core_set_clipboard`: `[{"mime": …, "data": base64}, …]`.
    pub fn to_json(&self) -> Value {
        Value::Array(
            self.formats
                .iter()
                .map(|format| json!({ "mime": format.mime, "data": BASE64.encode(&format.data) }))
                .collect(),
        )
    }

    /// Reads `{"formats": [{"mime": …, "data": base64}, …]}`.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let Some(formats) = value["formats"].as_array() else {
            return Err("clipboard JSON needs a \"formats\" array".to_owned());
        };
        let formats = formats
            .iter()
            .map(|format| {
                let mime = format["mime"].as_str().ok_or("clipboard format needs \"mime\"")?;
                let data = format["data"].as_str().ok_or("clipboard format needs \"data\"")?;
                let data = BASE64
                    .decode(data)
                    .map_err(|_| "clipboard data is not base64")?;
                Ok(ClipboardFormat {
                    mime: mime.to_owned(),
                    data,
                })
            })
            .collect::<Result<Vec<_>, &str>>()?;
        let clipboard = Self { formats };
        clipboard.validate()?;
        Ok(clipboard)
    }
}

/// Identifies contents, to send only what a peer does not have yet.
pub fn digest(encoded: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(encoded);
    hasher.finish()
}

/// Splits encoded contents into the frames of transfer `id`.
pub fn chunks(id: u32, encoded: &[u8]) -> Vec<WireMessage> {
    let total = encoded.len() as u32;
    let mut offset = 0;
    let mut frames = Vec::with_capacity(encoded.len().div_ceil(CLIPBOARD_CHUNK_SIZE).max(1));
    loop {
        let end = (offset + CLIPBOARD_CHUNK_SIZE).min(encoded.len());
        frames.push(WireMessage::ClipboardChunk {
            id,
            total,
            offset: offset as u32,
            data: encoded[offset..end].to_vec(),
        });
        offset = end;
        if offset == encoded.len() {
            return frames;
        }
    }
}

/// Puts a peer's chunks back together. A chunk of a newer transfer drops a
/// transfer still in progress: the newer contents replace it anyway.
#[derive(Debug, Default)]
pub struct ClipboardAssembler {
    id: Option<u32>,
    total: usize,
    bytes: Vec<u8>,
}

impl ClipboardAssembler {
    /// The encoded contents once the last chunk of a transfer arrived.
    pub fn push(
        &mut self,
        id: u32,
        total: u32,
        offset: u32,
        data: &[u8],
    ) -> Result<Option<Vec<u8>>, &'static str> {
        let (total, offset) = (total as usize, offset as usize);
        if self.id != Some(id) {
            if offset != 0 {
                return Err("clipboard transfer does not start at offset 0");
            }
            self.id = Some(id);
            self.total = total;
            self.bytes = Vec::with_capacity(total);
        }
        if total != self.total || offset != self.bytes.len() {
            self.id = None;
            return Err("clipboard chunk out of order");
        }
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > self.total {
            self.id = None;
            return Err("clipboard chunk past the announced size");
        }
        if self.bytes.len() < self.total {
            return Ok(None);
        }
        self.id = None;
        Ok(Some(std::mem::take(&mut self.bytes)))
    }
}
