//! Network priority per connection.
//!
//! Input is a trickle of tiny packets that must arrive at once; files are a
//! flood that only needs to finish eventually. On Wi-Fi both otherwise wait
//! in the same queues, in the network card and in the access point, so a
//! large copy makes the pointer stutter. Each connection is tagged with what
//! it carries: control connections as interactive, data links as bulk. The
//! operating system then queues them apart (WMM access categories on Wi-Fi)
//! and marks their packets (DSCP), so access points that honour WMM keep
//! them apart on their side too.
//!
//! - macOS and iOS: `SO_NET_SERVICE_TYPE`, voice for input and background
//!   for bulk. That sets the Wi-Fi queue and DSCP together, and background
//!   sockets also yield in congestion control.
//! - Linux and Android: DSCP through `IP_TOS` (EF / CS1) and the queue
//!   through `SO_PRIORITY`.
//! - Windows: qWAVE flows (`QOSAddSocketToFlow`), voice or background.
//!   Windows ignores `IP_TOS` set by applications.
//!
//! All of it is best effort: a connection that cannot be tagged works as
//! before.
//!
//! QUIC connections share their endpoint's UDP socket, so there the socket
//! carries the class: one endpoint for control, one for bulk (see
//! `quic.rs`). On Windows, qWAVE takes an unconnected socket only per
//! destination, so each peer the endpoint talks to is added.

use std::net::SocketAddr;
use tokio::net::TcpStream;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrafficClass {
    /// Input and control messages: tiny, latency-critical.
    Interactive,
    /// Clipboards and files: large, latency-tolerant.
    Bulk,
}

/// Sets a connection's class, also after its stream moved into its tasks.
/// The connection must still be open when [`Qos::set`] runs; an
/// `ActiveConnection` keeps it open until it is aborted.
pub struct Qos {
    #[cfg(unix)]
    raw: std::os::fd::RawFd,
    #[cfg(windows)]
    raw: std::os::windows::io::RawSocket,
    #[cfg(windows)]
    flow: std::sync::Mutex<Option<u32>>,
    /// An unconnected socket's class and, on Windows, its flow per
    /// destination.
    #[cfg_attr(not(windows), allow(dead_code))]
    udp_class: Option<TrafficClass>,
    #[cfg_attr(not(windows), allow(dead_code))]
    destinations: std::sync::Mutex<Vec<(SocketAddr, u32)>>,
}

