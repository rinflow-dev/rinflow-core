// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Waking a sleeping device over the network (Wake-on-LAN).
//!
//! A device's network adapter stays listening while the device sleeps and
//! wakes it on a "magic packet": six `0xFF` bytes, then the adapter's
//! hardware address sixteen times, anywhere in a frame that reaches it. A
//! sleeping device has no IP address to send to, so the packet goes to the
//! broadcast address of every network this device is on; it does not cross
//! routers.
//!
//! - **Whom to wake.** Each device tells its peers the hardware addresses it
//!   can be woken by (`lc_core_set_wake_addresses`, the `WakeInfo` message,
//!   event `peer_wake`). The shell keeps them, as it keeps placements and
//!   paired devices, because they are needed exactly when the peer is gone.
//! - **Waking.** [`send`] needs no running core and no session: a shell
//!   calls it with what it kept. It is one round of packets; a device takes
//!   seconds to wake and packets get lost, so shells send a few rounds until
//!   the peer announces itself again (`peer_up`).
//! - **Asleep or gone.** A device that goes to sleep says so as it closes
//!   its sessions (`lc_core_system_will_sleep`): peers report `peer_asleep`
//!   just before `disconnected`.
//!
//! The packet carries no secret: anyone on the network who knows a hardware
//! address can wake its device, with or without Rinflow. Hardware addresses
//! are only sent over established (paired, encrypted) sessions, never in
//! announcements.

use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

/// A network adapter's hardware (MAC) address.
pub type HardwareAddress = [u8; 6];

/// The discard port, where magic packets are sent by custom. Nothing
/// listens: the adapter matches the payload.
pub const WAKE_PORT: u16 = 9;
/// Hardware addresses a device may name in one `WakeInfo`, at most.
pub const MAX_WAKE_ADDRESSES: usize = 8;
const PACKET_LENGTH: usize = 6 + 16 * 6;

/// The magic packet for one hardware address.
pub fn magic_packet(address: HardwareAddress) -> [u8; PACKET_LENGTH] {
    let mut packet = [0xFF_u8; PACKET_LENGTH];
    let (repeats, _) = packet[6..].as_chunks_mut::<6>();
    repeats.fill(address);
    packet
}

/// An address an adapter can have for itself: not all zeros, and not a
/// group address (which broadcast is).
pub fn is_usable(address: HardwareAddress) -> bool {
    address != [0; 6] && address[0] & 1 == 0
}

/// `aa:bb:cc:dd:ee:ff`, as events and shells write a hardware address.
pub fn format_address(address: HardwareAddress) -> String {
    address
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Reads `aa:bb:cc:dd:ee:ff` (or with `-`, in either case).
pub fn parse_address(text: &str) -> Option<HardwareAddress> {
    let mut address = [0_u8; 6];
    let mut parts = text.trim().split([':', '-']);
    for byte in &mut address {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(address)
}

/// What a device may tell its peers: usable addresses, each once, in the
/// order given, at most [`MAX_WAKE_ADDRESSES`].
pub fn normalized(addresses: &[HardwareAddress]) -> Vec<HardwareAddress> {
    let mut seen = BTreeSet::new();
    addresses
        .iter()
        .copied()
        .filter(|address| is_usable(*address) && seen.insert(*address))
        .take(MAX_WAKE_ADDRESSES)
        .collect()
}

/// One IPv4 network this device is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Network {
    address: Ipv4Addr,
    netmask: Ipv4Addr,
    broadcast: Option<Ipv4Addr>,
}

impl Network {
    /// Where a packet for everyone on this network goes.
    fn broadcast(&self) -> Option<Ipv4Addr> {
        let (address, netmask) = (u32::from(self.address), u32::from(self.netmask));
        // A point-to-point link (many VPNs) has nobody to broadcast to.
        self.broadcast
            .or_else(|| (netmask != u32::MAX && netmask != 0).then(|| (address | !netmask).into()))
    }

    fn contains(&self, other: Ipv4Addr) -> bool {
        let netmask = u32::from(self.netmask);
        netmask != 0 && u32::from(self.address) & netmask == u32::from(other) & netmask
    }

    /// Where to send on this network: its broadcast address, the limited
    /// broadcast (for networks that drop the directed one), and the device's
    /// last address if it was on this network. The last one reaches an
    /// adapter that still answers for its address while asleep, where
    /// broadcasts are filtered.
    fn targets(&self, last_address: Option<Ipv4Addr>) -> Vec<Ipv4Addr> {
        let mut targets = Vec::with_capacity(3);
        targets.extend(self.broadcast());
        targets.push(Ipv4Addr::BROADCAST);
        targets.extend(last_address.filter(|address| self.contains(*address)));
        targets.dedup();
        targets
    }
}

fn networks() -> Vec<Network> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    interfaces
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(v4) => Some(Network {
                address: v4.ip,
                netmask: v4.netmask,
                broadcast: v4.broadcast,
            }),
            if_addrs::IfAddr::V6(_) => None,
        })
        .collect()
}

