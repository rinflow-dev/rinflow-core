// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device identity and TLS for the QUIC transport.
//!
//! Every device has a long-lived key pair and a self-signed certificate. Both
//! sides of a connection present theirs (TLS 1.3 with client certificates),
//! and TLS proves each side holds the private key of the certificate it
//! shows. Nothing is checked against a certificate authority: a device is
//! known by its certificate's SHA-256 **fingerprint**, which the runtime
//! compares with the peers the user paired (see `runtime/trust.rs`).
//!
//! Pairing shows the same six-digit code on both devices, derived from both
//! fingerprints and a random value from each side. A device in the middle
//! has to show each side its own certificate, so it runs two pairings and
//! needs their codes to match. Codes from fingerprints alone let it search
//! offline for a certificate that does that. So the dialer commits to its
//! random value (`pairing_commitment`) before it sees the other side's, and
//! the other side sends its own before it learns the dialer's: nobody can
//! steer a code, and a device in the middle gets matching codes one time
//! in a million.

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::DigitallySignedStruct;
use rustls::DistinguishedName;
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// ALPN of the Rinflow protocol over QUIC.
const ALPN: &[u8] = b"rinflow/1";
/// The name a client asks for; certificates are not checked against it.
pub const SERVER_NAME: &str = "rinflow";
/// A connection that hears nothing for this long is gone (sessions also
/// ping every second and give up after 8 s).
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE: Duration = Duration::from_secs(2);
/// How much of its one stream a peer may send ahead of what this side read
/// (quinn's default per stream), and the most the whole connection may.
const RECEIVE_WINDOW: u32 = 1_250_000;
/// Acknowledgements of input (QUIC's ACK frequency extension, which both
/// sides of a Rinflow connection speak; a peer without it keeps its own
/// timing). By default a peer acknowledges every second packet, or after
/// 25 ms. Pointer motion is hundreds of small packets a second, and on Wi-Fi
/// every acknowledgement takes air time from the motion going the other way:
/// so one acknowledgement per this many packets. And the 25 ms are part of
/// how long the sender waits before it sends again the last packet of a
/// burst that got lost (a button release, the end of a movement), which no
/// later packet shows to be lost: so a shorter delay.
const INPUT_ACK_PACKETS: u32 = 10;
const INPUT_ACK_DELAY: Duration = Duration::from_millis(8);

/// This device's key pair and certificate.
pub struct Identity {
    certificate: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    /// SHA-256 of the certificate, lowercase hex.
    pub fingerprint: String,
}

impl Identity {
    /// The identity saved at `path`, created there on first use. Without a
    /// path a new one is made every start (tests, or shells that do not keep
    /// one; peers then see a new device each time).
    ///
    /// Only a file that does not exist is created. One that exists but
    /// cannot be read now (locked by another program, no permission, a disk
    /// error) is an error, never replaced: a new identity would make every
    /// paired peer refuse this device as changed.
    pub fn load_or_create(path: Option<&Path>, device_id: &str) -> Result<Self, String> {
        let Some(path) = path else {
            return Self::generate(device_id).map(|(identity, _)| identity);
        };
        match std::fs::read(path) {
            Ok(pem) => {
                return Self::from_pem(&pem).map_err(|error| {
                    format!(
                        "identity file {} is unreadable ({error}); remove it to create a new identity",
                        path.display()
                    )
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot read identity file {} ({error}); it is kept, not replaced",
                    path.display()
                ));
            }
        }
        let (identity, pem) = Self::generate(device_id)?;
        save_private(path, pem.as_bytes())
            .map_err(|error| format!("cannot save identity to {}: {error}", path.display()))?;
        Ok(identity)
    }

