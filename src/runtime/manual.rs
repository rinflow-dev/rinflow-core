// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Devices found by address (`CoreConfig::manual_addresses`).
//!
//! Discovery normally rides on multicast (and broadcast), which many
//! networks do not carry: guest and company Wi-Fi, access points that keep
//! clients apart, separate subnets, VPNs. For those the user adds the other
//! device's address by hand. This device then also sends its announcement
//! straight to that address (UDP, the discovery port), asking for an answer
//! (`Announcement::reply`), and the other device answers the same way. From
//! there everything is as if multicast had worked: the peer is listed, can
//! be connected and paired, and auto-connect keeps it connected, since the
//! announcements keep coming every two seconds. Only one of the two devices
//! needs the other's address.
//!
//! The shell hears how each address fares in `manual_addresses` events, one
//! entry per address in the order given: `searching` (no answer yet),
//! `found` (a device answers; with its `peer_id` and `name`),
//! `no_response` (asked for a while, nothing came back) or `unresolved`
//! (the name could not be looked up; `error` says why).
//!
//! Answers are rate-limited per sender, and are no larger than the probe
//! that asked for them, so the discovery port cannot be used to flood
//! someone else.

use super::Runtime;
use crate::model::MAX_MANUAL_ADDRESSES;
use crate::transport::ConnectionEvent;
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

/// How long an address may stay unanswered before it shows `no_response`.
const ANSWER_WAIT: Duration = Duration::from_secs(6);
/// A device counts as found while its answers keep coming at least this
/// often (it answers every probe, one every two seconds).
const FOUND_FOR: Duration = Duration::from_secs(10);
/// Host names are looked up again this often: DHCP may move the device.
const LOOKUP_AGAIN: Duration = Duration::from_secs(300);
/// How long a lookup may take.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Each sender gets an answer at most this often.
const ANSWER_INTERVAL: Duration = Duration::from_secs(1);
/// A host name (253) with a port.
const MAX_ADDRESS_LENGTH: usize = 253 + 6;

pub(super) struct ManualAddress {
    /// As the user entered it (trimmed).
    text: String,
    host: String,
    port: u16,
    /// An IP address rather than a name: never looked up.
    literal: bool,
    /// Where probes go: the discovery port at each IPv4 address found.
    targets: Vec<SocketAddr>,
    looking_up: bool,
    looked_up: Option<Instant>,
    error: Option<String>,
    added: Instant,
    /// The device that answered from one of `targets`, and when it last did.
    peer_id: Option<String>,
    answered: Option<Instant>,
}

/// Splits `host`, `host:port` or `[ipv6]:port` into host and port; without
/// a port, `default_port`. A bare IPv6 address (several colons) has no port.
pub fn parse_address(text: &str, default_port: u16) -> Result<(String, u16), String> {
    let text = text.trim();
    let invalid = || format!("not an address: {text}");
    if text.is_empty() || text.len() > MAX_ADDRESS_LENGTH || text.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let port_of = |port: &str| match port.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(invalid()),
    };
    let (host, port) = if let Some(rest) = text.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(invalid)?;
        let port = match after.strip_prefix(':') {
            Some(port) => port_of(port)?,
            None if after.is_empty() => default_port,
            None => return Err(invalid()),
        };
        (host, port)
    } else {
        match text.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, port_of(port)?),
            _ => (text, default_port),
        }
    };
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%');
    if host.is_empty() || !host.chars().all(allowed) {
        return Err(invalid());
    }
    Ok((host.to_owned(), port))
}

