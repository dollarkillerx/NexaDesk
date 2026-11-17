//! Browser access (`nexa-web`, docs/WEB_CLIENT.md) on top of this agent: the same
//! screen, input, one-time code, consent dialog and session indicator as native viewers.
//!
//! One session at a time across both: a browser is told the host is busy while a native
//! viewer is connected, and native viewers are refused while a browser session runs, so
//! the banner always describes the one session there is.

use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use nexa_auth::ConsentPrompt;
use nexa_clip::{ClipKind, DeviceId, FormatInfo};
use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig};
use nexa_session::clip::ClipPlatform;
use nexa_session::{
    handshake::{AuthKind, ConsentPeer, ConsentRequest},
    input::InputSink,
};
use nexa_web::{
    BrowserAnswer, BrowserAuth, BrowserRequest, WebClipboard, WebEvent, WebHost, WebScreen,
};
use nexa_wire::control::Permissions;

use crate::{AgentEvent, AgentUi, Platform, SessionInfo, agent::Shared};

/// Browser session ids are kept apart from native ones in the UI's events.
const ID_BASE: u64 = 1 << 48;

/// The browser session behind a session id the UI was given, if it is one.
pub(crate) fn browser_session(id: u64) -> Option<u64> {
    id.checked_sub(ID_BASE)
}

pub(crate) struct AgentWeb {
    pub platform: Arc<dyn Platform>,
    pub ui: Arc<dyn AgentUi>,
    pub shared: Weak<Shared>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn browser_name(browser: &str) -> String {
    format!("{browser} (browser)")
}

impl WebHost for AgentWeb {
    fn screen(&self) -> Result<WebScreen, String> {
        let s = self.platform.screen()?;
        Ok(WebScreen {
            display: s.display_id,
            origin_points: s.origin_points,
            size_points: s.size_points,
            size_pixels: s.size_pixels,
        })
    }

    fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        self.platform.capturer(config)
    }

    fn cursor_source(&self) -> Option<Box<dyn nexa_media::CursorSource>> {
        self.platform.cursor_source()
    }

    fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        self.platform.encoder(config)
    }

    fn input_available(&self) -> bool {
        self.platform.input_available()
    }

    fn terminal(&self, cols: u16, rows: u16) -> Result<nexa_term::Pty, String> {
        nexa_term::Pty::spawn(&nexa_term::Spawn {
            cols,
            rows,
            ..Default::default()
        })
        .map_err(|e| format!("could not start a shell: {e}"))
    }

    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        self.platform.input_sink()
    }

    fn clipboard(&self) -> Result<Box<dyn WebClipboard>, String> {
        // Everything is written inline, so nothing is ever promised or fetched later.
        let board = self.platform.clipboard()?;
        Ok(Box::new(BrowserClipboard {
            board,
            next: 1,
            // Only has to tell our own writes apart when they come back from the pasteboard.
            device: DeviceId(*b"nexa-web-browser"),
        }))
    }

    fn busy(&self) -> bool {
        self.shared.upgrade().is_none_or(|s| s.full_for(None))
    }

    fn ask(&self, r: &BrowserRequest, timeout: Duration) -> BrowserAnswer {
        // This browser already counts among the sessions: deny only if others filled up
        // the room while it was signing in.
        let over = self
            .shared
            .upgrade()
            .is_none_or(|s| s.session_count() > crate::agent::MAX_SESSIONS);
        if !self.ui.can_present() || over {
            return BrowserAnswer::DENY;
        }
        // The fixed password is the owner's own way in: nobody has to click Allow. The
        // banner still shows the session.
        if r.auth == BrowserAuth::Password {
            return BrowserAnswer {
                allow: true,
                input: r.input_available,
                clipboard: true,
                terminal: true,
            };
        }
        let request = ConsentRequest {
            peer: ConsentPeer::Browser(r.fingerprint.0),
            device_name: r.browser.clone(),
            remote: r.remote,
            auth: match r.auth {
                BrowserAuth::Request => AuthKind::Request,
                _ => AuthKind::Code,
            },
            prompt: ConsentPrompt {
                perms: Permissions {
                    view: true,
                    input: r.input_available && !r.terminal_only,
                    // Offered, not proposed: the person ticks it if they want it.
                    clipboard: false,
                    file: false,
                    audio: false,
                    // Proposed when it is what they asked for; unticked otherwise.
                    terminal: r.terminal_only,
                },
                offer_save: false,
                save_default: false,
                offer_skip_next_time: false,
            },
            takeover: false,
            cancelled: r.cancelled.clone(),
        };
        let answer = self.ui.ask(&request, timeout);
        BrowserAnswer {
            allow: answer.allow && answer.perms.view,
            input: answer.perms.input,
            clipboard: answer.perms.clipboard,
            terminal: answer.perms.terminal,
        }
    }

    fn event(&self, event: WebEvent) {
        let event = match event {
            WebEvent::SessionStarted {
                id,
                browser,
                remote,
                fingerprint,
                input,
                clipboard,
                terminal,
                video,
            } => AgentEvent::SessionStarted(SessionInfo {
                id: ID_BASE + id,
                device_name: browser_name(&browser),
                device_id: hex(&fingerprint.0[..16]),
                device_key: hex(&fingerprint.0),
                remote,
                perms: Permissions {
                    view: video,
                    input,
                    clipboard,
                    file: false,
                    audio: false,
                    terminal,
                },
            }),
            WebEvent::SessionEnded { id, reason } => AgentEvent::SessionEnded {
                id: ID_BASE + id,
                device_name: "browser".into(),
                reason,
            },
            WebEvent::Refused { remote, reason } => AgentEvent::ConnectionFailed {
                remote,
                reason: format!("browser: {reason}"),
            },
        };
        self.ui.event(event);
        if let Some(s) = self.shared.upgrade() {
            s.report_code();
        }
    }
}

/// The Mac's clipboard for one browser session: plain text both ways.
struct BrowserClipboard {
    board: Box<dyn ClipPlatform>,
    next: u64,
    device: DeviceId,
}

impl WebClipboard for BrowserClipboard {
    fn poll(&mut self) -> Option<String> {
        let snapshot = self.board.poll().ok()??;
        // Our own write coming back, or a password manager's: neither goes out.
        if snapshot.owner_marker || snapshot.sensitive {
            return None;
        }
        let (_, text) = snapshot.data.iter().find(|(k, _)| *k == ClipKind::Text)?;
        String::from_utf8(text.to_vec()).ok()
    }

    fn set_text(&mut self, text: &str) -> Result<(), String> {
        let generation = (self.next, self.device);
        self.next += 1;
        let bytes = bytes::Bytes::copy_from_slice(text.as_bytes());
        self.board
            .write_promise(
                generation,
                &[FormatInfo::of(ClipKind::Text, &bytes)],
                &[(ClipKind::Text, bytes)],
            )
            .map_err(|e| e.to_string())
    }
}
