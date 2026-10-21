//! The data link of a session and what travels on it: clipboards and files.
//!
//! Every session with a peer sharing a data-link capability gets a second
//! connection, so bulk data never waits in front of input in one stream,
//! and the network can queue it apart (see `qos.rs`). The side that
//! accepted the session sends `DataInvite { token, port }`; the side that
//! dialed it connects to the inviting side's bulk endpoint (`port`) and opens
//! with `DataHello { device_id, token }`, which binds the new connection to
//! the session. It must show the same certificate as the session. When the
//! data link drops, the accepting side invites again from `maintenance`.

use super::{Runtime, random_u64};
use crate::clipboard::{self, Clipboard, ClipboardFormat};
use crate::files::{self, FILE_CHUNK_SIZE, FileWriter, MIME_FILES, MIME_URI_LIST, Manifest, Offer};
use crate::model::{CAPABILITY_CLIPBOARD, DATA_LINK_CAPABILITIES, WireMessage};
use crate::transport::{
    ActiveConnection, ConnectionEvent, Link, connect_data_in_background, spawn_connection,
};
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// How often the accepting side invites again while a data link is missing.
const REINVITE_INTERVAL: Duration = Duration::from_secs(5);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

pub(super) struct DataLink {
    /// The control connection (session) this link belongs to.
    pub session: u64,
    pub connection: ActiveConnection,
}

/// Files this device is receiving: one transfer at a time.
pub(super) struct Download {
    session: u64,
    peer_id: String,
    manifest: Manifest,
    accepted: bool,
    /// Destination of each entry, and the top-level ones for the clipboard.
    targets: Vec<PathBuf>,
    tops: Vec<PathBuf>,
    /// The entry being received, and its bytes received so far.
    entry: usize,
    entry_received: u64,
    /// Writes the files; started with the first file that has content.
    disk: Option<DiskWriter>,
    /// Every entry is queued to `disk`: done once it reports them written.
    finishing: bool,
    received: u64,
    last_progress: Instant,
    /// What else came with the files (their names as text, usually).
    rest: Clipboard,
}

/// Writes a download's files on a thread of its own. This runtime also
/// handles input, so a slow disk, the final flush of a large file, or an
/// antivirus scanning each finished file must never hold it up: received
/// chunks are only queued here, in order.
struct DiskWriter {
    id: u64,
    jobs: std::sync::mpsc::Sender<DiskJob>,
    aborted: Arc<AtomicBool>,
}

enum DiskJob {
    Create {
        target: PathBuf,
        size: u64,
        salt: u64,
    },
    Write {
        offset: u64,
        data: Vec<u8>,
    },
    /// Everything is queued: report once it is written.
    Done,
}

impl DiskWriter {
    fn spawn(events: mpsc::UnboundedSender<ConnectionEvent>) -> std::io::Result<Self> {
        let id = random_u64();
        let (jobs, queue) = std::sync::mpsc::channel::<DiskJob>();
        let aborted = Arc::new(AtomicBool::new(false));
        let stop = aborted.clone();
        std::thread::Builder::new()
            .name("linkcursor-files".to_owned())
            .spawn(move || {
                // Dropped unfinished (cancelled, failed), it removes its
                // partial file.
                let mut file: Option<FileWriter> = None;
                let outcome = loop {
                    // The queue closes when the download is dropped.
                    let Ok(job) = queue.recv() else {
                        break Err("stopped".to_owned());
                    };
                    if stop.load(Ordering::Relaxed) {
                        break Err("stopped".to_owned());
                    }
                    let result = match job {
                        DiskJob::Create { target, size, salt } => {
                            match FileWriter::create(target, size, salt) {
                                Ok(mut writer) if size == 0 => writer.finish(),
                                Ok(writer) => {
                                    file = Some(writer);
                                    Ok(())
                                }
                                Err(error) => Err(error.to_string()),
                            }
                        }
                        DiskJob::Write { offset, data } => {
                            match file.as_mut().map(|writer| writer.write(offset, &data)) {
                                Some(Ok(true)) => {
                                    file = None;
                                    Ok(())
                                }
                                Some(Ok(false)) => Ok(()),
                                Some(Err(reason)) => Err(reason),
                                None => Err("no file to write to".to_owned()),
                            }
                        }
                        DiskJob::Done => break Ok(()),
                    };
                    if let Err(reason) = result {
                        break Err(reason);
                    }
                };
                // Always reported, and only once nothing partial is left.
                drop(file);
                let _ = events.send(ConnectionEvent::FilesWritten {
                    writer: id,
                    outcome,
                });
            })?;
        Ok(Self { id, jobs, aborted })
    }