impl Qos {
    pub fn new(stream: &TcpStream) -> Self {
        Self {
            #[cfg(unix)]
            raw: std::os::fd::AsRawFd::as_raw_fd(stream),
            #[cfg(windows)]
            raw: std::os::windows::io::AsRawSocket::as_raw_socket(stream),
            #[cfg(windows)]
            flow: std::sync::Mutex::new(None),
            udp_class: None,
            destinations: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// A UDP socket that sends everything in `class`. The socket must stay
    /// open as long as this value lives.
    pub fn udp(socket: &std::net::UdpSocket, class: TrafficClass) -> Self {
        let qos = Self {
            #[cfg(unix)]
            raw: std::os::fd::AsRawFd::as_raw_fd(socket),
            #[cfg(windows)]
            raw: std::os::windows::io::AsRawSocket::as_raw_socket(socket),
            #[cfg(windows)]
            flow: std::sync::Mutex::new(None),
            udp_class: Some(class),
            destinations: std::sync::Mutex::new(Vec::new()),
        };
        // The socket option covers every destination, except on Windows.
        #[cfg(not(windows))]
        platform::set(&qos, class);
        qos
    }

    pub fn set(&self, class: TrafficClass) {
        platform::set(self, class);
    }

    /// A UDP socket now also sends to `address` (Windows needs to know).
    pub fn add_destination(&self, address: SocketAddr) {
        #[cfg(windows)]
        if let Some(class) = self.udp_class {
            platform::add_destination(self, class, address);
        }
        #[cfg(not(windows))]
        let _ = address;
    }
}

#[cfg(windows)]
impl Drop for Qos {
    fn drop(&mut self) {
        platform::remove_flow(self);
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    use super::{Qos, TrafficClass};

    // <sys/socket.h>
    const SO_NET_SERVICE_TYPE: libc::c_int = 0x1116;
    const NET_SERVICE_TYPE_BK: libc::c_int = 1;
    const NET_SERVICE_TYPE_VO: libc::c_int = 4;

    pub fn set(qos: &Qos, class: TrafficClass) {
        let value = match class {
            TrafficClass::Interactive => NET_SERVICE_TYPE_VO,
            TrafficClass::Bulk => NET_SERVICE_TYPE_BK,
        };
        // SAFETY: an int option on a socket the connection keeps open.
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

    pub fn set(qos: &Qos, class: TrafficClass) {
        // DSCP EF (46) or CS1 (8), in the upper six bits of the TOS byte;
        // queue priority 6 or 1 (Wi-Fi drivers map them to voice and
        // background).
        let (tos, priority): (libc::c_int, libc::c_int) = match class {
            TrafficClass::Interactive => (46 << 2, 6),
            TrafficClass::Bulk => (8 << 2, 1),
        };
        // SAFETY: int options on a socket the connection keeps open.
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
    use std::ptr::{null, null_mut};
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

    pub fn set(qos: &Qos, class: TrafficClass) {
        let Some(handle) = handle() else {
            return;
        };
        let Ok(mut flow) = qos.flow.lock() else {
            return;
        };
        let socket = qos.raw as SOCKET;
        if let Some(id) = flow.take() {
            // SAFETY: removes this socket from the flow it was added to.
            unsafe {
                QOSRemoveSocketFromFlow(handle as _, socket, id, 0);
            }
        }
        let traffic = match class {
            TrafficClass::Interactive => QOSTrafficTypeVoice,
            TrafficClass::Bulk => QOSTrafficTypeBackground,
        };
        let mut id = 0;
        // SAFETY: adds the connected socket (no destination needed) to a new
        // flow of this traffic type.
        let added = unsafe {
            QOSAddSocketToFlow(
                handle as _,
                socket,
                null(),
                traffic,
                QOS_NON_ADAPTIVE_FLOW,
                &mut id,
            )
        } != 0;
        if added {
            *flow = Some(id);
        } else {
            // Loopback connections are never taken (no network adapter to
            // tag on); said once, for diagnosing real ones.
            static REPORTED: std::sync::Once = std::sync::Once::new();
            let error = std::io::Error::last_os_error();
            REPORTED.call_once(|| {
                eprintln!("linkcursor-core: no network priority for a connection: {error}");
            });
        }
    }

    /// An unconnected socket joins a flow per destination.
    pub fn add_destination(qos: &Qos, class: TrafficClass, address: std::net::SocketAddr) {
        let (Some(handle), Ok(mut destinations)) = (handle(), qos.destinations.lock()) else {
            return;
        };
        if destinations.iter().any(|(known, _)| *known == address) {
            return;
        }
        let traffic = match class {
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
            static REPORTED: std::sync::Once = std::sync::Once::new();
            let error = std::io::Error::last_os_error();
            REPORTED.call_once(|| {
                eprintln!("linkcursor-core: no network priority for a QUIC peer: {error}");
            });
        }
    }

    pub fn remove_flow(qos: &Qos) {
        let Some(handle) = handle() else {
            return;
        };
        let mut ids: Vec<u32> = Vec::new();
        if let Ok(mut flow) = qos.flow.lock() {
            ids.extend(flow.take());
        }
        if let Ok(mut destinations) = qos.destinations.lock() {
            ids.extend(destinations.drain(..).map(|(_, id)| id));
        }
        for id in ids {
            // SAFETY: as in `set`; harmless if the socket is already closed.
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
    use super::{Qos, TrafficClass};

    pub fn set(_qos: &Qos, _class: TrafficClass) {}
}

#[cfg(test)]
mod tests {
    use super::{Qos, TrafficClass};

    #[tokio::test]
    async fn connections_can_change_class() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) =
            tokio::join!(tokio::net::TcpStream::connect(address), listener.accept());
        let client = client.unwrap();
        let (_server, _) = accepted.unwrap();

        let qos = Qos::new(&client);
        qos.set(TrafficClass::Interactive);
        qos.set(TrafficClass::Bulk);

        // Windows: qWAVE takes only connections over a network adapter, never
        // loopback (ERROR_NOT_FOUND), so here it may only fail quietly.
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        {
            use std::os::fd::AsRawFd;
            let mut value: libc::c_int = 0;
            let mut length = size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: reads an int option of an open socket.
            unsafe {
                libc::getsockopt(
                    client.as_raw_fd(),
                    libc::IPPROTO_IP,
                    libc::IP_TOS,
                    (&mut value as *mut libc::c_int).cast(),
                    &mut length,
                );
            }
            // Background is CS1 (DSCP 8) on every one of these systems.
            assert_eq!(value >> 2, 8);
        }
    }
}
