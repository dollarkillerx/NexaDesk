//! Input and clipboard from the browser's `input` DataChannel, carried out on a thread of
//! its own.
//!
//! Messages are small JSON objects. Keys arrive as USB HID usages (the page maps
//! `KeyboardEvent.code`), positions as fractions of the picture, so the page never needs
//! to know the host's resolution and the host checks every value.
//!
//! Clipboard text travels on the same channel and is written on the same thread as keys,
//! so text pasted from the browser is on the host's clipboard before the paste keystroke
//! that follows it is carried out. Text copied on the host is looked for on a thread of
//! its own ([`ClipWatch`]) and goes back on the same channel.

use std::{
    sync::{
        Arc,
        mpsc::{self, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use data_encoding::BASE64;
use nexa_session::input::InputSink;
use serde::Deserialize;

use crate::{WebHost, WebScreen, display::Shown};

/// The HID keyboard page; keys travel to `InputSink` as `(page << 16) | usage`.
const PAGE_KEYBOARD: u32 = 0x07;
/// Keyboard page usages a browser can produce: letters through the right GUI key.
const HID_MIN: u32 = 0x04;
const HID_MAX: u32 = 0xE7;
const MAX_TEXT: usize = 256;
/// More keys than anyone holds at once.
const MAX_HELD: usize = 32;
/// Clipboard text, UTF-8. Leaves room for JSON escaping under the DataChannel's 256 KiB.
pub(crate) const MAX_CLIP: usize = 192 * 1024;
/// Largest message the page is sent, encoded.
pub(crate) const MAX_OUTBOUND: usize = 255 * 1024;
/// How often the host's clipboard is looked at.
const CLIP_POLL: Duration = Duration::from_millis(250);
/// How long a paste waits for the host's clipboard to take the browser's text before
/// the keys after it go on anyway.
const CLIP_WRITE_WAIT: Duration = Duration::from_secs(1);
/// The same for a picture, which the host may have to convert.
const IMAGE_WRITE_WAIT: Duration = Duration::from_secs(3);
/// The largest picture a browser may paste, as a PNG file.
pub(crate) const MAX_IMAGE: usize = 16 * 1024 * 1024;
/// One piece of it, base64 (pages before binary pieces sent 96 KiB at a time).
const MAX_IMAGE_PART: usize = 192 * 1024;
/// One binary piece: the page sends 64 KiB at a time.
const MAX_IMAGE_BYTES: usize = 128 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG_SIGNATURE: &[u8] = &[0xFF, 0xD8, 0xFF];
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
    /// The keys the page holds (HID usages), sent every second while it holds keys or
    /// buttons. As the app's key-state sync, it lets go of any other key and keeps what
    /// is held from being let go after 3 s quiet, a long drag included.
    #[serde(rename = "ks")]
    KeyState { c: Vec<u32> },
    /// Text the browser has on its clipboard, put on the host's (only with permission).
    #[serde(rename = "c")]
    Clip { s: String },
    /// A piece of a picture the browser pastes, base64, in order (pages before binary
    /// pieces).
    #[serde(rename = "ci")]
    ClipImage { s: String },
    /// A piece of a picture the browser pastes, in order: a binary message on the input
    /// channel, never JSON ([`Msg::picture_piece`]).
    #[serde(skip)]
    ClipImageBytes(Vec<u8>),
    /// The picture is whole: `n` bytes of a PNG, or of a JPEG (`f: "jpeg"`) for a large
    /// photo, with any text copied beside it. It goes on the host's clipboard before the
    /// paste keystroke after it.
    #[serde(rename = "ce")]
    ClipImageEnd {
        n: usize,
        #[serde(default)]
        s: String,
        #[serde(default)]
        f: String,
    },
}

impl Msg {
    /// A binary message on the input channel: a piece of a pasted picture, if not too big.
    pub(crate) fn picture_piece(data: Vec<u8>) -> Option<Self> {
        (data.len() <= MAX_IMAGE_BYTES).then_some(Self::ClipImageBytes(data))
    }

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
            Msg::KeyState { c } => {
                c.len() <= MAX_HELD && c.iter().all(|c| (HID_MIN..=HID_MAX).contains(c))
            }
            Msg::Clip { s } => s.len() <= MAX_CLIP && !s.contains('\0'),
            Msg::ClipImage { s } => s.len() <= MAX_IMAGE_PART,
            Msg::ClipImageBytes(_) => false,
            Msg::ClipImageEnd { n, s, f } => {
                *n <= MAX_IMAGE
                    && s.len() <= MAX_CLIP
                    && !s.contains('\0')
                    && matches!(f.as_str(), "" | "png" | "jpeg")
            }
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
    /// Positions land on the display `shown` is when each arrives.
    pub(crate) fn spawn(
        host: Arc<dyn WebHost>,
        shown: Arc<Shown>,
        grants: Grants,
        to_browser: tokio::sync::mpsc::Sender<String>,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Msg>(QUEUE);
        let handle = thread::Builder::new()
            .name("web-input".into())
            .spawn(move || {
                let clip = grants
                    .clipboard
                    .then(|| ClipWatch::spawn(host.clone(), to_browser));
                let mut sink = grants.input.then(|| host.input_sink().ok()).flatten();
                let mut picture = Picture::default();
                loop {
                    match rx.recv_timeout(TICK) {
                        Ok(Msg::Clip { s }) => {
                            if let Some(c) = &clip {
                                c.set_text(&s);
                            }
                        }
                        // Nothing is gathered for a session without the clipboard.
                        Ok(Msg::ClipImage { s }) if clip.is_some() => picture.add_base64(&s),
                        Ok(Msg::ClipImageBytes(b)) if clip.is_some() => picture.add(&b),
                        Ok(Msg::ClipImageEnd { n, s, f }) => {
                            if let (Some(c), Some(file)) = (&clip, picture.finish(n, &f)) {
                                c.set_image(file, (!s.is_empty()).then_some(s));
                            }
                        }
                        Ok(msg) => {
                            if let Some(sink) = sink.as_mut() {
                                carry_out(sink.as_mut(), &shown.screen(), msg);
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

/// The host's clipboard for one session, made, looked at and written on a thread of its
/// own. Looking can be slow (on a Mac every change is read whole, a copied picture
/// converted to PNG before its text is found) and making or reading it can fail outright;
/// it used to happen between input messages on the input thread, where a slow look held
/// up the pointer and keys and a panic ended them for the rest of the session. Only
/// sessions with the clipboard did that, and a fixed password always grants it.
struct ClipWatch {
    /// What the browser puts on the host's clipboard, and where to say it is there.
    writes: mpsc::Sender<(Write, mpsc::SyncSender<()>)>,
}

enum Write {
    Text(String),
    Image { file: Vec<u8>, text: Option<String> },
}

/// A picture the browser is pasting, as its pieces arrive.
#[derive(Default)]
struct Picture {
    bytes: Vec<u8>,
    /// A piece that was not base64, or one too many: the picture is dropped.
    broken: bool,
}

impl Picture {
    fn add(&mut self, piece: &[u8]) {
        if self.broken {
            return;
        }
        if self.bytes.len() + piece.len() <= MAX_IMAGE {
            self.bytes.extend_from_slice(piece);
        } else {
            self.broken();
        }
    }

    fn add_base64(&mut self, piece: &str) {
        match BASE64.decode(piece.as_bytes()) {
            Ok(b) => self.add(&b),
            Err(_) => self.broken(),
        }
    }

    fn broken(&mut self) {
        self.broken = true;
        self.bytes = Vec::new();
    }

    /// The file, if all `n` bytes of a PNG, or of a JPEG when `kind` says so, arrived; the
    /// next picture starts afresh either way.
    fn finish(&mut self, n: usize, kind: &str) -> Option<Vec<u8>> {
        let Picture { bytes, broken } = std::mem::take(self);
        let signature = if kind == "jpeg" {
            JPEG_SIGNATURE
        } else {
            PNG_SIGNATURE
        };
        (!broken && bytes.len() == n && bytes.starts_with(signature)).then_some(bytes)
    }
}

impl ClipWatch {
    /// `to_browser` receives text copied on the host, encoded for the page. The thread
    /// ends when this is dropped, once a look under way returns; it is not waited for.
    fn spawn(host: Arc<dyn WebHost>, to_browser: tokio::sync::mpsc::Sender<String>) -> Self {
        let (writes, requests) = mpsc::channel::<(Write, mpsc::SyncSender<()>)>();
        let _ = thread::Builder::new()
            .name("web-clipboard".into())
            .spawn(move || {
                let Ok(mut clip) = host.clipboard() else {
                    return;
                };
                // What was on the clipboard before the session is not the browser's.
                let _ = clip.poll();
                let mut polled = Instant::now();
                loop {
                    match requests.recv_timeout(CLIP_POLL.saturating_sub(polled.elapsed())) {
                        Ok((write, done)) => {
                            let _ = match write {
                                Write::Text(text) => clip.set_text(&text),
                                Write::Image { file, text } => {
                                    clip.set_image(&file, text.as_deref())
                                }
                            };
                            let _ = done.send(());
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            polled = Instant::now();
                            if let Some(text) = clip.poll().and_then(|t| clip_message(&t)) {
                                if to_browser.is_closed() {
                                    return;
                                }
                                let _ = to_browser.try_send(text);
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            });
        Self { writes }
    }

    /// Puts the browser's text on the host's clipboard before the input after it: a paste
    /// made while the clipboard is still being opened waits for it. A clipboard that is
    /// stuck holds the keys up for `CLIP_WRITE_WAIT` at most; one that could not be
    /// opened, not at all.
    fn set_text(&self, text: &str) {
        self.write(Write::Text(text.to_owned()), CLIP_WRITE_WAIT);
    }

    /// As `set_text`, for a picture (PNG or JPEG), which may take the host longer.
    fn set_image(&self, file: Vec<u8>, text: Option<String>) {
        self.write(Write::Image { file, text }, IMAGE_WRITE_WAIT);
    }

    fn write(&self, write: Write, wait: Duration) {
        let (done, written) = mpsc::sync_channel(1);
        if self.writes.send((write, done)).is_ok() {
            let _ = written.recv_timeout(wait);
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
        Msg::KeyState { c } => {
            let held: Vec<u32> = c.iter().map(|c| (PAGE_KEYBOARD << 16) | c).collect();
            sink.apply_key_state(&held, now);
        }
        Msg::Clip { .. }
        | Msg::ClipImage { .. }
        | Msg::ClipImageBytes(_)
        | Msg::ClipImageEnd { .. } => {}
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
            Msg::parse(br#"{"t":"ks","c":[224,6]}"#),
            Some(Msg::KeyState {
                c: vec![0xE0, 0x06]
            })
        );
        assert_eq!(
            Msg::parse(br#"{"t":"ks","c":[]}"#),
            Some(Msg::KeyState { c: vec![] })
        );
        assert_eq!(Msg::parse(br#"{"t":"ks","c":[3]}"#), None);
        let many = format!(
            r#"{{"t":"ks","c":[{}]}}"#,
            vec!["4"; MAX_HELD + 1].join(",")
        );
        assert_eq!(Msg::parse(many.as_bytes()), None);
        assert_eq!(
            Msg::parse(br#"{"t":"c","s":"line 1\nline 2\t"}"#),
            Some(Msg::Clip {
                s: "line 1\nline 2\t".into()
            })
        );
        assert_eq!(Msg::parse(br#"{"t":"c","s":"a\u0000"}"#), None);
        assert_eq!(
            Msg::parse(br#"{"t":"ci","s":"iVBO"}"#),
            Some(Msg::ClipImage { s: "iVBO".into() })
        );
        let big = format!(r#"{{"t":"ci","s":"{}"}}"#, "A".repeat(MAX_IMAGE_PART + 4));
        assert_eq!(Msg::parse(big.as_bytes()), None);
        assert_eq!(
            Msg::parse(br#"{"t":"ce","n":12}"#),
            Some(Msg::ClipImageEnd {
                n: 12,
                s: String::new(),
                f: String::new(),
            })
        );
        let huge = format!(r#"{{"t":"ce","n":{}}}"#, MAX_IMAGE + 1);
        assert_eq!(Msg::parse(huge.as_bytes()), None);
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

    use std::sync::{Mutex, MutexGuard};

    use crate::{WebClipboard, testing::FakeHost};

    type Log = Arc<Mutex<Vec<String>>>;

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What the host's clipboard does.
    #[derive(Clone, Copy)]
    enum Board {
        /// Every look takes this long, as a Mac reading a large copied picture does.
        Slow(Duration),
        /// Takes this long to open, then is quick.
        SlowToOpen(Duration),
        /// Cannot be made at all.
        Panics,
    }

    struct Sink(Log);

    impl InputSink for Sink {
        fn key(&mut self, hid: u32, down: bool, _: bool, _: Instant) {
            lock(&self.0).push(format!("key {:x} {down}", hid & 0xffff));
        }
        fn move_absolute(&mut self, x: f64, y: f64) {
            lock(&self.0).push(format!("move {x} {y}"));
        }
        fn move_relative(&mut self, _: i32, _: i32) {}
        fn button(&mut self, _: u32, _: bool, _: Option<(f64, f64)>, _: Instant) {}
        fn wheel(&mut self, _: i32, _: i32) {}
        fn text(&mut self, _: &str) {}
        fn apply_key_state(&mut self, pressed: &[u32], _: Instant) {
            lock(&self.0).push(format!("held {pressed:x?}"));
        }
        fn poll_timeout(&mut self, _: Instant) {}
        fn release_all(&mut self, _: Instant) {}
    }

    struct Clip(Log, Duration);

    impl WebClipboard for Clip {
        fn poll(&mut self) -> Option<String> {
            thread::sleep(self.1);
            None
        }
        fn set_text(&mut self, text: &str) -> Result<(), String> {
            lock(&self.0).push(format!("clipboard {text}"));
            Ok(())
        }
        fn set_image(&mut self, png: &[u8], text: Option<&str>) -> Result<(), String> {
            lock(&self.0).push(format!("picture {} {text:?}", png.len()));
            Ok(())
        }
    }

    /// Input and clipboard granted, as a fixed password grants them.
    fn session(board: Board) -> (InputThread, Log) {
        let (input, log, _) = session_on(board);
        (input, log)
    }

    /// As `session`, with the display it shows.
    fn session_on(board: Board) -> (InputThread, Log, Arc<Shown>) {
        let log = Log::default();
        let (sinks, boards) = (log.clone(), log.clone());
        let host = Arc::new(
            FakeHost::default()
                .with_input_sink(move || Ok(Box::new(Sink(sinks.clone()))))
                .with_clipboard(move || match board {
                    Board::Slow(d) => Ok(Box::new(Clip(boards.clone(), d))),
                    Board::SlowToOpen(d) => {
                        thread::sleep(d);
                        Ok(Box::new(Clip(boards.clone(), Duration::ZERO)))
                    }
                    Board::Panics => panic!("the pasteboard cannot be opened"),
                }),
        );
        let screen = WebScreen {
            display: 1,
            origin_points: (0.0, 0.0),
            size_points: (100.0, 100.0),
            size_pixels: (200, 200),
        };
        let (to_browser, _) = tokio::sync::mpsc::channel(4);
        let grants = Grants {
            input: true,
            clipboard: true,
        };
        let shown = Shown::new(0, screen);
        let input = InputThread::spawn(host, shown.clone(), grants, to_browser);
        (input, log, shown)
    }

    #[test]
    fn after_a_switch_the_pointer_lands_on_the_new_display() {
        let (input, log, shown) = session_on(Board::Slow(Duration::ZERO));
        input.send(Msg::Move { x: 0.5, y: 0.25 });
        assert_eq!(wait_for(&log, 1, Duration::from_secs(1)), ["move 50 25"]);
        // A display left of and above the main one, as on a Mac.
        shown.set(
            1,
            WebScreen {
                display: 2,
                origin_points: (-1920.0, -300.0),
                size_points: (1920.0, 1080.0),
                size_pixels: (3840, 2160),
            },
        );
        input.send(Msg::Move { x: 0.5, y: 0.5 });
        input.send(Msg::Move { x: 1.0, y: 1.0 });
        assert_eq!(
            wait_for(&log, 3, Duration::from_secs(1)),
            ["move 50 25", "move -960 240", "move 0 780"]
        );
    }

    fn wait_for(log: &Log, n: usize, within: Duration) -> Vec<String> {
        let deadline = Instant::now() + within;
        while lock(log).len() < n && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        lock(log).clone()
    }

    /// A PNG's signature and some bytes, as the page sends them: base64, in pieces.
    fn pieces(png: &[u8]) -> Vec<Msg> {
        png.chunks(9)
            .map(|c| Msg::ClipImage {
                s: BASE64.encode(c),
            })
            .collect()
    }

    fn fake_png() -> Vec<u8> {
        [PNG_SIGNATURE, b"0123456789abcdef"].concat()
    }

    #[test]
    fn a_pasted_picture_is_on_the_clipboard_before_the_key_after_it() {
        let (input, log) = session(Board::Slow(Duration::ZERO));
        let png = fake_png();
        for piece in pieces(&png) {
            input.send(piece);
        }
        input.send(Msg::ClipImageEnd {
            n: png.len(),
            s: "caption".into(),
            f: String::new(),
        });
        input.send(Msg::Key {
            c: 0x19,
            d: true,
            r: false,
        });
        assert_eq!(
            wait_for(&log, 2, Duration::from_secs(2)),
            ["picture 24 Some(\"caption\")", "key 19 true"]
        );
    }

    #[test]
    fn a_large_photo_comes_as_binary_jpeg_pieces() {
        let (input, log) = session(Board::Slow(Duration::ZERO));
        let jpeg = [JPEG_SIGNATURE, b"\xE0 a photo"].concat();
        for piece in jpeg.chunks(5) {
            input.send(Msg::picture_piece(piece.to_vec()).expect("small"));
        }
        input.send(Msg::ClipImageEnd {
            n: jpeg.len(),
            s: String::new(),
            f: "jpeg".into(),
        });
        // A PNG said to be a JPEG is not one.
        for piece in pieces(&fake_png()) {
            input.send(piece);
        }
        input.send(Msg::ClipImageEnd {
            n: fake_png().len(),
            s: String::new(),
            f: "jpeg".into(),
        });
        input.send(Msg::Key {
            c: 0x19,
            d: true,
            r: false,
        });
        assert_eq!(
            wait_for(&log, 2, Duration::from_secs(2)),
            ["picture 12 None", "key 19 true"]
        );
        assert_eq!(Msg::picture_piece(vec![0; MAX_IMAGE_BYTES + 1]), None);
        assert_eq!(
            Msg::parse(br#"{"t":"ce","n":3,"f":"gif"}"#),
            None,
            "only PNG and JPEG"
        );
    }

    #[test]
    fn a_broken_or_short_picture_is_dropped_and_input_goes_on() {
        let (input, log) = session(Board::Slow(Duration::ZERO));
        let png = fake_png();
        // One byte short of what the end says.
        for piece in pieces(&png[..png.len() - 1]) {
            input.send(piece);
        }
        input.send(Msg::ClipImageEnd {
            n: png.len(),
            s: String::new(),
            f: String::new(),
        });
        // Not base64.
        input.send(Msg::ClipImage { s: "!!".into() });
        input.send(Msg::ClipImageEnd {
            n: 1,
            s: String::new(),
            f: String::new(),
        });
        // Not a PNG.
        input.send(Msg::ClipImage {
            s: BASE64.encode(b"GIF89a"),
        });
        input.send(Msg::ClipImageEnd {
            n: 6,
            s: String::new(),
            f: String::new(),
        });
        input.send(Msg::Key {
            c: 0x19,
            d: true,
            r: false,
        });
        assert_eq!(wait_for(&log, 1, Duration::from_secs(2)), ["key 19 true"]);
        // And the next good one still goes through.
        for piece in pieces(&png) {
            input.send(piece);
        }
        input.send(Msg::ClipImageEnd {
            n: png.len(),
            s: String::new(),
            f: String::new(),
        });
        assert_eq!(
            wait_for(&log, 2, Duration::from_secs(2)),
            ["key 19 true", "picture 24 None"]
        );
    }

    #[test]
    fn the_keys_the_page_holds_reach_the_host_as_its_key_state() {
        let (input, log) = session(Board::Slow(Duration::ZERO));
        input.send(Msg::KeyState {
            c: vec![0xE0, 0x06],
        });
        assert_eq!(
            wait_for(&log, 1, Duration::from_secs(1)),
            ["held [700e0, 70006]"]
        );
    }

    #[test]
    fn a_slow_clipboard_never_holds_up_the_pointer() {
        let (input, log) = session(Board::Slow(Duration::from_secs(2)));
        // The first look at the clipboard is under way.
        thread::sleep(Duration::from_millis(50));
        let sent = Instant::now();
        input.send(Msg::Move { x: 0.5, y: 0.25 });
        assert_eq!(
            wait_for(&log, 1, Duration::from_secs(1)),
            ["move 50 25"],
            "the pointer waited for the clipboard"
        );
        assert!(sent.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn a_stuck_clipboard_holds_a_paste_up_for_a_moment_only() {
        let (input, log) = session(Board::Slow(Duration::from_secs(30)));
        // The first look is under way and will not return during the test.
        thread::sleep(Duration::from_millis(400));
        input.send(Msg::Clip { s: "hello".into() });
        input.send(Msg::Move { x: 0.0, y: 0.0 });
        let started = Instant::now();
        assert_eq!(
            wait_for(&log, 1, CLIP_WRITE_WAIT * 3),
            ["move 0 0"],
            "the pointer stayed stuck behind the paste"
        );
        assert!(started.elapsed() < CLIP_WRITE_WAIT * 2);
        // Ending the session does not wait for the clipboard either.
        let ending = Instant::now();
        drop(input);
        assert!(ending.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn a_clipboard_that_cannot_be_opened_leaves_the_keys_and_pointer_working() {
        let (input, log) = session(Board::Panics);
        input.send(Msg::Move { x: 1.0, y: 1.0 });
        input.send(Msg::Key {
            c: 0x04,
            d: true,
            r: false,
        });
        assert_eq!(
            wait_for(&log, 2, Duration::from_secs(1)),
            ["move 100 100", "key 4 true"]
        );
    }

    #[test]
    fn pasted_text_is_on_the_clipboard_before_the_key_after_it() {
        // Pasted right away, while the clipboard is still being opened.
        let (input, log) = session(Board::SlowToOpen(Duration::from_millis(200)));
        input.send(Msg::Clip { s: "hello".into() });
        input.send(Msg::Key {
            c: 0x19,
            d: true,
            r: false,
        });
        assert_eq!(
            wait_for(&log, 2, Duration::from_secs(1)),
            ["clipboard hello", "key 19 true"]
        );
    }
}
