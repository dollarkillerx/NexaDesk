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
//! only one browser session runs at a time. The page is served over HTTPS (`tls`), with
//! the host's own certificate unless the person provides one; plain HTTP only redirects.

mod cursor;
mod http;
mod input;
mod media;
mod rtc;
mod session;
#[cfg(test)]
mod testing;
mod tls;

use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize},
    },
    time::Duration,
};

use nexa_auth::OneTimeCode;
use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig};
use nexa_session::{handshake::PairLimiter, input::InputSink};
use tokio::{net::TcpListener, task::JoinHandle};

pub use nexa_auth::browser::DtlsFingerprint;
pub use nexa_turn::Credentials as TurnCredentials;
pub use tls::{CUSTOM_CERT, CUSTOM_KEY, WebTls, certificate_names};

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
    /// The host's name, told to a browser once it is let in: what it saves the
    /// connection as. Empty: the browser names it by its device ID.
    pub name: String,
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
            name: String::new(),
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
/// How a browser signed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserAuth {
    Code,
    /// The host's fixed password.
    Password,
    /// Nothing but the device ID, through the signaling server: nothing proves who is
    /// asking, so the person at the host decides.
    Request,
}

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
    /// How the browser signed in.
    pub auth: BrowserAuth,
    /// It asked for the command line only, or this host has no screen to show: the
    /// terminal is the whole session.
    pub terminal_only: bool,
    /// Set when the browser goes away; stop asking then.
    pub cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserAnswer {
    pub allow: bool,
    /// Keyboard and mouse. Without it the browser can only watch.
    pub input: bool,
    /// Text copied on either side reaches the other.
    pub clipboard: bool,
    /// A shell on the host, in the page.
    pub terminal: bool,
}

impl BrowserAnswer {
    pub const DENY: Self = Self {
        allow: false,
        input: false,
        clipboard: false,
        terminal: false,
    };
}

