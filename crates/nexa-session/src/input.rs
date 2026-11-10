//! Host input (§3.3, §7.5, §7.7): the `input-inject` thread and the pointer rules.
//!
//! Permission is structural: a session without the input permission never creates an
//! [`InjectThread`], so there is nothing for its Input stream or Pointer datagrams to
//! reach. See `nexa_agent::session`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use nexa_wire::{
    control::{ButtonEvent, KeyEvent, KeyStateSync, TextInput},
    datagram::Pointer,
};

use crate::{
    SessionError,
    framed::FramedRecv,
    proto::{InputBody, InputMsg},
};

/// The injection backend. On macOS the implementation wraps `nexa_input`'s `Injector`,
/// whose held-key tracking this mirrors.
pub trait InputSink: Send {
    fn key(&mut self, hid: u32, down: bool, repeat: bool, now: Instant);
    /// Global display points.
    fn move_absolute(&mut self, x: f64, y: f64);
    fn move_relative(&mut self, dx: i32, dy: i32);
    fn button(&mut self, button: u32, down: bool, at: Option<(f64, f64)>, now: Instant);
    fn wheel(&mut self, dx: i32, dy: i32);
    fn text(&mut self, text: &str);
    fn apply_key_state(&mut self, pressed: &[u32], now: Instant);
    fn poll_timeout(&mut self, now: Instant);
    fn release_all(&mut self, now: Instant);
}

/// Converts wire pixels to the platform's coordinates. `None` drops the event.
pub trait PointMap: Send + Sync {
    fn to_points(&self, display: u32, x: i32, y: i32) -> Option<(f64, f64)>;
}

#[derive(Debug)]
pub enum InjectEvent {
    Input(InputBody),
    Pointer(Pointer),
}

#[derive(Default, Debug)]
pub struct InjectStats {
    pub injected: AtomicU64,
    pub dropped_full: AtomicU64,
    pub malformed: AtomicU64,
}

const INJECT_QUEUE: usize = 1024;
const INJECT_TICK: Duration = Duration::from_millis(100);

/// Applies the §3.3 rules to pointer datagrams and aligned button events.
#[derive(Default)]
struct PointerState {
    last_seq: Option<u16>,
    was_relative: bool,
    x: i32,
    y: i32,
    wheel_h: i32,
    wheel_v: i32,
    have_position: bool,
}

/// Whether `seq` comes after `last`, allowing for wrap-around.
fn newer(seq: u16, last: u16) -> bool {
    let delta = seq.wrapping_sub(last);
    delta != 0 && delta < 0x8000
}

impl PointerState {
    fn apply(&mut self, p: Pointer, map: &dyn PointMap, sink: &mut dyn InputSink) {
        if self.last_seq.is_some_and(|last| !newer(p.seq, last)) {
            return; // A duplicate from the trailing resend, or reordered and now stale.
        }
        self.last_seq = Some(p.seq);

        if p.relative {
            // The totals carry on through a mode switch, so its first packet is a baseline.
            let (dx, dy) = if self.was_relative {
                (p.x.wrapping_sub(self.x), p.y.wrapping_sub(self.y))
            } else {
                (0, 0)
            };
            if dx != 0 || dy != 0 {
                sink.move_relative(dx, dy);
            }
        } else if (!self.have_position || self.was_relative || p.x != self.x || p.y != self.y)
            && let Some((x, y)) = map.to_points(u32::from(p.display), p.x, p.y)
        {
            sink.move_absolute(x, y);
        }
        self.was_relative = p.relative;
        self.have_position = true;
        self.x = p.x;
        self.y = p.y;

        // Lost packets lose no scrolling, since these are running totals.
        let (dx, dy) = (
            p.wheel_h_total.wrapping_sub(self.wheel_h),
            p.wheel_v_total.wrapping_sub(self.wheel_v),
        );
        if dx != 0 || dy != 0 {
            sink.wheel(dx, dy);
        }
        self.wheel_h = p.wheel_h_total;
        self.wheel_v = p.wheel_v_total;
    }

    /// Aligns the pointer to where the button happened, so a click always lands
    /// correctly however the datagrams fared (§3.3).
    fn button(
        &mut self,
        ev: &ButtonEvent,
        map: &dyn PointMap,
        sink: &mut dyn InputSink,
        now: Instant,
    ) {
        let at = if self.was_relative {
            let (dx, dy) = (ev.x.wrapping_sub(self.x), ev.y.wrapping_sub(self.y));
            if dx != 0 || dy != 0 {
                sink.move_relative(dx, dy);
            }
            self.x = ev.x;
            self.y = ev.y;
            None
        } else {
            match map.to_points(ev.display, ev.x, ev.y) {
                Some(at) => Some(at),
                None => return,
            }
        };
        sink.button(ev.button, ev.down, at, now);
    }
}

