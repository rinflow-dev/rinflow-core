// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Stable codes for why things ended, for shells to show in their own
//! language.
//!
//! Events that carry a `reason` (`disconnected`, `pairing_ended`,
//! `remote_control_ended`, `control_revoked`, `files_failed`,
//! `files_send_ended`, `files_not_shared`, …) and `error` events (with their
//! `message`) also carry `reason_code`: a snake_case word from the lists below. The
//! English `reason` and `message` stay as they were, for logs and for shells
//! that match on them. A reason this table does not know (a newer peer's,
//! or an I/O error's text) has the code `other`.
//!
//! Every event goes through [`annotate`] on its way out
//! (`EventSink::emit_value`), so no call site has to remember it.

use crate::files::{TOO_DEEP, TOO_MANY_FILES};
use crate::runtime::reconnect::{GOING_TO_SLEEP, INCOMPATIBLE, LOCAL_REQUEST, WRONG_DEVICE};
use crate::runtime::trust::{
    IDENTITY_CHANGED, PAIRING_BUSY, PAIRING_FAILED, PAIRING_OUTDATED, PAIRING_REJECTED,
    PAIRING_REQUIRED, PAIRING_TIMED_OUT, UNPAIRED,
};
use crate::transport::{PAIRING_DECLINED_BY_PEER, PEER_CLOSED, PEER_DISCONNECTED, PEER_REFUSED};
use serde_json::Value;

/// The code of a reason nobody listed.
pub const OTHER: &str = "other";

/// Exact reasons, with their code.
const REASONS: &[(&str, &str)] = &[
    // Sessions and pairing (`disconnected`, `pairing_ended`).
    (LOCAL_REQUEST, "local_request"),
    (PEER_CLOSED, "peer_closed"),
    (PEER_DISCONNECTED, "peer_disconnected"),
    (GOING_TO_SLEEP, "going_to_sleep"),
    ("core stopped", "core_stopped"),
    ("heartbeat timeout", "heartbeat_timeout"),
    ("handshake timeout", "handshake_timeout"),
    ("connection lost", "connection_lost"),
    (PEER_REFUSED, "peer_refused"),
    (IDENTITY_CHANGED, "identity_changed"),
    ("another device under this ID", "identity_conflict"),
    ("duplicate connection", "replaced_connection"),
    ("replaced by a newer connection", "replaced_connection"),
    (INCOMPATIBLE, "incompatible"),
    (WRONG_DEVICE, "wrong_device"),
    ("message before hello", "protocol_error"),
    ("unknown data link", "protocol_error"),
    ("data link from another device", "protocol_error"),
    ("unexpected message on the data link", "protocol_error"),
    (PAIRING_REJECTED, "pairing_declined"),
    (PAIRING_DECLINED_BY_PEER, "pairing_declined_by_peer"),
    (PAIRING_TIMED_OUT, "pairing_timed_out"),
    (PAIRING_REQUIRED, "pairing_required"),
    (PAIRING_BUSY, "pairing_busy"),
    (PAIRING_OUTDATED, "pairing_outdated"),
    (PAIRING_FAILED, "pairing_failed"),
    (UNPAIRED, "unpaired"),
    // Control (`remote_control_ended`, `control_revoked`).
    ("remote reset", "remote_reset"),
    (
        "taken over by another device",
        "taken_over_by_another_device",
    ),
    ("taken back locally", "taken_back_locally"),
    ("remote control turned off", "remote_control_off"),
    ("taken_over", "taken_over"),
    ("revoked", "revoked"),
    // Files (`files_failed`, `files_send_ended`, `files_not_shared`, …).
    ("done", "done"),
    ("cancelled", "cancelled"),
    ("replaced by a newer copy", "replaced_by_newer_copy"),
    ("files are no longer on the clipboard", "files_gone"),
    ("too_many_files", "too_many_files"),
    (TOO_MANY_FILES, "too_many_files"),
    ("too_deep", "too_deep"),
    (TOO_DEEP, "too_deep"),
    ("file operation timed out", "file_timed_out"),
    ("remote file read timed out", "file_timed_out"),
    ("file larger than announced", "file_corrupted"),
    ("file chunk out of order", "file_corrupted"),
    ("file operation ended", "file_operation_ended"),
    ("too many file reads", "too_many_file_reads"),
];

/// Reasons with details after a fixed start.
const REASON_PREFIXES: &[(&str, &str)] = &[
    ("read failed:", "connection_lost"),
    ("write failed:", "connection_lost"),
    ("connect failed:", "connect_failed"),
];

