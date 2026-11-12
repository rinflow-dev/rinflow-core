// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The QUIC endpoints of a core.
//!
//! Two UDP sockets, each its own endpoint, so the network can queue them
//! apart (see `qos.rs`): control on the control port, interactive; bulk on a
//! port of the system's choosing, for data links, which a session's
//! `DataInvite` names. Both accept connections and dial with this device's
//! certificate (see `secure.rs`).
//!
//! A device that sends or watches pictures (`CoreConfig::media`,
//! `media_source`) has a third, also on a port of the system's choosing,
//! which a `MediaOffer` names (see `media.rs`).

use crate::qos::{Qos, TrafficClass};
use crate::secure::Identity;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

#[derive(Clone)]
pub struct QuicEndpoints {
    pub control: quinn::Endpoint,
    pub bulk: quinn::Endpoint,
    pub bulk_port: u16,
    /// The media endpoint, its port and its traffic class.
    pub media: Option<(quinn::Endpoint, u16, Arc<Qos>)>,
    control_qos: Arc<Qos>,
    bulk_qos: Arc<Qos>,
}

impl QuicEndpoints {
    /// Binds the endpoints. Must run inside the core's Tokio runtime.
    pub fn bind(control_port: u16, identity: &Identity, media: bool) -> Result<Self, String> {
        let (control, control_qos) = endpoint(control_port, identity, TrafficClass::Interactive)?;
        let (bulk, bulk_qos) = endpoint(0, identity, TrafficClass::Bulk)?;
        let bulk_port = bulk.local_addr().map_err(|error| error.to_string())?.port();
        let media = if media {
            let (endpoint, port, qos) = media_endpoint(identity)?;
            Some((endpoint, port, Arc::new(qos)))
        } else {
            None
        };
        Ok(Self {
            control,
            bulk,
            bulk_port,
            media,
            control_qos: Arc::new(control_qos),
            bulk_qos: Arc::new(bulk_qos),
        })
    }

    /// An endpoint for dialing: the bulk one for data links.
    pub fn dialer(&self, bulk: bool) -> quinn::Endpoint {
        if bulk {
            self.bulk.clone()
        } else {
            self.control.clone()
        }
    }

    /// A connection to `address` started on one endpoint (Windows tags
    /// traffic per destination).
    pub fn note_peer(&self, address: SocketAddr, bulk: bool) {
        if bulk {
            self.bulk_qos.add_destination(address);
        } else {
            self.control_qos.add_destination(address);
        }
    }

    pub fn close(&self) {
        self.control.close(0u32.into(), b"core stopped");
        self.bulk.close(0u32.into(), b"core stopped");
        if let Some((media, _, _)) = &self.media {
            media.close(0u32.into(), b"core stopped");
        }
    }
}

fn media_endpoint(identity: &Identity) -> Result<(quinn::Endpoint, u16, Qos), String> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|error| format!("UDP port for media: {error}"))?;
    let port = socket
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let qos = Qos::udp(&socket, TrafficClass::Video);
    let runtime = quinn::default_runtime().ok_or("no async runtime for QUIC")?;
    let mut endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(identity.media_server_config()?),
        socket,
        runtime,
    )
    .map_err(|error| error.to_string())?;
    endpoint.set_default_client_config(identity.media_client_config()?);
    Ok((endpoint, port, qos))
}

fn endpoint(
    port: u16,
    identity: &Identity,
    class: TrafficClass,
) -> Result<(quinn::Endpoint, Qos), String> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
        .map_err(|error| format!("UDP port {port}: {error}"))?;
    let qos = Qos::udp(&socket, class);
    let interactive = class == TrafficClass::Interactive;
    let runtime = quinn::default_runtime().ok_or("no async runtime for QUIC")?;
    let mut endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(identity.server_config(interactive)?),
        socket,
        runtime,
    )
    .map_err(|error| error.to_string())?;
    endpoint.set_default_client_config(identity.client_config(interactive)?);
    Ok((endpoint, qos))
}
