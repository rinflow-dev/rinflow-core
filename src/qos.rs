// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Network priority per endpoint.
//!
//! Input is a trickle of tiny packets that must arrive at once; files are a
//! flood that only needs to finish eventually. On Wi-Fi both otherwise wait
//! in the same queues, in the network card and in the access point, so a
//! large copy makes the pointer stutter. So a core has two QUIC endpoints,
//! each its own UDP socket (see `quic.rs`): control connections on one,
//! tagged interactive, and data links on the other, tagged bulk. The
//! operating system then queues them apart (WMM access categories on Wi-Fi)
//! and marks their packets (DSCP), so access points that honour WMM keep
//! them apart on their side too.
//!
//! - macOS and iOS: `SO_NET_SERVICE_TYPE`, voice for input and background
//!   for bulk. That sets the Wi-Fi queue and DSCP together.
//! - Linux and Android: DSCP through `IP_TOS` (EF / CS1) and the queue
//!   through `SO_PRIORITY`.
//! - Windows: qWAVE flows (`QOSAddSocketToFlow`), voice or background.
//!   Windows ignores `IP_TOS` set by applications, and qWAVE takes an
//!   unconnected socket only per destination, so each peer the endpoint
//!   talks to is added ([`Qos::add_destination`]).
//!
//! All of it is best effort: a socket that cannot be tagged works as before.

use std::net::SocketAddr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrafficClass {
    /// Input and control messages: tiny, latency-critical.
    Interactive,
    /// Clipboards and files: large, latency-tolerant.
    Bulk,
}

/// The class of one UDP socket. The socket must stay open as long as this
/// value lives.
pub struct Qos {
    #[cfg(unix)]
    raw: std::os::fd::RawFd,
    #[cfg(windows)]
    raw: std::os::windows::io::RawSocket,
    #[cfg_attr(not(windows), allow(dead_code))]
    class: TrafficClass,
    /// Windows: the qWAVE flow of each destination added so far.
    #[cfg(windows)]
    destinations: std::sync::Mutex<Vec<(SocketAddr, u32)>>,
}

impl Qos {
    /// Tags everything `socket` sends with `class` (on Windows, once its
    /// destinations are added).
    pub fn udp(socket: &std::net::UdpSocket, class: TrafficClass) -> Self {
        let qos = Self {
            #[cfg(unix)]
            raw: std::os::fd::AsRawFd::as_raw_fd(socket),
            #[cfg(windows)]
            raw: std::os::windows::io::AsRawSocket::as_raw_socket(socket),
            class,
            #[cfg(windows)]
            destinations: std::sync::Mutex::new(Vec::new()),
        };
        #[cfg(not(windows))]
        platform::set(&qos);
        qos
    }

    /// The socket now also sends to `address`. Only Windows needs to know.
    pub fn add_destination(&self, address: SocketAddr) {
        #[cfg(windows)]
        platform::add_destination(self, address);
        #[cfg(not(windows))]
        let _ = address;
    }
}

