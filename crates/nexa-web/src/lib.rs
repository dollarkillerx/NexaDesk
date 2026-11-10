//! Browser access to a host (docs/WEB_CLIENT.md), without any server of ours.
//!
//! The host serves everything itself on one TCP port: the page (`web/`), and a WebSocket
//! that carries signaling and pairing. Media and input then go over WebRTC directly
//! between the browser and this machine: an H.264 video track, and input on a DataChannel.
//! That reaches browsers on the same LAN or tailnet, the same places a native viewer can
//! reach without the optional signaling server.
//!
//! ```text
//! browser                          host (this crate)
//!   GET /  ─────────────────────▶  page, script, pake.wasm
//!   WS /ws: offer SDP  ─────────▶  str0m answers; BrowserPairing starts
//!          ◀── answer SDP, SPAKE2 offer
//!          SPAKE2 reply, confirm ─▶ verified against both DTLS fingerprints
//!          ◀── waiting               the person at the host is asked (WebHost::ask)
//!          ◀── accepted, confirm     the browser checks the host in turn
//!   ═════ DTLS/SRTP ═════════════  video ▶, ◀ input DataChannel
//! ```
//!
//! Security: the one-time code is the native one with the same attempt limits, and the
//! exchange is bound to both DTLS fingerprints (`nexa_auth::browser`). The host's consent
//! is asked as for any viewer. Nothing is accepted from the DataChannel before that, and
//! only one browser session runs at a time. The page itself is served over plain HTTP: on
//! a tailnet that path is already encrypted, on a LAN it is not (see WEB_CLIENT.md §8).

mod assets;
mod http;
mod input;
mod media;
mod rtc;
mod session;

use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64},
    },
    time::Duration,
};

use nexa_auth::OneTimeCode;
use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig};
use nexa_session::{handshake::PairLimiter, input::InputSink};
use tokio::{net::TcpListener, task::JoinHandle};

pub use nexa_auth::browser::DtlsFingerprint;

/// The port the agent uses unless told otherwise: next to the native 47800.
pub const DEFAULT_PORT: u16 = 47801;

#[derive(Clone, Debug)]
pub struct WebConfig {
    /// Frames per second sent to a browser.
    pub fps: u32,
    /// The picture is scaled down, keeping its shape, to fit this box.
    pub max_size: (u32, u32),
    pub start_bitrate_bps: u32,
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    /// How long the person at the host has to answer.
    pub consent_timeout: Duration,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            fps: 60,
            max_size: (1920, 1200),
            start_bitrate_bps: 8_000_000,
            min_bitrate_bps: 1_000_000,
            max_bitrate_bps: 25_000_000,
            consent_timeout: Duration::from_secs(60),
        }
    }
}

/// The display a browser sees, and how its pixels map to the platform's points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WebScreen {
    pub display: u32,
    pub origin_points: (f64, f64),
    pub size_points: (f64, f64),
    pub size_pixels: (u32, u32),
}

/// What the person at the host is asked about a browser.
#[derive(Clone, Debug)]
pub struct BrowserRequest {
    pub id: u64,
    /// What the browser says it is ("Chrome on macOS"), sanitized; not proof of anything.
    pub browser: String,
    pub remote: SocketAddr,
    /// The browser's DTLS certificate, which the exchange was bound to.
    pub fingerprint: DtlsFingerprint,
    /// Whether this host could carry out input at all (macOS: Accessibility).
    pub input_available: bool,
    /// Set when the browser goes away; stop asking then.
    pub cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserAnswer {
    pub allow: bool,
    /// Keyboard and mouse. Without it the browser can only watch.
    pub input: bool,
}

impl BrowserAnswer {
    pub const DENY: Self = Self {
        allow: false,
        input: false,
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebEvent {
    SessionStarted {
        id: u64,
        browser: String,
        remote: SocketAddr,
        fingerprint: DtlsFingerprint,
        input: bool,
    },
    SessionEnded {
        id: u64,
        reason: String,
    },
    /// Refused before becoming a session: wrong code, denied, busy...
    Refused {
        remote: SocketAddr,
        reason: String,
    },
}

/// The machine and the person at it, as the agent provides them.
pub trait WebHost: Send + Sync + 'static {
    fn screen(&self) -> Result<WebScreen, String>;
    fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>>;
    fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>>;
    fn input_available(&self) -> bool;
    /// Called once per session on its input thread, only if input was allowed.
    fn input_sink(&self) -> Result<Box<dyn InputSink>, String>;
    /// Whether the host is taken by something else (a native viewer): browsers are then
    /// told it is busy before any pairing starts.
    fn busy(&self) -> bool {
        false
    }
    /// Blocks for up to `timeout`, on a blocking thread. Must deny when no one can answer.
    fn ask(&self, request: &BrowserRequest, timeout: Duration) -> BrowserAnswer;
    fn event(&self, event: WebEvent);
}

pub(crate) struct Shared {
    pub host: Arc<dyn WebHost>,
    pub code: OneTimeCode,
    pub config: WebConfig,
    pub limiter: PairLimiter,
    /// One browser session at a time.
    pub busy: AtomicBool,
    pub next_id: AtomicU64,
    /// Ends the running session: the person at the host pressed Disconnect.
    pub kick: tokio::sync::Notify,
}

/// A running gateway. Dropping it stops accepting; sessions already running end when
/// their browser goes away.
pub struct WebGateway {
    local: SocketAddr,
    task: JoinHandle<()>,
    shared: Arc<Shared>,
}

impl WebGateway {
    /// Binds `addr` and starts serving. Must be called inside a Tokio runtime.
    pub async fn bind(
        addr: SocketAddr,
        host: Arc<dyn WebHost>,
        code: OneTimeCode,
        config: WebConfig,
    ) -> io::Result<Self> {
        rtc::install_crypto();
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let shared = Arc::new(Shared {
            host,
            code,
            config,
            limiter: PairLimiter::default(),
            busy: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            kick: tokio::sync::Notify::new(),
        });
        let accepting = shared.clone();
        let task = tokio::spawn(async move {
            let shared = accepting;
            loop {
                let Ok((stream, peer)) = listener.accept().await else {
                    continue;
                };
                let shared = shared.clone();
                tokio::spawn(http::handle(stream, peer, shared));
            }
        });
        Ok(Self {
            local,
            task,
            shared,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Whether a browser session is being set up or running.
    pub fn busy(&self) -> bool {
        self.shared.busy.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Ends the browser session, if there is one, and any exchange in progress.
    pub fn disconnect(&self) {
        self.shared.kick.notify_waiters();
    }
}

impl Drop for WebGateway {
    fn drop(&mut self) {
        self.task.abort();
        self.disconnect();
    }
}

/// Keeps what a browser says about itself short and printable.
pub(crate) fn clean_label(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        "Browser".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_cleaned() {
        assert_eq!(clean_label("  Chrome on macOS \n"), "Chrome on macOS");
        assert_eq!(clean_label("\u{7}\u{1b}"), "Browser");
        assert_eq!(clean_label(&"x".repeat(200)).len(), 60);
    }
}
