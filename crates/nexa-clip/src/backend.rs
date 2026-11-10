//! The platform seam (design §2) and an in-memory implementation for tests.

use bytes::Bytes;

use crate::sync::LocalSnapshot;
use crate::types::{ClipKind, FormatInfo, Gen};

/// Errors from the clipboard backends and the pure helpers in this crate.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClipError {
    /// The OS clipboard is held by another process.
    #[error("the clipboard is busy")]
    Busy,
    /// The clipboard does not offer this format.
    #[error("clipboard format {0:?} is not available")]
    NotAvailable(ClipKind),
    /// The format is a promise that has not been fulfilled yet.
    #[error("clipboard format {0:?} has not been rendered yet")]
    Deferred(ClipKind),
    /// A message could not be decoded.
    #[error("malformed clipboard message: {0}")]
    Decode(String),
    /// A CF_HTML payload could not be parsed.
    #[error("malformed CF_HTML: {0}")]
    BadHtml(&'static str),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, ClipError>;

/// The OS clipboard, as specified by design §2.
///
/// The parameter §2 calls `gen` is named `generation` here, because `gen` is a
/// reserved word in the 2024 edition.
pub trait ClipboardBackend: Send {
    /// Lists the formats currently on the clipboard.
    fn read_formats(&mut self) -> Result<Vec<FormatInfo>>;
    /// Reads one format in canonical form.
    fn read(&mut self, kind: ClipKind) -> Result<Vec<u8>>;
    /// Replaces the clipboard: `inline` content directly, the remaining
    /// `formats` as promises, tagged with the owner marker.
    fn write_promise(
        &mut self,
        generation: Gen,
        formats: &[FormatInfo],
        inline: &[(ClipKind, Bytes)],
    ) -> Result<()>;
}

#[derive(Clone, Debug)]
struct FakeFormat {
    info: FormatInfo,
    data: Option<Bytes>,
}

/// An in-memory clipboard. It models what the state machine cares about:
/// promises, the owner marker, a change counter, and transient write failures.
#[derive(Clone, Debug, Default)]
pub struct FakeBackend {
    formats: Vec<FakeFormat>,
    owner: Option<Gen>,
    sensitive: bool,
    change_count: u64,
    busy_writes: u32,
}

impl FakeBackend {
    /// Creates an empty clipboard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulates the user copying `items` locally.
    pub fn user_copy(&mut self, items: &[(ClipKind, &[u8])]) {
        self.formats = items
            .iter()
            .map(|&(kind, data)| FakeFormat {
                info: FormatInfo::of(kind, data),
                data: Some(Bytes::copy_from_slice(data)),
            })
            .collect();
        self.owner = None;
        self.sensitive = false;
        self.change_count += 1;
    }

    /// Simulates the user copying content that the OS marks as sensitive.
    pub fn user_copy_sensitive(&mut self, items: &[(ClipKind, &[u8])]) {
        self.user_copy(items);
        self.sensitive = true;
    }

    /// Simulates a clipboard manager that rewrites the content and drops the
    /// owner marker.
    pub fn strip_owner(&mut self) {
        self.owner = None;
        self.change_count += 1;
    }

    /// Makes the next `count` writes fail with [`ClipError::Busy`].
    pub fn fail_next_writes(&mut self, count: u32) {
        self.busy_writes = count;
    }

    /// Fulfils a promised format, as the OS render callback would.
    pub fn fulfill(&mut self, kind: ClipKind, data: Bytes) {
        if let Some(format) = self.formats.iter_mut().find(|f| f.info.kind == kind) {
            format.data = Some(data);
        }
    }

    /// Drops an unfulfilled promise, as writing "empty" on timeout would.
    pub fn abandon(&mut self, kind: ClipKind) {
        self.formats
            .retain(|f| f.info.kind != kind || f.data.is_some());
    }

    /// Number of clipboard changes so far.
    pub fn change_count(&self) -> u64 {
        self.change_count
    }

    /// The generation in the owner marker, if the marker is present.
    pub fn owner(&self) -> Option<Gen> {
        self.owner
    }

    /// Kinds that are still unfulfilled promises.
    pub fn promised_kinds(&self) -> Vec<ClipKind> {
        self.formats
            .iter()
            .filter(|f| f.data.is_none())
            .map(|f| f.info.kind)
            .collect()
    }

    /// The fulfilled content of `kind`, if any.
    pub fn content(&self, kind: ClipKind) -> Option<Bytes> {
        self.formats
            .iter()
            .find(|f| f.info.kind == kind)
            .and_then(|f| f.data.clone())
    }

    /// Describes the clipboard as a change event would. Promised formats are
    /// listed without being rendered.
    pub fn snapshot(&self) -> LocalSnapshot {
        LocalSnapshot {
            formats: self.formats.iter().map(|f| f.info).collect(),
            data: self
                .formats
                .iter()
                .filter_map(|f| f.data.clone().map(|d| (f.info.kind, d)))
                .collect(),
            owner_marker: self.owner.is_some(),
            sensitive: self.sensitive,
        }
    }
}

impl ClipboardBackend for FakeBackend {
    fn read_formats(&mut self) -> Result<Vec<FormatInfo>> {
        Ok(self.formats.iter().map(|f| f.info).collect())
    }

    fn read(&mut self, kind: ClipKind) -> Result<Vec<u8>> {
        let format = self
            .formats
            .iter()
            .find(|f| f.info.kind == kind)
            .ok_or(ClipError::NotAvailable(kind))?;
        format
            .data
            .as_ref()
            .map(|d| d.to_vec())
            .ok_or(ClipError::Deferred(kind))
    }

    fn write_promise(
        &mut self,
        generation: Gen,
        formats: &[FormatInfo],
        inline: &[(ClipKind, Bytes)],
    ) -> Result<()> {
        if self.busy_writes > 0 {
            self.busy_writes -= 1;
            return Err(ClipError::Busy);
        }
        self.formats = formats
            .iter()
            .map(|info| FakeFormat {
                info: *info,
                data: inline
                    .iter()
                    .find(|(kind, _)| *kind == info.kind)
                    .map(|(_, data)| data.clone()),
            })
            .collect();
        self.owner = Some(generation);
        self.sensitive = false;
        self.change_count += 1;
        Ok(())
    }
}
