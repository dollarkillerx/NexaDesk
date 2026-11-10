//! The `input-capture` thread (§1.2): Rewriter → InputGate → Input stream, and Pointer
//! datagrams. Everything that must keep its order goes through the gate, including
//! `KeyStateSync`: a sync racing ahead of held keys would release them early.

use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex, MutexGuard,
        mpsc::{Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use bytes::Bytes;
use nexa_clip::{EventClass, GateStats, Gen, InputGate, ReleaseReason};
use nexa_input::{
    FocusClass, Output, PhysButton, PhysKey, PhysWheel, RewriteMode, Rewriter, hid::Hid,
};
use nexa_session::{
    clip::ClipHandle,
    proto::{InputBody, InputMsg},
};
use nexa_wire::{
    control::{ButtonEvent, KeyEvent, KeyStateSync, TextInput},
    datagram::Pointer,
};
use quinn::Connection;
use tokio::sync::mpsc;

use crate::{
    client::EventTx,
    deps::{KeySource, KeySourceError},
    proto::{CaptureState, EventBody, InputCapture, PasteGate},
};

const SYNC_INTERVAL: Duration = Duration::from_secs(1);
/// §3.3: resend the last movement after 20 ms and 60 ms.
const RESEND_AFTER: [Duration; 2] = [Duration::from_millis(20), Duration::from_millis(60)];
/// Longest a paste may wait to learn whether the clipboard still has to arrive.
const PASTE_QUERY_LIMIT: Duration = Duration::from_millis(30);
const IDLE_WAIT: Duration = Duration::from_millis(100);

/// Everything the input thread reacts to.
pub enum InputCmd {
    Key(PhysKey),
    HostKey,
    Interrupted,
    Focus(FocusClass),
    SetActive(bool),
    GameMode(bool),
    /// View pixels.
    Move(f64, f64),
    Button(u32, bool),
    Wheel(i32, i32),
    Text(String),
    GateRelease(Gen),
    Stop,
}

/// Where the stream is drawn inside the view, to turn view pixels into stream pixels.
#[derive(Clone, Copy, Debug, Default)]
pub struct ViewGeometry {
    pub view_w: u32,
    pub view_h: u32,
    pub stream_w: u32,
    pub stream_h: u32,
}

impl ViewGeometry {
    pub fn to_stream(&self, x: f64, y: f64) -> Option<(i32, i32)> {
        if self.view_w == 0 || self.view_h == 0 || self.stream_w == 0 || self.stream_h == 0 {
            return None;
        }
        let (vx, vy, vw, vh) =
            fit_viewport((self.view_w, self.view_h), (self.stream_w, self.stream_h));
        let (sw, sh) = (f64::from(self.stream_w), f64::from(self.stream_h));
        let sx = ((x - vx) * sw / vw).clamp(0.0, sw - 1.0);
        let sy = ((y - vy) * sh / vh).clamp(0.0, sh - 1.0);
        Some((sx as i32, sy as i32))
    }
}

#[cfg(target_os = "macos")]
use nexa_media::macos::fit_viewport;

#[cfg(not(target_os = "macos"))]
fn fit_viewport(_: (u32, u32), frame: (u32, u32)) -> (f64, f64, f64, f64) {
    (0.0, 0.0, f64::from(frame.0), f64::from(frame.1))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sends pointer datagrams with the trailing resend from §3.3.
struct PointerSender<S> {
    send: S,
    current: Pointer,
    last_change: Option<Instant>,
    resent: usize,
}

impl<S: FnMut(&Pointer)> PointerSender<S> {
    fn new(send: S) -> Self {
        Self {
            send,
            current: Pointer::default(),
            last_change: None,
            resent: 0,
        }
    }

    fn changed(&mut self, now: Instant) {
        self.current.seq = self.current.seq.wrapping_add(1);
        (self.send)(&self.current);
        self.last_change = Some(now);
        self.resent = 0;
    }

    fn poll(&mut self, now: Instant) {
        let Some(changed_at) = self.last_change else {
            return;
        };
        if now >= changed_at + RESEND_AFTER[self.resent] {
            (self.send)(&self.current); // same seq: a host that has it ignores this
            self.resent += 1;
            if self.resent == RESEND_AFTER.len() {
                self.last_change = None;
            }
        }
    }

    fn next_due(&self) -> Option<Instant> {
        self.last_change.map(|t| t + RESEND_AFTER[self.resent])
    }
}

type PointerTx = Box<dyn FnMut(&Pointer) + Send>;

pub struct InputThreadConfig {
    pub conn: Connection,
    /// Bounded and drained by the Input stream writer task.
    pub stream: mpsc::Sender<InputMsg>,
    pub rewriter: Rewriter,
    pub key_source: Box<dyn KeySource>,
    /// Used when the tap needs a permission that is missing.
    pub fallback: Arc<Mutex<Option<Box<dyn KeySource>>>>,
    pub clip: Arc<Mutex<Option<ClipHandle>>>,
    pub view: Arc<Mutex<ViewGeometry>>,
    pub events: EventTx,
    pub tag: u64,
    pub start_active: bool,
    pub gate_stats: Arc<Mutex<GateStats>>,
}

struct InputState {
    cfg: InputThreadConfig,
    gate: InputGate<InputMsg, Instant>,
    focus: FocusClass,
    active: bool,
    on_fallback: bool,
    pointer: PointerSender<PointerTx>,
    pos: (i32, i32),
    sync_seq: u64,
    last_pressed: BTreeSet<Hid>,
    next_sync: Instant,
}

impl InputState {
    fn event(&self, body: EventBody) {
        self.cfg.events.send(self.cfg.tag, body);
    }

    fn send_stream(&self, msg: InputMsg) {
        let _ = self.cfg.stream.blocking_send(msg);
    }

    fn flush(&mut self, released: &mut Vec<InputMsg>) {
        for msg in released.drain(..) {
            self.send_stream(msg);
        }
        for n in self.gate.take_notices() {
            if n.reason != ReleaseReason::Flushed {
                self.event(EventBody::PasteGate(PasteGate {
                    waited_ms: n.waited.as_millis().min(u128::from(u32::MAX)) as u32,
                    timed_out: n.reason == ReleaseReason::Deadline,
                }));
            }
        }
        *lock(&self.cfg.gate_stats) = self.gate.stats();
    }

    fn paste_requirement(&self) -> Option<Gen> {
        let clip = lock(&self.cfg.clip).clone()?;
        clip.paste_requirement(PASTE_QUERY_LIMIT)
    }

    fn ordered(&mut self, body: InputBody, now: Instant, out: &mut Vec<InputMsg>) {
        self.gate
            .on_event(body.into(), EventClass::Ordered, now, out);
    }

    fn emit(&mut self, outputs: impl IntoIterator<Item = Output>, now: Instant) {
        let mut released = Vec::new();
        for output in outputs {
            match output {
                Output::PasteMarker => {
                    let required = self.paste_requirement();
                    self.gate.on_paste(required, now, &mut released);
                }
                Output::Key { hid, down, repeat } => {
                    self.ordered(
                        InputBody::Key(KeyEvent {
                            hid: hid.0,
                            down,
                            repeat,
                        }),
                        now,
                        &mut released,
                    );
                }
                Output::Button { button, down } => {
                    let (x, y) = self.pos;
                    self.ordered(
                        InputBody::Button(ButtonEvent {
                            button,
                            down,
                            x,
                            y,
                            display: 0,
                        }),
                        now,
                        &mut released,
                    );
                }
                Output::Wheel { dx, dy } => {
                    let p = &mut self.pointer.current;
                    p.wheel_h_total = p.wheel_h_total.wrapping_add(dx);
                    p.wheel_v_total = p.wheel_v_total.wrapping_add(dy);
                    self.pointer.changed(now);
                }
            }
        }
        self.flush(&mut released);
        self.sync_keys_if_changed(now);
    }

    fn send_key_state(&mut self, now: Instant) {
        self.sync_seq += 1;
        let pressed: Vec<u32> = self.cfg.rewriter.pressed_keys().map(|h| h.0).collect();
        self.last_pressed = self.cfg.rewriter.pressed_keys().collect();
        self.next_sync = now + SYNC_INTERVAL;
        let mut released = Vec::new();
        self.ordered(
            InputBody::Sync(KeyStateSync {
                pressed,
                seq: self.sync_seq,
            }),
            now,
            &mut released,
        );
        self.flush(&mut released);
    }

    fn sync_keys_if_changed(&mut self, now: Instant) {
        let pressed: BTreeSet<Hid> = self.cfg.rewriter.pressed_keys().collect();
        if pressed != self.last_pressed {
            self.send_key_state(now);
        }
    }

    fn set_capture(&mut self, active: bool) {
        let result = if self.on_fallback {
            lock(&self.cfg.fallback)
                .as_mut()
                .map_or(Err(KeySourceError::Failed), |f| f.set_active(active))
        } else {
            self.cfg.key_source.set_active(active)
        };
        let state = match result {
            Ok(()) if !active => CaptureState::Inactive,
            Ok(()) if self.on_fallback => CaptureState::Degraded,
            Ok(()) => CaptureState::Active,
            Err(KeySourceError::PermissionDenied) if active && !self.on_fallback => {
                // §7.4: no Accessibility, so fall back and tell the user.
                self.on_fallback = true;
                let degraded = lock(&self.cfg.fallback)
                    .as_mut()
                    .is_some_and(|f| f.set_active(true).is_ok());
                if degraded {
                    CaptureState::Degraded
                } else {
                    self.on_fallback = false;
                    CaptureState::Unavailable
                }
            }
            Err(_) => CaptureState::Unavailable,
        };
        self.event(EventBody::InputCapture(InputCapture {
            state: state as i32,
        }));
    }

    fn apply_active(&mut self, active: bool, now: Instant) {
        if active == self.active {
            return;
        }
        self.active = active;
        self.set_capture(active);
        if !active {
            // §7.4: leaving capture releases everything on the remote side.
            let outputs = self.cfg.rewriter.reset();
            self.emit(outputs, now);
        }
        self.next_sync = now + SYNC_INTERVAL;
    }

    fn on_cmd(&mut self, cmd: InputCmd, now: Instant) -> bool {
        match cmd {
            InputCmd::Stop => return false,
            InputCmd::SetActive(active) => self.apply_active(active, now),
            InputCmd::Focus(class) => self.focus = class,
            InputCmd::GameMode(game) => {
                let mode = if game {
                    RewriteMode::Passthrough
                } else {
                    RewriteMode::Rewrite
                };
                let outputs = self.cfg.rewriter.set_mode(mode);
                self.emit(outputs, now);
            }
            InputCmd::Key(key) if self.active => {
                let outputs = self.cfg.rewriter.on_key(key, self.focus);
                self.emit(outputs, now);
            }
            InputCmd::Interrupted => {
                let outputs = self.cfg.rewriter.reset();
                self.emit(outputs, now);
            }
            InputCmd::HostKey => self.event(EventBody::HostKey(crate::proto::HostKeyPressed {})),
            InputCmd::Move(x, y) if self.active => {
                let mapped = lock(&self.cfg.view).to_stream(x, y);
                if let Some((sx, sy)) = mapped {
                    self.pos = (sx, sy);
                    self.pointer.current.x = sx;
                    self.pointer.current.y = sy;
                    self.pointer.changed(now);
                }
            }
            InputCmd::Button(button, down) if self.active => {
                let outputs = self.cfg.rewriter.on_button(PhysButton { button, down });
                self.emit(outputs, now);
            }
            InputCmd::Wheel(dx, dy) if self.active => {
                let outputs = self.cfg.rewriter.on_wheel(PhysWheel { dx, dy });
                self.emit(outputs, now);
            }
            InputCmd::Text(utf8) if self.active => {
                let mut released = Vec::new();
                self.ordered(InputBody::Text(TextInput { utf8 }), now, &mut released);
                self.flush(&mut released);
            }
            InputCmd::GateRelease(generation) => {
                let mut released = Vec::new();
                self.gate.on_applied(generation, now, &mut released);
                self.flush(&mut released);
            }
            // Input arriving while inactive is ignored.
            InputCmd::Key(_)
            | InputCmd::Move(..)
            | InputCmd::Button(..)
            | InputCmd::Wheel(..)
            | InputCmd::Text(_) => {}
        }
        true
    }

    fn wait_time(&self, now: Instant) -> Duration {
        let mut next = now + IDLE_WAIT;
        if let Some(d) = self.gate.next_deadline() {
            next = next.min(d);
        }
        if let Some(d) = self.pointer.next_due() {
            next = next.min(d);
        }
        if self.active {
            next = next.min(self.next_sync);
        }
        next.saturating_duration_since(now)
    }

    fn poll_timers(&mut self, now: Instant) {
        let mut released = Vec::new();
        self.gate.poll(now, &mut released);
        self.flush(&mut released);
        self.pointer.poll(now);
        if self.active && now >= self.next_sync {
            self.send_key_state(now);
        }
    }

    fn run(mut self, rx: Receiver<InputCmd>) {
        if self.cfg.start_active {
            self.apply_active(true, Instant::now());
        }
        loop {
            let now = Instant::now();
            match rx.recv_timeout(self.wait_time(now)) {
                Ok(cmd) => {
                    if !self.on_cmd(cmd, Instant::now()) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.poll_timers(Instant::now());
        }
        // Local capture must end before anything else: the keyboard is the user's.
        let now = Instant::now();
        self.apply_active(false, now);
        let mut released = Vec::new();
        self.gate.flush(now, &mut released);
        self.flush(&mut released);
    }
}

/// Spawns the input-capture thread.
pub fn spawn(cfg: InputThreadConfig, rx: Receiver<InputCmd>) -> JoinHandle<()> {
    let next_sync = Instant::now() + SYNC_INTERVAL;
    let conn = cfg.conn.clone();
    let pointer: PointerSender<PointerTx> = PointerSender::new(Box::new(move |p: &Pointer| {
        let _ = conn.send_datagram(Bytes::copy_from_slice(&p.to_bytes()));
    }));
    let state = InputState {
        cfg,
        gate: InputGate::new(),
        focus: FocusClass::Default,
        active: false,
        on_fallback: false,
        pointer,
        pos: (0, 0),
        sync_seq: 0,
        last_pressed: BTreeSet::new(),
        next_sync,
    };
    thread::Builder::new()
        .name("input-capture".into())
        .spawn(move || state.run(rx))
        .expect("spawn thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_maps_through_the_letterbox() {
        let g = ViewGeometry {
            view_w: 2000,
            view_h: 1000,
            stream_w: 1000,
            stream_h: 1000,
        };
        assert_eq!(g.to_stream(500.0, 0.0), Some((0, 0)));
        assert_eq!(g.to_stream(1000.0, 500.0), Some((500, 500)));
        assert_eq!(g.to_stream(10.0, 5000.0), Some((0, 999)));
    }

    #[test]
    fn trailing_resends_at_20_and_60_ms_then_stop() {
        let sent = std::cell::RefCell::new(Vec::new());
        let mut pointer = PointerSender::new(|p: &Pointer| sent.borrow_mut().push(p.seq));
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        pointer.changed(t0);
        assert_eq!(sent.borrow().clone(), vec![1]);
        assert_eq!(pointer.next_due(), Some(ms(20)));
        pointer.poll(ms(19));
        assert_eq!(sent.borrow().len(), 1);
        pointer.poll(ms(20));
        assert_eq!(
            sent.borrow().clone(),
            vec![1, 1],
            "the resend keeps the sequence"
        );
        assert_eq!(pointer.next_due(), Some(ms(60)));
        pointer.poll(ms(60));
        assert_eq!(sent.borrow().len(), 3);
        assert_eq!(pointer.next_due(), None);
        pointer.poll(ms(500));
        assert_eq!(sent.borrow().len(), 3);
    }

    #[test]
    fn a_new_movement_restarts_the_resends() {
        let sent = std::cell::RefCell::new(Vec::new());
        let mut pointer = PointerSender::new(|p: &Pointer| sent.borrow_mut().push(p.seq));
        let t0 = Instant::now();
        pointer.changed(t0);
        pointer.poll(t0 + Duration::from_millis(20));
        pointer.changed(t0 + Duration::from_millis(30));
        assert_eq!(pointer.next_due(), Some(t0 + Duration::from_millis(50)));
        assert_eq!(sent.borrow().clone(), vec![1, 1, 2]);
    }

    #[test]
    fn no_view_or_stream_means_no_position() {
        let g = ViewGeometry::default();
        assert_eq!(g.to_stream(1.0, 1.0), None);
    }
}
