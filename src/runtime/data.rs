//! The data link of a session and what travels on it: clipboards and files.
//!
//! Every session with a peer sharing a data-link capability gets a second
//! TCP connection to the same port, so bulk data never waits in front of
//! input in one byte stream. The side that accepted the control connection
//! sends `DataInvite { token }`; the side that dialed it connects again and
//! opens with `DataHello { device_id, token }`, which binds the new
//! connection to the session. When the data link drops, the accepting side
//! invites again from `maintenance`.

use super::{Runtime, random_u64};
use crate::clipboard::{self, Clipboard, ClipboardFormat};
use crate::files::{self, FILE_CHUNK_SIZE, FileWriter, MIME_FILES, MIME_URI_LIST, Manifest, Offer};
use crate::model::{CAPABILITY_CLIPBOARD, DATA_LINK_CAPABILITIES, WireMessage};
use crate::transport::{ActiveConnection, connect_data_in_background, spawn_connection};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpStream;
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
    /// The entry being received.
    entry: usize,
    writer: Option<FileWriter>,
    received: u64,
    last_progress: Instant,
    /// What else came with the files (their names as text, usually).
    rest: Clipboard,
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
        let token = random_u64();
        let session = self.sessions.get_mut(&id).expect("checked above");
        session.data_token = Some(token);
        session.data_invited = Some(Instant::now());
        let _ = session
            .connection
            .send_control(WireMessage::DataInvite { token });
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
    pub(super) fn data_invited(&mut self, id: u64, token: u64) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        if !session.outgoing
            || session.data.is_some()
            || !self.shares_data_link(session.connection.capabilities)
        {
            return;
        }
        if let Some(remote) = session.connection.remote {
            connect_data_in_background(id, token, remote, self.connection_events.clone());
        }
    }

    pub(super) fn data_connected(&mut self, session: u64, token: u64, stream: TcpStream) {
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
        match spawn_connection(id, stream, hello, self.connection_events.clone()) {
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
            } => self.file_chunk(session, transfer, entry as usize, offset, &data),
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
            writer: None,
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

    /// Creates folders and empty files, and asks for the next file with
    /// content; finishes when none is left.
    fn advance_download(&mut self) {
        loop {
            let Some(download) = self.download.as_mut() else {
                return;
            };
            let index = download.entry;
            let Some(entry) = download.manifest.entries.get(index).cloned() else {
                return self.finish_download();
            };
            let target = download.targets[index].clone();
            if entry.dir {
                if let Err(error) = std::fs::create_dir_all(&target) {
                    return self.fail_download(&error.to_string(), true);
                }
                download.entry += 1;
                continue;
            }
            let mut writer = match FileWriter::create(target, entry.size, random_u64()) {
                Ok(writer) => writer,
                Err(error) => return self.fail_download(&error.to_string(), true),
            };
            if entry.size == 0 {
                if let Err(error) = writer.finish() {
                    return self.fail_download(&error, true);
                }
                download.entry += 1;
                continue;
            }
            download.writer = Some(writer);
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

    fn file_chunk(&mut self, session: u64, transfer: u32, entry: usize, offset: u64, data: &[u8]) {
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
        let Some(writer) = download.writer.as_mut() else {
            return;
        };
        match writer.write(offset, data) {
            Ok(done) => {
                download.received += data.len() as u64;
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
                    download.writer = None;
                    download.entry += 1;
                    self.advance_download();
                }
            }
            Err(reason) => self.fail_download(&reason, true),
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
        self.sink.emit_value(json!({
            "event": "files_failed",
            "peer_id": download.peer_id,
            "transfer": transfer,
            "reason": reason
        }));
    }
}
