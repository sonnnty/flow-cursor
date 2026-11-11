//! Certificate pinning for rustls (used inside QUIC).
//!
//! Nobody in FlowCursor has a CA-issued certificate; identities are
//! self-signed and authenticated by fingerprint:
//!
//! * agents pin the hub fingerprint (taken from the join token before the
//!   first connection, stored after it),
//! * the hub requires a client certificate and maps its fingerprint to an
//!   enrolled node (or enrols it when a valid join token is presented),
//! * agents accept P2P connections only from fingerprints the hub
//!   distributed in the peer list, and dial a peer expecting exactly that
//!   peer's fingerprint.
//!
//! TLS still verifies the handshake signature, which proves possession of
//! the private key behind the pinned certificate.

use crate::identity::fingerprint_of;
use flow_proto::Fingerprint;
use quinn::rustls::{
    self,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
    CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme,
};
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// What the server certificate must match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerPin {
    Exact(Fingerprint),
    /// Leading bytes of the fingerprint (from a join token; ≥ 16 bytes).
    Prefix(Vec<u8>),
}

impl ServerPin {
    pub fn matches(&self, fp: &Fingerprint) -> bool {
        match self {
            ServerPin::Exact(e) => bool::from(e.0.ct_eq(&fp.0)),
            ServerPin::Prefix(p) => p.len() >= 16 && p.len() <= 32 && bool::from(fp.0[..p.len()].ct_eq(p)),
        }
    }
}

fn rejected() -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

#[derive(Debug)]
pub struct PinnedServerVerifier {
    pin: ServerPin,
    provider: Arc<CryptoProvider>,
}

impl PinnedServerVerifier {
    pub fn new(pin: ServerPin) -> Arc<Self> {
        Arc::new(PinnedServerVerifier { pin, provider: provider() })
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self.pin.matches(&fingerprint_of(end_entity)) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rejected())
        }
    }

    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // QUIC is TLS 1.3 only.
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Decides which client fingerprints a server accepts at the TLS layer.
pub trait ClientAuthorizer: Send + Sync + std::fmt::Debug {
    fn allowed(&self, fp: &Fingerprint) -> bool;
}

/// Accepts any client certificate (the hub decides at the application
/// layer: known node, or unknown + valid join token).
#[derive(Debug)]
pub struct AnyClient;

impl ClientAuthorizer for AnyClient {
    fn allowed(&self, _fp: &Fingerprint) -> bool {
        true
    }
}

/// Requires a client certificate and asks an authorizer about it.
#[derive(Debug)]
pub struct PinnedClientVerifier {
    auth: Arc<dyn ClientAuthorizer>,
    provider: Arc<CryptoProvider>,
}

impl PinnedClientVerifier {
    pub fn new(auth: Arc<dyn ClientAuthorizer>) -> Arc<Self> {
        Arc::new(PinnedClientVerifier { auth, provider: provider() })
    }
}

impl ClientCertVerifier for PinnedClientVerifier {
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
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        if self.auth.allowed(&fingerprint_of(end_entity)) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rejected())
        }
    }

    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Fingerprint of the certificate the remote side presented.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<Fingerprint> {
    let any = conn.peer_identity()?;
    let certs = any.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certs.first().map(|c| fingerprint_of(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins() {
        let fp = Fingerprint([7; 32]);
        assert!(ServerPin::Exact(fp).matches(&fp));
        assert!(!ServerPin::Exact(Fingerprint([8; 32])).matches(&fp));
        assert!(ServerPin::Prefix(vec![7; 16]).matches(&fp));
        assert!(!ServerPin::Prefix(vec![7; 15]).matches(&fp), "too-short prefixes are refused");
        assert!(!ServerPin::Prefix(vec![7; 33]).matches(&fp));
        let mut p = vec![7; 16];
        p[15] = 0;
        assert!(!ServerPin::Prefix(p).matches(&fp));
    }
}
