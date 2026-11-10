//! Device identity (§9.1): an Ed25519 key pair, generated on first start.

use std::fmt;

use data_encoding::HEXLOWER;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey, pkcs8::EncodePrivateKey};
use hkdf::Hkdf;
use rand_core::OsRng;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::keystore::{KeyStore, KeyStoreError};

const SEED_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("stored device key has the wrong length")]
    BadStoredKey,
    #[error("device key did not read back after being stored")]
    StoreNotDurable,
    #[error("could not encode device key")]
    Encode,
    #[error(transparent)]
    KeyStore(#[from] KeyStoreError),
}

/// A validated Ed25519 public key. This, not [`DeviceId`], is what authorization compares.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DevicePublicKey([u8; 32]);

impl DevicePublicKey {
    /// Rejects encodings that are not a curve point and low-order keys, for which a
    /// signature can be made to verify without any secret.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, IdentityError> {
        let key = VerifyingKey::from_bytes(&bytes).map_err(|_| IdentityError::InvalidPublicKey)?;
        if key.is_weak() {
            return Err(IdentityError::InvalidPublicKey);
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn device_id(&self) -> DeviceId {
        let mut h = Sha256::new();
        h.update(b"nexa/1 device id");
        h.update(self.0);
        let digest = h.finalize();
        let mut id = [0u8; 16];
        id.copy_from_slice(&digest[..16]);
        DeviceId(id)
    }

    /// Strict verification: rejects non-canonical signatures and low-order components.
    pub(crate) fn verify(&self, message: &[u8], signature: &[u8; 64]) -> bool {
        match VerifyingKey::from_bytes(&self.0) {
            Ok(key) => key
                .verify_strict(message, &Signature::from_bytes(signature))
                .is_ok(),
            Err(_) => false,
        }
    }

    /// Verifies a [`DeviceIdentity::sign_for`] signature. Strict, like [`Self::verify`].
    pub fn verify_for(&self, purpose: &str, message: &[u8], signature: &[u8; 64]) -> bool {
        self.verify(&purposed(purpose, message), signature)
    }

    pub fn to_hex(&self) -> String {
        HEXLOWER.encode(&self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, IdentityError> {
        let bytes = HEXLOWER
            .decode(s.as_bytes())
            .map_err(|_| IdentityError::InvalidPublicKey)?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| IdentityError::InvalidPublicKey)?;
        Self::from_bytes(bytes)
    }
}

impl fmt::Debug for DevicePublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DevicePublicKey({})", self.to_hex())
    }
}

impl Serialize for DevicePublicKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for DevicePublicKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).map_err(de::Error::custom)
    }
}

/// Domain separation for [`DeviceIdentity::sign_for`]: a signature made for one
/// purpose never verifies for another, and never as a TLS handshake signature
/// (those sign a fixed 64-byte 0x20 prefix, which cannot start with "nexa/1 sig").
fn purposed(purpose: &str, message: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(16 + purpose.len() + message.len());
    m.extend_from_slice(b"nexa/1 sig\0");
    m.extend_from_slice(&(purpose.len() as u32).to_be_bytes());
    m.extend_from_slice(purpose.as_bytes());
    m.extend_from_slice(message);
    m
}

/// A short name for a device: the first 128 bits of a domain-separated SHA-256 of the
/// public key. Fine for display, routing and ordering, never for an access check.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId([u8; 16]);

impl DeviceId {
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&HEXLOWER.encode(&self.0))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

/// The device's key pair. Deliberately neither `Clone` nor printable.
pub struct DeviceIdentity {
    signing: SigningKey,
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

impl DeviceIdentity {
    /// A new key held only in memory. See [`Self::load_or_create`] for a persistent one.
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut OsRng),
        }
    }

    /// Loads the device key, generating and persisting one on first start.
    pub fn load_or_create(store: &dyn KeyStore) -> Result<Self, IdentityError> {
        if let Some(seed) = store.load()? {
            return Self::from_seed(&seed);
        }
        let identity = Self::generate();
        let seed = Zeroizing::new(identity.signing.to_bytes());
        store.store(seed.as_slice())?;
        // Read back so a key that was not really saved fails now, not after the
        // peers have pinned it.
        match store.load()? {
            Some(back) if back.as_slice() == seed.as_slice() => Ok(identity),
            _ => Err(IdentityError::StoreNotDurable),
        }
    }

    fn from_seed(seed: &[u8]) -> Result<Self, IdentityError> {
        let seed: Zeroizing<[u8; SEED_LEN]> =
            Zeroizing::new(seed.try_into().map_err(|_| IdentityError::BadStoredKey)?);
        Ok(Self {
            signing: SigningKey::from_bytes(&seed),
        })
    }

    pub fn public_key(&self) -> DevicePublicKey {
        DevicePublicKey(self.signing.verifying_key().to_bytes())
    }

    pub fn device_id(&self) -> DeviceId {
        self.public_key().device_id()
    }

    /// Signs `message` for one named `purpose`, e.g. a signaling registration. Used
    /// only for statements to untrusted infrastructure; authorization never rests on it.
    pub fn sign_for(&self, purpose: &str, message: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        self.signing.sign(&purposed(purpose, message)).to_bytes()
    }

    /// Key for the `pairings.json` HMAC, derived so it needs no storage of its own.
    pub fn store_mac_key(&self) -> Zeroizing<[u8; 32]> {
        let seed = Zeroizing::new(self.signing.to_bytes());
        let mut key = Zeroizing::new([0u8; 32]);
        Hkdf::<Sha256>::new(None, seed.as_slice())
            .expand(b"nexa/1 pairings.json hmac key", key.as_mut_slice())
            .expect("32 bytes is a valid HKDF output length");
        key
    }

    pub(crate) fn pkcs8_der(&self) -> Result<Zeroizing<Vec<u8>>, IdentityError> {
        let doc = self
            .signing
            .to_pkcs8_der()
            .map_err(|_| IdentityError::Encode)?;
        Ok(Zeroizing::new(doc.as_bytes().to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_order_and_invalid_public_keys_are_rejected() {
        let mut identity_point = [0u8; 32];
        identity_point[0] = 1;
        assert!(DevicePublicKey::from_bytes(identity_point).is_err());
        assert!(DevicePublicKey::from_bytes([0u8; 32]).is_err());
        let mut not_a_point = [0xFFu8; 32];
        not_a_point[0] = 0xED;
        assert!(DevicePublicKey::from_bytes(not_a_point).is_err());
    }

    #[test]
    fn purpose_signatures_are_domain_separated() {
        let id = DeviceIdentity::generate();
        let key = id.public_key();
        let sig = id.sign_for("a", b"message");
        assert!(key.verify_for("a", b"message", &sig));
        assert!(!key.verify_for("b", b"message", &sig));
        assert!(!key.verify_for("a", b"messagf", &sig));
        assert!(!key.verify(b"message", &sig));
        // The length prefix keeps purpose and message from sliding into each other.
        assert!(!key.verify_for("am", b"essage", &sig));
        let other = DeviceIdentity::generate().public_key();
        assert!(!other.verify_for("a", b"message", &sig));
    }

    #[test]
    fn hex_round_trip_rejects_bad_input() {
        let key = DeviceIdentity::generate().public_key();
        assert_eq!(DevicePublicKey::from_hex(&key.to_hex()).unwrap(), key);
        assert!(DevicePublicKey::from_hex("zz").is_err());
        assert!(DevicePublicKey::from_hex(&key.to_hex()[2..]).is_err());
    }
}
