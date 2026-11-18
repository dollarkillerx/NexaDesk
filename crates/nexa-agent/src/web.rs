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
    BrowserAnswer, BrowserAuth, BrowserRequest, StreamStats, WebClipboard, WebDisplay, WebEvent,
    WebHost, WebScreen,
};
use nexa_wire::control::Permissions;

use crate::{AgentEvent, AgentUi, Platform, ScreenInfo, SessionInfo, agent::Shared};

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

/// A browser stream's statistics as the app's sessions write theirs.
fn stream_stats_line(session: u64, s: &StreamStats) -> String {
    let secs = s.over.as_secs_f64().max(0.001);
    let per_s = |n: u32| f64::from(n) / secs;
    let encode_ms = if s.frames + s.failures == 0 {
        0.0
    } else {
        s.encode_time.as_secs_f64() * 1000.0 / f64::from(s.frames + s.failures)
    };
    format!(
        "nexa-agent: session {session} stream {}x{}@{} (browser): {:.1} frames/s, {:.1} skipped/s, {:.1} repeats/s, {} failures, encode {encode_ms:.1} ms, sent {:.2} Mbit/s, target {:.2} Mbit/s",
        s.width,
        s.height,
        s.fps,
        per_s(s.frames),
        per_s(s.skipped),
        per_s(s.repeats),
        s.failures,
        s.bytes as f64 * 8.0 / secs / 1e6,
        f64::from(s.target_bps) / 1e6,
    )
}

fn browser_name(browser: &str) -> String {
    format!("{browser} (browser)")
}

fn web_screen(s: &ScreenInfo) -> WebScreen {
    WebScreen {
        display: s.display_id,
        origin_points: s.origin_points,
        size_points: s.size_points,
        size_pixels: s.size_pixels,
    }
}

impl WebHost for AgentWeb {
    fn screen(&self) -> Result<WebScreen, String> {
        Ok(web_screen(&self.platform.screen()?))
    }

    /// As native viewers see them (§6.7): the main display first, each one's place in
    /// the list its id.
    fn displays(&self) -> Result<Vec<WebDisplay>, String> {
        Ok(self
            .platform
            .displays()?
            .iter()
            .map(|s| WebDisplay {
                name: s.name.clone(),
                screen: web_screen(s),
            })
            .collect())
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
            // For agent.log, beside the app's sessions' lines; nothing for the UI.
            WebEvent::StreamStats { id, stats } => {
                eprintln!("{}", stream_stats_line(ID_BASE + id, &stats));
                return;
            }
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
        self.write(&[(ClipKind::Text, text.as_bytes())])
    }

    fn set_image(&mut self, file: &[u8], text: Option<&str>) -> Result<(), String> {
        // A large photo comes as JPEG; the clipboard is given PNG.
        let converted;
        let png = if file.starts_with(b"\x89PNG") {
            file
        } else {
            converted = nexa_clip::platform::png_of_picture(file)
                .ok_or("the picture from the browser could not be read")?;
            &converted
        };
        let mut content = vec![(ClipKind::Png, png)];
        content.extend(text.map(|t| (ClipKind::Text, t.as_bytes())));
        self.write(&content)
    }
}

impl BrowserClipboard {
    /// Puts `content` on the clipboard, all of it inline.
    fn write(&mut self, content: &[(ClipKind, &[u8])]) -> Result<(), String> {
        let generation = (self.next, self.device);
        self.next += 1;
        let inline: Vec<(ClipKind, bytes::Bytes)> = content
            .iter()
            .map(|(kind, data)| (*kind, bytes::Bytes::copy_from_slice(data)))
            .collect();
        let formats: Vec<FormatInfo> = inline
            .iter()
            .map(|(kind, data)| FormatInfo::of(*kind, data))
            .collect();
        self.board
            .write_promise(generation, &formats, &inline)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_display_reaches_the_browser_with_its_place_on_the_desktop() {
        let s = ScreenInfo {
            display_id: 5,
            origin_points: (1512.0, -200.0),
            size_points: (1920.0, 1080.0),
            size_pixels: (3840, 2160),
            refresh_hz: 60,
            name: "DELL U2720Q".into(),
        };
        assert_eq!(
            web_screen(&s),
            WebScreen {
                display: 5,
                origin_points: (1512.0, -200.0),
                size_points: (1920.0, 1080.0),
                size_pixels: (3840, 2160),
            }
        );
    }

    #[test]
    fn a_browser_stream_is_logged_as_the_apps_are() {
        let stats = StreamStats {
            width: 2560,
            height: 1440,
            fps: 60,
            over: Duration::from_secs(5),
            frames: 150,
            skipped: 5,
            repeats: 10,
            failures: 1,
            encode_time: Duration::from_millis(1510),
            bytes: 12_500_000,
            target_bps: 22_000_000,
        };
        assert_eq!(
            stream_stats_line(ID_BASE + 1, &stats),
            "nexa-agent: session 281474976710657 stream 2560x1440@60 (browser): 30.0 frames/s, \
             1.0 skipped/s, 2.0 repeats/s, 1 failures, encode 10.0 ms, sent 20.00 Mbit/s, \
             target 22.00 Mbit/s"
        );
        // Nothing sent: no division by nothing.
        let idle = StreamStats {
            over: Duration::from_secs(5),
            ..StreamStats::default()
        };
        assert!(stream_stats_line(1, &idle).contains("encode 0.0 ms, sent 0.00 Mbit/s"));
    }
}
