//! Mutual TLS 1.3 with self-signed device certificates (§9.2 step 1).
//!
//! There is no CA: a certificate is only a signed container for the device's public
//! key, and the key is the identity. The verifiers check that the certificate is
//! well-formed, that it is signed by the key it carries, and that the peer's TLS
//! handshake is signed by that same key.
//!
//! [`PeerPolicy::AcceptUnknown`] lets an unknown peer finish the TLS handshake so
//! pairing can happen. It grants nothing: access is decided afterwards by
//! [`crate::access::decide`], which needs a pinned pairing or a completed exchange.

use std::sync::Arc;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rcgen::{CertificateParams, KeyPair, PKCS_ED25519};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error as RustlsError,
    PeerIncompatible, SignatureScheme,
    client::{
        Resumption,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::{CryptoProvider, ring},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::{
        NoServerSessionStorage,
        danger::{ClientCertVerified, ClientCertVerifier},
    },
    version::TLS13,
};
use thiserror::Error;
use x509_parser::{
    parse_x509_certificate,
    x509::{AlgorithmIdentifier, X509Version},
};

use crate::identity::{DeviceIdentity, DevicePublicKey, IdentityError};

/// QUIC ALPN (§3.1).
pub const ALPN: &[u8] = b"nexa/1";
/// A placeholder: the identity is the key, so no name is ever checked.
pub const SERVER_NAME: &str = "nexa";
const EXPORTER_LABEL: &[u8] = b"EXPORTER-nexa-pair";
const ED25519_OID: &str = "1.3.101.112";

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("could not build device certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("tls: {0}")]
    Rustls(#[from] RustlsError),
    #[error("quic crypto configuration: {0}")]
    Quic(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error("connection has no usable peer certificate")]
    NoPeerIdentity,
    #[error("could not export keying material")]
    Exporter,
}

/// What this end expects of the other end's public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerPolicy {
    /// Any well-formed, correctly signed peer. For pairing, and for a host that
    /// looks the viewer up in its store after the handshake.
    AcceptUnknown,
    /// Only this key; anything else fails the TLS handshake.
    Pinned(DevicePublicKey),
}

/// Why a certificate was refused.
#[derive(Debug, PartialEq, Eq)]
enum CertRejection {
    Malformed,
    WrongAlgorithm,
    BadSignature,
}

impl From<CertRejection> for RustlsError {
    fn from(r: CertRejection) -> Self {
        match r {
            CertRejection::Malformed | CertRejection::WrongAlgorithm => {
                RustlsError::InvalidCertificate(CertificateError::BadEncoding)
            }
            CertRejection::BadSignature => {
                RustlsError::InvalidCertificate(CertificateError::BadSignature)
            }
        }
    }
}

fn is_plain_ed25519(alg: &AlgorithmIdentifier<'_>) -> bool {
    alg.algorithm.to_id_string() == ED25519_OID && alg.parameters.is_none()
}

/// Checks the certificate's form and self-signature and returns the key it carries.
/// Validity dates are deliberately not checked (§9.2): pinning, not expiry, is what
/// ends trust in a key.
fn device_key_from_cert(der: &[u8]) -> Result<DevicePublicKey, CertRejection> {
    let (rest, cert) = parse_x509_certificate(der).map_err(|_| CertRejection::Malformed)?;
    if !rest.is_empty() || cert.version() != X509Version::V3 {
        return Err(CertRejection::Malformed);
    }
    let tbs = &cert.tbs_certificate;
    if tbs.issuer() != tbs.subject() {
        return Err(CertRejection::Malformed);
    }
    if tbs.extensions().iter().any(|e| e.critical) {
        return Err(CertRejection::Malformed);
    }
    let spki = tbs.public_key();
    if !is_plain_ed25519(&cert.signature_algorithm)
        || !is_plain_ed25519(&tbs.signature)
        || !is_plain_ed25519(&spki.algorithm)
    {
        return Err(CertRejection::WrongAlgorithm);
    }
    let key: [u8; 32] = spki
        .subject_public_key
        .as_ref()
        .try_into()
        .map_err(|_| CertRejection::Malformed)?;
    let key = DevicePublicKey::from_bytes(key).map_err(|_| CertRejection::Malformed)?;
    let signature: &[u8; 64] = cert
        .signature_value
        .as_ref()
        .try_into()
        .map_err(|_| CertRejection::BadSignature)?;
    if !key.verify(tbs.as_ref(), signature) {
        return Err(CertRejection::BadSignature);
    }
    Ok(key)
}

