// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Attached screens (see `docs/attached-screens.md`).
//!
//! A peer without a keyboard and mouse of its own (`CAPABILITY_ATTACHABLE`,
//! a KVM), connected and placed right against this device's own displays, is
//! this device's attached screen:
//!
//! - Every other peer is told its displays as this device's own
//!   (`ScreenInfo`, and the announcement), so a device driving this one
//!   pushes the pointer onto it like onto any monitor, with nothing new on
//!   its side. Only one hop: displays a peer describes are never passed on.
//! - While a peer drives this device, the shell passes the input that lands
//!   on the attached screen on to it (`relay`). That is not driving another
//!   device: whoever drives this one keeps doing so, and the relay ends with
//!   that control.

use super::Runtime;
use crate::layout::{self, Display, MAX_DISPLAYS, Offset};
use crate::model::{CAPABILITY_ATTACHABLE, LcInputEvent, WireMessage};
use serde::Serialize;
use serde_json::json;

/// Why a relay ended (`relay_ended`).
pub const NOT_ATTACHED: &str = "not attached";
pub const NOT_DRIVEN: &str = "not driven";
pub const CONTROL_ENDED: &str = "control ended";

/// An attached screen: the peer, where it is placed, and its displays in
/// this device's space (reported as such in `attached_screens`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct AttachedScreen {
    pub peer_id: String,
    pub offset: Offset,
    pub displays: Vec<Display>,
}

/// Input passed on to an attached screen for the peer that drives this
/// device.
pub(super) struct Relay {
    pub peer_id: String,
    /// The session of the peer that drove this device when the relay began.
    pub controller: u64,
}

impl Runtime<'_> {
    /// The attached screens as things stand: each peer with its displays in
    /// this device's space, by peer ID.
    fn current_attached(&self) -> Vec<AttachedScreen> {
        if self.local_displays.is_empty() {
            return Vec::new();
        }
        let mut attached: Vec<AttachedScreen> = self
            .placements
            .values()
            .filter_map(|record| {
                let offset = record.offset?;
                let displays = self.peer_displays.get(&record.peer_id)?;
                let id = self.established_session(&record.peer_id)?;
                if self.sessions[&id].connection.capabilities & CAPABILITY_ATTACHABLE == 0
                    || layout::portals(&self.local_displays, displays, offset).is_empty()
                {
                    return None;
                }
                Some(AttachedScreen {
                    peer_id: record.peer_id.clone(),
                    offset,
                    displays: layout::placed(displays, offset),
                })
            })
            .collect();
        attached.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        attached
    }

    /// The displays this device describes to a peer in a session: its own,
    /// then its attached screens except the peer's own, which would sit
    /// where the peer itself does. (Announcements carry only its own: an
    /// attached screen that hears one before its session starts must not
    /// see its host overlap it.)
    pub(super) fn described_displays(&self, to: &str) -> Vec<Display> {
        let mut displays = self.local_displays.clone();
        for screen in &self.attached {
            if screen.peer_id == to {
                continue;
            }
            if displays.len() + screen.displays.len() > MAX_DISPLAYS {
                break;
            }
            displays.extend_from_slice(&screen.displays);
        }
        displays
    }

    /// Looks at the attached screens again after anything that may change
    /// them (placements, displays on either side, sessions). When they
    /// changed, the shell hears `attached_screens` and peers get the new
    /// displays; returns whether that happened.
    pub(super) fn refresh_attached(&mut self) -> bool {
        let attached = self.current_attached();
        if attached == self.attached {
            return false;
        }
        self.attached = attached;
        self.sink.emit_value(json!({
            "event": "attached_screens",
            "screens": self.attached
        }));
        self.publish_displays();
        self.check_relay();
        true
    }

    /// Tells every peer in a session this device's displays.
    pub(super) fn publish_displays(&mut self) {
        for session in self
            .sessions
            .values()
            .filter(|session| session.established())
        {
            let Some(peer_id) = session.peer_id() else {
                continue;
            };
            let _ = session.connection.send_control(WireMessage::ScreenInfo {
                displays: self.described_displays(peer_id),
            });
        }
    }

    /// The session of the peer that drives this device now.
    fn controller(&self) -> Option<u64> {
        self.sessions
            .iter()
            .find(|(_, session)| {
                session.established() && session.remote_input.is_controlling() && !session.revoked
            })
            .map(|(id, _)| *id)
    }

    /// Starts relaying to an attached screen, or stops (`None`). Refused
    /// with `relay_ended` when the peer is not attached or nobody drives
    /// this device.
    pub(super) fn set_relay(&mut self, peer_id: Option<String>) {
        if peer_id.is_some() && self.relay.as_ref().map(|relay| &relay.peer_id) == peer_id.as_ref()
        {
            self.check_relay();
            return;
        }
        self.end_relay(None);
        let Some(peer_id) = peer_id else {
            return;
        };
        let attached = self.attached.iter().any(|screen| screen.peer_id == peer_id);
        match self.controller() {
            Some(controller) if attached => {
                self.relay = Some(Relay {
                    peer_id,
                    controller,
                })
            }
            _ => self.sink.emit_value(json!({
                "event": "relay_ended",
                "peer_id": peer_id,
                "reason": if attached { NOT_DRIVEN } else { NOT_ATTACHED }
            })),
        }
    }

    /// Input for the attached screen being relayed to; dropped when there
    /// is none.
    pub(super) fn relay_input(&mut self, event: LcInputEvent) {
        self.check_relay();
        let Some(id) = self
            .relay
            .as_ref()
            .and_then(|relay| self.established_session(&relay.peer_id))
        else {
            return;
        };
        if let Err(error) = self.sessions[&id].connection.send_input(event) {
            self.sink.emit_error(error);
        }
    }

    /// Ends the relay once the peer it serves no longer drives this device,
    /// or the screen is no longer attached.
    pub(super) fn check_relay(&mut self) {
        let Some(relay) = &self.relay else {
            return;
        };
        let attached = self
            .attached
            .iter()
            .any(|screen| screen.peer_id == relay.peer_id);
        let driving = self
            .sessions
            .get(&relay.controller)
            .is_some_and(|session| session.remote_input.is_controlling() && !session.revoked);
        if !attached {
            self.end_relay(Some(NOT_ATTACHED));
        } else if !driving {
            self.end_relay(Some(CONTROL_ENDED));
        }
    }

    /// Stops relaying: the attached screen gets a reset, so it releases
    /// what it holds and takes it as control given back. Reported as
    /// `relay_ended` with `reason`; the shell's own request is not.
    pub(super) fn end_relay(&mut self, reason: Option<&str>) {
        let Some(relay) = self.relay.take() else {
            return;
        };
        if let Some(id) = self.established_session(&relay.peer_id) {
            self.sessions[&id].connection.reset_input();
        }
        if let Some(reason) = reason {
            self.sink.emit_value(json!({
                "event": "relay_ended",
                "peer_id": relay.peer_id,
                "reason": reason
            }));
        }
    }

    /// The attached screen being relayed to told this device to stop
    /// (another device took it over): only the relay ends, whoever drives
    /// this device carries on.
    pub(super) fn relay_revoked(&mut self, peer_id: &str) {
        if self
            .relay
            .as_ref()
            .is_some_and(|relay| relay.peer_id == peer_id)
        {
            self.end_relay(Some("revoked"));
        }
    }
}
