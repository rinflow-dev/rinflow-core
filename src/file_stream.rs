//! Blocking, bounded reads for native clipboard providers. Never call these
//! methods from a core event callback or an input/UI event loop.
use crate::runtime::Command;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

pub const MAX_READ_SIZE: usize = 256 * 1024;
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(15);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug)]
pub enum StreamCommand {
    Open {
        peer: String,
        transfer: u32,
        lease: u64,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    Read {
        lease: u64,
        request: u64,
        entry: u32,
        offset: u64,
        length: u32,
        reply: mpsc::SyncSender<Result<Vec<u8>, String>>,
    },
    Close {
        lease: u64,
        reason: String,
    },
    AbandonRead(u64),
    Revoke,
}

/// A source snapshot pinned for one native copy operation. Changing either
/// clipboard does not cancel it. Drop releases source resources; disconnect,
/// sharing being disabled, and explicit cancellation invalidate it.
pub struct RemoteFileLease {
    pub(crate) commands: UnboundedSender<Command>,
    pub(crate) id: u64,
}

impl RemoteFileLease {
    /// Returns at most `MAX_READ_SIZE` bytes, or an error. Offsets are u64 so
    /// large files and seeking do not require an entire local cache.
    pub fn read(&self, entry: u32, offset: u64, length: usize) -> Result<Vec<u8>, String> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let request = next_id();
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .send(Command::FileStream(StreamCommand::Read {
                lease: self.id,
                request,
                entry,
                offset,
                length: length.min(MAX_READ_SIZE) as u32,
                reply,
            }))
            .map_err(|_| "core stopped".to_owned())?;
        match result.recv_timeout(READ_TIMEOUT) {
            Ok(result) => result,
            Err(_) => {
                let _ = self
                    .commands
                    .send(Command::FileStream(StreamCommand::AbandonRead(request)));
                Err("remote file read timed out".to_owned())
            }
        }
    }

    /// The target finished or cancelled. `delivered` means bytes supplied,
    /// not that an arbitrary target application has committed its output.
    pub fn finish(&self, reason: &str) {
        let _ = self
            .commands
            .send(Command::FileStream(StreamCommand::Close {
                lease: self.id,
                reason: reason.to_owned(),
            }));
    }
}

impl Drop for RemoteFileLease {
    fn drop(&mut self) {
        self.finish("released");
    }
}
