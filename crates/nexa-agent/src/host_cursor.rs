//! The host half of §6.9: a `cursor` thread that samples the system cursor and sends
//! shape, visibility and program-initiated moves to the viewer, and an `InputSink`
//! wrapper that remembers where injected input put the cursor.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use nexa_media::CursorSource;
use nexa_session::{
    cursor::{CursorTracker, InjectHistory, StreamGeometry},
    input::InputSink,
    proto::ControlBody,
};
use tokio::sync::mpsc;

/// 60 Hz: fast enough that a shape change is not noticeably late.
const POLL: Duration = Duration::from_millis(16);

/// Whether the viewer draws the cursor right now. The pipeline builder sets it: off
/// in game mode, where the cursor is in the video.
#[derive(Default)]
pub(crate) struct CursorControl {
    tracking: AtomicBool,
    /// Set when tracking (re)starts, so the viewer hears the current state again.
    resync: AtomicBool,
}

impl CursorControl {
    pub fn set_tracking(&self, on: bool) {
        // Any rebuild may come with a new display or stream size: say it all again.
        self.resync.store(true, Relaxed);
        self.tracking.store(on, Relaxed);
    }
}

pub(crate) struct CursorThread {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

impl CursorThread {
    pub fn spawn(
        mut source: Box<dyn CursorSource>,
        geometry: Arc<dyn StreamGeometry>,
        injected: Arc<InjectHistory>,
        control: Arc<CursorControl>,
        out: mpsc::UnboundedSender<ControlBody>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let join = thread::Builder::new()
            .name("cursor".into())
            .spawn(move || {
                let mut tracker = CursorTracker::new();
                let mut messages = Vec::new();
                while !s.load(Relaxed) {
                    if !control.tracking.load(Relaxed) {
                        thread::sleep(POLL);
                        continue;
                    }
                    if control.resync.swap(false, Relaxed) {
                        tracker.resync();
                    }
                    let sample = source.sample();
                    tracker.update(
                        &sample,
                        &*geometry,
                        &injected,
                        Instant::now(),
                        &mut messages,
                    );
                    for m in messages.drain(..) {
                        if out.send(m).is_err() {
                            return;
                        }
                    }
                    thread::sleep(POLL);
                }
            })
            .expect("spawn thread");
        Self { stop, join }
    }

    pub fn stop(self) {
        self.stop.store(true, Relaxed);
        let _ = self.join.join();
    }
}

/// Records every absolute position the viewer's input moves the cursor to.
pub(crate) struct TrackingSink {
    inner: Box<dyn InputSink>,
    injected: Arc<InjectHistory>,
}

impl TrackingSink {
    pub fn wrap(inner: Box<dyn InputSink>, injected: Arc<InjectHistory>) -> Box<dyn InputSink> {
        Box::new(Self { inner, injected })
    }
}

impl InputSink for TrackingSink {
    fn key(&mut self, hid: u32, down: bool, repeat: bool, now: Instant) {
        self.inner.key(hid, down, repeat, now);
    }
    fn move_absolute(&mut self, x: f64, y: f64) {
        self.injected.record(x, y, Instant::now());
        self.inner.move_absolute(x, y);
    }
    fn move_relative(&mut self, dx: i32, dy: i32) {
        self.inner.move_relative(dx, dy);
    }
    fn button(&mut self, button: u32, down: bool, at: Option<(f64, f64)>, now: Instant) {
        if let Some((x, y)) = at {
            self.injected.record(x, y, now);
        }
        self.inner.button(button, down, at, now);
    }
    fn wheel(&mut self, dx: i32, dy: i32) {
        self.inner.wheel(dx, dy);
    }
    fn text(&mut self, text: &str) {
        self.inner.text(text);
    }
    fn apply_key_state(&mut self, pressed: &[u32], now: Instant) {
        self.inner.apply_key_state(pressed, now);
    }
    fn poll_timeout(&mut self, now: Instant) {
        self.inner.poll_timeout(now);
    }
    fn release_all(&mut self, now: Instant) {
        self.inner.release_all(now);
    }
}