/// `error` messages: by their fixed start, or a fixed part after a device
/// ID.
const ERROR_PREFIXES: &[(&str, &str)] = &[
    ("failed to create async runtime:", "core_start_failed"),
    ("discovery socket failed:", "discovery_unavailable"),
    ("identity failed:", "identity_unavailable"),
    ("control port failed:", "control_port_unavailable"),
    ("peer is no longer available:", "peer_unavailable"),
    ("connection setup failed:", "connect_failed"),
    ("invalid displays:", "placement_failed"),
    (
        "cannot place a peer before the local displays are known",
        "placement_failed",
    ),
    ("placement offset must be finite", "placement_failed"),
    ("no room to place ", "placement_failed"),
    ("data link failed:", "data_link_failed"),
    ("data link answered by another device", "data_link_failed"),
    ("no randomness for a pairing code", "pairing_failed"),
    ("files not shared:", "files_not_shared"),
    ("clipboard not shared:", "clipboard_not_shared"),
    ("clipboard not received:", "clipboard_not_received"),
    ("files not received:", "files_not_received"),
    ("a connection claiming to be ", "identity_conflict"),
];
const ERROR_PARTS: &[(&str, &str)] = &[
    (
        " presented a different certificate than when it was paired",
        "identity_changed",
    ),
    (" has to be updated before it can pair", "pairing_outdated"),
    ("; pairing refused", "pairing_failed"),
];

/// The code of an event's `reason`.
pub fn reason_code(reason: &str) -> &'static str {
    REASONS
        .iter()
        .find(|(text, _)| *text == reason)
        .or_else(|| {
            REASON_PREFIXES
                .iter()
                .find(|(start, _)| reason.starts_with(start))
        })
        .map_or(OTHER, |(_, code)| code)
}

/// The code of an `error` event's `message`.
pub fn error_code(message: &str) -> &'static str {
    ERROR_PREFIXES
        .iter()
        .find(|(start, _)| message.starts_with(start))
        .or_else(|| ERROR_PARTS.iter().find(|(part, _)| message.contains(part)))
        .map_or(OTHER, |(_, code)| code)
}

/// Adds `reason_code` to an event that has a `reason`, or to an `error` event.
pub(crate) fn annotate(event: &mut Value) {
    let Some(object) = event.as_object_mut() else {
        return;
    };
    let code = if object.get("event").and_then(Value::as_str) == Some("error") {
        object
            .get("message")
            .and_then(Value::as_str)
            .map(error_code)
    } else {
        object
            .get("reason")
            .and_then(Value::as_str)
            .map(reason_code)
    };
    if let Some(code) = code {
        object.insert("reason_code".to_owned(), Value::from(code));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_listed_reason_has_its_own_code() {
        for (reason, code) in REASONS {
            assert_eq!(reason_code(reason), *code, "{reason}");
            assert_ne!(*code, OTHER);
        }
        assert_eq!(
            reason_code("read failed: connection reset"),
            "connection_lost"
        );
        assert_eq!(reason_code("connect failed: timed out"), "connect_failed");
        assert_eq!(reason_code("disk full"), OTHER);
    }

    #[test]
    fn errors_are_coded_by_their_fixed_parts() {
        assert_eq!(
            error_code("control port failed: address in use"),
            "control_port_unavailable"
        );
        assert_eq!(
            error_code("no room to place pc on that side"),
            "placement_failed"
        );
        assert_eq!(
            error_code("pc presented a different certificate than when it was paired; refused"),
            "identity_changed"
        );
        assert_eq!(
            error_code("pc runs an older Rinflow and has to be updated before it can pair"),
            "pairing_outdated"
        );
        assert_eq!(
            error_code("pc sent a bad code; pairing refused"),
            "pairing_failed"
        );
        assert_eq!(error_code("something new"), OTHER);
    }

    #[test]
    fn events_with_a_reason_and_errors_get_a_code_others_do_not() {
        let mut ended =
            json!({"event": "disconnected", "peer_id": "pc", "reason": "heartbeat timeout"});
        annotate(&mut ended);
        assert_eq!(ended["reason_code"], "heartbeat_timeout");
        assert_eq!(ended["reason"], "heartbeat timeout");
        let mut error = json!({"event": "error", "message": "identity failed: no key"});
        annotate(&mut error);
        assert_eq!(error["reason_code"], "identity_unavailable");
        let mut started = json!({"event": "started"});
        annotate(&mut started);
        assert!(started.get("reason_code").is_none());
    }
}
