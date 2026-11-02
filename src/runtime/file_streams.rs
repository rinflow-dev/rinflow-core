// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A bounded request/response lane for native, seekable file providers.
//! Leases pin offers, not whole-file buffers. No clipboard writes on finish.
use super::{Runtime, random_u64};
use crate::file_stream::{MAX_READ_SIZE, READ_TIMEOUT, StreamCommand};
use crate::files::{Manifest, Offer};
use crate::model::{CAPABILITY_FILE_STREAMS, WireMessage};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const MAX_LEASES: usize = 16;
const MAX_READS: usize = 16;
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
type Reply<T> = mpsc::SyncSender<Result<T, String>>;

pub(super) struct StreamState {
    pub offers: HashMap<u64, Arc<Manifest>>,
    sources: HashMap<(u64, u64), SourceLease>,
    clients: HashMap<u64, ClientLease>,
    reads: HashMap<u64, PendingRead>,
    workers: Arc<Semaphore>,
}
impl Default for StreamState {
    fn default() -> Self {
        Self {
            offers: HashMap::new(),
            sources: HashMap::new(),
            clients: HashMap::new(),
            reads: HashMap::new(),
            workers: Arc::new(Semaphore::new(MAX_READS)),
        }
    }
}
struct SourceLease {
    offer: Arc<Offer>,
    stop: Arc<AtomicBool>,
    touched: Instant,
    job: u32,
    peer: String,
    contiguous: Vec<u64>,
    progress_at: Instant,
}
struct ClientLease {
    session: u64,
    manifest: Arc<Manifest>,
    opening: Option<Reply<()>>,
    touched: Instant,
    progress_at: Instant,
    contiguous: Vec<u64>,
    job: u32,
    peer: String,
}
struct PendingRead {
    lease: u64,
    entry: usize,
    offset: u64,
    expected: usize,
    at: Instant,
    reply: Reply<Vec<u8>>,
}

