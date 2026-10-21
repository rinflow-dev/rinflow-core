//! Who may use a connection: admission and pairing.
//!
//! Every peer shows a certificate (see `secure.rs`), so after its `Hello`
//! the runtime knows the peer's device ID and fingerprint:
//!
//! - **Paired** (the fingerprint the user paired for this device ID): let in.
//! - **Paired, but another certificate**: refused. Something else is
//!   claiming to be that device.
//! - **Unknown**: with `require_pairing`, the two devices pair first; without
//!   it, let in (encrypted, but anyone on the network could be on the other
//!   end).
//!
//! Pairing: the side that wants it sends `PairRequest`. Both sides show the
//! same six-digit code (`pairing_requested`, computed from both
//! certificates); each user checks it and accepts (`accept_pairing` →
//! `PairAccept`). Once a side has both its own and the peer's acceptance it
//! trusts the peer (`paired`, which the shell saves for
//! `CoreConfig::trusted_peers`) and lets it in. A side that already trusts
//! the requester accepts at once. Declining or two minutes without an answer
//! closes the connection; a decline travels in the QUIC close frame
//! (`CloseCode::PairingDeclined`), so the other side can say so.

use super::Runtime;
use crate::model::WireMessage;
use crate::secure;
use crate::transport::CloseCode;
use serde_json::json;
use std::time::{Duration, Instant};

/// `disconnected` reasons. The other side of a declined pairing reports
/// `transport::PAIRING_DECLINED_BY_PEER`.
pub const IDENTITY_CHANGED: &str = "the device's identity changed";
pub const PAIRING_REJECTED: &str = "pairing declined";
pub const PAIRING_TIMED_OUT: &str = "pairing timed out";
pub const UNPAIRED: &str = "unpaired";
pub const PAIRING_REQUIRED: &str = "pairing required";

const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Not let in yet: before `Hello`, or while pairing.
    Waiting,
    Admitted,
}

/// A pairing in progress on a session.
#[derive(Clone, Copy, Debug)]
pub struct Pairing {
    /// This side's user accepted.
    pub local: bool,
    /// The peer accepted (`PairAccept`).
    pub remote: bool,
    pub since: Instant,
}

