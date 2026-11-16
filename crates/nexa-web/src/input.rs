//! Input and clipboard from the browser's `input` DataChannel, carried out on a thread of
//! its own.
//!
//! Messages are small JSON objects. Keys arrive as USB HID usages (the page maps
//! `KeyboardEvent.code`), positions as fractions of the picture, so the page never needs
//! to know the host's resolution and the host checks every value.
//!
//! Clipboard text travels on the same channel and is handled on the same thread as keys,
//! so text pasted from the browser is on the host's clipboard before the paste keystroke
//! that follows it is carried out. Text copied on the host goes back the same way.

use std::{
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use nexa_session::input::InputSink;
use serde::Deserialize;

use crate::{WebHost, WebScreen};

/// The HID keyboard page; keys travel to `InputSink` as `(page << 16) | usage`.
const PAGE_KEYBOARD: u32 = 0x07;
/// Keyboard page usages a browser can produce: letters through the right GUI key.
const HID_MIN: u32 = 0x04;
const HID_MAX: u32 = 0xE7;
const MAX_TEXT: usize = 256;
/// Clipboard text, UTF-8. Leaves room for JSON escaping under the DataChannel's 256 KiB.
pub(crate) const MAX_CLIP: usize = 192 * 1024;
/// Largest message the page is sent, encoded.
pub(crate) const MAX_OUTBOUND: usize = 255 * 1024;
/// How often the host's clipboard is looked at.
const CLIP_POLL: Duration = Duration::from_millis(250);
/// One wheel notch is 120; more than a few pages per event is not a person scrolling.
const MAX_WHEEL: i32 = 120 * 40;
const TICK: Duration = Duration::from_millis(100);
const QUEUE: usize = 512;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "t")]
pub(crate) enum Msg {
    /// Key: keyboard-page HID usage, down, and whether it is the browser's auto-repeat.
    #[serde(rename = "k")]
    Key {
        c: u32,
        d: bool,
        #[serde(default)]
        r: bool,
    },
    /// Move to a fraction of the picture.
    #[serde(rename = "m")]
    Move { x: f64, y: f64 },
    /// Button 0 left, 1 right, 2 middle, at a fraction of the picture.
    #[serde(rename = "b")]
    Button { b: u32, d: bool, x: f64, y: f64 },
    /// Wheel, in 120ths of a notch; positive `dy` scrolls content up.
    #[serde(rename = "w")]
    Wheel { dx: i32, dy: i32 },
    /// Committed text (IME, the phone keyboard).
    #[serde(rename = "s")]
    Text { s: String },
    /// Let go of everything: the page lost focus.
    #[serde(rename = "r")]
    Release,
    /// Text the browser has on its clipboard, put on the host's (only with permission).
    #[serde(rename = "c")]
    Clip { s: String },
}

impl Msg {
    /// Parses and checks one message; anything odd is dropped, not clamped into meaning.
    pub(crate) fn parse(data: &[u8]) -> Option<Self> {
        let msg: Msg = serde_json::from_slice(data).ok()?;
        let fraction = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
        let ok = match &msg {
            Msg::Key { c, .. } => (HID_MIN..=HID_MAX).contains(c),
            Msg::Move { x, y } => fraction(*x) && fraction(*y),
            Msg::Button { b, x, y, .. } => *b < 3 && fraction(*x) && fraction(*y),
            Msg::Wheel { dx, dy } => dx.abs() <= MAX_WHEEL && dy.abs() <= MAX_WHEEL,
            Msg::Text { s } => {
                !s.is_empty() && s.len() <= MAX_TEXT && !s.chars().any(char::is_control)
            }
            Msg::Release => true,
            Msg::Clip { s } => s.len() <= MAX_CLIP && !s.contains('\0'),
        };
        ok.then_some(msg)
    }
}

/// A fraction of the picture in the platform's global points.
pub(crate) fn to_points(screen: &WebScreen, x: f64, y: f64) -> (f64, f64) {
    (
        screen.origin_points.0 + x * screen.size_points.0,
        screen.origin_points.1 + y * screen.size_points.1,
    )
}

pub(crate) struct InputThread {
    tx: Option<mpsc::SyncSender<Msg>>,
    handle: Option<JoinHandle<()>>,
}

/// What the thread may do for this session.
pub(crate) struct Grants {
    pub input: bool,
    pub clipboard: bool,
}

