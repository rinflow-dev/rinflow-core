// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The discovery socket. Announcements go out of every IPv4 interface and
//! the group is joined on each, not only on the one the system routes the
//! group to: a Mac on Ethernet and Wi-Fi with a VPN up has three default
//! routes, and which one the system picks changes as the network does.
//!
//! The socket is made anew when a round of announcements fails everywhere,
//! when receiving fails, and after a network change: one that went bad
//! (seen on macOS after the network changed: nothing in or out, every
//! send failing) is never trusted to recover. Each time is reported as
//! `discovery_reset` with what failed, for the logs.

use socket2::{Domain, Protocol, Socket, Type};
use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// What the receiving task hands the runtime.
pub(super) enum Received {
    Datagram(Vec<u8>, SocketAddr),
    /// Receiving failed; the socket is made anew.
    Failed(io::Error),
}

/// One IPv4 interface to announce on: its address, and its subnet's
/// broadcast address if it has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Interface {
    address: Ipv4Addr,
    broadcast: Option<Ipv4Addr>,
}

pub(super) struct Discovery {
    port: u16,
    group: Ipv4Addr,
    socket: Arc<UdpSocket>,
    reader: JoinHandle<()>,
    received: mpsc::UnboundedSender<Received>,
    /// Interfaces the group is joined on.
    joined: BTreeSet<Interface>,
}

impl Discovery {
    /// Binds the discovery port; datagrams come on the returned channel.
    pub(super) fn bind(
        port: u16,
        group: Ipv4Addr,
    ) -> io::Result<(Self, mpsc::UnboundedReceiver<Received>)> {
        let (received, rx) = mpsc::unbounded_channel();
        let socket = Arc::new(create_socket(port)?);
        let reader = spawn_reader(socket.clone(), received.clone());
        let mut discovery = Self {
            port,
            group,
            socket,
            reader,
            received,
            joined: BTreeSet::new(),
        };
        discovery.refresh();
        Ok((discovery, rx))
    }

    /// Joins the group on interfaces that came, forgets those that went.
    /// Not fatal: without a network yet (a PC that starts Rinflow at sign-in
    /// before Wi-Fi is up) there is nothing to join, and this runs again.
    pub(super) fn refresh(&mut self) {
        let current = interfaces();
        for interface in self
            .joined
            .difference(&current)
            .copied()
            .collect::<Vec<_>>()
        {
            let _ = self
                .socket
                .leave_multicast_v4(self.group, interface.address);
            self.joined.remove(&interface);
        }
        for interface in current {
            if self.joined.contains(&interface) {
                continue;
            }
            match self.socket.join_multicast_v4(self.group, interface.address) {
                // Already a member there.
                Err(error) if error.kind() != io::ErrorKind::AddrInUse => {}
                // Interfaces without multicast (many VPNs) refuse; their
                // subnet broadcast still goes out.
                _ => {
                    self.joined.insert(interface);
                }
            }
        }
    }

    /// A new socket on the same port, joined afresh. On failure the old one
    /// stays, to be replaced at the next try.
    pub(super) fn reset(&mut self, sink: &super::EventSink, why: &str, error: Option<&io::Error>) {
        match create_socket(self.port) {
            Ok(socket) => {
                self.reader.abort();
                self.socket = Arc::new(socket);
                self.reader = spawn_reader(self.socket.clone(), self.received.clone());
                self.joined.clear();
                self.refresh();
            }
            Err(failure) => {
                return sink.emit_value(serde_json::json!({
                    "event": "discovery_reset", "reason": why,
                    "error": error.map(ToString::to_string), "failed": failure.to_string(),
                }));
            }
        }
        sink.emit_value(serde_json::json!({
            "event": "discovery_reset", "reason": why, "error": error.map(ToString::to_string),
        }));
    }

