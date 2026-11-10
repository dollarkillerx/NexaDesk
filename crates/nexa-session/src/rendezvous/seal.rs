//! Sealed rendezvous blobs (§4.3): HPKE (RFC 9180) in Auth mode, DHKEM(X25519,
//! HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305.
//!
//! Each device registers a per-process X25519 key signed by its Ed25519 identity.
//! A blob is sealed to the peer's X25519 key and authenticated with ours, so the
//! signaling server can neither read candidates and credentials nor forge them
//! in someone else's name. The info string binds both identities; the
//! associated data binds the session and the kind of message.

use std::sync::Arc;

use hpke::{
    Deserializable, Kem as _, OpModeR, OpModeS, Serializable, aead::ChaCha20Poly1305,
    kdf::HkdfSha256, kem::X25519HkdfSha256,
};
use nexa_auth::{DeviceIdentity, DevicePublicKey};
use nexa_wire::signal::{PURPOSE_X25519, SESSION_ID_LEN, SealedKind};
use thiserror::Error;

type Kem = X25519HkdfSha256;
type PrivateKey = <Kem as hpke::Kem>::PrivateKey;
type PublicKey = <Kem as hpke::Kem>::PublicKey;
type EncappedKey = <Kem as hpke::Kem>::EncappedKey;

const ENC_LEN: usize = 32;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SealError {
    #[error("the peer's key exchange key is not signed by its identity")]
    BadSignature,
    #[error("malformed key")]
    BadKey,
    #[error("sealed message failed to open")]
    Open,
    #[error("sealing failed")]
    Seal,
}

fn rng() -> rand_core09::UnwrapErr<rand_core09::OsRng> {
    rand_core09::UnwrapErr(rand_core09::OsRng)
}

/// This device's signaling keys. The X25519 pair lives only in memory.
pub struct SignalKeys {
    identity: Arc<DeviceIdentity>,
    sk: PrivateKey,
    pk: PublicKey,
    pk_bytes: [u8; 32],
    sig: [u8; 64],
}

impl std::fmt::Debug for SignalKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalKeys")
            .field("identity", &self.identity.public_key())
            .finish_non_exhaustive()
    }
}

impl SignalKeys {
    pub fn new(identity: Arc<DeviceIdentity>) -> Self {
        let (sk, pk) = Kem::gen_keypair(&mut rng());
        let mut pk_bytes = [0u8; 32];
        pk_bytes.copy_from_slice(&pk.to_bytes());
        let sig = identity.sign_for(PURPOSE_X25519, &pk_bytes);
        Self {
            identity,
            sk,
            pk,
            pk_bytes,
            sig,
        }
    }

    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    pub fn public_key(&self) -> DevicePublicKey {
        self.identity.public_key()
    }

    pub fn x25519(&self) -> &[u8; 32] {
        &self.pk_bytes
    }

    pub fn x25519_sig(&self) -> &[u8; 64] {
        &self.sig
    }
}

/// A peer's identity and its signed X25519 key, checked.
#[derive(Clone)]
pub struct PeerKeys {
    pub identity: DevicePublicKey,
    pk: PublicKey,
}

impl std::fmt::Debug for PeerKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerKeys")
            .field("identity", &self.identity)
            .finish()
    }
}

impl PeerKeys {
    pub fn verify(pubkey: &[u8], x25519: &[u8], sig: &[u8]) -> Result<Self, SealError> {
        let pubkey: [u8; 32] = pubkey.try_into().map_err(|_| SealError::BadKey)?;
        let identity = DevicePublicKey::from_bytes(pubkey).map_err(|_| SealError::BadKey)?;
        let sig: [u8; 64] = sig.try_into().map_err(|_| SealError::BadSignature)?;
        if x25519.len() != 32 || !identity.verify_for(PURPOSE_X25519, x25519, &sig) {
            return Err(SealError::BadSignature);
        }
        let pk = PublicKey::from_bytes(x25519).map_err(|_| SealError::BadKey)?;
        Ok(Self { identity, pk })
    }
}

fn info(from: &DevicePublicKey, to: &DevicePublicKey) -> Vec<u8> {
    let mut i = b"nexa/1 rendezvous".to_vec();
    i.extend_from_slice(from.as_bytes());
    i.extend_from_slice(to.as_bytes());
    i
}