fn dispatch(
    event: InjectEvent,
    state: &mut PointerState,
    map: &dyn PointMap,
    sink: &mut dyn InputSink,
    now: Instant,
) {
    match event {
        InjectEvent::Pointer(p) => state.apply(p, map, sink),
        InjectEvent::Input(InputBody::Key(KeyEvent { hid, down, repeat })) => {
            sink.key(hid, down, repeat, now);
        }
        InjectEvent::Input(InputBody::Button(b)) => state.button(&b, map, sink, now),
        InjectEvent::Input(InputBody::Text(TextInput { utf8 })) => sink.text(&utf8),
        InjectEvent::Input(InputBody::Sync(KeyStateSync { pressed, .. })) => {
            sink.apply_key_state(&pressed, now);
        }
    }
}

fn run_inject(
    rx: Receiver<InjectEvent>,
    make_sink: Box<dyn FnOnce() -> Result<Box<dyn InputSink>, String> + Send>,
    map: Arc<dyn PointMap>,
    stats: Arc<InjectStats>,
) -> Result<(), String> {
    let mut sink = make_sink()?;
    let mut state = PointerState::default();
    loop {
        match rx.recv_timeout(INJECT_TICK) {
            Ok(event) => {
                stats.injected.fetch_add(1, Relaxed);
                dispatch(event, &mut state, &*map, &mut *sink, Instant::now());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        sink.poll_timeout(Instant::now());
    }
    // §7.5: disconnected, so nothing may stay held.
    sink.release_all(Instant::now());
    Ok(())
}

/// The `input-inject` thread (§1.1). Dropping every sender releases all held input.
pub struct InjectThread {
    tx: SyncSender<InjectEvent>,
    join: JoinHandle<Result<(), String>>,
    pub stats: Arc<InjectStats>,
}

impl InjectThread {
    /// `make_sink` runs on the new thread and is where the real injector is created.
    pub fn spawn(
        make_sink: Box<dyn FnOnce() -> Result<Box<dyn InputSink>, String> + Send>,
        map: Arc<dyn PointMap>,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel(INJECT_QUEUE);
        let stats = Arc::new(InjectStats::default());
        let thread_stats = stats.clone();
        let join = thread::Builder::new()
            .name("input-inject".into())
            .spawn(move || run_inject(rx, make_sink, map, thread_stats))
            .expect("spawn thread");
        Self { tx, join, stats }
    }

    pub fn sender(&self) -> SyncSender<InjectEvent> {
        self.tx.clone()
    }

    /// For the pointer callback, which must not block: a full queue drops the packet,
    /// and the running totals make up the distance on the next one.
    pub fn pointer_sink(&self) -> Box<dyn Fn(Pointer) + Send> {
        let (tx, stats) = (self.tx.clone(), self.stats.clone());
        Box::new(move |p| match tx.try_send(InjectEvent::Pointer(p)) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                stats.dropped_full.fetch_add(1, Relaxed);
            }
        })
    }

    /// Releases everything held and ends the thread, once the other senders are gone.
    pub fn stop(self) -> Result<(), String> {
        drop(self.tx);
        self.join
            .join()
            .unwrap_or_else(|_| Err("input thread panicked".into()))
    }
}