#[cfg(windows)]
impl Drop for Qos {
    fn drop(&mut self) {
        platform::remove_flows(self);
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    use super::{Qos, TrafficClass};

    // <sys/socket.h>
    pub const SO_NET_SERVICE_TYPE: libc::c_int = 0x1116;
    pub const NET_SERVICE_TYPE_BK: libc::c_int = 1;
    const NET_SERVICE_TYPE_VO: libc::c_int = 4;

    pub fn set(qos: &Qos) {
        let value = match qos.class {
            TrafficClass::Interactive => NET_SERVICE_TYPE_VO,
            TrafficClass::Bulk => NET_SERVICE_TYPE_BK,
        };
        // SAFETY: an int option on a socket the endpoint keeps open.
        unsafe {
            libc::setsockopt(
                qos.raw,
                libc::SOL_SOCKET,
                SO_NET_SERVICE_TYPE,
                (&value as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform {
    use super::{Qos, TrafficClass};

    pub fn set(qos: &Qos) {
        // DSCP EF (46) or CS1 (8), in the upper six bits of the TOS byte;
        // queue priority 6 or 1 (Wi-Fi drivers map them to voice and
        // background).
        let (tos, priority): (libc::c_int, libc::c_int) = match qos.class {
            TrafficClass::Interactive => (46 << 2, 6),
            TrafficClass::Bulk => (8 << 2, 1),
        };
        // SAFETY: int options on a socket the endpoint keeps open.
        unsafe {
            libc::setsockopt(
                qos.raw,
                libc::IPPROTO_IP,
                libc::IP_TOS,
                (&tos as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
            libc::setsockopt(
                qos.raw,
                libc::SOL_SOCKET,
                libc::SO_PRIORITY,
                (&priority as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{Qos, TrafficClass};
    use std::ptr::null_mut;
    use std::sync::OnceLock;
    use windows_sys::Win32::NetworkManagement::QoS::{
        QOS_NON_ADAPTIVE_FLOW, QOS_VERSION, QOSAddSocketToFlow, QOSCreateHandle,
        QOSRemoveSocketFromFlow, QOSTrafficTypeBackground, QOSTrafficTypeVoice,
    };
    use windows_sys::Win32::Networking::WinSock::SOCKET;

    /// One qWAVE handle for the process; None if qWAVE is unavailable.
    fn handle() -> Option<usize> {
        static HANDLE: OnceLock<Option<usize>> = OnceLock::new();
        *HANDLE.get_or_init(|| {
            let version = QOS_VERSION {
                MajorVersion: 1,
                MinorVersion: 0,
            };
            let mut handle = null_mut();
            // SAFETY: creates a qWAVE handle kept for the process's life.
            let created = unsafe { QOSCreateHandle(&version, &mut handle) } != 0;
            created.then_some(handle as usize)
        })
    }

    /// Adds the socket's flow to `address`. `QOS_NON_ADAPTIVE_FLOW` is
    /// required: without it qWAVE refuses (ERROR_NOT_SUPPORTED).
    pub fn add_destination(qos: &Qos, address: std::net::SocketAddr) {
        let (Some(handle), Ok(mut destinations)) = (handle(), qos.destinations.lock()) else {
            return;
        };
        if destinations.iter().any(|(known, _)| *known == address) {
            return;
        }
        let traffic = match qos.class {
            TrafficClass::Interactive => QOSTrafficTypeVoice,
            TrafficClass::Bulk => QOSTrafficTypeBackground,
        };
        let target = socket2::SockAddr::from(address);
        let mut id = 0;
        // SAFETY: `target` is a valid socket address for the call's duration.
        let added = unsafe {
            QOSAddSocketToFlow(
                handle as _,
                qos.raw as SOCKET,
                target.as_ptr().cast(),
                traffic,
                QOS_NON_ADAPTIVE_FLOW,
                &mut id,
            )
        } != 0;
        if added {
            destinations.push((address, id));
        } else {
            // Addresses of this machine are never taken (no network adapter
            // to tag on: ERROR_NOT_FOUND); said once, for diagnosing real ones.
            static REPORTED: std::sync::Once = std::sync::Once::new();
            let error = std::io::Error::last_os_error();
            REPORTED.call_once(|| {
                eprintln!("linkcursor-core: no network priority for a peer: {error}");
            });
        }
    }

    pub fn remove_flows(qos: &Qos) {
        let (Some(handle), Ok(mut destinations)) = (handle(), qos.destinations.lock()) else {
            return;
        };
        for (_, id) in destinations.drain(..) {
            // SAFETY: removes the socket from a flow it was added to; harmless
            // if the socket is already closed.
            unsafe {
                QOSRemoveSocketFromFlow(handle as _, qos.raw as SOCKET, id, 0);
            }
        }
    }
}

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    windows
)))]
mod platform {
    use super::Qos;

    pub fn set(_qos: &Qos) {}
}

#[cfg(test)]
mod tests {
    use super::{Qos, TrafficClass};

    #[test]
    fn sockets_carry_their_class() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let qos = Qos::udp(&socket, TrafficClass::Bulk);
        // Windows: qWAVE never takes this machine's own addresses, so here
        // it may only fail quietly.
        qos.add_destination("127.0.0.1:9".parse().unwrap());

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::fd::AsRawFd;
            let mut value: libc::c_int = 0;
            let mut length = size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: reads an int option of an open socket.
            unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::IPPROTO_IP,
                    libc::IP_TOS,
                    (&mut value as *mut libc::c_int).cast(),
                    &mut length,
                );
            }
            // Background is CS1 (DSCP 8).
            assert_eq!(value >> 2, 8);
        }
        #[cfg(target_vendor = "apple")]
        {
            use std::os::fd::AsRawFd;
            let mut value: libc::c_int = 0;
            let mut length = size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: reads an int option of an open socket.
            unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    super::platform::SO_NET_SERVICE_TYPE,
                    (&mut value as *mut libc::c_int).cast(),
                    &mut length,
                );
            }
            assert_eq!(value, super::platform::NET_SERVICE_TYPE_BK);
        }
    }
}
