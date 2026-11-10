//! Input from the browser's `input` DataChannel, carried out on a thread of its own.
//!
//! Messages are small JSON objects. Keys arrive as USB HID usages (the page maps
//! `KeyboardEvent.code`), positions as fractions of the picture, so the page never needs
//! to know the host's resolution and the host checks every value.

use std::{
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::{WebHost, WebScreen};

/// The HID keyboard page; keys travel to `InputSink` as `(page << 16) | usage`.
const PAGE_KEYBOARD: u32 = 0x07;
/// Keyboard page usages a browser can produce: letters through the right GUI key.
const HID_MIN: u32 = 0x04;
const HID_MAX: u32 = 0xE7;
const MAX_TEXT: usize = 256;
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

impl InputThread {
    pub(crate) fn spawn(host: Arc<dyn WebHost>, screen: WebScreen) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Msg>(QUEUE);
        let handle = thread::Builder::new()
            .name("web-input".into())
            .spawn(move || {
                let mut sink = match host.input_sink() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                loop {
                    match rx.recv_timeout(TICK) {
                        Ok(msg) => {
                            let now = Instant::now();
                            match msg {
                                Msg::Key { c, d, r } => {
                                    sink.key((PAGE_KEYBOARD << 16) | c, d, r, now)
                                }
                                Msg::Move { x, y } => {
                                    let (px, py) = to_points(&screen, x, y);
                                    sink.move_absolute(px, py);
                                }
                                Msg::Button { b, d, x, y } => {
                                    sink.button(b, d, Some(to_points(&screen, x, y)), now)
                                }
                                Msg::Wheel { dx, dy } => sink.wheel(dx, dy),
                                Msg::Text { s } => sink.text(&s),
                                Msg::Release => sink.release_all(now),
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => sink.poll_timeout(Instant::now()),
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            sink.release_all(Instant::now());
                            return;
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
        assert_eq!(Msg::parse(br#"{"t":"x"}"#), None);
        assert_eq!(Msg::parse(b"not json"), None);
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