impl InputThread {
    /// `to_browser` receives text copied on the host, already encoded for the page.
    pub(crate) fn spawn(
        host: Arc<dyn WebHost>,
        screen: WebScreen,
        grants: Grants,
        to_browser: tokio::sync::mpsc::Sender<String>,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Msg>(QUEUE);
        let handle = thread::Builder::new()
            .name("web-input".into())
            .spawn(move || {
                let mut sink = grants.input.then(|| host.input_sink().ok()).flatten();
                let mut clip = grants.clipboard.then(|| host.clipboard().ok()).flatten();
                // What was on the clipboard before the session is not the browser's.
                if let Some(c) = clip.as_mut() {
                    let _ = c.poll();
                }
                let mut polled = Instant::now();
                loop {
                    match rx.recv_timeout(TICK) {
                        Ok(Msg::Clip { s }) => {
                            if let Some(c) = clip.as_mut() {
                                let _ = c.set_text(&s);
                            }
                        }
                        Ok(msg) => {
                            if let Some(sink) = sink.as_mut() {
                                carry_out(sink.as_mut(), &screen, msg);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if let Some(sink) = sink.as_mut() {
                                sink.poll_timeout(Instant::now());
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            if let Some(sink) = sink.as_mut() {
                                sink.release_all(Instant::now());
                            }
                            return;
                        }
                    }
                    if let Some(c) = clip.as_mut()
                        && polled.elapsed() >= CLIP_POLL
                    {
                        polled = Instant::now();
                        if let Some(text) = c.poll().and_then(|t| clip_message(&t)) {
                            let _ = to_browser.try_send(text);
                        }
                    }
                }
            })
            .expect("spawn input thread");
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    /// Never blocks: a flooded queue drops input rather than stalling the network loop.
    pub(crate) fn send(&self, msg: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(msg);
        }
    }
}

fn carry_out(sink: &mut dyn InputSink, screen: &WebScreen, msg: Msg) {
    let now = Instant::now();
    match msg {
        Msg::Key { c, d, r } => sink.key((PAGE_KEYBOARD << 16) | c, d, r, now),
        Msg::Move { x, y } => {
            let (px, py) = to_points(screen, x, y);
            sink.move_absolute(px, py);
        }
        Msg::Button { b, d, x, y } => sink.button(b, d, Some(to_points(screen, x, y)), now),
        Msg::Wheel { dx, dy } => sink.wheel(dx, dy),
        Msg::Text { s } => sink.text(&s),
        Msg::Release => sink.release_all(now),
        Msg::Clip { .. } => {}
    }
}

/// Host clipboard text as the page receives it, or nothing if it is too large to send.
pub(crate) fn clip_message(text: &str) -> Option<String> {
    if text.is_empty() || text.len() > MAX_CLIP {
        return None;
    }
    let encoded = serde_json::json!({"t": "c", "s": text}).to_string();
    (encoded.len() <= MAX_OUTBOUND).then_some(encoded)
}

impl Drop for InputThread {
    /// Releases every held key and button before the session is gone.
    fn drop(&mut self) {
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_parse_and_are_checked() {
        assert_eq!(
            Msg::parse(br#"{"t":"k","c":4,"d":true}"#),
            Some(Msg::Key {
                c: 4,
                d: true,
                r: false
            })
        );
        assert_eq!(
            Msg::parse(br#"{"t":"k","c":227,"d":true,"r":true}"#),
            Some(Msg::Key {
                c: 0xE3,
                d: true,
                r: true
            })
        );
        assert_eq!(Msg::parse(br#"{"t":"k","c":3,"d":true}"#), None);
        assert_eq!(Msg::parse(br#"{"t":"k","c":232,"d":true}"#), None);
        assert_eq!(
            Msg::parse(br#"{"t":"m","x":0.5,"y":1}"#),
            Some(Msg::Move { x: 0.5, y: 1.0 })
        );
        assert_eq!(Msg::parse(br#"{"t":"m","x":1.5,"y":0}"#), None);
        assert_eq!(Msg::parse(br#"{"t":"b","b":3,"d":true,"x":0,"y":0}"#), None);
        assert_eq!(Msg::parse(br#"{"t":"w","dx":0,"dy":99999}"#), None);
        assert_eq!(
            Msg::parse(r#"{"t":"s","s":"你好"}"#.as_bytes()),
            Some(Msg::Text { s: "你好".into() })
        );
        assert_eq!(Msg::parse(br#"{"t":"s","s":"a\u0007"}"#), None);
        assert_eq!(Msg::parse(br#"{"t":"r"}"#), Some(Msg::Release));
        assert_eq!(
            Msg::parse(br#"{"t":"c","s":"line 1\nline 2\t"}"#),
            Some(Msg::Clip {
                s: "line 1\nline 2\t".into()
            })
        );
        assert_eq!(Msg::parse(br#"{"t":"c","s":"a\u0000"}"#), None);
        let big = format!(r#"{{"t":"c","s":"{}"}}"#, "x".repeat(MAX_CLIP + 1));
        assert_eq!(Msg::parse(big.as_bytes()), None);
        assert_eq!(Msg::parse(br#"{"t":"x"}"#), None);
        assert_eq!(Msg::parse(b"not json"), None);
    }

    #[test]
    fn host_clipboard_text_is_sent_unless_it_is_too_large() {
        assert_eq!(
            clip_message("a\"b").as_deref(),
            Some(r#"{"s":"a\"b","t":"c"}"#)
        );
        assert_eq!(clip_message(""), None);
        assert_eq!(clip_message(&"x".repeat(MAX_CLIP + 1)), None);
        // Escaping can make text that fits too large to send.
        assert_eq!(clip_message(&"\u{1}".repeat(MAX_CLIP / 2)), None);
    }

    #[test]
    fn fractions_map_to_the_display() {
        let s = WebScreen {
            display: 1,
            origin_points: (-1440.0, 100.0),
            size_points: (1440.0, 900.0),
            size_pixels: (2880, 1800),
        };
        assert_eq!(to_points(&s, 0.0, 0.0), (-1440.0, 100.0));
        assert_eq!(to_points(&s, 0.5, 1.0), (-720.0, 1000.0));
    }
}
