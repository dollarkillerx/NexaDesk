//! The macOS host (§1.1): one listening QUIC endpoint, at most one session (§5.3).
//!
//! Everything that touches the machine arrives through [`Platform`] and [`AgentUi`],
//! so this library contains no injector, no pasteboard and no consent shortcut. The
//! real implementations live in the binary (`main.rs`, `real.rs`, `terminal.rs`).
//! [`ipc`] is the desktop UI's consent surface; it answers questions, it cannot ask
//! them, so it adds no way in.

mod agent;
pub mod ipc;
mod session;
mod ui_host;

use std::net::SocketAddr;

use nexa_clip::ClipNotice;
use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig};
use nexa_session::{
    clip::{ClipPlatform, PromiseResolver},
    handshake::Consent,
    input::InputSink,
};
use nexa_wire::control::Permissions;

pub use agent::{Agent, AgentConfig, AgentError, start};
pub use ui_host::{UiChain, UiHost, UiHostError, start_with_ui};

/// The display being shared. Wire coordinates are in stream pixels of this display;
/// CoreGraphics uses points (§7.7).
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenInfo {
    /// The platform's display id; `CaptureConfig::display`.
    pub display_id: u32,
    pub origin_points: (f64, f64),
    pub size_points: (f64, f64),
    pub size_pixels: (u32, u32),
    pub refresh_hz: u32,
    pub name: String,
}

/// The real world, or a test's fake of it.
pub trait Platform: Send + Sync + 'static {
    fn screen(&self) -> Result<ScreenInfo, String>;
    fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>>;
    fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>>;
    /// Whether injected input would actually work (macOS: Accessibility is granted).
    fn input_available(&self) -> bool;
    /// Called once per session, on the inject thread, only if the session may inject.
    fn input_sink(&self) -> Result<Box<dyn InputSink>, String>;
    /// Called once per session, only if the session may use the clipboard.
    fn clipboard(&self, resolver: PromiseResolver) -> Result<Box<dyn ClipPlatform>, String>;
    /// Thread setup for the media threads (§1.1).
    fn thread_init(&self) -> Option<fn()> {
        None
    }
}

/// A session as shown to the person at the host.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub id: u64,
    pub device_name: String,
    pub device_id: String,
    /// The peer's public key, hex: the fingerprint shown to the person at the host.
    pub device_key: String,
    pub remote: SocketAddr,
    pub perms: Permissions,
}

/// The agent's API events (§5.3), consumed by the terminal and the IPC UI.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    Listening(SocketAddr),
    /// The one-time code to display.
    CodeChanged(String),
    SessionStarted(SessionInfo),
    SessionEnded {
        id: u64,
        device_name: String,
        reason: String,
    },
    /// A connection was refused or failed before becoming a session.
    ConnectionFailed {
        remote: SocketAddr,
        reason: String,
    },
    ClipboardNotice(ClipNotice),
    Warning(String),
}

/// The person at the host. §9.3's "always shows who is connected" lives here.
pub trait AgentUi: Consent + 'static {
    fn event(&self, event: AgentEvent);
}