/// Checks a TLS 1.3 `CertificateVerify` against the key in `cert`.
fn verify_handshake_signature(
    cert: &[u8],
    message: &[u8],
    scheme: SignatureScheme,
    signature: &[u8],
) -> Result<(), RustlsError> {
    if scheme != SignatureScheme::ED25519 {
        return Err(RustlsError::PeerIncompatible(
            PeerIncompatible::NoSignatureSchemesInCommon,
        ));
    }
    let key = device_key_from_cert(cert)?;
    let signature: &[u8; 64] = signature
        .try_into()
        .map_err(|_| RustlsError::InvalidCertificate(CertificateError::BadSignature))?;
    if !key.verify(message, signature) {
        return Err(RustlsError::InvalidCertificate(
            CertificateError::BadSignature,
        ));
    }
    Ok(())
}

/// The verifier for both directions; they differ only in the rustls trait.
#[derive(Debug)]
struct DeviceCertVerifier {
    policy: PeerPolicy,
}

impl DeviceCertVerifier {
    fn check_certificate(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), RustlsError> {
        if !intermediates.is_empty() {
            return Err(RustlsError::InvalidCertificate(
                CertificateError::BadEncoding,
            ));
        }
        let key = device_key_from_cert(end_entity)?;
        match self.policy {
            PeerPolicy::Pinned(expected) if key != expected => Err(
                RustlsError::InvalidCertificate(CertificateError::ApplicationVerificationFailure),
            ),
            _ => Ok(()),
        }
    }
}

fn no_tls12() -> RustlsError {
    RustlsError::PeerIncompatible(PeerIncompatible::Tls13RequiredForQuic)
}

impl ServerCertVerifier for DeviceCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        self.check_certificate(end_entity, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Err(no_tls12())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_handshake_signature(cert, message, dss.scheme, dss.signature())?;
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

impl ClientCertVerifier for DeviceCertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, RustlsError> {
        self.check_certificate(end_entity, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Err(no_tls12())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_handshake_signature(cert, message, dss.scheme, dss.signature())?;
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// A self-signed certificate for the device key (§9.1).
fn device_certificate(
    identity: &DeviceIdentity,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), TlsError> {
    let pkcs8 = identity.pkcs8_der()?;
    let key_der = PrivatePkcs8KeyDer::from(pkcs8.as_slice());
    let key_pair = KeyPair::from_pkcs8_der_and_sign_algo(&key_der, &PKCS_ED25519)?;
    let cert = CertificateParams::new(vec![SERVER_NAME.to_string()])?.self_signed(&key_pair)?;
    Ok((
        cert.der().clone(),
        PrivateKeyDer::Pkcs8(key_der.clone_key()),
    ))
}

/// Viewer (QUIC client) configuration.
///
/// Session resumption is off so that every connection re-verifies the peer's
/// certificate against the policy.
pub fn client_config(
    identity: &DeviceIdentity,
    peer: PeerPolicy,
) -> Result<quinn::ClientConfig, TlsError> {
    let (cert, key) = device_certificate(identity)?;
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DeviceCertVerifier { policy: peer }))
        .with_client_auth_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.resumption = Resumption::disabled();
    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(nexa_net::cc::transport_config());
    Ok(config)
}

/// Host (QUIC server) configuration. The client certificate is mandatory.
///
/// Resumption tickets and the session cache are off, for the same reason as in
/// [`client_config`], which also rules out 0-RTT.
pub fn server_config(
    identity: &DeviceIdentity,
    peer: PeerPolicy,
) -> Result<quinn::ServerConfig, TlsError> {
    let (cert, key) = device_certificate(identity)?;
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&TLS13])?
        .with_client_cert_verifier(Arc::new(DeviceCertVerifier { policy: peer }))
        .with_single_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.send_tls13_tickets = 0;
    tls.session_storage = Arc::new(NoServerSessionStorage {});
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(nexa_net::cc::transport_config());
    config.migration(false); // §4.8
    Ok(config)
}

/// The public key the peer authenticated with during the TLS handshake.
pub fn peer_public_key(connection: &quinn::Connection) -> Result<DevicePublicKey, TlsError> {
    let chain = connection
        .peer_identity()
        .ok_or(TlsError::NoPeerIdentity)?
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| TlsError::NoPeerIdentity)?;
    match chain.as_slice() {
        [cert] => device_key_from_cert(cert).map_err(|_| TlsError::NoPeerIdentity),
        _ => Err(TlsError::NoPeerIdentity),
    }
}

