//! The pairing store: `pairings.json` (§9.1, §11).
//!
//! A record can be created only from a completed exchange (see [`VerifiedExchange`]).
//! The file is replaced atomically and carries an HMAC, checked before the contents
//! are parsed, under a key derived from the device key ([`DeviceIdentity::store_mac_key`]).
//!
//! The MAC stops editing and forgery by anyone without the device key. It does not
//! stop an older, validly signed copy of the file being restored, which could undo a
//! revoke: there is no monotonic state to compare against.
//!
//! The file is owned by one process at a time; concurrent writers are last-write-wins.
//!
//! [`DeviceIdentity::store_mac_key`]: crate::identity::DeviceIdentity::store_mac_key

use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

use data_encoding::HEXLOWER;
use hmac::{Hmac, Mac};
use nexa_wire::control::Permissions;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{
    fsio::atomic_write,
    identity::DevicePublicKey,
    pairing::{HostVerified, ViewerVerified},
};

const FORMAT_VERSION: u32 = 1;
const MAC_DOMAIN: &[u8] = b"nexa/1 pairings.json\0";
const MAX_FILE_LEN: u64 = 16 << 20;
const MAX_NAME_LEN: usize = 128;

#[derive(Debug, Error)]
pub enum StoreError {
    /// Fail closed: the UI must surface this. It is not safe to start empty and carry on.
    #[error("pairing store failed its integrity check")]
    Tampered,
    #[error("pairing store is malformed")]
    Malformed,
    #[error("pairing store i/o: {0}")]
    Io(#[from] io::Error),
    #[error("pairing store serialization: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// What a paired device may do (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Perms {
    pub view: bool,
    pub input: bool,
    pub clipboard: bool,
    pub file: bool,
    pub audio: bool,
}

impl From<Perms> for Permissions {
    fn from(p: Perms) -> Self {
        Permissions {
            view: p.view,
            input: p.input,
            clipboard: p.clipboard,
            file: p.file,
            audio: p.audio,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingRecord {
    pub pubkey: DevicePublicKey,
    pub name: String,
    pub perms: Perms,
    /// Unix seconds.
    pub added_at: u64,
    /// Unix seconds.
    pub last_seen: u64,
    /// §9.3: this device may connect without a consent prompt.
    pub unattended: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    version: u32,
    records: Vec<PairingRecord>,
}

/// On disk: the MAC and the exact bytes it covers, so nothing depends on JSON
/// being re-encoded identically.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    mac: String,
    body: String,
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::pairing::HostVerified {}
    impl Sealed for crate::pairing::ViewerVerified {}
}

/// A completed code or password exchange. Sealed, so only those can create a record.
pub trait VerifiedExchange: sealed::Sealed {
    fn verified_peer(&self) -> DevicePublicKey;
}

impl VerifiedExchange for HostVerified {
    fn verified_peer(&self) -> DevicePublicKey {
        self.peer()
    }
}

impl VerifiedExchange for ViewerVerified {
    fn verified_peer(&self) -> DevicePublicKey {
        self.peer()
    }
}

/// A peer found in the store. Only [`PairingStore::pinned`] constructs one, which is
/// what stops a caller from conjuring pairing evidence for an access decision.
#[derive(Debug, Clone, Copy)]
pub struct PinnedPeer<'a> {
    record: &'a PairingRecord,
}

impl PinnedPeer<'_> {
    pub fn record(&self) -> &PairingRecord {
        self.record
    }
}

pub struct PairingStore {
    path: PathBuf,
    key: Zeroizing<[u8; 32]>,
    records: Vec<PairingRecord>,
}

fn mac(key: &[u8; 32]) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(MAC_DOMAIN);
    m
}

/// Device names come from the peer and end up in a consent dialog.
fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_LEN)
        .collect()
}

