//! Dev TLS for the QUIC endpoint — self-signed certs and (for now) skipped
//! peer verification.
//!
//! # TODO(M7): certificate pinning is NOT implemented here.
//!
//! PROTOCOL.md §2 requires mutual TLS with the pinning rules of PAIRING.md:
//! "Except during an explicit pairing handshake, a peer presenting an unpinned
//! certificate MUST be rejected." Until M7 (SPAKE2 + pin stores) lands, this
//! module generates a throwaway self-signed identity and **accepts any peer
//! certificate**. That is a security hole by construction, which is why `loomd`
//! refuses to serve unless `--insecure-dev` is passed (see `main.rs`). Do not
//! ship this. Do not copy this verifier anywhere real.

use std::sync::Arc;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

/// ALPN token for protocol version 1 (PROTOCOL.md §2 / ARCHITECTURE §4.1).
pub const ALPN: &[u8] = b"loom/1";

/// A throwaway self-signed identity (cert chain + private key).
pub struct DevIdentity {
    /// Single-element chain: the self-signed cert.
    pub chain: Vec<CertificateDer<'static>>,
    /// The matching PKCS#8 private key.
    pub key: PrivateKeyDer<'static>,
}

impl DevIdentity {
    /// Generate a fresh `localhost` self-signed identity via rcgen.
    pub fn generate() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
        let chain = vec![certified.cert.der().clone()];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.key_pair.serialize_der(),
        ));
        Ok(Self { chain, key })
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// A `quinn::ServerConfig` that presents a fresh dev cert and **accepts any
/// client certificate** (TODO(M7): pin instead). ALPN pinned to `loom/1`.
pub fn insecure_server_config(
) -> Result<quinn::ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let id = DevIdentity::generate()?;
    let mut crypto = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])? // QUIC is TLS 1.3 only.
        .with_client_cert_verifier(Arc::new(AcceptAnyClient))
        .with_single_cert(id.chain, id.key)?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    Ok(quinn::ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(crypto)?,
    )))
}

/// A `quinn::ClientConfig` that presents a fresh dev cert and **accepts any
/// server certificate** (TODO(M7)). Used by the in-process handshake test; the
/// real SDL/Quest clients use msquic, not this. ALPN pinned to `loom/1`.
pub fn insecure_client_config(
) -> Result<quinn::ClientConfig, Box<dyn std::error::Error + Send + Sync>> {
    let id = DevIdentity::generate()?;
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer))
        .with_client_auth_cert(id.chain, id.key)?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    Ok(quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto)?,
    )))
}

fn all_schemes() -> Vec<SignatureScheme> {
    provider()
        .signature_verification_algorithms
        .supported_schemes()
}

/// TODO(M7): replace with pin comparison. Accepts every server cert.
#[derive(Debug)]
struct AcceptAnyServer;

impl ServerCertVerifier for AcceptAnyServer {
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
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        all_schemes()
    }
}

/// TODO(M7): replace with pin comparison. Accepts every client cert.
#[derive(Debug)]
struct AcceptAnyClient;

impl ClientCertVerifier for AcceptAnyClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    // Dev stub: don't force clients to present a cert. Real mutual-TLS pinning
    // (both sides authenticated) is TODO(M7); until then the msquic client
    // connects without any client-cert plumbing.
    fn client_auth_mandatory(&self) -> bool {
        false
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
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        all_schemes()
    }
}
