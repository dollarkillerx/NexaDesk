//! The macOS host (§1.1): one listening QUIC endpoint, at most one session (§5.3).
//!
//! Everything that touches the machine arrives through [`Platform`] and [`AgentUi`],
//! so this library contains no injector, no pasteboard and no consent shortcut. The
//! real implementations live in the binary (`main.rs`, `real.rs`, `terminal.rs`).
//! [`ipc`] is the desktop UI's consent surface; it answers questions, it cannot ask
//! them, so it adds no way in.

mod agent;
mod host_audio;
mod host_cursor;
pub mod ipc;
mod session;
pub mod signal;
mod ui_host;

use std::net::SocketAddr;

use nexa_clip::ClipNotice;
use nexa_media::{
    AudioCapture, AudioEncoder, CaptureConfig, Capturer, CursorSource, Encoder, EncoderConfig,
};
use nexa_session::{
    clip::{ClipPlatform, PromiseResolver},
    handshake::Consent,
    input::InputSink,
};
use nexa_wire::control::Permissions;

pub use agent::{Agent, AgentConfig, AgentError, FileSettings, start};
pub use signal::{SignalSetup, SignalState, SignalStatus};
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
    /// The main display.
    fn screen(&self) -> Result<ScreenInfo, String>;
    /// Every display a viewer may pick (§6.7), main display first. Read again for
    /// every session and every switch, since monitors come and go.
    fn displays(&self) -> Result<Vec<ScreenInfo>, String> {
        self.screen().map(|s| vec![s])
    }
    /// The host cursor (§6.9). Only asked for in sessions with the input permission.
    /// `None` keeps the cursor drawn into the video in every mode.
    fn cursor_source(&self) -> Option<Box<dyn CursorSource>> {
        None
    }
    /// Starts system-audio capture. Only called for sessions granted `audio`, and
    /// only while the viewer has it switched on.
    fn audio_capture(&self, display_id: u32) -> Result<Box<dyn AudioCapture>, String> {
        let _ = display_id;
        Err("system audio is not available on this host".into())
    }
    fn audio_encoder(&self) -> Result<Box<dyn AudioEncoder>, String> {
        Err("no audio encoder on this host".into())
    }
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
    /// The real network peer: the viewer's address, or the relay's when relayed.
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
    /// Reachability by ID changed (§4.3).
    Signal(SignalStatus),
    /// A file transfer of session `session` moved on.
    Transfer {
        session: u64,
        event: nexa_session::transfer::TransferEvent,
    },
}

/// The person at the host. §9.3's "always shows who is connected" lives here.
pub trait AgentUi: Consent + 'static {
    fn event(&self, event: AgentEvent);
}