impl Runtime<'_> {
    /// Decides what happens to a session right after the peer's `Hello`.
    pub(super) fn admit_or_pair(&mut self, id: u64) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        let Some(peer_id) = session.peer_id().map(str::to_owned) else {
            return;
        };
        match self.trusted.get(&peer_id) {
            Some(known) if *known == session.connection.fingerprint => self.admit(id),
            Some(_) => {
                self.sink.emit_error(format!(
                    "{peer_id} presented a different certificate than when it was paired; refused"
                ));
                self.close_session(id, IDENTITY_CHANGED, true);
            }
            None if self.require_pairing => self.start_pairing(id, true),
            None => self.admit(id),
        }
    }

    /// Starts pairing on a session: asks the peer (`request`) and shows the
    /// code here.
    fn start_pairing(&mut self, id: u64, request: bool) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if session.pairing.is_some() {
            return;
        }
        session.pairing = Some(Pairing {
            local: false,
            remote: false,
            since: Instant::now(),
        });
        if request {
            let _ = session.connection.send_control(WireMessage::PairRequest);
        }
        self.announce_pairing(id);
    }

    /// Tells the shell to show the pairing code for this session.
    pub(super) fn announce_pairing(&self, id: u64) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        let (Some(peer_id), true) = (session.peer_id(), session.pairing.is_some()) else {
            return;
        };
        let theirs = &session.connection.fingerprint;
        let name = self
            .peers
            .get(peer_id)
            .map(|peer| peer.name.clone())
            .or_else(|| self.peer_names.get(peer_id).map(|(name, _)| name.clone()))
            .unwrap_or_else(|| peer_id.to_owned());
        let platform = self
            .peers
            .get(peer_id)
            .map_or("unknown", |peer| peer.platform.as_str());
        self.sink.emit_value(json!({
            "event": "pairing_requested",
            "peer_id": peer_id,
            "name": name,
            "platform": platform,
            "code": secure::pairing_code(&self.fingerprint, theirs),
            "fingerprint": theirs
        }));
    }

    /// `PairRequest` from the peer: it does not trust this device yet.
    pub(super) fn pair_requested(&mut self, id: u64) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        let Some(peer_id) = session.peer_id() else {
            return;
        };
        if self.trusted.get(peer_id) == Some(&session.connection.fingerprint) {
            // Paired on this side already: the peer's user decides.
            let _ = session.connection.send_control(WireMessage::PairAccept);
            return;
        }
        // Not paired here either: this side's user has to check the code too.
        self.start_pairing(id, false);
    }

    /// `PairAccept` from the peer.
    pub(super) fn pair_accepted(&mut self, id: u64) {
        let Some(pairing) = self
            .sessions
            .get_mut(&id)
            .and_then(|session| session.pairing.as_mut())
        else {
            return;
        };
        pairing.remote = true;
        self.finish_pairing(id);
    }

    /// The user accepted the code shown for `peer_id`.
    pub(super) fn accept_pairing(&mut self, peer_id: &str) {
        let Some(id) = self.pairing_session(peer_id) else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if let Some(pairing) = session.pairing.as_mut()
            && !pairing.local
        {
            pairing.local = true;
            let _ = session.connection.send_control(WireMessage::PairAccept);
        }
        self.finish_pairing(id);
    }

    pub(super) fn reject_pairing(&mut self, peer_id: &str) {
        let Some(id) = self.pairing_session(peer_id) else {
            return;
        };
        if let Some(session) = self.sessions.get(&id) {
            // The close frame tells the peer why (`PAIRING_REJECTED_BY_PEER`).
            session.connection.close_with(CloseCode::PairingDeclined);
        }
        self.close_session(id, PAIRING_REJECTED, true);
    }

    /// Paired once both users accepted: trusted from now on, and let in.
    fn finish_pairing(&mut self, id: u64) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if !session
            .pairing
            .is_some_and(|pairing| pairing.local && pairing.remote)
        {
            return;
        }
        session.pairing = None;
        let Some(peer_id) = session.peer_id().map(str::to_owned) else {
            return;
        };
        let fingerprint = session.connection.fingerprint.clone();
        let waiting = !session.established();
        self.trusted.insert(peer_id.clone(), fingerprint.clone());
        self.sink.emit_value(json!({
            "event": "paired",
            "peer_id": peer_id,
            "fingerprint": fingerprint
        }));
        if waiting {
            self.admit(id);
        }
    }

    /// Forgets a paired peer. With `require_pairing`, its session ends.
    pub(super) fn unpair(&mut self, peer_id: &str) {
        if self.trusted.remove(peer_id).is_none() {
            return;
        }
        self.sink
            .emit_value(json!({ "event": "unpaired", "peer_id": peer_id }));
        if self.require_pairing {
            self.close_peer_sessions(peer_id, UNPAIRED);
        }
    }

    /// Turning it on ends the sessions of peers that are not paired.
    pub(super) fn set_require_pairing(&mut self, required: bool) {
        self.require_pairing = required;
        if !required {
            return;
        }
        let untrusted: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.handshaken() && session.pairing.is_none())
            .filter(|(_, session)| {
                let paired = session
                    .peer_id()
                    .and_then(|peer_id| self.trusted.get(peer_id));
                paired != Some(&session.connection.fingerprint)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in untrusted {
            self.close_session(id, PAIRING_REQUIRED, true);
        }
    }

    fn pairing_session(&self, peer_id: &str) -> Option<u64> {
        self.sessions
            .iter()
            .find(|(_, session)| session.pairing.is_some() && session.peer_id() == Some(peer_id))
            .map(|(id, _)| *id)
    }

    fn close_peer_sessions(&mut self, peer_id: &str, reason: &str) {
        let ids: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.peer_id() == Some(peer_id))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.close_session(id, reason, true);
        }
    }

    /// Ends pairings nobody answered.
    pub(super) fn expire_pairings(&mut self) {
        let expired: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                session
                    .pairing
                    .is_some_and(|pairing| pairing.since.elapsed() > PAIRING_TIMEOUT)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.close_session(id, PAIRING_TIMED_OUT, true);
        }
    }
}