    /// A new identity, and its PEM (private key, then certificate).
    fn generate(device_id: &str) -> Result<(Self, String), String> {
        let key = rcgen::KeyPair::generate().map_err(|error| error.to_string())?;
        let mut params = rcgen::CertificateParams::new(vec![SERVER_NAME.to_owned()])
            .map_err(|error| error.to_string())?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, format!("Rinflow {device_id}"));
        let certificate = params
            .self_signed(&key)
            .map_err(|error| error.to_string())?;
        let pem = format!("{}{}", key.serialize_pem(), certificate.pem());
        let identity = Self::from_pem(pem.as_bytes())?;
        Ok((identity, pem))
    }

    fn from_pem(pem: &[u8]) -> Result<Self, String> {
        let certificate =
            CertificateDer::from_pem_slice(pem).map_err(|error| format!("certificate: {error}"))?;
        let key = PrivateKeyDer::from_pem_slice(pem).map_err(|error| format!("key: {error}"))?;
        Ok(Self {
            fingerprint: fingerprint(&certificate),
            certificate,
            key,
        })
    }

    /// Accepts connections; asks every client for its certificate.
    /// `interactive`: for the control endpoint (see [`transport`]).
    pub fn server_config(&self, interactive: bool) -> Result<quinn::ServerConfig, String> {
        self.server_with(transport(true, interactive))
    }

    /// Dials; shows this device's certificate to the server.
    pub fn client_config(&self, interactive: bool) -> Result<quinn::ClientConfig, String> {
        self.client_with(transport(false, interactive))
    }

    /// For the media endpoint (see `media.rs`): the side that accepts a
    /// media link sends the picture.
    pub fn media_server_config(&self) -> Result<quinn::ServerConfig, String> {
        self.server_with(media_transport(false))
    }

    /// For the media endpoint: the side that dials a media link watches.
    pub fn media_client_config(&self) -> Result<quinn::ClientConfig, String> {
        self.client_with(media_transport(true))
    }

    fn server_with(
        &self,
        transport: Arc<quinn::TransportConfig>,
    ) -> Result<quinn::ServerConfig, String> {
        let provider = provider();
        let mut crypto = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|error| error.to_string())?
            .with_client_cert_verifier(Arc::new(AnyCertificate::new(&provider)))
            .with_single_cert(vec![self.certificate.clone()], self.key.clone_key())
            .map_err(|error| error.to_string())?;
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        // The cipher is the client's choice unless this side has the
        // stronger reason (see [`provider`]).
        crypto.ignore_client_order = !HARDWARE_AES;
        let crypto = QuicServerConfig::try_from(crypto).map_err(|error| error.to_string())?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        config.transport_config(transport);
        Ok(config)
    }

    fn client_with(
        &self,
        transport: Arc<quinn::TransportConfig>,
    ) -> Result<quinn::ClientConfig, String> {
        let provider = provider();
        let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|error| error.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate::new(&provider)))
            .with_client_auth_cert(vec![self.certificate.clone()], self.key.clone_key())
            .map_err(|error| error.to_string())?;
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = QuicClientConfig::try_from(crypto).map_err(|error| error.to_string())?;
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));
        config.transport_config(transport);
        Ok(config)
    }
}

/// Whether this processor encrypts AES in hardware, as every PC, Mac and
/// phone of the last decade does. One that does not (a RISC-V or 32-bit ARM
/// board, such as a KVM) takes twice as long over AES-GCM as over
/// ChaCha20-Poly1305: measured on a 1 GHz RISC-V core sending 4 Mbit/s,
/// 23 % of the core against 13 %.
const HARDWARE_AES: bool = cfg!(any(
    target_arch = "x86_64",
    target_arch = "x86",
    target_arch = "aarch64"
));

/// rustls with ring. On a processor without hardware AES, ChaCha20-Poly1305
/// goes first: as the dialing side that is the order it asks in, which the
/// other side follows, and as the accepting side its own order decides
/// (`ignore_client_order`). On every other processor nothing changes.
fn provider() -> Arc<CryptoProvider> {
    let mut provider = rustls::crypto::ring::default_provider();
    if !HARDWARE_AES {
        provider.cipher_suites.sort_by_key(|suite| {
            suite.suite() != rustls::CipherSuite::TLS13_CHACHA20_POLY1305_SHA256
        });
    }
    Arc::new(provider)
}

/// How much of a picture a watching device takes ahead of what it read:
/// a key frame of a busy 1080p screen is a few hundred kilobytes.
const MEDIA_RECEIVE_WINDOW: u32 = 8 << 20;
/// Frames in flight at once on a media link. Each is a stream of its own.
const MEDIA_STREAMS: u32 = 16;