fn aad(kind: SealedKind, session: &[u8; SESSION_ID_LEN]) -> Vec<u8> {
    let mut a = vec![kind as u8];
    a.extend_from_slice(session);
    a
}

/// `encapsulated key (32) || ciphertext`.
pub fn seal(
    me: &SignalKeys,
    to: &PeerKeys,
    kind: SealedKind,
    session: &[u8; SESSION_ID_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, SealError> {
    let mode = OpModeS::Auth((me.sk.clone(), me.pk.clone()));
    let (enc, ct) = hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, Kem, _>(
        &mode,
        &to.pk,
        &info(&me.public_key(), &to.identity),
        plaintext,
        &aad(kind, session),
        &mut rng(),
    )
    .map_err(|_| SealError::Seal)?;
    let mut out = enc.to_bytes().to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn open(
    me: &SignalKeys,
    from: &PeerKeys,
    kind: SealedKind,
    session: &[u8; SESSION_ID_LEN],
    sealed: &[u8],
) -> Result<Vec<u8>, SealError> {
    if sealed.len() < ENC_LEN {
        return Err(SealError::Open);
    }
    let enc = EncappedKey::from_bytes(&sealed[..ENC_LEN]).map_err(|_| SealError::Open)?;
    hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, Kem>(
        &OpModeR::Auth(from.pk.clone()),
        &me.sk,
        &enc,
        &info(&from.identity, &me.public_key()),
        &sealed[ENC_LEN..],
        &aad(kind, session),
    )
    .map_err(|_| SealError::Open)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> SignalKeys {
        SignalKeys::new(Arc::new(DeviceIdentity::generate()))
    }

    fn peer(k: &SignalKeys) -> PeerKeys {
        PeerKeys::verify(k.public_key().as_bytes(), k.x25519(), k.x25519_sig()).unwrap()
    }

    #[test]
    fn round_trip_binds_kind_session_and_both_identities() {
        let (a, b, eve) = (keys(), keys(), keys());
        let s = [1u8; 16];
        let sealed = seal(&a, &peer(&b), SealedKind::Offer, &s, b"candidates").unwrap();
        assert_eq!(
            open(&b, &peer(&a), SealedKind::Offer, &s, &sealed).unwrap(),
            b"candidates"
        );
        assert_eq!(
            open(&b, &peer(&a), SealedKind::Answer, &s, &sealed),
            Err(SealError::Open)
        );
        assert_eq!(
            open(&b, &peer(&a), SealedKind::Offer, &[2u8; 16], &sealed),
            Err(SealError::Open)
        );
        // Someone else claiming to be the sender.
        assert_eq!(
            open(&b, &peer(&eve), SealedKind::Offer, &s, &sealed),
            Err(SealError::Open)
        );
        // Someone else receiving it.
        assert_eq!(
            open(&eve, &peer(&a), SealedKind::Offer, &s, &sealed),
            Err(SealError::Open)
        );
        let mut bad = sealed.clone();
        bad[40] ^= 1;
        assert_eq!(
            open(&b, &peer(&a), SealedKind::Offer, &s, &bad),
            Err(SealError::Open)
        );
        assert_eq!(
            open(&b, &peer(&a), SealedKind::Offer, &s, &sealed[..10]),
            Err(SealError::Open)
        );
    }

    #[test]
    fn a_key_signed_by_someone_else_is_refused() {
        let (a, b) = (keys(), keys());
        // b's X25519 key presented under a's identity.
        assert_eq!(
            PeerKeys::verify(a.public_key().as_bytes(), b.x25519(), b.x25519_sig()).unwrap_err(),
            SealError::BadSignature
        );
        assert_eq!(
            PeerKeys::verify(&[0; 31], a.x25519(), a.x25519_sig()).unwrap_err(),
            SealError::BadKey
        );
        assert!(PeerKeys::verify(a.public_key().as_bytes(), a.x25519(), &[0; 63]).is_err());
    }

    #[test]
    fn low_order_keys_cannot_be_sealed_to() {
        let a = keys();
        let id = DeviceIdentity::generate();
        let zero = [0u8; 32];
        let sig = id.sign_for(PURPOSE_X25519, &zero);
        let p = PeerKeys::verify(id.public_key().as_bytes(), &zero, &sig).unwrap();
        assert!(seal(&a, &p, SealedKind::Offer, &[0; 16], b"x").is_err());
    }
}