/// Pumps the Input stream into the inject thread until it ends. The queue blocks the
/// reader when full, which is what we want for a reliable stream.
pub async fn pump_input_stream(
    mut recv: FramedRecv,
    tx: SyncSender<InjectEvent>,
) -> Result<(), SessionError> {
    while let Some(msg) = recv.next::<InputMsg>().await? {
        let Some(body) = msg.body else { continue };
        let event = match tx.try_send(InjectEvent::Input(body)) {
            Ok(()) => continue,
            Err(TrySendError::Disconnected(_)) => break,
            Err(TrySendError::Full(event)) => event,
        };
        // Blocking here is the point, but never on an async worker.
        let tx = tx.clone();
        let sent = tokio::task::spawn_blocking(move || tx.send(event).is_ok())
            .await
            .unwrap_or(false);
        if !sent {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Abs(f64, f64),
        Rel(i32, i32),
        Wheel(i32, i32),
        Button(u32, bool, Option<(f64, f64)>),
    }

    #[derive(Default, Clone)]
    struct Fake(Arc<Mutex<Vec<Call>>>);

    impl InputSink for Fake {
        fn key(&mut self, _: u32, _: bool, _: bool, _: Instant) {}
        fn move_absolute(&mut self, x: f64, y: f64) {
            self.0.lock().unwrap().push(Call::Abs(x, y));
        }
        fn move_relative(&mut self, dx: i32, dy: i32) {
            self.0.lock().unwrap().push(Call::Rel(dx, dy));
        }
        fn button(&mut self, b: u32, down: bool, at: Option<(f64, f64)>, _: Instant) {
            self.0.lock().unwrap().push(Call::Button(b, down, at));
        }
        fn wheel(&mut self, dx: i32, dy: i32) {
            self.0.lock().unwrap().push(Call::Wheel(dx, dy));
        }
        fn text(&mut self, _: &str) {}
        fn apply_key_state(&mut self, _: &[u32], _: Instant) {}
        fn poll_timeout(&mut self, _: Instant) {}
        fn release_all(&mut self, _: Instant) {}
    }

    struct Half;
    impl PointMap for Half {
        fn to_points(&self, display: u32, x: i32, y: i32) -> Option<(f64, f64)> {
            (display == 0).then_some((f64::from(x) / 2.0, f64::from(y) / 2.0))
        }
    }

    fn pointer(seq: u16, x: i32, y: i32) -> Pointer {
        Pointer {
            seq,
            x,
            y,
            ..Pointer::default()
        }
    }

    fn run(events: impl IntoIterator<Item = Pointer>) -> Vec<Call> {
        let (mut sink, mut state) = (Fake::default(), PointerState::default());
        for p in events {
            state.apply(p, &Half, &mut sink);
        }
        sink.0.lock().unwrap().clone()
    }

    #[test]
    fn absolute_pixels_become_points_and_duplicates_vanish() {
        let calls = run([
            pointer(1, 100, 200),
            pointer(1, 100, 200),
            pointer(2, 102, 200),
        ]);
        assert_eq!(calls, vec![Call::Abs(50.0, 100.0), Call::Abs(51.0, 100.0)]);
    }

    #[test]
    fn stale_packets_are_ignored_across_wrap() {
        let calls = run([pointer(u16::MAX, 2, 2), pointer(1, 4, 4), pointer(0, 8, 8)]);
        assert_eq!(calls, vec![Call::Abs(1.0, 1.0), Call::Abs(2.0, 2.0)]);
    }

    #[test]
    fn relative_takes_differences_and_loses_nothing() {
        let rel = |seq, x, y| Pointer {
            relative: true,
            ..pointer(seq, x, y)
        };
        let calls = run([rel(1, 0, 0), rel(2, 5, 5), rel(5, 20, 0)]);
        assert_eq!(calls, vec![Call::Rel(5, 5), Call::Rel(15, -5)]);
    }

    #[test]
    fn wheel_totals_wrap() {
        let wheel = |seq, v| Pointer {
            seq,
            wheel_v_total: v,
            ..Pointer::default()
        };
        let calls = run([wheel(1, i32::MAX), wheel(2, i32::MIN + 9)]);
        assert_eq!(
            calls,
            vec![
                Call::Abs(0.0, 0.0),
                Call::Wheel(0, i32::MAX),
                Call::Wheel(0, 10)
            ]
        );
    }

    #[test]
    fn button_aligns_first() {
        let (mut sink, mut state) = (Fake::default(), PointerState::default());
        let b = ButtonEvent {
            button: 0,
            down: true,
            x: 10,
            y: 20,
            display: 0,
        };
        state.button(&b, &Half, &mut sink, Instant::now());
        assert_eq!(
            sink.0.lock().unwrap().clone(),
            vec![Call::Button(0, true, Some((5.0, 10.0)))]
        );
    }

    #[test]
    fn unknown_display_drops_the_button() {
        let (mut sink, mut state) = (Fake::default(), PointerState::default());
        let b = ButtonEvent {
            button: 0,
            down: true,
            x: 10,
            y: 20,
            display: 9,
        };
        state.button(&b, &Half, &mut sink, Instant::now());
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn thread_releases_on_stop() {
        #[derive(Default, Clone)]
        struct Rel(Arc<Mutex<u32>>);
        impl InputSink for Rel {
            fn key(&mut self, _: u32, _: bool, _: bool, _: Instant) {}
            fn move_absolute(&mut self, _: f64, _: f64) {}
            fn move_relative(&mut self, _: i32, _: i32) {}
            fn button(&mut self, _: u32, _: bool, _: Option<(f64, f64)>, _: Instant) {}
            fn wheel(&mut self, _: i32, _: i32) {}
            fn text(&mut self, _: &str) {}
            fn apply_key_state(&mut self, _: &[u32], _: Instant) {}
            fn poll_timeout(&mut self, _: Instant) {}
            fn release_all(&mut self, _: Instant) {
                *self.0.lock().unwrap() += 1;
            }
        }
        let sink = Rel::default();
        let probe = sink.0.clone();
        let t = InjectThread::spawn(Box::new(move || Ok(Box::new(sink))), Arc::new(Half));
        t.stop().unwrap();
        assert_eq!(*probe.lock().unwrap(), 1);
    }
}
