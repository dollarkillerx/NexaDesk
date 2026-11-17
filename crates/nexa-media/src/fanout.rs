//! One capture of a display, shared by every stream of it.
//!
//! Some captures can be open only once per display in a process: a second DXGI
//! `DuplicateOutput` of an output fails with E_INVALIDARG while the first is alive. Yet
//! each viewer's stream wants its own size, frame rate and cursor. So a [`Fanout`] keeps
//! one [`Source`] per display, pumped on a thread of its own, and every stream reads the
//! latest picture through its own [`Feed`] and makes its frames from that.
//!
//! The source is let go as soon as its last feed is: dropping that feed returns only once
//! the source is gone, so a stream rebuilt old stages first can open the display again at
//! once. A source that is lost (a mode change, a secure desktop) ends all its feeds, and
//! the next feed of that display opens a new one.

use std::{
    sync::{
        Arc, Condvar, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{MediaError, Result};

/// The longest the pump waits for the display before it looks whether to stop: also
/// the longest dropping the last feed of a display takes.
const PUMP_WAIT: Duration = Duration::from_millis(20);

/// A display's capture, as the platform makes it.
pub trait Source: Send + 'static {
    /// A picture of the display that every stream reads. Cheap to clone.
    type Picture: Clone + Send + 'static;
    /// What the streams share besides pictures (on Windows, the D3D11 device).
    type Info: Send + Sync + 'static;

    fn info(&self) -> Self::Info;
    /// What changed on the display within `timeout`. Returning sooner, with `Idle`, is
    /// fine: it is asked again at once.
    fn acquire(&mut self, timeout: Duration) -> Update<Self::Picture>;
}

/// What happened on a display.
#[derive(Clone)]
pub enum Update<P> {
    /// A new picture.
    Picture(P),
    /// Only the pointer moved or changed shape.
    Pointer,
    /// Nothing new within the timeout.
    Idle,
    /// The capture must be made again: a mode change, a secure desktop.
    Lost,
    Failed(MediaError),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct State<P> {
    picture: Option<P>,
    /// Counts new pictures, and pointer-only changes.
    pictures: u64,
    pointer: u64,
    /// Set once the source has gone by itself: `Lost` or `Failed`.
    ended: Option<Update<P>>,
}

struct Shared<P> {
    state: Mutex<State<P>>,
    changed: Condvar,
    stop: AtomicBool,
}

/// One display's running capture.
struct Hub<S: Source> {
    display: u32,
    info: S::Info,
    shared: Arc<Shared<S::Picture>>,
    pump: Option<JoinHandle<()>>,
}

impl<S: Source> Hub<S> {
    fn ended(&self) -> bool {
        lock(&self.shared.state).ended.is_some()
    }
}

impl<S: Source> Drop for Hub<S> {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        // The source is dropped on the pump's thread; it is gone once this returns.
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

/// Tells the feeds that the source ended, once it is dropped.
struct Ending<'a, P> {
    shared: &'a Shared<P>,
    why: Update<P>,
}

impl<P> Drop for Ending<'_, P> {
    fn drop(&mut self) {
        let why = std::mem::replace(&mut self.why, Update::Idle);
        lock(&self.shared.state).ended = Some(why);
        self.shared.changed.notify_all();
    }
}

fn pump<S: Source>(source: S, shared: Arc<Shared<S::Picture>>) {
    // Made before `source` so that it is dropped after it, on a panic too: the display
    // is let go before any feed hears that the capture ended and opens it again.
    let mut ending = Ending {
        shared: &shared,
        why: Update::Failed(MediaError::Other("the display's capture stopped".into())),
    };
    let mut source = source;
    while !shared.stop.load(Relaxed) {
        match source.acquire(PUMP_WAIT) {
            Update::Idle => {}
            Update::Picture(p) => {
                let mut state = lock(&shared.state);
                state.picture = Some(p);
                state.pictures += 1;
                drop(state);
                shared.changed.notify_all();
            }
            Update::Pointer => {
                lock(&shared.state).pointer += 1;
                shared.changed.notify_all();
            }
            end => {
                ending.why = end;
                return;
            }
        }
    }
}

/// The running captures of a process, at most one per display.
pub struct Fanout<S: Source> {
    hubs: Mutex<Vec<Weak<Hub<S>>>>,
}

impl<S: Source> Default for Fanout<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Source> Fanout<S> {
    pub const fn new() -> Self {
        Self {
            hubs: Mutex::new(Vec::new()),
        }
    }

    /// A feed of `display`: from the capture already running for it, or else from the
    /// one `open` makes. `open` must not drop a feed of this fanout.
    pub fn subscribe(
        &'static self,
        display: u32,
        open: impl FnOnce() -> Result<S>,
    ) -> Result<Feed<S>> {
        // Feeds let go of their hub under this lock too, so a hub found here is not
        // being dropped, and one that is gone has let go of its source.
        let mut hubs = lock(&self.hubs);
        hubs.retain(|h| h.strong_count() > 0);
        let running = hubs
            .iter()
            .filter_map(Weak::upgrade)
            .find(|h| h.display == display && !h.ended());
        let hub = match running {
            Some(hub) => hub,
            None => {
                // An ended hub of this display still feeds its last streams, but its
                // source is gone, so the display can be opened again.
                let source = open()?;
                let info = source.info();
                let shared = Arc::new(Shared {
                    state: Mutex::new(State {
                        picture: None,
                        pictures: 0,
                        pointer: 0,
                        ended: None,
                    }),
                    changed: Condvar::new(),
                    stop: AtomicBool::new(false),
                });
                let pumped = shared.clone();
                let pump = thread::Builder::new()
                    .name("display-capture".into())
                    .spawn(move || pump(source, pumped))
                    .map_err(|e| MediaError::Other(format!("capture thread: {e}")))?;
                let hub = Arc::new(Hub {
                    display,
                    info,
                    shared,
                    pump: Some(pump),
                });
                hubs.push(Arc::downgrade(&hub));
                hub
            }
        };
        let pointer = lock(&hub.shared.state).pointer;
        Ok(Feed {
            fanout: self,
            hub: Some(hub),
            pictures: 0,
            pointer,
        })
    }
}

/// One stream's view of a display's capture.
pub struct Feed<S: Source> {
    fanout: &'static Fanout<S>,
    /// Taken only when dropped.
    hub: Option<Arc<Hub<S>>>,
    /// The counts this stream has seen.
    pictures: u64,
    pointer: u64,
}

impl<S: Source> Feed<S> {
    fn hub(&self) -> &Hub<S> {
        self.hub
            .as_deref()
            .expect("a feed has its hub until dropped")
    }

    pub fn info(&self) -> &S::Info {
        &self.hub().info
    }

    /// The display's latest picture, if there is one yet, without waiting for one.
    pub fn latest(&self) -> Option<S::Picture> {
        lock(&self.hub().shared.state).picture.clone()
    }

    /// What is new for this stream, waiting up to `timeout`: the latest picture if it
    /// has not had it, or else, with `pointer`, a pointer that moved on its own. A
    /// stream that has not had a picture yet gets the latest one at once.
    pub fn next(&mut self, timeout: Duration, pointer: bool) -> Update<S::Picture> {
        let Self {
            hub,
            pictures,
            pointer: moves,
            ..
        } = self;
        let shared = &hub
            .as_ref()
            .expect("a feed has its hub until dropped")
            .shared;
        let deadline = Instant::now() + timeout.min(Duration::from_secs(3600));
        let mut state = lock(&shared.state);
        loop {
            if let Some(end) = &state.ended {
                return end.clone();
            }
            if state.pictures != *pictures
                && let Some(picture) = &state.picture
            {
                *pictures = state.pictures;
                *moves = state.pointer;
                return Update::Picture(picture.clone());
            }
            if pointer && state.pointer != *moves {
                *moves = state.pointer;
                return Update::Pointer;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Update::Idle;
            }
            state = shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl<S: Source> Drop for Feed<S> {
    fn drop(&mut self) {
        // Under the fanout's lock: nobody can look for this display's capture between
        // its last feed going and the capture being let go, and open a second beside it.
        let _hubs = lock(&self.fanout.hubs);
        drop(self.hub.take());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    use super::*;

    /// What a fake display does next, set by the test.
    #[derive(Default)]
    struct Display {
        /// Fake captures of this display alive now, and ever opened.
        open: AtomicU32,
        opened: AtomicU32,
        /// The next picture number, 0 for none yet.
        picture: AtomicU32,
        pointer: AtomicBool,
        lose: AtomicBool,
    }

    /// Like DXGI desktop duplication: one capture of a display at a time.
    struct Exclusive(Arc<Display>);

    impl Exclusive {
        fn open(display: &Arc<Display>) -> Result<Self> {
            if display.open.fetch_add(1, SeqCst) > 0 {
                display.open.fetch_sub(1, SeqCst);
                return Err(MediaError::Other("already duplicating this display".into()));
            }
            display.opened.fetch_add(1, SeqCst);
            Ok(Self(display.clone()))
        }
    }

    impl Drop for Exclusive {
        fn drop(&mut self) {
            self.0.open.fetch_sub(1, SeqCst);
        }
    }

    impl Source for Exclusive {
        type Picture = u32;
        type Info = u32;

        fn info(&self) -> u32 {
            self.0.opened.load(SeqCst)
        }

        fn acquire(&mut self, timeout: Duration) -> Update<u32> {
            thread::sleep(timeout.min(Duration::from_millis(2)));
            if self.0.lose.swap(false, SeqCst) {
                return Update::Lost;
            }
            if self.0.pointer.swap(false, SeqCst) {
                return Update::Pointer;
            }
            match self.0.picture.swap(0, SeqCst) {
                0 => Update::Idle,
                n => Update::Picture(n),
            }
        }
    }

    fn fanout() -> &'static Fanout<Exclusive> {
        Box::leak(Box::new(Fanout::new()))
    }

    fn feed(f: &'static Fanout<Exclusive>, id: u32, d: &Arc<Display>) -> Result<Feed<Exclusive>> {
        f.subscribe(id, || Exclusive::open(d))
    }

    const WAIT: Duration = Duration::from_secs(2);

    fn picture(update: Update<u32>) -> Option<u32> {
        match update {
            Update::Picture(n) => Some(n),
            _ => None,
        }
    }

    #[test]
    fn streams_of_one_display_share_its_one_capture() {
        let (f, d) = (fanout(), Arc::new(Display::default()));
        let mut a = feed(f, 1, &d).unwrap();
        let mut b = feed(f, 1, &d).unwrap();
        assert_eq!(
            d.open.load(SeqCst),
            1,
            "the second stream did not open another"
        );
        d.picture.store(7, SeqCst);
        assert_eq!(picture(a.next(WAIT, false)), Some(7));
        assert_eq!(picture(b.next(WAIT, false)), Some(7));
        // Each stream gets each picture once.
        assert!(matches!(
            a.next(Duration::from_millis(30), false),
            Update::Idle
        ));
        // A stream that joins later starts from the latest picture, at once.
        let mut c = feed(f, 1, &d).unwrap();
        assert_eq!(c.latest(), Some(7));
        assert_eq!(picture(c.next(Duration::ZERO, false)), Some(7));
        assert_eq!(d.opened.load(SeqCst), 1);
    }

    #[test]
    fn the_last_stream_to_go_lets_go_of_the_display_before_its_drop_returns() {
        let (f, d) = (fanout(), Arc::new(Display::default()));
        let a = feed(f, 1, &d).unwrap();
        let b = feed(f, 1, &d).unwrap();
        drop(a);
        assert_eq!(d.open.load(SeqCst), 1, "still watched by the other");
        drop(b);
        assert_eq!(d.open.load(SeqCst), 0);
        // As a stream rebuilt old stages first does: open the display again at once.
        let _again = feed(f, 1, &d).unwrap();
        assert_eq!(d.opened.load(SeqCst), 2);
    }

    #[test]
    fn a_lost_capture_ends_every_stream_and_the_next_opens_another() {
        let (f, d) = (fanout(), Arc::new(Display::default()));
        let mut a = feed(f, 1, &d).unwrap();
        let mut b = feed(f, 1, &d).unwrap();
        d.lose.store(true, SeqCst);
        assert!(matches!(a.next(WAIT, false), Update::Lost));
        // Lost was said only once the capture had gone.
        assert_eq!(d.open.load(SeqCst), 0);
        // A rebuilt stream opens the display again while the other still holds the old
        // capture...
        drop(a);
        let mut a = feed(f, 1, &d).unwrap();
        assert_eq!((d.open.load(SeqCst), d.opened.load(SeqCst)), (1, 2));
        assert_eq!(*a.info(), 2);
        // ...which still says Lost, every time, until that stream is rebuilt too.
        assert!(matches!(b.next(WAIT, false), Update::Lost));
        assert!(matches!(b.next(WAIT, false), Update::Lost));
        drop(b);
        let mut b = feed(f, 1, &d).unwrap();
        assert_eq!(d.opened.load(SeqCst), 2, "the two share the new capture");
        d.picture.store(3, SeqCst);
        assert_eq!(picture(a.next(WAIT, false)), Some(3));
        assert_eq!(picture(b.next(WAIT, false)), Some(3));
    }

    #[test]
    fn a_pointer_that_moves_alone_wakes_only_streams_that_draw_it() {
        let (f, d) = (fanout(), Arc::new(Display::default()));
        let mut draws = feed(f, 1, &d).unwrap();
        let mut not = feed(f, 1, &d).unwrap();
        d.pointer.store(true, SeqCst);
        assert!(matches!(draws.next(WAIT, true), Update::Pointer));
        assert!(matches!(
            not.next(Duration::from_millis(50), false),
            Update::Idle
        ));
        assert!(matches!(
            draws.next(Duration::from_millis(30), true),
            Update::Idle
        ));
    }

    #[test]
    fn each_display_has_its_own_capture() {
        let f = fanout();
        let (one, two) = (Arc::new(Display::default()), Arc::new(Display::default()));
        let _a = feed(f, 1, &one).unwrap();
        let _b = feed(f, 2, &two).unwrap();
        let _c = feed(f, 2, &two).unwrap();
        assert_eq!(one.open.load(SeqCst), 1);
        assert_eq!((two.open.load(SeqCst), two.opened.load(SeqCst)), (1, 1));
    }

    #[test]
    fn a_display_that_cannot_be_opened_fails_that_stream_only() {
        let (f, d) = (fanout(), Arc::new(Display::default()));
        // Held elsewhere, as by another process or the old code's second duplication.
        let held = Exclusive::open(&d).unwrap();
        assert!(feed(f, 1, &d).is_err());
        drop(held);
        assert!(feed(f, 1, &d).is_ok());
    }

    #[test]
    fn streams_coming_and_going_from_many_threads_never_open_a_second_capture() {
        let (f, d) = (fanout(), Arc::new(Display::default()));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let d = d.clone();
                thread::spawn(move || {
                    for _ in 0..20 {
                        let mut s = feed(f, 1, &d).expect("never refused by its own fanout");
                        d.picture.store(1, SeqCst);
                        let _ = s.next(Duration::from_millis(5), false);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(d.open.load(SeqCst), 0);
    }
}
