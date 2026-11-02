// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
//! Pairing: the side that wants it sends `PairRequest`, and both sides
//! pair, also one that already trusts the other: a user can only check a
//! code against the other device's screen. Then the two build the code
//! together (`secure.rs` says why it takes three messages):
//!
//! 1. The dialer of the connection sends `PairCommit`: a hash of its random
//!    nonce and both certificates.
//! 2. The other side answers `PairNonce` with its own nonce.
//! 3. The dialer reveals its nonce (`PairReveal`), and the other side checks
//!    it against the commitment.
//!
//! Each side then shows the six-digit code (`pairing_requested`, from both
//! certificates and both nonces); each user checks it and accepts
//! (`accept_pairing` → `PairAccept`). Once a side has both its own and the
//! peer's acceptance it trusts the peer (`paired`, which the shell saves for
//! `CoreConfig::trusted_peers`) and lets it in. Declining or two minutes
//! without an answer closes the connection; a decline travels in the QUIC
//! close frame (`CloseCode::PairingDeclined`), so the other side can say
//! so. A pairing message out of turn, or a reveal that does not match its
//! commitment, ends the connection (`PAIRING_FAILED`). A peer without
//! `CAPABILITY_PAIRING_V2` (an older build) cannot pair: its code could be
//! forged (`PAIRING_OUTDATED`).

use super::Runtime;
use crate::model::{CAPABILITY_PAIRING_V2, WireMessage};
use crate::secure;
use crate::transport::CloseCode;
use serde_json::json;
use std::time::{Duration, Instant};