impl PairingStore {
    /// Opens the store, or starts empty if the file does not exist. Deleting the
    /// file can only remove pairings, never add one.
    pub fn open(path: impl Into<PathBuf>, key: Zeroizing<[u8; 32]>) -> Result<Self, StoreError> {
        let path = path.into();
        let records = match read_limited(&path) {
            Ok(bytes) => parse(&bytes, &key)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, key, records })
    }

    pub fn records(&self) -> &[PairingRecord] {
        &self.records
    }

    /// Looks up a key that was authenticated by the TLS handshake.
    pub fn pinned(&self, peer: &DevicePublicKey) -> Option<PinnedPeer<'_>> {
        self.records
            .iter()
            .find(|r| r.pubkey == *peer)
            .map(|record| PinnedPeer { record })
    }

    /// Records a peer that completed an exchange, replacing any earlier record for it.
    pub fn add(
        &mut self,
        proof: &impl VerifiedExchange,
        name: &str,
        perms: Perms,
        now: u64,
    ) -> Result<&PairingRecord, StoreError> {
        let pubkey = proof.verified_peer();
        self.records.retain(|r| r.pubkey != pubkey);
        self.records.push(PairingRecord {
            pubkey,
            name: clean_name(name),
            perms,
            added_at: now,
            last_seen: now,
            unattended: false,
        });
        self.save()?;
        Ok(&self.records[self.records.len() - 1])
    }

    /// Removes a pairing. Returns whether it existed.
    pub fn revoke(&mut self, peer: &DevicePublicKey) -> Result<bool, StoreError> {
        let before = self.records.len();
        self.records.retain(|r| r.pubkey != *peer);
        if self.records.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    pub fn touch(&mut self, peer: &DevicePublicKey, now: u64) -> Result<bool, StoreError> {
        self.modify(peer, |r| r.last_seen = now)
    }

    /// §9.3: the "don't ask next time" checkbox.
    pub fn set_unattended(
        &mut self,
        peer: &DevicePublicKey,
        unattended: bool,
    ) -> Result<bool, StoreError> {
        self.modify(peer, |r| r.unattended = unattended)
    }

    pub fn set_perms(&mut self, peer: &DevicePublicKey, perms: Perms) -> Result<bool, StoreError> {
        self.modify(peer, |r| r.perms = perms)
    }

    fn modify(
        &mut self,
        peer: &DevicePublicKey,
        f: impl FnOnce(&mut PairingRecord),
    ) -> Result<bool, StoreError> {
        let Some(record) = self.records.iter_mut().find(|r| r.pubkey == *peer) else {
            return Ok(false);
        };
        f(record);
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> Result<(), StoreError> {
        let body = serde_json::to_string(&Body {
            version: FORMAT_VERSION,
            records: self.records.clone(),
        })?;
        let mut m = mac(&self.key);
        m.update(body.as_bytes());
        let envelope = Envelope {
            mac: HEXLOWER.encode(&m.finalize().into_bytes()),
            body,
        };
        atomic_write(&self.path, serde_json::to_string(&envelope)?.as_bytes())?;
        Ok(())
    }
}

fn read_limited(path: &Path) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    File::open(path)?
        .take(MAX_FILE_LEN + 1)
        .read_to_end(&mut data)?;
    Ok(data)
}

fn parse(bytes: &[u8], key: &[u8; 32]) -> Result<Vec<PairingRecord>, StoreError> {
    if bytes.len() as u64 > MAX_FILE_LEN {
        return Err(StoreError::Tampered);
    }
    // Everything before the MAC check is attacker-controlled; any failure is tampering.
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| StoreError::Tampered)?;
    let tag = HEXLOWER
        .decode(envelope.mac.as_bytes())
        .map_err(|_| StoreError::Tampered)?;
    let mut m = mac(key);
    m.update(envelope.body.as_bytes());
    // `verify_slice` compares in constant time.
    m.verify_slice(&tag).map_err(|_| StoreError::Tampered)?;

    let body: Body = serde_json::from_str(&envelope.body).map_err(|_| StoreError::Malformed)?;
    if body.version != FORMAT_VERSION {
        return Err(StoreError::Malformed);
    }
    let mut seen = std::collections::HashSet::new();
    if !body.records.iter().all(|r| seen.insert(r.pubkey)) {
        return Err(StoreError::Malformed);
    }
    Ok(body.records)
}

/// Unix seconds now, for the `now` arguments above.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