/// A media link (see `media.rs`): the dialer opens one bidirectional stream
/// to say who it is and to ask for key frames, and the accepting side sends
/// each frame on a unidirectional stream. Nothing else, either way.
fn media_transport(watching: bool) -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(IDLE_TIMEOUT.try_into().ok());
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(u32::from(!watching)));
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(if watching {
        MEDIA_STREAMS
    } else {
        0
    }));
    let window = if watching {
        MEDIA_RECEIVE_WINDOW
    } else {
        64 * 1024
    };
    transport.stream_receive_window(quinn::VarInt::from_u32(window));
    transport.receive_window(quinn::VarInt::from_u32(window));
    transport.datagram_receive_buffer_size(None);
    Arc::new(transport)
}

/// A connection carries one bidirectional stream, which the dialer opens,
/// and nothing else. QUIC buffers whatever a peer sends on streams it opens,
/// up to their windows, until the application reads it; quinn's defaults
/// let a peer open 200 streams with no limit for the connection as a whole,
/// and anyone can get this far: certificates are checked after the
/// handshake. So the peer may open only the one stream (`accepts_stream`:
/// the side that accepts connections), the connection gets no more window
/// than that stream, and there are no datagrams.
///
/// `interactive` (the control endpoint, which carries input): the peer is
/// asked to acknowledge less often and sooner (see [`INPUT_ACK_PACKETS`]).
fn transport(accepts_stream: bool, interactive: bool) -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(IDLE_TIMEOUT.try_into().ok());
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(u32::from(accepts_stream)));
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(0));
    transport.stream_receive_window(quinn::VarInt::from_u32(RECEIVE_WINDOW));
    transport.receive_window(quinn::VarInt::from_u32(RECEIVE_WINDOW));
    transport.datagram_receive_buffer_size(None);
    if interactive {
        let mut acks = quinn::AckFrequencyConfig::default();
        acks.ack_eliciting_threshold(quinn::VarInt::from_u32(INPUT_ACK_PACKETS));
        acks.max_ack_delay(Some(INPUT_ACK_DELAY));
        transport.ack_frequency_config(Some(acks));
    }
    Arc::new(transport)
}

/// SHA-256 of a certificate, lowercase hex.
pub fn fingerprint(certificate: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, certificate);
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The fingerprint of the certificate the peer of `connection` presented.
pub fn peer_fingerprint(connection: &quinn::Connection) -> Option<String> {
    let identity = connection.peer_identity()?;
    let certificates = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certificates
        .first()
        .map(|certificate| fingerprint(certificate))
}

/// One side's random part of a pairing code. `None` only if the system has
/// no randomness to give.
pub fn pairing_nonce() -> Option<[u8; 32]> {
    use ring::rand::SecureRandom;
    let mut nonce = [0; 32];
    ring::rand::SystemRandom::new().fill(&mut nonce).ok()?;
    Some(nonce)
}

