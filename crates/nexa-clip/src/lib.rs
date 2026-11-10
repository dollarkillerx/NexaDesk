//! Pure-logic clipboard synchronisation for NexaDesk.
//!
//! Nothing in this crate performs I/O or reads a clock. The assembly layer owns
//! the OS clipboard, the QUIC stream and the timers; it feeds [`ClipEvent`]s to
//! [`ClipSync`] and carries out the [`ClipCommand`]s that come back. Every
//! entry point takes the current time as a value, so behaviour is reproducible.
//!
//! - [`ClipSync`]: generations, echo suppression, policy, fetch (design §8.1/§8.2).
//! - [`InputGate`]: holds input behind a paste until the clipboard landed (§8.3).
//! - [`ClipMessage`] and friends: the Clipboard-stream messages and their encoding.
//! - [`ClipboardBackend`] and [`FakeBackend`]: the platform seam and a test double.
//! - [`format`]: newline normalisation and CF_HTML helpers.
//!
//! Content hashes and sizes always describe the *canonical* form of a format
//! (LF newlines, HTML without the CF_HTML header), so a format that went through
//! a platform conversion still hashes the same when it echoes back.

mod backend;
pub mod format;
mod gate;
#[cfg(target_os = "macos")]
pub mod platform;
mod sync;
mod time;
mod types;
mod wire;

pub use backend::{ClipError, ClipboardBackend, FakeBackend, Result};
pub use gate::{EventClass, GateStats, HoldEnded, InputGate, PASTE_GATE_TIMEOUT, ReleaseReason};
pub use sync::{
    ClipCommand, ClipConfig, ClipEvent, ClipNotice, ClipPolicy, ClipSync, FetchFailure,
    LocalSnapshot,
};
pub use time::{ClipInstant, ManualInstant};
pub use types::{ClipKind, ContentHash, DeviceId, FormatInfo, Gen};
pub use wire::{ClipAnnounce, ClipApplied, ClipData, ClipFetch, ClipMessage, ClipStatus};