    /// One announcement out of every interface: to the group, and to the
    /// subnet's broadcast for networks that suppress multicast (receivers
    /// deduplicate by device ID). The error if not one send got out.
    pub(super) async fn announce(&self, datagram: &[u8]) -> Result<(), io::Error> {
        let group = SocketAddr::V4(SocketAddrV4::new(self.group, self.port));
        let mut last_error = None;
        let mut sent = false;
        let mut outcome = |result: io::Result<usize>| match result {
            Ok(_) => sent = true,
            Err(error) => last_error = Some(error),
        };
        for interface in &self.joined {
            if let Err(error) = self.socket_ref().set_multicast_if_v4(&interface.address) {
                outcome(Err(error));
                continue;
            }
            outcome(self.socket.send_to(datagram, group).await);
        }
        let broadcasts: BTreeSet<Ipv4Addr> = interfaces()
            .iter()
            .filter_map(|interface| interface.broadcast)
            .collect();
        for broadcast in broadcasts {
            outcome(
                self.socket
                    .send_to(
                        datagram,
                        SocketAddr::V4(SocketAddrV4::new(broadcast, self.port)),
                    )
                    .await,
            );
        }
        // With no interface known (no network, or none listed), the system
        // picks: the limited broadcast.
        if self.joined.is_empty() {
            outcome(
                self.socket
                    .send_to(
                        datagram,
                        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, self.port)),
                    )
                    .await,
            );
        }
        match (sent, last_error) {
            (false, Some(error)) => Err(error),
            _ => Ok(()),
        }
    }

    /// A probe or an answer to one address (`manual.rs`).
    pub(super) async fn send_to(&self, datagram: &[u8], target: SocketAddr) {
        let _ = self.socket.send_to(datagram, target).await;
    }

    fn socket_ref(&self) -> socket2::SockRef<'_> {
        socket2::SockRef::from(self.socket.as_ref())
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

fn spawn_reader(
    socket: Arc<UdpSocket>,
    received: mpsc::UnboundedSender<Received>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            match socket.recv_from(&mut buffer).await {
                Ok((length, source)) => {
                    if received
                        .send(Received::Datagram(buffer[..length].to_vec(), source))
                        .is_err()
                    {
                        return;
                    }
                }
                // A datagram larger than the buffer, or an ICMP error from
                // an earlier send (Windows reports those here): not the
                // socket's fault.
                Err(error) if is_transient(&error) => {}
                Err(error) => {
                    let _ = received.send(Received::Failed(error));
                    return;
                }
            }
        }
    })
}

fn is_transient(error: &io::Error) -> bool {
    // Unix cuts an oversized datagram short; Windows reports WSAEMSGSIZE.
    const WSAEMSGSIZE: i32 = 10040;
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::Interrupted
    ) || (cfg!(windows) && error.raw_os_error() == Some(WSAEMSGSIZE))
}

fn create_socket(port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_broadcast(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into())
}

/// The IPv4 interfaces to announce on: up, not loopback.
fn interfaces() -> BTreeSet<Interface> {
    let Ok(list) = if_addrs::get_if_addrs() else {
        return BTreeSet::new();
    };
    list.into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(v4) => Some(Interface {
                address: v4.ip,
                broadcast: v4.broadcast,
            }),
            if_addrs::IfAddr::V6(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    fn free_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn next_datagram(rx: &mut mpsc::UnboundedReceiver<Received>, what: &str) -> Vec<u8> {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(Received::Datagram(datagram, _))) => datagram,
            _ => panic!("no datagram {what}"),
        }
    }

    #[tokio::test]
    async fn a_new_socket_takes_over_and_says_why() {
        let port = free_port();
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        let sink =
            super::super::EventSink::new(move |event| seen.lock().unwrap().push(event), |_| {});
        let (mut discovery, mut rx) =
            Discovery::bind(port, Ipv4Addr::new(239, 255, 42, 98)).unwrap();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();

        sender.send_to(b"one", ("127.0.0.1", port)).unwrap();
        assert_eq!(next_datagram(&mut rx, "before").await, b"one");

        discovery.reset(&sink, "send_failed", Some(&io::Error::other("test")));
        // The old socket closes once its aborted reader is dropped; until
        // then a datagram may go to it (an announcement lost, sent again).
        tokio::time::sleep(Duration::from_millis(50)).await;
        sender.send_to(b"two", ("127.0.0.1", port)).unwrap();
        assert_eq!(next_datagram(&mut rx, "after the reset").await, b"two");

        let events = events.lock().unwrap();
        let reset: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(reset["event"], "discovery_reset");
        assert_eq!(reset["reason"], "send_failed");
        assert_eq!(reset["error"], "test");
    }
}