/// What the dialer of a connection sends first while pairing: a hash of its
/// nonce and both fingerprints (`dialer`'s, then `acceptor`'s), which it
/// cannot change once the other side has answered.
pub fn pairing_commitment(nonce: &[u8; 32], dialer: &str, acceptor: &str) -> [u8; 32] {
    let digest = pairing_digest(
        b"rinflow pairing commitment v2\0",
        dialer,
        acceptor,
        &[nonce],
    );
    digest.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

/// The six digits both devices show while pairing (0 to 999999): from both
/// fingerprints and both nonces, each in the role its device has on the
/// connection.
pub fn pairing_code(
    dialer: &str,
    acceptor: &str,
    dialer_nonce: &[u8; 32],
    acceptor_nonce: &[u8; 32],
) -> u32 {
    let digest = pairing_digest(
        b"rinflow pairing code v2\0",
        dialer,
        acceptor,
        &[dialer_nonce, acceptor_nonce],
    );
    let bytes = digest.as_ref();
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % 1_000_000
}

fn pairing_digest(
    label: &[u8],
    dialer: &str,
    acceptor: &str,
    nonces: &[&[u8; 32]],
) -> ring::digest::Digest {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(label);
    // Fingerprints are 64 hex digits; the separator keeps them apart anyway.
    context.update(dialer.as_bytes());
    context.update(b"\0");
    context.update(acceptor.as_bytes());
    context.update(b"\0");
    for nonce in nonces {
        context.update(&nonce[..]);
    }
    context.finish()
}

/// Accepts any certificate whose holder proves its key; who the holder is
/// gets decided by its fingerprint afterwards.
#[derive(Debug)]
struct AnyCertificate {
    algorithms: WebPkiSupportedAlgorithms,
}

impl AnyCertificate {
    fn new(provider: &CryptoProvider) -> Self {
        Self {
            algorithms: provider.signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, certificate, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, certificate, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for AnyCertificate {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, certificate, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, certificate, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Writes a file only this user can read (on Unix; a per-user folder on
/// Windows already is), through a temporary file and a rename.
fn save_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        std::io::Write::write_all(&mut file, contents)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_persist_and_fingerprints_are_stable() {
        let directory =
            std::env::temp_dir().join(format!("rinflow-identity-{}", std::process::id()));
        let path = directory.join("identity.pem");
        let _ = std::fs::remove_file(&path);
        let first = Identity::load_or_create(Some(&path), "device-a").unwrap();
        let again = Identity::load_or_create(Some(&path), "device-a").unwrap();
        assert_eq!(first.fingerprint, again.fingerprint);
        assert_eq!(first.fingerprint.len(), 64);
        let other = Identity::load_or_create(None, "device-b").unwrap();
        assert_ne!(first.fingerprint, other.fingerprint);
        assert!(first.server_config(true).is_ok() && first.client_config(false).is_ok());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_identity_that_exists_but_cannot_be_read_is_not_replaced() {
        // A directory where the file should be: it exists, and reading it
        // fails with something other than "not found" (as a locked file or
        // one without permission would).
        let path = std::env::temp_dir().join(format!(
            "rinflow-identity-unreadable-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        let error = Identity::load_or_create(Some(&path), "device-a")
            .err()
            .expect("an unreadable identity must not be replaced");
        assert!(error.contains("kept, not replaced"), "{error}");
        assert!(path.is_dir());
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn pairing_codes_depend_on_both_certificates_and_both_nonces() {
        assert_ne!(pairing_nonce().unwrap(), pairing_nonce().unwrap());
        let (dialer, acceptor) = ("aa".repeat(32), "bb".repeat(32));
        let (mine, theirs) = ([1; 32], [2; 32]);
        let code = pairing_code(&dialer, &acceptor, &mine, &theirs);
        assert!(code < 1_000_000);
        assert_eq!(code, pairing_code(&dialer, &acceptor, &mine, &theirs));
        // Roles count: the same values the other way round give another
        // code (each side computes it with the roles of the connection).
        assert_ne!(code, pairing_code(&acceptor, &dialer, &mine, &theirs));
        assert_ne!(code, pairing_code(&dialer, &acceptor, &theirs, &mine));
        assert_ne!(
            code,
            pairing_code(&dialer, &"cc".repeat(32), &mine, &theirs)
        );
        assert_ne!(code, pairing_code(&dialer, &acceptor, &mine, &[3; 32]));
    }

    #[test]
    fn a_commitment_binds_the_nonce_and_both_certificates() {
        let (dialer, acceptor) = ("aa".repeat(32), "bb".repeat(32));
        let nonce = pairing_nonce().unwrap();
        let commitment = pairing_commitment(&nonce, &dialer, &acceptor);
        assert_eq!(commitment, pairing_commitment(&nonce, &dialer, &acceptor));
        let mut other = nonce;
        other[0] ^= 1;
        assert_ne!(commitment, pairing_commitment(&other, &dialer, &acceptor));
        assert_ne!(commitment, pairing_commitment(&nonce, &acceptor, &dialer));
        assert_ne!(
            commitment,
            pairing_commitment(&nonce, &dialer, &"cc".repeat(32))
        );
    }
}