/// The host's clipboard as a browser session uses it: text both ways, and pictures from
/// the browser. Used on one thread of the session's, so its calls never race.
pub trait WebClipboard: Send {
    /// Text someone copied on the host since the last call. Never what `set_text` put
    /// there, nor anything the OS marks as sensitive (passwords).
    fn poll(&mut self) -> Option<String>;
    /// Puts text from the browser on the host's clipboard.
    fn set_text(&mut self, text: &str) -> Result<(), String>;
    /// Puts a picture from the browser, with any text copied beside it, on the host's
    /// clipboard. `file` is a PNG, or a JPEG for a large photo (which clipboards want as
    /// PNG: see `nexa_clip::platform::png_of_picture`).
    fn set_image(&mut self, file: &[u8], text: Option<&str>) -> Result<(), String> {
        let _ = (file, text);
        Err("this host does not take pictures".into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebEvent {
    SessionStarted {
        id: u64,
        browser: String,
        remote: SocketAddr,
        fingerprint: DtlsFingerprint,
        input: bool,
        clipboard: bool,
        terminal: bool,
        /// False for a terminal-only session: the screen is not shown at all.
        video: bool,
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
    /// Every few seconds while a session shows the screen: what its stream did.
    StreamStats {
        id: u64,
        stats: StreamStats,
    },
}

/// What a browser's stream did over `over`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub over: Duration,
    /// Frames encoded and sent.
    pub frames: u32,
    /// Frames let go before encoding: the network loop was behind.
    pub skipped: u32,
    /// Of `frames`, still pictures sent again, or tried again after a failure.
    pub repeats: u32,
    pub failures: u32,
    pub encode_time: Duration,
    pub bytes: u64,
    /// The encoder's bitrate, from the browser's bandwidth estimate.
    pub target_bps: u32,
}

/// The machine and the person at it, as the agent provides them.
pub trait WebHost: Send + Sync + 'static {
    fn screen(&self) -> Result<WebScreen, String>;
    fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>>;
    /// The host's cursor, read beside the picture so the page can draw it at once.
    /// `None`: it is drawn into the picture instead, and follows the video's delay.
    fn cursor_source(&self) -> Option<Box<dyn nexa_media::CursorSource>> {
        None
    }
    fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>>;
    fn input_available(&self) -> bool;
    /// Called once per session on its input thread, only if input was allowed.
    fn input_sink(&self) -> Result<Box<dyn InputSink>, String>;
    /// Called once per session on a thread of its own, only if the clipboard was allowed;
    /// `poll` (every 250 ms, and it may be slow) and `set_text` are called there too.
    fn clipboard(&self) -> Result<Box<dyn WebClipboard>, String> {
        Err("this host does not share its clipboard".into())
    }
    /// Whether there is a screen to show. A command-line host has none: every session
    /// is the terminal.
    fn has_screen(&self) -> bool {
        true
    }
    /// Starts a shell for a session granted the terminal. Called when the page opens
    /// its terminal; dropping the result ends the shell.
    fn terminal(&self, cols: u16, rows: u16) -> Result<nexa_term::Pty, String> {
        let _ = (cols, rows);
        Err("this host has no terminal".into())
    }
    /// Whether the host has no room for another person: browsers are then told it is
    /// busy before any pairing starts.
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
    /// Browsers being let in or connected now.
    pub active: AtomicUsize,
    pub next_id: AtomicU64,
    /// Ends every browser session and exchange: the person at the host stopped them all.
    pub kick: tokio::sync::Notify,
    /// Ends one session, by id, while it runs.
    pub kicks: std::sync::Mutex<std::collections::HashMap<u64, Arc<tokio::sync::Notify>>>,
    /// HTTPS; plain HTTP then only redirects.
    pub tls: Option<WebTls>,
    /// The host's fixed password, if it has one, with the guard shared with native
    /// viewers so guesses count the same wherever they come from.
    pub password:
        std::sync::Mutex<Option<(Arc<nexa_auth::HostPassword>, nexa_auth::PasswordGuard)>>,
}

/// A browser the signaling server put through to this host (docs/WEB_CLIENT.md §12).
#[derive(Clone, Debug)]
pub struct RemoteBrowser {
    /// Where the browser is, as the server saw it.
    pub ip: IpAddr,
    /// TURN on the server, and this host's credentials for it.
    pub turn_server: SocketAddr,
    pub turn: TurnCredentials,
}

/// A running gateway. Dropping it stops accepting; sessions already running end when
/// their browser goes away.
pub struct WebGateway {
    local: SocketAddr,
    task: JoinHandle<()>,
    shared: Arc<Shared>,
}

impl WebGateway {
    /// Binds `addr` and starts serving, over HTTPS when `tls` is given. Must be called
    /// inside a Tokio runtime.
    /// Serves only browsers the signaling server puts through ([`WebGateway::serve_remote`]):
    /// no page and no listening socket here.
    pub fn remote_only(host: Arc<dyn WebHost>, code: OneTimeCode, config: WebConfig) -> Self {
        rtc::install_crypto();
        let shared = Self::shared(host, code, config, None);
        Self {
            local: SocketAddr::from(([0, 0, 0, 0], 0)),
            task: tokio::spawn(async {}),
            shared,
        }
    }

    fn shared(
        host: Arc<dyn WebHost>,
        code: OneTimeCode,
        config: WebConfig,
        tls: Option<WebTls>,
    ) -> Arc<Shared> {
        Arc::new(Shared {
            host,
            code,
            config,
            limiter: PairLimiter::default(),
            active: AtomicUsize::new(0),
            next_id: AtomicU64::new(1),
            kick: tokio::sync::Notify::new(),
            kicks: std::sync::Mutex::new(std::collections::HashMap::new()),
            tls,
            password: std::sync::Mutex::new(None),
        })
    }

    pub async fn bind(
        addr: SocketAddr,
        host: Arc<dyn WebHost>,
        code: OneTimeCode,
        config: WebConfig,
        tls: Option<WebTls>,
    ) -> io::Result<Self> {
        rtc::install_crypto();
        let listener = listen(addr)?;
        let local = listener.local_addr()?;
        let shared = Self::shared(host, code, config, tls);
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

    /// Runs a session for a browser that came through the signaling server: its frames
    /// arrive on `rx`, this host's go out on `tx`, media goes through TURN. It ends when
    /// `rx` closes or the session does, and `tx` is dropped then.
    pub fn serve_remote(
        &self,
        browser: RemoteBrowser,
        rx: tokio::sync::mpsc::Receiver<String>,
        tx: tokio::sync::mpsc::Sender<String>,
    ) -> JoinHandle<()> {
        let shared = self.shared.clone();
        tokio::spawn(session::run(
            session::Pipe::Relayed { rx, tx },
            SocketAddr::new(browser.ip, 0),
            rtc::Path::Turn {
                server: browser.turn_server,
                creds: browser.turn,
            },
            shared,
        ))
    }

    /// Lets browsers sign in with the host's fixed password as well as the one-time
    /// code; `None` takes that away. `guard` should be the native viewers' own.
    pub fn set_password(
        &self,
        password: Option<(Arc<nexa_auth::HostPassword>, nexa_auth::PasswordGuard)>,
    ) {
        *self
            .shared
            .password
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = password;
    }

    /// The certificate browsers see, when serving HTTPS.
    pub fn tls(&self) -> Option<&WebTls> {
        self.shared.tls.as_ref()
    }

    /// How many browsers are being let in or are connected.
    pub fn active_count(&self) -> usize {
        self.shared
            .active
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Ends every browser session and any exchange in progress.
    pub fn disconnect(&self) {
        self.shared.kick.notify_waiters();
    }

    /// Ends the browser session `id` ([`WebEvent::SessionStarted`]), if it still runs.
    pub fn disconnect_one(&self, id: u64) {
        let kicks = self.shared.kicks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = kicks.get(&id) {
            k.notify_one();
        }
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

/// A listening socket on `addr`. All IPv6 interfaces (`[::]`) take IPv4 as well, as
/// the native port does: Windows makes IPv6 sockets IPv6-only by default, which left a
/// browser at `https://192.168.x.y:47801` refused there.
fn listen(addr: SocketAddr) -> io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if let SocketAddr::V6(v6) = addr {
        socket.set_only_v6(!v6.ip().is_unspecified())?;
    }
    // As tokio's own bind: restart without waiting out old connections (Windows'
    // SO_REUSEADDR means something else, sharing a port, so not there).
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
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

    #[tokio::test]
    async fn all_interfaces_take_ipv4_browsers_too() {
        let any: SocketAddr = "[::]:0".parse().unwrap();
        let listener = listen(any).unwrap();
        // Explicitly, not by the platform default: Windows' default is IPv6 only.
        assert!(!socket2::SockRef::from(&listener).only_v6().unwrap());
        let port = listener.local_addr().unwrap().port();
        let client = tokio::net::TcpStream::connect(("127.0.0.1", port));
        let (accepted, connected) = tokio::join!(listener.accept(), client);
        connected.unwrap();
        let (_, peer) = accepted.unwrap();
        assert_eq!(peer.ip().to_canonical(), IpAddr::from([127, 0, 0, 1]));

        let v4 = listen("127.0.0.1:0".parse().unwrap()).unwrap();
        assert!(v4.local_addr().unwrap().is_ipv4());
    }
}
