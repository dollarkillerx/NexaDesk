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
use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig};
use nexa_session::{
    handshake::{AuthKind, ConsentPeer, ConsentRequest},
    input::InputSink,
};
use nexa_web::{BrowserAnswer, BrowserRequest, WebEvent, WebHost, WebScreen};
use nexa_wire::control::Permissions;

use crate::{AgentEvent, AgentUi, Platform, SessionInfo, agent::Shared};

/// Browser session ids are kept apart from native ones in the UI's events.
const ID_BASE: u64 = 1 << 48;

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

    fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        self.platform.encoder(config)
    }

    fn input_available(&self) -> bool {
        self.platform.input_available()
    }

    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        self.platform.input_sink()
    }

    fn busy(&self) -> bool {
        self.shared.upgrade().is_none_or(|s| s.session_active())
    }

    fn ask(&self, r: &BrowserRequest, timeout: Duration) -> BrowserAnswer {
        if !self.ui.can_present() || self.busy() {
            return BrowserAnswer::DENY;
        }
        let request = ConsentRequest {
            peer: ConsentPeer::Browser(r.fingerprint.0),
            device_name: r.browser.clone(),
            remote: r.remote,
            auth: AuthKind::Code,
            prompt: ConsentPrompt {
                perms: Permissions {
                    view: true,
                    input: r.input_available,
                    clipboard: false,
                    file: false,
                    audio: false,
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
            } => AgentEvent::SessionStarted(SessionInfo {
                id: ID_BASE + id,
                device_name: browser_name(&browser),
                device_id: hex(&fingerprint.0[..16]),
                device_key: hex(&fingerprint.0),
                remote,
                perms: Permissions {
                    view: true,
                    input,
                    clipboard: false,
                    file: false,
                    audio: false,
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