/// The 32-byte TLS exporter value that binds pairing to this connection (§9.2).
pub fn pair_exporter(connection: &quinn::Connection) -> Result<[u8; 32], TlsError> {
    let mut out = [0u8; 32];
    connection
        .export_keying_material(&mut out, EXPORTER_LABEL, b"")
        .map_err(|_| TlsError::Exporter)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rcgen::{Issuer, PKCS_ECDSA_P256_SHA256};

    use super::*;

    fn cert_of(identity: &DeviceIdentity) -> Vec<u8> {
        device_certificate(identity).unwrap().0.to_vec()
    }

    fn signing_key_pair(identity: &DeviceIdentity) -> KeyPair {
        let der = identity.pkcs8_der().unwrap();
        KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(der.as_slice()),
            &PKCS_ED25519,
        )
        .unwrap()
    }

    #[test]
    fn device_certificate_yields_the_device_key() {
        let id = DeviceIdentity::generate();
        assert_eq!(device_key_from_cert(&cert_of(&id)), Ok(id.public_key()));
    }

    #[test]
    fn flipped_signature_bit_is_rejected() {
        let mut der = cert_of(&DeviceIdentity::generate());
        // The signature is the last element of the certificate.
        let last = der.len() - 1;
        der[last] ^= 0x01;
        assert_eq!(device_key_from_cert(&der), Err(CertRejection::BadSignature));
    }

    #[test]
    fn flipped_tbs_bit_is_rejected() {
        let id = DeviceIdentity::generate();
        let der = cert_of(&id);
        // Flip a bit in the middle of the signed portion of the certificate. Which
        // field it lands in varies, but it must never parse and still verify.
        let mut tampered = der.clone();
        tampered[der.len() / 3] ^= 0x01;
        assert!(device_key_from_cert(&tampered).is_err());
    }

    #[test]
    fn victim_key_signed_by_someone_else_is_rejected() {
        // An attacker wraps the victim's public key in a certificate of its own.
        let victim = signing_key_pair(&DeviceIdentity::generate());
        let attacker = signing_key_pair(&DeviceIdentity::generate());
        let params = CertificateParams::new(vec![SERVER_NAME.to_string()]).unwrap();
        let issuer = Issuer::from_params(&params, &attacker);
        let cert = params.signed_by(&victim, &issuer).unwrap();
        assert_eq!(
            device_key_from_cert(cert.der()),
            Err(CertRejection::BadSignature)
        );
    }

    #[test]
    fn non_ed25519_certificate_is_rejected() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = CertificateParams::new(vec![SERVER_NAME.to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        assert_eq!(
            device_key_from_cert(cert.der()),
            Err(CertRejection::WrongAlgorithm)
        );
    }

    #[test]
    fn truncated_and_trailing_bytes_are_rejected() {
        let der = cert_of(&DeviceIdentity::generate());
        assert!(device_key_from_cert(&der[..der.len() - 1]).is_err());
        let mut long = der.clone();
        long.push(0);
        assert_eq!(device_key_from_cert(&long), Err(CertRejection::Malformed));
        assert_eq!(device_key_from_cert(&[]), Err(CertRejection::Malformed));
    }

    #[test]
    fn handshake_signature_must_match_the_certificate_key() {
        use ed25519_dalek::{Signer, SigningKey};

        let id = DeviceIdentity::generate();
        let cert = cert_of(&id);
        let message = b"tls 1.3 certificate verify content";
        let seed: [u8; 32] = {
            let der = id.pkcs8_der().unwrap();
            // The Ed25519 seed is the final 32 bytes of a v1 PKCS#8 prefix (16 bytes) body.
            der[16..48].try_into().unwrap()
        };
        let good = SigningKey::from_bytes(&seed).sign(message).to_bytes();
        verify_handshake_signature(&cert, message, SignatureScheme::ED25519, &good).unwrap();

        let other = SigningKey::from_bytes(&[7u8; 32]).sign(message).to_bytes();
        assert!(
            verify_handshake_signature(&cert, message, SignatureScheme::ED25519, &other).is_err()
        );

        let mut flipped = good;
        flipped[0] ^= 1;
        assert!(
            verify_handshake_signature(&cert, message, SignatureScheme::ED25519, &flipped).is_err()
        );
        assert!(
            verify_handshake_signature(&cert, b"other", SignatureScheme::ED25519, &good).is_err()
        );
        assert!(
            verify_handshake_signature(&cert, message, SignatureScheme::ED25519, &good[..63])
                .is_err()
        );
        assert!(
            verify_handshake_signature(
                &cert,
                message,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                &good
            )
            .is_err()
        );
    }

    #[test]
    fn pin_mismatch_is_rejected_by_the_verifier() {
        let id = DeviceIdentity::generate();
        let cert = device_certificate(&id).unwrap().0;
        let other = DeviceIdentity::generate().public_key();
        let pinned = DeviceCertVerifier {
            policy: PeerPolicy::Pinned(other),
        };
        assert!(pinned.check_certificate(&cert, &[]).is_err());
        let right = DeviceCertVerifier {
            policy: PeerPolicy::Pinned(id.public_key()),
        };
        right.check_certificate(&cert, &[]).unwrap();
        // A pin does not excuse extra chain entries.
        assert!(
            right
                .check_certificate(&cert, std::slice::from_ref(&cert))
                .is_err()
        );
    }
}
