//! Keeping the user's devices connected (`CoreConfig::auto_connect`).
//!
//! The core connects by itself to the peers that are clearly the user's:
//! paired ones, and placed ones (a placed peer is meant to be one push away)
//! unless only paired peers may connect, so it never asks a stranger to
//! pair. It connects when such a peer shows up and again after its session
//! drops, while the peer is still announced, waiting 1 s, 2 s, 4 s … up to
//! 30 s between failed attempts. When both devices do this, the one with the
//! lower device ID dials first and the other waits a little, so they rarely
//! dial each other at once (which `hello` would sort out anyway).
//!
//! Automatic attempts are quiet: no `connecting`, and no `disconnected`
//! unless the shell saw the session (`connected`), or the shell asked for
//! it, or the peer turned out to be an impostor. Automatic connections leave
//! the input target alone.
//!
//! The user's choices win. A peer is left alone, until the user connects to
//! it again (or it connects to this device), after:
//! - this device disconnected it (`lc_core_disconnect_peer`),
//! - the peer's user disconnected (close code `Disconnected`) or the peer
//!   refused this device (`Refused`),
//! - pairing was declined or timed out, or the peer's identity changed.
//!
//! Sleep and network changes, which the shell reports:
//! - **Going to sleep** (`lc_core_system_will_sleep`): every session ends now,
//!   in order. Peers see `"peer closed the connection"` (no error), get
//!   control back and release what this device held, at once instead of
//!   after a heartbeat timeout. Until the system is asleep, no connections
//!   are made or taken, so the peers' attempts to reconnect are refused.
//! - **Woken up, or the network changed** (`lc_core_network_changed`): this
//!   device announces itself and rejoins the discovery group now, dials the
//!   user's devices without waiting for backoff, and sessions that did not
//!   survive (no pong) end after `PROBE_TIMEOUT` instead of the full
//!   heartbeat timeout. Sessions that did survive (QUIC follows an address
//!   change) carry on.

use super::trust;
use super::{Runtime, SESSION_TIMEOUT};
use crate::transport::{PAIRING_DECLINED_BY_PEER, PEER_DISCONNECTED, PEER_REFUSED};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Waits between attempts: doubled after every failure, up to the maximum.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// How long the device with the higher ID lets the other one dial first.
const YIELD: Duration = Duration::from_secs(2);
/// How long after `lc_core_system_will_sleep` the system is taken to be
/// asleep, if it does not report waking before (the sleep was cancelled).
const SLEEP_HOLD: Duration = Duration::from_secs(15);
/// How long a session may stay silent after a network change.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// `disconnected` reason when this device's shell disconnected a peer.
pub const LOCAL_REQUEST: &str = "local request";
/// `disconnected` reason for a peer this core cannot talk to.
pub const INCOMPATIBLE: &str = "incompatible peer";
/// `disconnected` reason when another device answered at a peer's address.
pub const WRONG_DEVICE: &str = "another device answered";
/// `disconnected` reason, on this device, for sessions it ended because the
/// system is going to sleep.
pub const GOING_TO_SLEEP: &str = "going to sleep";

/// When to try a peer next.
pub(super) struct Retry {
    next: Instant,
    failures: u32,
}

fn backoff(failures: u32) -> Duration {
    RETRY_MIN
        .saturating_mul(1 << failures.min(5))
        .min(RETRY_MAX)
}

/// Sessions that ended for one of these stay ended until someone connects
/// on purpose.
fn ends_for_good(reason: &str) -> bool {
    matches!(
        reason,
        LOCAL_REQUEST
            | INCOMPATIBLE
            | PEER_DISCONNECTED
            | PEER_REFUSED
            | PAIRING_DECLINED_BY_PEER
            | trust::PAIRING_REJECTED
            | trust::PAIRING_TIMED_OUT
            | trust::IDENTITY_CHANGED
            | trust::UNPAIRED
            | trust::PAIRING_REQUIRED
    )
}