    fn send(&self, job: DiskJob) {
        let _ = self.jobs.send(job);
    }

    /// Stops at the next job; the file being written is removed.
    fn abort(&self) {
        self.aborted.store(true, Ordering::Relaxed);
    }
}

impl Runtime<'_> {
    fn shares_data_link(&self, peer_capabilities: u64) -> bool {
        self.config.advertised_capabilities() & peer_capabilities & DATA_LINK_CAPABILITIES != 0
    }

    fn data_link(&self, session: u64) -> Option<&ActiveConnection> {
        let id = self.sessions.get(&session)?.data?;
        self.data_links.get(&id).map(|link| &link.connection)
    }

    /// The accepting side asks the dialing side for a data link.
    pub(super) fn invite_data_link(&mut self, id: u64) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        if session.outgoing
            || session.data.is_some()
            || !session.established()
            || !self.shares_data_link(session.connection.capabilities)
        {
            return;
        }
        let port = self.quic.bulk_port;
        let token = random_u64();
        let session = self.sessions.get_mut(&id).expect("checked above");
        session.data_token = Some(token);
        session.data_invited = Some(Instant::now());
        let _ = session
            .connection
            .send_control(WireMessage::DataInvite { token, port });
    }

    /// Invites again for sessions whose data link is missing.
    pub(super) fn reinvite_data_links(&mut self) {
        let due: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                !session.outgoing
                    && session.established()
                    && session.data.is_none()
                    && session
                        .data_invited
                        .is_none_or(|at| at.elapsed() >= REINVITE_INTERVAL)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.invite_data_link(id);
        }
    }

    /// `DataInvite` on the control connection of session `id`: dial back.
    pub(super) fn data_invited(&mut self, id: u64, token: u64, port: u16) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        if !session.outgoing
            || session.data.is_some()
            || !self.shares_data_link(session.connection.capabilities)
        {
            return;
        }
        // The peer's bulk endpoint, at the address the session goes to.
        let address = SocketAddr::new(session.connection.remote.ip(), port);
        self.quic.note_peer(address, true);
        connect_data_in_background(
            id,
            token,
            self.quic.dialer(true),
            address,
            self.connection_events.clone(),
        );
    }

    pub(super) fn data_connected(&mut self, session: u64, token: u64, link: Link) {
        let wanted = self
            .sessions
            .get(&session)
            .is_some_and(|s| s.established() && s.data.is_none());
        if !wanted {
            return;
        }
        let id = self.next_connection_id;
        let hello = WireMessage::DataHello {
            device_id: self.config.device_id.clone(),
            token,
        };
        match spawn_connection(id, link, hello, self.connection_events.clone()) {
            Ok(connection) => {
                self.next_connection_id += 1;
                self.data_links.insert(
                    id,
                    DataLink {
                        session,
                        connection,
                    },
                );
                self.data_link_up(session, id);
            }
            Err(error) => self.sink.emit_error(format!("data link failed: {error}")),
        }
    }

    /// `DataHello` as the first message of accepted connection `id`: it is
    /// the data link of the session that invited with this token.
    pub(super) fn attach_data_link(&mut self, id: u64, device_id: String, token: u64) {
        let owner = self
            .sessions
            .iter()
            .find(|(_, session)| {
                session.established()
                    && session.peer_id() == Some(device_id.as_str())
                    && session.data_token == Some(token)
                    && session.data.is_none()
            })
            .map(|(owner, _)| *owner);
        let Some(owner) = owner else {
            self.close_session(id, "unknown data link", false);
            return;
        };
        // The same certificate as the session: the token alone would let
        // anyone who saw it take the data link over.
        let matches = self
            .sessions
            .get(&id)
            .zip(self.sessions.get(&owner))
            .is_some_and(|(pending, owner)| {
                pending.connection.fingerprint == owner.connection.fingerprint
            });
        if !matches {
            self.close_session(id, "data link from another device", false);
            return;
        }
        let Some(mut pending) = self.sessions.remove(&id) else {
            return;
        };
        pending.connection.peer_id = Some(device_id);
        self.data_links.insert(
            id,
            DataLink {
                session: owner,
                connection: pending.connection,
            },
        );
        self.data_link_up(owner, id);
    }

    fn data_link_up(&mut self, session: u64, link: u64) {
        let Some(owner) = self.sessions.get_mut(&session) else {
            return;
        };
        owner.data = Some(link);
        owner.data_token = None;
        if std::mem::take(&mut owner.clipboard_pending) {
            self.sync_clipboard(session);
        }
    }

    /// Drops data link `id` (closed, or its session went away).
    pub(super) fn close_data_link(&mut self, id: u64, reason: &str) {
        let Some(link) = self.data_links.remove(&id) else {
            return;
        };
        link.connection.abort();
        if let Some(upload) = self.uploads.remove(&id) {
            upload.abort();
        }
        if let Some(session) = self.sessions.get_mut(&link.session) {
            session.data = None;
            session.data_token = None;
            session.data_invited = None;
        }
        if self
            .download
            .as_ref()
            .is_some_and(|d| d.session == link.session)
        {
            self.fail_download(reason, false);
        }
    }

    /// A message on data link `id`.
    pub(super) fn data_message(&mut self, id: u64, message: WireMessage) {
        let Some(session) = self.data_links.get(&id).map(|link| link.session) else {
            return;
        };
        match message {
            // The accepting side greets every connection; a data link
            // learns nothing from it.
            WireMessage::Hello { .. } => {}
            WireMessage::ClipboardChunk {
                id: transfer,
                total,
                offset,
                data,
            } => self.clipboard_chunk(session, transfer, total, offset, &data),
            WireMessage::FileRequest {
                transfer,
                entry,
                offset,
            } => self.file_requested(id, transfer, entry as usize, offset),
            WireMessage::FileChunk {
                transfer,
                entry,
                offset,
                data,
            } => self.file_chunk(session, transfer, entry as usize, offset, data),
            WireMessage::FileCancel { transfer, reason } => {
                if self
                    .download
                    .as_ref()
                    .is_some_and(|d| d.session == session && d.manifest.transfer == transfer)
                {
                    self.fail_download(&reason, false);
                }
                if let Some(upload) = self.uploads.remove(&id) {
                    upload.abort();
                }
            }
            _ => self.close_data_link(id, "unexpected message on the data link"),
        }
    }

    // ---- Clipboard ----------------------------------------------------

    /// Takes the system clipboard the shell reports. Files in it
    /// (`text/uri-list`) become an offer. Sent to a peer when control moves
    /// to it or comes back from it (see `clipboard.rs`).
    pub(super) fn set_clipboard(&mut self, mut clipboard: Clipboard) {
        self.offer_files(&mut clipboard);
        if clipboard.is_empty() {
            self.clipboard = None;
            return;
        }
        if let Err(error) = clipboard.validate() {
            self.sink
                .emit_error(format!("clipboard not shared: {error}"));
            self.clipboard = None;
            return;
        }
        let encoded = clipboard.encode();
        self.clipboard = Some((clipboard::digest(&encoded), Arc::new(encoded)));
    }

    /// Hands the peer of session `id` this device's clipboard, unless it has
    /// it already (sent to it, or received from it). Waits for the data link
    /// when it is not up yet.
    pub(super) fn sync_clipboard(&mut self, id: u64) {
        let Some((digest, encoded)) = &self.clipboard else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if !self.config.clipboard
            || session.connection.capabilities & CAPABILITY_CLIPBOARD == 0
            || session.clipboard_synced == Some(*digest)
        {
            return;
        }
        let Some(link) = session.data.and_then(|link| self.data_links.get(&link)) else {
            session.clipboard_pending = true;
            return;
        };
        session.clipboard_synced = Some(*digest);
        session.clipboard_pending = false;
        self.next_clipboard_id = self.next_clipboard_id.wrapping_add(1);
        link.connection
            .send_bulk(clipboard::chunks(self.next_clipboard_id, encoded));
        self.sink.emit_value(json!({
            "event": "clipboard_sent",
            "peer_id": session.peer_id(),
            "size": encoded.len()
        }));
    }

    fn clipboard_chunk(
        &mut self,
        session: u64,
        transfer: u32,
        total: u32,
        offset: u32,
        data: &[u8],
    ) {
        if !self.config.clipboard {
            return;
        }
        let Some(owner) = self.sessions.get_mut(&session) else {
            return;
        };
        let encoded = match owner.clipboard_in.push(transfer, total, offset, data) {
            Ok(Some(encoded)) => encoded,
            Ok(None) => return,
            Err(error) => {
                self.sink
                    .emit_error(format!("clipboard not received: {error}"));
                return;
            }
        };
        let clipboard = match Clipboard::decode(&encoded) {
            Ok(clipboard) => clipboard,
            Err(error) => {
                self.sink
                    .emit_error(format!("clipboard not received: {error}"));
                return;
            }
        };
        // The peer has these contents: they are not sent back to it. They are
        // this device's clipboard now, as the shell will make them, so they
        // travel on to the next peer.
        let digest = clipboard::digest(&encoded);
        owner.clipboard_synced = Some(digest);
        let peer_id = owner.peer_id().unwrap_or_default().to_owned();
        self.clipboard = Some((digest, Arc::new(encoded)));

        let (manifests, rest): (Vec<ClipboardFormat>, Vec<ClipboardFormat>) = clipboard
            .formats
            .into_iter()
            .partition(|format| format.mime == MIME_FILES);
        let rest = Clipboard { formats: rest };
        if self.config.files
            && let Some(manifest) = manifests.first()
        {
            match serde_json::from_slice::<Manifest>(&manifest.data)
                .map_err(|error| error.to_string())
                .and_then(|manifest| {
                    manifest
                        .validate()
                        .map(|()| manifest)
                        .map_err(str::to_owned)
                }) {
                Ok(manifest) => return self.start_download(session, peer_id, manifest, rest),
                Err(error) => self.sink.emit_error(format!("files not received: {error}")),
            }
        }
        if !rest.is_empty() {
            self.emit_clipboard(&peer_id, &rest);
        }
    }

    fn emit_clipboard(&self, peer_id: &str, clipboard: &Clipboard) {
        self.sink.emit_value(json!({
            "event": "clipboard",
            "peer_id": peer_id,
            "formats": clipboard.to_json()
        }));
    }

    // ---- Sending files ------------------------------------------------

    /// Replaces a `text/uri-list` of local files with their manifest, and
    /// remembers where they are.
    fn offer_files(&mut self, clipboard: &mut Clipboard) {
        let Some(index) = clipboard
            .formats
            .iter()
            .position(|format| format.mime == MIME_URI_LIST)
        else {
            self.offer = None;
            return;
        };
        // Local paths never leave this device.
        let roots = files::parse_uri_list(&clipboard.formats.remove(index).data);
        if !self.config.files || roots.is_empty() {
            self.offer = None;
            return;
        }
        // The same copy again keeps its number, so its clipboard digest stays
        // the same and a peer that has it gets nothing new.
        let transfer = match &self.offer {
            Some(offer) if offer.roots == roots => offer.manifest.transfer,
            _ => {
                self.next_transfer = self.next_transfer.wrapping_add(1);
                self.next_transfer
            }
        };
        match Offer::build(transfer, &roots) {
            Ok(offer) => {
                let manifest = serde_json::to_vec(&offer.manifest).expect("manifest serialises");
                clipboard.formats.insert(
                    0,
                    ClipboardFormat {
                        mime: MIME_FILES.to_owned(),
                        data: manifest,
                    },
                );
                self.offer = Some(Arc::new(offer));
            }
            Err(error) => {
                self.sink.emit_error(format!("files not shared: {error}"));
                self.offer = None;
            }
        }
    }

    /// A peer asks for an entry of this device's offer on data link `link`.
    fn file_requested(&mut self, link: u64, transfer: u32, entry: usize, offset: u64) {
        let Some(connection) = self.data_links.get(&link).map(|link| &link.connection) else {
            return;
        };
        let control = connection.control_sender();
        let opened = match &self.offer {
            Some(offer) if offer.manifest.transfer == transfer => offer.open(entry),
            _ => Err("files are no longer on the clipboard".to_owned()),
        }
        .and_then(|(path, size)| {
            if offset <= size {
                Ok((path, size))
            } else {
                Err("offset past the end".to_owned())
            }
        });
        let (path, size) = match opened {
            Ok(opened) => opened,
            Err(reason) => {
                let _ = control.try_send(WireMessage::FileCancel { transfer, reason });
                return;
            }
        };
        let chunks = connection.file_sender();
        if let Some(previous) = self.uploads.remove(&link) {
            previous.abort();
        }
        let upload: JoinHandle<()> = tokio::spawn(async move {
            let sent = async {
                let mut file = tokio::fs::File::open(&path).await?;
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                let mut position = offset;
                let mut buffer = vec![0_u8; FILE_CHUNK_SIZE];
                while position < size {
                    let wanted = FILE_CHUNK_SIZE.min((size - position) as usize);
                    let read = file.read(&mut buffer[..wanted]).await?;
                    if read == 0 {
                        return Err(std::io::Error::other("file shrank while sending"));
                    }
                    let chunk = WireMessage::FileChunk {
                        transfer,
                        entry: entry as u32,
                        offset: position,
                        data: buffer[..read].to_vec(),
                    };
                    if chunks.send(chunk).await.is_err() {
                        return Ok(()); // The link is gone.
                    }
                    position += read as u64;
                }
                Ok(())
            };
            if let Err(error) = sent.await {
                let _ = control
                    .send(WireMessage::FileCancel {
                        transfer,
                        reason: error.to_string(),
                    })
                    .await;
            }
        });
        self.uploads.insert(link, upload);
    }

    // ---- Receiving files ----------------------------------------------

    fn start_download(
        &mut self,
        session: u64,
        peer_id: String,
        manifest: Manifest,
        rest: Clipboard,
    ) {
        if self.download.is_some() {
            self.fail_download("replaced by a newer copy", true);
        }
        let root = PathBuf::from(self.config.download_directory.clone().unwrap_or_default());
        let transfer = manifest.transfer;
        let planned = std::fs::create_dir_all(&root)
            .map_err(|error| error.to_string())
            .and_then(|()| files::plan(&manifest, &root).map_err(str::to_owned));
        let (targets, tops) = match planned {
            Ok(plan) => plan,
            Err(reason) => {
                self.sink.emit_value(json!({
                    "event": "files_failed",
                    "peer_id": peer_id,
                    "transfer": transfer,
                    "reason": reason
                }));
                return;
            }
        };
        let accepted = manifest.total_size <= self.config.max_auto_receive_bytes;
        let count = manifest.entries.iter().filter(|entry| !entry.dir).count();
        let total_size = manifest.total_size;
        self.download = Some(Download {
            session,
            peer_id: peer_id.clone(),
            manifest,
            accepted,
            targets,
            tops,
            entry: 0,
            entry_received: 0,
            disk: None,
            finishing: false,
            received: 0,
            last_progress: Instant::now(),
            rest,
        });
        if accepted {
            self.advance_download();
        } else {
            self.sink.emit_value(json!({
                "event": "files_offered",
                "peer_id": peer_id,
                "transfer": transfer,
                "count": count,
                "total_size": total_size
            }));
        }
    }

    /// The session the files being received come from.
    pub(super) fn download_session(&self) -> Option<u64> {
        self.download.as_ref().map(|download| download.session)
    }

    pub(super) fn accept_files(&mut self, transfer: u32) {
        let Some(download) = self.download.as_mut() else {
            return;
        };
        if download.manifest.transfer == transfer && !download.accepted {
            download.accepted = true;
            self.advance_download();
        }
    }

    pub(super) fn cancel_files(&mut self, transfer: u32) {
        if self
            .download
            .as_ref()
            .is_some_and(|d| d.manifest.transfer == transfer)
        {
            self.fail_download("cancelled", true);
        }
    }

    /// Creates folders, queues files to the disk writer and asks for the next
    /// file with content; once none is left, waits for the disk writer.
    fn advance_download(&mut self) {
        let events = self.connection_events.clone();
        loop {
            let Some(download) = self.download.as_mut() else {
                return;
            };
            let index = download.entry;
            let Some(entry) = download.manifest.entries.get(index).cloned() else {
                download.finishing = true;
                match &download.disk {
                    // Done once it reports everything written (`files_written`).
                    Some(disk) => disk.send(DiskJob::Done),
                    // Only folders: nothing to wait for.
                    None => self.finish_download(),
                }
                return;
            };
            let target = download.targets[index].clone();
            if entry.dir {
                if let Err(error) = std::fs::create_dir_all(&target) {
                    return self.fail_download(&error.to_string(), true);
                }
                download.entry += 1;
                continue;
            }
            if download.disk.is_none() {
                match DiskWriter::spawn(events.clone()) {
                    Ok(disk) => download.disk = Some(disk),
                    Err(error) => return self.fail_download(&error.to_string(), true),
                }
            }
            if let Some(disk) = &download.disk {
                disk.send(DiskJob::Create {
                    target,
                    size: entry.size,
                    salt: random_u64(),
                });
            }
            download.entry_received = 0;
            if entry.size == 0 {
                download.entry += 1;
                continue;
            }
            let request = WireMessage::FileRequest {
                transfer: download.manifest.transfer,
                entry: index as u32,
                offset: 0,
            };
            let session = download.session;
            let sent = self
                .data_link(session)
                .is_some_and(|link| link.send_control(request).is_ok());
            if !sent {
                self.fail_download("connection lost", false);
            }
            return;
        }
    }

    fn file_chunk(
        &mut self,
        session: u64,
        transfer: u32,
        entry: usize,
        offset: u64,
        data: Vec<u8>,
    ) {
        let Some(download) = self.download.as_mut() else {
            return;
        };
        // Chunks of a cancelled or replaced transfer may still be in flight.
        if download.session != session
            || download.manifest.transfer != transfer
            || download.entry != entry
            || !download.accepted
        {
            return;
        }
        let size = download
            .manifest
            .entries
            .get(entry)
            .map_or(0, |entry| entry.size);
        let length = data.len() as u64;
        if offset != download.entry_received {
            return self.fail_download("file chunk out of order", true);
        }
        if download.entry_received + length > size {
            return self.fail_download("file larger than announced", true);
        }
        let Some(disk) = &download.disk else {
            return;
        };
        // Written on the disk writer's thread; the writer checks again.
        disk.send(DiskJob::Write { offset, data });
        download.entry_received += length;
        download.received += length;
        let done = download.entry_received == size;
        if download.last_progress.elapsed() >= PROGRESS_INTERVAL || done {
            download.last_progress = Instant::now();
            self.sink.emit_value(json!({
                "event": "files_progress",
                "transfer": transfer,
                "received": download.received,
                "total": download.manifest.total_size
            }));
        }
        if done {
            download.entry += 1;
            self.advance_download();
        }
    }

    /// The disk writer `writer` stopped: everything written, failed, or
    /// stopped by a failed download that waits for it to clean up.
    pub(super) fn files_written(&mut self, writer: u64, outcome: Result<(), String>) {
        if let Some(index) = self
            .download_failures
            .iter()
            .position(|(id, _)| *id == writer)
        {
            let (_, event) = self.download_failures.remove(index);
            return self.sink.emit_value(event);
        }
        let Some(download) = &self.download else {
            return;
        };
        if download.disk.as_ref().map(|disk| disk.id) != Some(writer) {
            return;
        }
        match outcome {
            Ok(()) if download.finishing => self.finish_download(),
            Ok(()) => {}
            Err(reason) => self.end_download(&reason, true, true),
        }
    }

    fn finish_download(&mut self) {
        let Some(download) = self.download.take() else {
            return;
        };
        let paths: Vec<String> = download
            .tops
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        self.sink.emit_value(json!({
            "event": "files_received",
            "peer_id": download.peer_id,
            "transfer": download.manifest.transfer,
            "paths": paths
        }));
        // Pasting now gives the files themselves.
        let mut clipboard = download.rest;
        clipboard.formats.insert(
            0,
            ClipboardFormat {
                mime: MIME_URI_LIST.to_owned(),
                data: files::uri_list(&download.tops),
            },
        );
        self.emit_clipboard(&download.peer_id, &clipboard);
    }

    /// Stops the transfer being received; its partial file is removed.
    /// `tell_peer` sends `FileCancel` so the sender stops too.
    pub(super) fn fail_download(&mut self, reason: &str, tell_peer: bool) {
        self.end_download(reason, tell_peer, false);
    }

    /// `writer_stopped`: the disk writer itself failed and has cleaned up.
    fn end_download(&mut self, reason: &str, tell_peer: bool, writer_stopped: bool) {
        let Some(download) = self.download.take() else {
            return;
        };
        let transfer = download.manifest.transfer;
        if tell_peer && let Some(link) = self.data_link(download.session) {
            let _ = link.send_control(WireMessage::FileCancel {
                transfer,
                reason: reason.to_owned(),
            });
        }
        let failed = json!({
            "event": "files_failed",
            "peer_id": download.peer_id,
            "transfer": transfer,
            "reason": reason
        });
        match &download.disk {
            // Reported once the writer stopped and removed its partial file
            // (`files_written`); dropping the download closes its queue.
            Some(disk) if !writer_stopped => {
                disk.abort();
                self.download_failures.push((disk.id, failed));
            }
            _ => self.sink.emit_value(failed),
        }
    }
}