impl Runtime<'_> {
    pub(super) fn revoke_current_file_offer(&self) {
        if let Some(offer) = &self.offer {
            for &session in &self.offered_to {
                if self
                    .sessions
                    .get(&session)
                    .is_some_and(|s| s.connection.capabilities & CAPABILITY_FILE_STREAMS != 0)
                {
                    self.stream_send(
                        session,
                        WireMessage::FileOfferRevoke {
                            transfer: offer.manifest.transfer,
                        },
                    );
                }
            }
        }
    }
    fn stream_job(&self) -> u32 {
        loop {
            let id = random_u64() as u32;
            if self.streams.sources.values().all(|v| v.job != id)
                && self.streams.clients.values().all(|v| v.job != id)
                && self
                    .download
                    .as_ref()
                    .is_none_or(|v| v.manifest.transfer != id)
                && self
                    .offer
                    .as_ref()
                    .is_none_or(|v| v.manifest.transfer != id)
            {
                return id;
            }
        }
    }

    pub(super) fn publish_stream_offer(
        &mut self,
        session: u64,
        peer: &str,
        manifest: Manifest,
    ) -> bool {
        if !self.config.on_demand_files
            || self
                .sessions
                .get(&session)
                .is_none_or(|s| s.connection.capabilities & CAPABILITY_FILE_STREAMS == 0)
        {
            return false;
        }
        self.sink.emit_value(
            json!({ "event": "files_available", "peer_id": peer, "manifest": manifest }),
        );
        self.streams.offers.insert(session, Arc::new(manifest));
        true
    }

    fn stream_send(&self, session: u64, message: WireMessage) -> bool {
        self.data_link(session)
            .is_some_and(|link| link.send_control(message).is_ok())
    }

    pub(super) fn stream_command(&mut self, command: StreamCommand) {
        match command {
            StreamCommand::Open {
                peer,
                transfer,
                lease,
                reply,
            } => {
                let offer = self
                    .streams
                    .offers
                    .iter()
                    .find(|(session, m)| {
                        m.transfer == transfer
                            && self.sessions.get(session).and_then(|s| s.peer_id())
                                == Some(peer.as_str())
                    })
                    .map(|(&session, manifest)| (session, manifest.clone()));
                let Some((session, manifest)) = offer else {
                    let _ = reply.send(Err("file offer is no longer current".into()));
                    return;
                };
                if self.streams.clients.len() >= MAX_LEASES {
                    let _ = reply.send(Err("too many file operations".into()));
                    return;
                }
                if !self.stream_send(session, WireMessage::FileStreamOpen { transfer, lease }) {
                    let _ = reply.send(Err("data connection unavailable".into()));
                    return;
                }
                let job = self.stream_job();
                self.streams.clients.insert(
                    lease,
                    ClientLease {
                        session,
                        contiguous: vec![0; manifest.entries.len()],
                        manifest,
                        opening: Some(reply),
                        touched: Instant::now(),
                        progress_at: Instant::now(),
                        job,
                        peer,
                    },
                );
            }
            StreamCommand::Read {
                lease,
                request,
                entry,
                offset,
                length,
                reply,
            } => {
                let Some(client) = self.streams.clients.get_mut(&lease) else {
                    let _ = reply.send(Err("file operation ended".into()));
                    return;
                };
                let Some(file) = client
                    .manifest
                    .entries
                    .get(entry as usize)
                    .filter(|e| !e.dir)
                else {
                    let _ = reply.send(Err("invalid file entry".into()));
                    return;
                };
                if client.opening.is_some()
                    || length == 0
                    || length as usize > MAX_READ_SIZE
                    || offset > file.size
                {
                    let _ = reply.send(Err("invalid file read".into()));
                    return;
                }
                let expected = (file.size - offset).min(u64::from(length)) as usize;
                if expected == 0 {
                    let _ = reply.send(Ok(Vec::new()));
                    return;
                }
                if self.streams.reads.len() >= MAX_READS {
                    let _ = reply.send(Err("too many pending file reads".into()));
                    return;
                }
                client.touched = Instant::now();
                let session = client.session;
                if !self.stream_send(
                    session,
                    WireMessage::FileStreamRead {
                        lease,
                        request,
                        entry,
                        offset,
                        length,
                    },
                ) {
                    let _ = reply.send(Err("data connection unavailable".into()));
                    return;
                }
                self.streams.reads.insert(
                    request,
                    PendingRead {
                        lease,
                        entry: entry as usize,
                        offset,
                        expected,
                        at: Instant::now(),
                        reply,
                    },
                );
            }
            StreamCommand::Close { lease, reason } => {
                self.close_client_stream(lease, &reason, true)
            }
            StreamCommand::AbandonRead(request) => {
                self.streams.reads.remove(&request);
            }
            StreamCommand::Revoke => {
                self.replace_clipboard_snapshot();
                self.clipboard = None;
                let ids: Vec<_> = self.streams.clients.keys().copied().collect();
                for id in ids {
                    self.close_client_stream(id, "cancelled", true);
                }
                let ids: Vec<_> = self.streams.sources.keys().copied().collect();
                for (session, lease) in ids {
                    self.close_source_stream(session, lease, "cancelled", true);
                }
            }
        }
    }

    pub(super) fn stream_message(&mut self, session: u64, message: WireMessage) {
        if !self.config.files
            || self
                .sessions
                .get(&session)
                .is_none_or(|s| s.connection.capabilities & CAPABILITY_FILE_STREAMS == 0)
        {
            return;
        }
        match message {
            WireMessage::FileOfferRevoke { transfer } => {
                if self
                    .streams
                    .offers
                    .get(&session)
                    .is_some_and(|m| m.transfer == transfer)
                {
                    self.streams.offers.remove(&session);
                    self.sink.emit_value(json!({ "event": "files_unavailable", "peer_id": self.sessions[&session].peer_id() }));
                }
            }
            WireMessage::FileStreamOpen { transfer, lease } => {
                // A new operation may pin only the current offer, and only
                // after that offer was shared with this authenticated session.
                let offer = self.offer.clone().filter(|o| {
                    o.manifest.transfer == transfer && self.offered_to.contains(&session)
                });
                let reason = if self.streams.sources.contains_key(&(session, lease)) {
                    "duplicate file operation"
                } else if self.streams.sources.len() >= MAX_LEASES {
                    "too many file operations"
                } else if offer.is_none() {
                    "files are no longer on the clipboard"
                } else {
                    ""
                };
                if !reason.is_empty() {
                    self.stream_send(
                        session,
                        WireMessage::FileStreamOpened {
                            lease,
                            reason: reason.into(),
                        },
                    );
                    return;
                }
                let offer = offer.unwrap();
                let job = self.stream_job();
                let peer = self.sessions[&session]
                    .peer_id()
                    .unwrap_or_default()
                    .to_owned();
                let (name, items) = offer.manifest.summary();
                self.sink.emit_value(
                    json!({ "event": "files_sending", "peer_id": peer, "transfer": job,
                    "name": name, "items": items, "count": offer.manifest.file_count(), "sent": 0,
                    "total": offer.manifest.total_size }),
                );
                let contiguous = vec![0; offer.manifest.entries.len()];
                self.streams.sources.insert(
                    (session, lease),
                    SourceLease {
                        offer,
                        stop: Arc::new(AtomicBool::new(false)),
                        touched: Instant::now(),
                        job,
                        peer,
                        contiguous,
                        progress_at: Instant::now(),
                    },
                );
                if !self.stream_send(
                    session,
                    WireMessage::FileStreamOpened {
                        lease,
                        reason: String::new(),
                    },
                ) {
                    self.close_source_stream(session, lease, "connection lost", false);
                }
            }
            WireMessage::FileStreamOpened { lease, reason } => {
                if self
                    .streams
                    .clients
                    .get(&lease)
                    .is_none_or(|c| c.session != session || c.opening.is_none())
                {
                    return;
                }
                if !reason.is_empty() {
                    self.close_client_stream(lease, &reason, false);
                    return;
                }
                let client = self.streams.clients.get_mut(&lease).unwrap();
                let (name, items) = client.manifest.summary();
                self.sink.emit_value(json!({ "event": "files_receiving", "peer_id": client.peer,
                    "transfer": client.job, "name": name, "items": items, "count": client.manifest.file_count(),
                    "total_size": client.manifest.total_size }));
                if client.opening.take().unwrap().send(Ok(())).is_err() {
                    self.close_client_stream(lease, "cancelled", true);
                }
            }
            WireMessage::FileStreamRead {
                lease,
                request,
                entry,
                offset,
                length,
            } => {
                let Some(source) = self.streams.sources.get_mut(&(session, lease)) else {
                    self.stream_send(
                        session,
                        WireMessage::FileStreamData {
                            request,
                            data: vec![],
                            reason: "file operation ended".into(),
                        },
                    );
                    return;
                };
                source.touched = Instant::now();
                let offer = source.offer.clone();
                let stop = source.stop.clone();
                let Ok(permit) = self.streams.workers.clone().try_acquire_owned() else {
                    self.stream_send(
                        session,
                        WireMessage::FileStreamData {
                            request,
                            data: vec![],
                            reason: "too many file reads".into(),
                        },
                    );
                    return;
                };
                let Some(link) = self.data_link(session) else {
                    return;
                };
                let sender = link.file_sender();
                let events = self.connection_events.clone();
                tokio::spawn(async move {
                    // The permit covers disk work AND the bounded response
                    // queue, including cancellation during a slow read.
                    let outcome = tokio::task::spawn_blocking(move || {
                        if stop.load(Ordering::Relaxed) {
                            return Err("cancelled".to_owned());
                        }
                        offer.read_range(entry as usize, offset, length as usize)
                    })
                    .await
                    .unwrap_or_else(|_| Err("file reader stopped".into()));
                    let (data, reason) = match outcome {
                        Ok(data) => (data, String::new()),
                        Err(reason) => (vec![], reason.chars().take(256).collect()),
                    };
                    let length = data.len();
                    if sender
                        .send(WireMessage::FileStreamData {
                            request,
                            data,
                            reason,
                        })
                        .await
                        .is_ok()
                    {
                        let _ = events.send(crate::transport::ConnectionEvent::FileStreamServed {
                            session,
                            lease,
                            entry: entry as usize,
                            offset,
                            length,
                        });
                    }
                    drop(permit);
                });
            }
            WireMessage::FileStreamData {
                request,
                data,
                reason,
            } => {
                let Some(pending) = self.streams.reads.get(&request) else {
                    return;
                };
                if self
                    .streams
                    .clients
                    .get(&pending.lease)
                    .is_none_or(|c| c.session != session)
                {
                    return;
                }
                let pending = self.streams.reads.remove(&request).unwrap();
                if !reason.is_empty() || data.len() != pending.expected {
                    let error = if reason.is_empty() {
                        "incomplete file read"
                    } else {
                        &reason
                    };
                    let _ = pending.reply.send(Err(error.to_owned()));
                    self.close_client_stream(pending.lease, error, true);
                    return;
                }
                let client = self.streams.clients.get_mut(&pending.lease).unwrap();
                let end = &mut client.contiguous[pending.entry];
                if pending.offset <= *end {
                    *end = (*end).max(pending.offset + data.len() as u64);
                }
                let received: u64 = client.contiguous.iter().sum();
                if client.progress_at.elapsed() >= Duration::from_millis(250)
                    || received == client.manifest.total_size
                {
                    self.sink.emit_value(json!({ "event": "files_progress", "peer_id": client.peer, "transfer": client.job,
                        "received": received, "total": client.manifest.total_size }));
                    client.progress_at = Instant::now();
                }
                if pending.reply.send(Ok(data)).is_err() {
                    self.close_client_stream(pending.lease, "cancelled", true);
                }
            }
            WireMessage::FileStreamClose {
                lease,
                from_source,
                reason,
            } => {
                // Lease numbers are scoped by direction as well as session.
                // A close from the requesting side closes its source lease;
                // a close from the source only exists when it has revoked it.
                if !from_source && self.streams.sources.contains_key(&(session, lease)) {
                    self.close_source_stream(session, lease, &reason, false);
                } else if from_source
                    && self
                        .streams
                        .clients
                        .get(&lease)
                        .is_some_and(|c| c.session == session)
                {
                    self.close_client_stream(lease, &reason, false);
                }
            }
            _ => {}
        }
    }

    pub(super) fn stream_served(
        &mut self,
        session: u64,
        lease: u64,
        entry: usize,
        offset: u64,
        length: usize,
    ) {
        let Some(source) = self.streams.sources.get_mut(&(session, lease)) else {
            return;
        };
        let Some(end) = source.contiguous.get_mut(entry) else {
            return;
        };
        if offset <= *end {
            *end = (*end).max(offset + length as u64);
        }
        let sent: u64 = source.contiguous.iter().sum();
        if source.progress_at.elapsed() >= Duration::from_millis(250)
            || sent == source.offer.manifest.total_size
        {
            let (name, items) = source.offer.manifest.summary();
            self.sink.emit_value(
                json!({ "event": "files_sending", "peer_id": source.peer, "transfer": source.job,
                "name": name, "items": items, "count": source.offer.manifest.file_count(),
                "sent": sent, "total": source.offer.manifest.total_size }),
            );
            source.progress_at = Instant::now();
        }
    }

    fn close_client_stream(&mut self, lease: u64, reason: &str, notify: bool) {
        let Some(client) = self.streams.clients.remove(&lease) else {
            return;
        };
        if let Some(reply) = client.opening {
            let _ = reply.send(Err(reason.into()));
        }
        self.streams.reads.retain(|_, read| {
            if read.lease == lease {
                let _ = read.reply.send(Err(reason.into()));
                false
            } else {
                true
            }
        });
        if notify {
            self.stream_send(
                client.session,
                WireMessage::FileStreamClose {
                    lease,
                    from_source: false,
                    reason: reason.into(),
                },
            );
        }
        self.sink.emit_value(json!({ "event": "files_stream_ended", "peer_id": client.peer, "transfer": client.job, "reason": reason }));
    }

    fn close_source_stream(&mut self, session: u64, lease: u64, reason: &str, notify: bool) {
        let Some(source) = self.streams.sources.remove(&(session, lease)) else {
            return;
        };
        source.stop.store(true, Ordering::Relaxed);
        if notify {
            self.stream_send(
                session,
                WireMessage::FileStreamClose {
                    lease,
                    from_source: true,
                    reason: reason.into(),
                },
            );
        }
        self.sink.emit_value(json!({ "event": "files_send_ended", "peer_id": source.peer, "transfer": source.job, "reason": reason }));
    }

    pub(super) fn close_session_streams(&mut self, session: u64, reason: &str) {
        self.streams.offers.remove(&session);
        let ids: Vec<_> = self
            .streams
            .clients
            .iter()
            .filter(|(_, c)| c.session == session)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.close_client_stream(id, reason, false);
        }
        let ids: Vec<_> = self
            .streams
            .sources
            .keys()
            .filter(|(s, _)| *s == session)
            .copied()
            .collect();
        for (session, lease) in ids {
            self.close_source_stream(session, lease, reason, false);
        }
        self.sink.emit_value(json!({ "event": "files_unavailable", "peer_id": self.sessions.get(&session).and_then(|s| s.peer_id()) }));
    }

    pub(super) fn cancel_stream_job(&mut self, job: u32) {
        let ids: Vec<_> = self
            .streams
            .clients
            .iter()
            .filter(|(_, c)| c.job == job)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.close_client_stream(id, "cancelled", true);
        }
        let ids: Vec<_> = self
            .streams
            .sources
            .iter()
            .filter(|(_, s)| s.job == job)
            .map(|(&id, _)| id)
            .collect();
        for (session, lease) in ids {
            self.close_source_stream(session, lease, "cancelled", true);
        }
    }

    pub(super) fn expire_streams(&mut self) {
        let ids: Vec<_> = self
            .streams
            .clients
            .iter()
            .filter(|(_, c)| {
                c.touched.elapsed()
                    > if c.opening.is_some() {
                        READ_TIMEOUT
                    } else {
                        IDLE_TIMEOUT
                    }
            })
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.close_client_stream(id, "file operation timed out", true);
        }
        let ids: Vec<_> = self
            .streams
            .sources
            .iter()
            .filter(|(_, s)| s.touched.elapsed() > IDLE_TIMEOUT)
            .map(|(&id, _)| id)
            .collect();
        for (session, lease) in ids {
            self.close_source_stream(session, lease, "file operation timed out", true);
        }
        let expired: Vec<_> = self
            .streams
            .reads
            .iter()
            .filter(|(_, r)| r.at.elapsed() > READ_TIMEOUT)
            .map(|(_, r)| r.lease)
            .collect();
        for lease in expired {
            self.close_client_stream(lease, "remote file read timed out", true);
        }
    }
}