impl Runtime<'_> {
    /// The addresses to look for devices at, replacing the earlier ones.
    /// Addresses kept from before keep their state.
    pub(super) fn set_manual_addresses(&mut self, addresses: Vec<String>) {
        let mut wanted: Vec<String> = Vec::new();
        for address in addresses {
            let address = address.trim().to_owned();
            if !address.is_empty()
                && !wanted
                    .iter()
                    .any(|seen| seen.eq_ignore_ascii_case(&address))
            {
                wanted.push(address);
            }
        }
        wanted.truncate(MAX_MANUAL_ADDRESSES);
        let mut previous = std::mem::take(&mut self.manual);
        for text in wanted {
            if let Some(index) = previous
                .iter()
                .position(|entry| entry.text.eq_ignore_ascii_case(&text))
            {
                self.manual.push(previous.swap_remove(index));
                continue;
            }
            let (host, port, error) = match parse_address(&text, self.config.discovery_port) {
                Ok((host, port)) => (host, port, None),
                Err(error) => (String::new(), 0, Some(error)),
            };
            let literal = host.parse::<IpAddr>().is_ok();
            self.manual.push(ManualAddress {
                text,
                host,
                port,
                literal,
                targets: Vec::new(),
                looking_up: false,
                looked_up: None,
                error,
                added: Instant::now(),
                peer_id: None,
                answered: None,
            });
        }
        for index in 0..self.manual.len() {
            if self.manual[index].looked_up.is_none() && self.manual[index].error.is_none() {
                self.look_up(index);
            }
        }
        self.publish_manual();
    }

    /// Finds where to send probes: at once for an IP address, in the
    /// background for a name (the answer comes as `ManualResolved`).
    fn look_up(&mut self, index: usize) {
        let entry = &mut self.manual[index];
        if entry.looking_up || entry.host.is_empty() {
            return;
        }
        if entry.literal {
            let found = entry
                .host
                .parse::<IpAddr>()
                .map(|ip| vec![SocketAddr::new(ip, entry.port)]);
            let text = entry.text.clone();
            let result = found.map_err(|error| error.to_string()).and_then(ipv4_only);
            self.manual_resolved(&text, result);
            return;
        }
        entry.looking_up = true;
        let (text, host, port) = (entry.text.clone(), entry.host.clone(), entry.port);
        let events = self.connection_events.clone();
        tokio::spawn(async move {
            let result =
                match tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((host, port)))
                    .await
                {
                    Ok(Ok(found)) => ipv4_only(found.collect()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err("lookup timed out".to_owned()),
                };
            let _ = events.send(ConnectionEvent::ManualResolved {
                address: text,
                result,
            });
        });
    }

    /// A lookup finished: probe at once, so the device shows up without
    /// waiting for the next announcement.
    pub(super) fn manual_resolved(
        &mut self,
        address: &str,
        result: Result<Vec<SocketAddr>, String>,
    ) {
        let Some(entry) = self.manual.iter_mut().find(|entry| entry.text == address) else {
            return;
        };
        entry.looking_up = false;
        entry.looked_up = Some(Instant::now());
        match result {
            Ok(targets) => {
                entry.error = None;
                entry.targets = targets;
                for target in entry.targets.clone() {
                    self.datagrams.push((self.probe.clone(), target));
                }
            }
            // A name that worked before keeps its addresses meanwhile.
            Err(error) if entry.targets.is_empty() => entry.error = Some(error),
            Err(_) => {}
        }
        self.publish_manual();
    }

    /// With every announcement: a probe to each address, and names looked up
    /// again from time to time.
    pub(super) fn probe_manual(&mut self) {
        for index in 0..self.manual.len() {
            let entry = &self.manual[index];
            let stale = entry
                .looked_up
                .is_some_and(|at| at.elapsed() >= LOOKUP_AGAIN);
            if !entry.literal && stale {
                self.look_up(index);
            }
            for target in self.manual[index].targets.clone() {
                self.datagrams.push((self.probe.clone(), target));
            }
        }
    }

    /// The network changed: names may point elsewhere now.
    pub(super) fn manual_network_changed(&mut self) {
        for index in 0..self.manual.len() {
            if !self.manual[index].literal && self.manual[index].error.is_none() {
                self.look_up(index);
            }
        }
    }

    /// An announcement came from `source`. Answers a probe, and notes which
    /// manually added address the device answers from.
    pub(super) fn manual_announcement(&mut self, source: SocketAddr, peer_id: &str, probe: bool) {
        for entry in &mut self.manual {
            if entry
                .targets
                .iter()
                .any(|target| target.ip() == source.ip())
            {
                entry.peer_id = Some(peer_id.to_owned());
                entry.answered = Some(Instant::now());
            }
        }
        if !probe {
            return;
        }
        let now = Instant::now();
        let recent = self
            .answered_probes
            .get(&source.ip())
            .is_some_and(|at| now.duration_since(*at) < ANSWER_INTERVAL);
        if recent {
            return;
        }
        self.answered_probes.insert(source.ip(), now);
        self.datagrams.push((self.announcement.clone(), source));
    }

    /// Reports the addresses' state when it changed; also from `maintenance`,
    /// as time alone turns `searching` into `no_response`.
    pub(super) fn publish_manual(&mut self) {
        let now = Instant::now();
        self.answered_probes
            .retain(|_, at| now.duration_since(*at) < ANSWER_INTERVAL * 10);
        let addresses: Vec<Value> = self
            .manual
            .iter()
            .map(|entry| {
                let found = entry
                    .answered
                    .is_some_and(|at| now.duration_since(at) < FOUND_FOR);
                let peer_id = entry.peer_id.as_deref().filter(|_| found);
                let status = if let Some(error) = &entry.error {
                    json!({ "status": "unresolved", "error": error })
                } else if let Some(peer_id) = peer_id {
                    json!({
                        "status": "found",
                        "peer_id": peer_id,
                        "name": self.peers.get(peer_id).map(|peer| peer.name.as_str())
                    })
                } else if entry.looking_up || now.duration_since(entry.added) < ANSWER_WAIT {
                    json!({ "status": "searching" })
                } else {
                    json!({ "status": "no_response" })
                };
                let mut value = status;
                value["address"] = json!(entry.text);
                value
            })
            .collect();
        let value = Value::Array(addresses);
        if value == self.manual_status {
            return;
        }
        self.sink.emit_value(json!({
            "event": "manual_addresses",
            "addresses": value
        }));
        self.manual_status = value;
    }
}

/// Discovery runs over IPv4 (see `create_discovery_socket`).
fn ipv4_only(found: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
    let mut targets: Vec<SocketAddr> = found.into_iter().filter(SocketAddr::is_ipv4).collect();
    targets.dedup();
    if targets.is_empty() {
        Err("no IPv4 address".to_owned())
    } else {
        Ok(targets)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_address;

    #[test]
    fn addresses_parse_with_and_without_ports() {
        assert_eq!(
            parse_address(" 192.168.1.20 ", 42420),
            Ok(("192.168.1.20".to_owned(), 42420))
        );
        assert_eq!(
            parse_address("mac-mini.local:5000", 42420),
            Ok(("mac-mini.local".to_owned(), 5000))
        );
        assert_eq!(
            parse_address("[fe80::1]:6000", 42420),
            Ok(("fe80::1".to_owned(), 6000))
        );
        assert_eq!(parse_address("fe80::1", 7), Ok(("fe80::1".to_owned(), 7)));
        for bad in [
            "",
            "a b",
            "host:0",
            "host:99999",
            "[::1",
            "host/x",
            "[::1]x",
        ] {
            assert!(parse_address(bad, 42420).is_err(), "{bad}");
        }
    }
}