impl Runtime<'_> {
    /// One of the user's devices, not left alone on purpose.
    fn wants_session(&self, peer_id: &str) -> bool {
        if !self.config.auto_connect || self.auto_suppressed.contains(peer_id) {
            return false;
        }
        let placed = self
            .placements
            .get(peer_id)
            .is_some_and(|record| record.offset.is_some());
        self.trusted.contains_key(peer_id) || (placed && !self.require_pairing)
    }

    fn first_wait(&self, peer_id: &str) -> Duration {
        if self.config.device_id.as_str() < peer_id {
            Duration::ZERO
        } else {
            YIELD
        }
    }

    /// The system is going to sleep: no connections in or out.
    pub(super) fn asleep(&self) -> bool {
        self.asleep_until
            .is_some_and(|until| Instant::now() < until)
    }

    pub(super) fn will_sleep(&mut self) {
        self.asleep_until = Some(Instant::now() + SLEEP_HOLD);
        let ids: Vec<u64> = self.sessions.keys().copied().collect();
        for id in ids {
            // An ordinary close: the peers try again, and find this device
            // once it is awake and announces itself.
            self.close_session(id, GOING_TO_SLEEP, true);
        }
    }

    pub(super) fn network_changed(&mut self) {
        self.asleep_until = None;
        // Backoff from attempts that could not work is no reason to wait.
        self.reconnect.clear();
        // A session that no longer answers ends `PROBE_TIMEOUT` from now:
        // as if it had last been heard of that much before the timeout.
        // Pings go out every second; a pong puts it right.
        if let Some(probe) = Instant::now().checked_sub(SESSION_TIMEOUT - PROBE_TIMEOUT) {
            for session in self
                .sessions
                .values_mut()
                .filter(|session| session.handshaken())
            {
                session.last_seen = session.last_seen.min(probe);
            }
        }
        self.auto_connect();
    }

    /// Dials the user's devices that are around and have no session, when
    /// their turn has come. Runs every second and when a peer shows up.
    pub(super) fn auto_connect(&mut self) {
        if self.asleep() {
            return;
        }
        let wanted: HashSet<String> = self
            .peers
            .keys()
            .filter(|peer_id| self.wants_session(peer_id))
            .cloned()
            .collect();
        // A peer that went away starts afresh when it comes back.
        self.reconnect.retain(|peer_id, _| wanted.contains(peer_id));
        let now = Instant::now();
        for peer_id in wanted {
            let busy = self.dialing.contains_key(&peer_id)
                || self
                    .sessions
                    .values()
                    .any(|session| session.peer_id() == Some(peer_id.as_str()));
            if busy {
                continue;
            }
            let first = now + self.first_wait(&peer_id);
            let retry = self.reconnect.entry(peer_id.clone()).or_insert(Retry {
                next: first,
                failures: 0,
            });
            if now < retry.next {
                continue;
            }
            // Not again before this attempt has had its chance.
            retry.next = now + backoff(retry.failures);
            self.dial(peer_id, true);
        }
    }

    /// An automatic or requested connect failed.
    pub(super) fn connect_failed(&mut self, peer_id: &str) {
        if let Some(retry) = self.reconnect.get_mut(peer_id) {
            retry.failures += 1;
            retry.next = Instant::now() + backoff(retry.failures);
        }
    }

    /// A session with `peer_id` ended for `reason`: try again later, or not.
    pub(super) fn session_ended(&mut self, peer_id: &str, reason: &str) {
        if ends_for_good(reason) {
            self.leave_alone(peer_id);
            return;
        }
        if self
            .sessions
            .values()
            .any(|session| session.peer_id() == Some(peer_id))
        {
            // A duplicate connection went; the other one carries on.
            return;
        }
        let now = Instant::now();
        let first_wait = self.first_wait(peer_id);
        match self.reconnect.get_mut(peer_id) {
            // It never got through: one more failure.
            Some(retry) => {
                retry.failures += 1;
                retry.next = now + backoff(retry.failures);
            }
            // A working session dropped (the peer quit or slept, the network
            // blinked): soon.
            None => {
                self.reconnect.insert(
                    peer_id.to_owned(),
                    Retry {
                        next: now + RETRY_MIN + first_wait,
                        failures: 0,
                    },
                );
            }
        }
    }

    /// No more automatic connects to this peer for now.
    pub(super) fn leave_alone(&mut self, peer_id: &str) {
        self.auto_suppressed.insert(peer_id.to_owned());
        self.reconnect.remove(peer_id);
    }

    /// The user connected (or the peer did): automatic connects resume,
    /// from scratch, once this session ends.
    pub(super) fn resume_auto_connect(&mut self, peer_id: &str) {
        self.auto_suppressed.remove(peer_id);
        self.reconnect.remove(peer_id);
    }
}