/// `disconnected` reasons. The other side of a declined pairing reports
/// `transport::PAIRING_DECLINED_BY_PEER`.
pub const IDENTITY_CHANGED: &str = "the device's identity changed";
pub const PAIRING_REJECTED: &str = "pairing declined";
pub const PAIRING_TIMED_OUT: &str = crate::transport::PAIRING_TIMED_OUT;
pub const UNPAIRED: &str = "unpaired";
pub const PAIRING_REQUIRED: &str = "pairing required";
/// Another device is pairing with this one right now.
pub const PAIRING_BUSY: &str = "another pairing is in progress";
/// The peer runs a build whose pairing code a device in the middle could
/// forge; it has to be updated before it can pair.
pub const PAIRING_OUTDATED: &str = "the other device needs a newer Rinflow to pair";
/// The peer broke the pairing exchange: a message out of turn, or a nonce
/// that does not match its commitment. Possibly a device in the middle.
pub const PAIRING_FAILED: &str = "pairing failed";

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
    /// This side dialed the connection: it commits to its nonce first.
    dialer: bool,
    /// This side's random part of the code.
    nonce: [u8; 32],
    /// The side that did not dial: the dialer's commitment, once received.
    commitment: Option<[u8; 32]>,
    /// The code, once both nonces are known. Shown, and accepted, only then.
    code: Option<u32>,
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
                self.refuse_session(id, IDENTITY_CHANGED);
            }
            None if self.require_pairing => self.start_pairing(id, true),
            None => self.admit(id),
        }
    }

    /// Starts pairing on a session: asks the peer (`request`), and as the
    /// dialer commits to this side's nonce. The code shows once both nonces
    /// are in (`pair_nonce` on the dialer, `pair_revealed` on the other side).
    fn start_pairing(&mut self, id: u64, request: bool) {
        // One pairing at a time: the code the user is comparing must not be
        // swapped for another device's under the same dialog.
        let busy = self
            .sessions
            .iter()
            .any(|(other, session)| *other != id && session.pairing.is_some());
        if busy {
            if let Some(session) = self.sessions.get(&id) {
                session.connection.close_with(CloseCode::Refused);
            }
            self.close_session(id, PAIRING_BUSY, true);
            return;
        }
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        if session.pairing.is_some() {
            return;
        }
        if session.connection.capabilities & CAPABILITY_PAIRING_V2 == 0 {
            // It may still connect once paired, but its pairing code could
            // be forged by a device in the middle.
            let peer_id = session.peer_id().unwrap_or("a device").to_owned();
            self.sink.emit_error(format!(
                "{peer_id} runs an older Rinflow and has to be updated before it can pair"
            ));
            self.refuse_session(id, PAIRING_OUTDATED);
            return;
        }
        let Some(nonce) = secure::pairing_nonce() else {
            self.sink
                .emit_error("no randomness for a pairing code; cannot pair".to_owned());
            self.refuse_session(id, PAIRING_FAILED);
            return;
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let dialer = session.outgoing;
        session.pairing = Some(Pairing {
            local: false,
            remote: false,
            since: Instant::now(),
            dialer,
            nonce,
            commitment: None,
            code: None,
        });
        // The request first: the peer pairs from then on, so it expects the
        // commitment that follows on the same stream.
        if request {
            let _ = session.connection.send_control(WireMessage::PairRequest);
        }
        if dialer {
            let commitment = secure::pairing_commitment(
                &nonce,
                &self.fingerprint,
                &session.connection.fingerprint,
            );
            let _ = session
                .connection
                .send_control(WireMessage::PairCommit { commitment });
        }
    }

    /// `PairCommit` from the dialer, to the side that accepted the
    /// connection: keep it, and answer with this side's nonce.
    pub(super) fn pair_committed(&mut self, id: u64, commitment: [u8; 32]) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let Some(pairing) = session
            .pairing
            .as_mut()
            .filter(|pairing| !pairing.dialer && pairing.commitment.is_none())
        else {
            return self.pairing_failed(id, "an unexpected pairing commitment");
        };
        pairing.commitment = Some(commitment);
        let nonce = pairing.nonce;
        let _ = session
            .connection
            .send_control(WireMessage::PairNonce { nonce });
    }

    /// `PairNonce` from the other side, to the dialer: both nonces are in,
    /// so reveal this side's and show the code.
    pub(super) fn pair_nonce(&mut self, id: u64, theirs: [u8; 32]) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let Some(pairing) = session
            .pairing
            .as_mut()
            .filter(|pairing| pairing.dialer && pairing.code.is_none())
        else {
            return self.pairing_failed(id, "an unexpected pairing nonce");
        };
        let nonce = pairing.nonce;
        pairing.code = Some(secure::pairing_code(
            &self.fingerprint,
            &session.connection.fingerprint,
            &nonce,
            &theirs,
        ));
        let _ = session
            .connection
            .send_control(WireMessage::PairReveal { nonce });
        self.announce_pairing(id);
    }

    /// `PairReveal` from the dialer: the nonce it committed to. It has to
    /// match the commitment; then show the code.
    pub(super) fn pair_revealed(&mut self, id: u64, theirs: [u8; 32]) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let Some(pairing) = session
            .pairing
            .as_mut()
            .filter(|pairing| !pairing.dialer && pairing.code.is_none())
        else {
            return self.pairing_failed(id, "an unexpected pairing reveal");
        };
        let dialer = &session.connection.fingerprint;
        let Some(commitment) = pairing.commitment else {
            return self.pairing_failed(id, "a pairing reveal before its commitment");
        };
        if secure::pairing_commitment(&theirs, dialer, &self.fingerprint) != commitment {
            return self.pairing_failed(id, "a pairing nonce that does not match its commitment");
        }
        pairing.code = Some(secure::pairing_code(
            dialer,
            &self.fingerprint,
            &theirs,
            &pairing.nonce,
        ));
        self.announce_pairing(id);
    }

    /// The peer broke the pairing exchange: whatever it is, it is not let in
    /// and not asked again by itself.
    fn pairing_failed(&mut self, id: u64, what: &str) {
        let peer_id = self
            .sessions
            .get(&id)
            .and_then(|session| session.peer_id())
            .unwrap_or("a device")
            .to_owned();
        self.sink
            .emit_error(format!("{peer_id} sent {what}; pairing refused"));
        self.refuse_session(id, PAIRING_FAILED);
    }

    /// Tells the shell to show the pairing code for this session, once it is
    /// known.
    pub(super) fn announce_pairing(&self, id: u64) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        let (Some(peer_id), Some(code)) = (
            session.peer_id(),
            session.pairing.and_then(|pairing| pairing.code),
        ) else {
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
            "code": format!("{code:06}"),
            "fingerprint": theirs
        }));
    }

    /// `PairRequest` from the peer: it does not trust this device yet. This
    /// side pairs too, also if it trusts the peer already: the peer's user
    /// can only check its code against the code shown here.
    pub(super) fn pair_requested(&mut self, id: u64) {
        if !self
            .sessions
            .get(&id)
            .is_some_and(|session| session.peer_id().is_some())
        {
            return;
        }
        self.start_pairing(id, false);
    }

    /// `PairAccept` from the peer. Its user can only have accepted a code,
    /// so it comes after the exchange that makes one.
    pub(super) fn pair_accepted(&mut self, id: u64) {
        let Some(pairing) = self
            .sessions
            .get_mut(&id)
            .and_then(|session| session.pairing.as_mut())
        else {
            return;
        };
        if pairing.code.is_none() {
            return self.pairing_failed(id, "a pairing acceptance before the code");
        }
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
        // Accepting means the user checked the code, so there must be one.
        if let Some(pairing) = session.pairing.as_mut()
            && pairing.code.is_some()
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
            self.refuse_peer_sessions(peer_id, UNPAIRED);
        }
    }

    /// Turning it on ends the sessions of peers that are not paired.
    pub(super) fn set_require_pairing(&mut self, required: bool) {
        self.require_pairing = required;
        if !required {
            return;
        }
        // Every session let in without being paired, also one that is
        // pairing now: until both users confirm, it gets nothing.
        let untrusted: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.handshaken() && (session.pairing.is_none() || session.established())
            })
            .filter(|(_, session)| {
                let paired = session
                    .peer_id()
                    .and_then(|peer_id| self.trusted.get(peer_id));
                paired != Some(&session.connection.fingerprint)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in untrusted {
            self.refuse_session(id, PAIRING_REQUIRED);
        }
    }

    fn pairing_session(&self, peer_id: &str) -> Option<u64> {
        self.sessions
            .iter()
            .find(|(_, session)| session.pairing.is_some() && session.peer_id() == Some(peer_id))
            .map(|(id, _)| *id)
    }

    fn refuse_peer_sessions(&mut self, peer_id: &str, reason: &str) {
        let ids: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.peer_id() == Some(peer_id))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.refuse_session(id, reason);
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
            // The peer is told, so it does not dial again and ask anew.
            if let Some(session) = self.sessions.get(&id) {
                session.connection.close_with(CloseCode::PairingTimedOut);
            }
            self.close_session(id, PAIRING_TIMED_OUT, true);
        }
    }
}
