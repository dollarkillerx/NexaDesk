//! Identifiers shared by the sync state machine, the gate and the wire format.

use sha2::{Digest, Sha256};

/// A device identifier. The assembly layer derives it from the device identity;
/// this crate only compares it and uses it to break generation ties.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub [u8; 16]);

/// A clipboard generation: `(lamport, device)`, ordered lexicographically, so
/// the larger Lamport counter wins and the device id breaks ties.
pub type Gen = (u64, DeviceId);

/// Canonical clipboard formats (design §8.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClipKind {
    /// UTF-8 text with LF newlines.
    Text,
    /// HTML, without any CF_HTML header.
    Html,
    /// Rich Text Format.
    Rtf,
    /// PNG image.
    Png,
}

impl ClipKind {
    pub(crate) fn to_wire(self) -> u32 {
        match self {
            Self::Text => 1,
            Self::Html => 2,
            Self::Rtf => 3,
            Self::Png => 4,
        }
    }

    pub(crate) fn from_wire(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Text),
            2 => Some(Self::Html),
            3 => Some(Self::Rtf),
            4 => Some(Self::Png),
            _ => None,
        }
    }
}

/// A 128-bit content hash: SHA-256 of the canonical bytes, truncated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash(pub [u8; 16]);

impl ContentHash {
    /// Hashes canonical format content.
    pub fn of(content: &[u8]) -> Self {
        let digest = Sha256::digest(content);
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest[..16]);
        Self(out)
    }
}

/// One format offered by a clipboard entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatInfo {
    /// The canonical format.
    pub kind: ClipKind,
    /// Size of the canonical content in bytes.
    pub size: u64,
    /// Hash of the canonical content.
    pub hash: ContentHash,
}

impl FormatInfo {
    /// Describes `content`, which must already be in canonical form.
    pub fn of(kind: ClipKind, content: &[u8]) -> Self {
        Self {
            kind,
            size: content.len() as u64,
            hash: ContentHash::of(content),
        }
    }
}