/// One round of magic packets for a device with these hardware addresses,
/// out of every IPv4 interface: each socket is bound to its interface's
/// address, since a broadcast otherwise leaves through one interface only
/// (the one the system routes it to, often a VPN's). `last_address` is
/// where the device was last seen, if known.
///
/// Returns how many packets got out; an error if none did (no network, or
/// every send refused). Blocks only for the sends themselves.
pub fn send(addresses: &[HardwareAddress], last_address: Option<Ipv4Addr>) -> io::Result<usize> {
    let addresses = normalized(addresses);
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no hardware address to wake",
        ));
    }
    let mut sent = 0;
    let mut last_error = None;
    for network in networks() {
        let socket = match UdpSocket::bind(SocketAddrV4::new(network.address, 0)) {
            Ok(socket) => socket,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        if let Err(error) = socket.set_broadcast(true) {
            last_error = Some(error);
            continue;
        }
        for target in network.targets(last_address) {
            for address in &addresses {
                match socket.send_to(
                    &magic_packet(*address),
                    SocketAddrV4::new(target, WAKE_PORT),
                ) {
                    Ok(_) => sent += 1,
                    Err(error) => last_error = Some(error),
                }
            }
        }
    }
    match (sent, last_error) {
        (0, Some(error)) => Err(error),
        (0, None) => Err(io::Error::new(
            io::ErrorKind::NetworkUnreachable,
            "no network to send on",
        )),
        _ => Ok(sent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDRESS: HardwareAddress = [0x00, 0x1b, 0x63, 0x84, 0x45, 0xe6];

    #[test]
    fn the_magic_packet_is_six_ff_then_the_address_sixteen_times() {
        let packet = magic_packet(ADDRESS);
        assert_eq!(packet.len(), 102);
        assert_eq!(packet[..6], [0xFF; 6]);
        assert!(packet[6..].chunks(6).all(|repeat| repeat == ADDRESS));
        assert_eq!(packet[6..].chunks(6).count(), 16);
    }

    #[test]
    fn hardware_addresses_read_and_write_as_text() {
        assert_eq!(format_address(ADDRESS), "00:1b:63:84:45:e6");
        assert_eq!(parse_address("00:1b:63:84:45:e6"), Some(ADDRESS));
        assert_eq!(parse_address(" 00-1B-63-84-45-E6 "), Some(ADDRESS));
        for wrong in [
            "",
            "00:1b:63:84:45",
            "00:1b:63:84:45:e6:00",
            "0:1b:63:84:45:e6",
            "zz:1b:63:84:45:e6",
        ] {
            assert_eq!(parse_address(wrong), None, "{wrong}");
        }
    }

    #[test]
    fn only_an_adapters_own_address_is_told_and_each_once() {
        let multicast = [0x01, 0x00, 0x5e, 0x00, 0x00, 0x01];
        assert!(!is_usable([0; 6]));
        assert!(!is_usable([0xFF; 6]));
        assert!(!is_usable(multicast));
        // A randomised (locally administered) address is an adapter's own.
        let private = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
        assert_eq!(
            normalized(&[ADDRESS, [0; 6], private, ADDRESS, multicast]),
            vec![ADDRESS, private]
        );
        let many: Vec<HardwareAddress> = (0..20).map(|n| [0x02, 0, 0, 0, 0, n]).collect();
        assert_eq!(normalized(&many).len(), MAX_WAKE_ADDRESSES);
    }

    #[test]
    fn packets_go_to_each_networks_broadcast_and_the_last_address_on_its_network() {
        let home = Network {
            address: Ipv4Addr::new(192, 168, 1, 10),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            broadcast: None,
        };
        assert_eq!(home.broadcast(), Some(Ipv4Addr::new(192, 168, 1, 255)));
        assert_eq!(
            home.targets(Some(Ipv4Addr::new(192, 168, 1, 20))),
            vec![
                Ipv4Addr::new(192, 168, 1, 255),
                Ipv4Addr::BROADCAST,
                Ipv4Addr::new(192, 168, 1, 20)
            ]
        );
        // The device was on another network: not through this one.
        assert_eq!(
            home.targets(Some(Ipv4Addr::new(10, 0, 0, 20))),
            vec![Ipv4Addr::new(192, 168, 1, 255), Ipv4Addr::BROADCAST]
        );
        // What the system says the broadcast address is wins.
        let odd = Network {
            broadcast: Some(Ipv4Addr::new(192, 168, 1, 127)),
            ..home
        };
        assert_eq!(odd.broadcast(), Some(Ipv4Addr::new(192, 168, 1, 127)));
        // A point-to-point link: only the limited broadcast.
        let vpn = Network {
            address: Ipv4Addr::new(10, 8, 0, 2),
            netmask: Ipv4Addr::new(255, 255, 255, 255),
            broadcast: None,
        };
        assert_eq!(vpn.targets(None), vec![Ipv4Addr::BROADCAST]);
    }

    /// Broadcasts for an address no adapter has, on the networks of the
    /// machine running the test: `cargo test -- --ignored a_round_goes_out`.
    #[test]
    #[ignore = "sends broadcasts on the local networks"]
    fn a_round_goes_out_on_every_network() {
        let sent = send(&[[0x02, 0, 0, 0, 0, 1]], None).expect("a network to send on");
        assert!(sent >= networks().len(), "{sent} packets");
    }

    #[test]
    fn nothing_is_sent_without_an_address() {
        assert_eq!(
            send(&[[0; 6]], None).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
