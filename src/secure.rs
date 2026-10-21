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
//! fingerprints. A device in the middle would have to show each side its own
//! certificate, and the two codes would then differ.

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

/// ALPN of the LinkCursor protocol over QUIC.
const ALPN: &[u8] = b"linkcursor/1";
/// The name a client asks for; certificates are not checked against it.
pub const SERVER_NAME: &str = "linkcursor";
/// A connection that hears nothing for this long is gone (sessions also
/// ping every second and give up after 8 s).
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE: Duration = Duration::from_secs(2);

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
    pub fn load_or_create(path: Option<&Path>, device_id: &str) -> Result<Self, String> {
        let Some(path) = path else {
            return Self::generate(device_id).map(|(identity, _)| identity);
        };
        if let Ok(pem) = std::fs::read(path) {
            match Self::from_pem(&pem) {
                Ok(identity) => return Ok(identity),
                Err(error) => {
                    return Err(format!(
                        "identity file {} is unreadable ({error}); remove it to create a new identity",
                        path.display()
                    ));
                }
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
            .push(rcgen::DnType::CommonName, format!("LinkCursor {device_id}"));
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
    pub fn server_config(&self) -> Result<quinn::ServerConfig, String> {
        let provider = provider();
        let mut crypto = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|error| error.to_string())?
            .with_client_cert_verifier(Arc::new(AnyCertificate::new(&provider)))
            .with_single_cert(vec![self.certificate.clone()], self.key.clone_key())
            .map_err(|error| error.to_string())?;
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = QuicServerConfig::try_from(crypto).map_err(|error| error.to_string())?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        config.transport_config(transport());
        Ok(config)
    }

    /// Dials; shows this device's certificate to the server.
    pub fn client_config(&self) -> Result<quinn::ClientConfig, String> {
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
        config.transport_config(transport());
        Ok(config)
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(IDLE_TIMEOUT.try_into().ok());
    transport.keep_alive_interval(Some(KEEP_ALIVE));
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

/// The six digits both devices show while pairing: the same on both sides,
/// whichever is which.
pub fn pairing_code(a: &str, b: &str) -> String {
    let (low, high) = if a <= b { (a, b) } else { (b, a) };
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(b"linkcursor pairing v1\0");
    context.update(low.as_bytes());
    context.update(b"\0");
    context.update(high.as_bytes());
    let digest = context.finish();
    let bytes = digest.as_ref();
    let value = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    format!("{:06}", value % 1_000_000)
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
            std::env::temp_dir().join(format!("linkcursor-identity-{}", std::process::id()));
        let path = directory.join("identity.pem");
        let _ = std::fs::remove_file(&path);
        let first = Identity::load_or_create(Some(&path), "device-a").unwrap();
        let again = Identity::load_or_create(Some(&path), "device-a").unwrap();
        assert_eq!(first.fingerprint, again.fingerprint);
        assert_eq!(first.fingerprint.len(), 64);
        let other = Identity::load_or_create(None, "device-b").unwrap();
        assert_ne!(first.fingerprint, other.fingerprint);
        assert!(first.server_config().is_ok() && first.client_config().is_ok());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn pairing_codes_agree_on_both_sides_and_differ_otherwise() {
        let code = pairing_code("aa", "bb");
        assert_eq!(code, pairing_code("bb", "aa"));
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert_ne!(code, pairing_code("aa", "cc"));
    }
}
