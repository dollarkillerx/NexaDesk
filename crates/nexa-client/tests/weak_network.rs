//! Weak networks end to end: the real host, viewer and QUIC, with a relay between them
//! that loses, delays, jitters, rate-limits and cuts packets. The encoder is a fake whose
//! frames are as large as the rate controller allows (key frames four times that), so
//! the rate control, loss recovery, pacing and reconnecting are what is tested.
//!
//! Each test prints what it measured (`cargo test -p nexa-client --test weak_network --
//! --nocapture`): the picture's frame rate, its longest freeze, how long a pointer move
//! took to reach the host, and how much the relay's queue held back.

#[path = "../../nexa-agent/tests/common/mod.rs"]
mod common;

use std::{
    ffi::c_void,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering::SeqCst},
        mpsc::Sender,
    },
    time::{Duration, Instant},
};

use common::{FakeDecoder, FakePlatform, FakeUi, caps, lock, perms, until};
use nexa_agent::{Agent, AgentConfig, AgentUi, Platform, ScreenInfo, start};
use nexa_auth::{DeviceIdentity, FileKeyStore, KeyStore, PairingStore};
use nexa_client::{
    abi::{self, NexaClient, NexaSession},
    deps::{Deps, KeySource, KeySourceError},
    input::InputCmd,
    proto::{Command, ConnectAuth, ConnectRequest, Event, EventBody, PointerMove, SessionCommand},
};
use nexa_media::{
    CaptureConfig, CapturedFrame, Capturer, DecodedFrame, Decoder, EncodeOpts, EncodedMeta,
    Encoder, EncoderCaps, EncoderConfig, FrameId, PresentMode, PresentStat, Presenter, Recovery,
};
use nexa_session::{clip::ClipPlatform, handshake::Timeouts, input::InputSink};
use nexa_wire::control::{Caps, Codec, Mode, Os};
use prost::Message;
use tokio::net::UdpSocket;

const WAIT: Duration = Duration::from_secs(20);
/// The host's fixed password. A one-time code is spent by the first connection, so
/// only a password (or a pairing) lets the viewer reconnect by itself.
const PASSWORD: &str = "weak network test";

// ---------------------------------------------------------------------------------
// The relay
// ---------------------------------------------------------------------------------

/// What one direction of the link does to packets.
#[derive(Clone, Copy, Debug)]
struct Impairment {
    /// Chance each packet is lost.
    loss: f64,
    /// Every `.0`, everything is lost for `.1` (a Wi-Fi stall, a tunnel).
    bursts: Option<(Duration, Duration)>,
    delay: Duration,
    /// Up to this much more, at random per packet (so packets can arrive out of order).
    jitter: Duration,
    /// The bottleneck's rate, and how much it may queue before dropping.
    rate_bps: Option<u64>,
    queue: Duration,
    /// Packets are handed over in batches this far apart (a cellular link).
    batch: Duration,
}

impl Impairment {
    const CLEAN: Self = Self {
        loss: 0.0,
        bursts: None,
        delay: Duration::ZERO,
        jitter: Duration::ZERO,
        rate_bps: None,
        queue: Duration::from_millis(200),
        batch: Duration::ZERO,
    };
}

#[derive(Default)]
struct DirStats {
    sent: u64,
    lost: u64,
    /// Dropped because the bottleneck's queue was full.
    overflowed: u64,
    /// How long each packet waited in the bottleneck's queue.
    queued: Vec<Duration>,
}

struct Dir {
    imp: Impairment,
    next_free: Instant,
    stats: DirStats,
}

struct LinkState {
    up: Dir,
    down: Dir,
    started: Instant,
    rng: u64,
    /// Everything is lost, both ways.
    cut: bool,
    /// Until then, everything from the host is lost (see [`Link::lose`]).
    lose_down_until: Option<Instant>,
}

impl LinkState {
    fn random(&mut self) -> f64 {
        // xorshift64*: reproducible, good enough to pick lost packets.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// When the packet should arrive, or `None` if it is lost.
    fn admit(&mut self, upward: bool, bytes: usize, now: Instant) -> Option<Instant> {
        let (cut, started) = (self.cut, self.started);
        let (r1, r2) = (self.random(), self.random());
        if !upward && self.lose_down_until.is_some_and(|t| now < t) {
            self.down.stats.lost += 1;
            return None;
        }
        let dir = if upward { &mut self.up } else { &mut self.down };
        let imp = dir.imp;
        let in_burst = imp.bursts.is_some_and(|(every, length)| {
            let t = now.saturating_duration_since(started).as_nanos();
            t % every.as_nanos() < length.as_nanos()
        });
        if cut || in_burst || r1 < imp.loss {
            dir.stats.lost += 1;
            return None;
        }
        let leaves = match imp.rate_bps {
            Some(bps) => {
                let start = dir.next_free.max(now);
                if start - now > imp.queue {
                    dir.stats.overflowed += 1;
                    return None;
                }
                dir.stats.queued.push(start - now);
                let leaves = start + Duration::from_secs_f64(bytes as f64 * 8.0 / bps as f64);
                dir.next_free = leaves;
                leaves
            }
            None => now,
        };
        dir.stats.sent += 1;
        let mut at = leaves + imp.delay + imp.jitter.mul_f64(r2);
        if !imp.batch.is_zero() {
            let since = at.saturating_duration_since(started).as_nanos();
            let step = imp.batch.as_nanos();
            at = started + Duration::from_nanos((since.div_ceil(step) * step) as u64);
        }
        Some(at)
    }
}

/// Packets waiting to leave the relay, sent by one task in order of time (and of
/// admission, for equal times).
#[derive(Clone)]
struct Delivery {
    queue: Arc<Mutex<std::collections::BinaryHeap<std::cmp::Reverse<Pending>>>>,
    wake: Arc<tokio::sync::Notify>,
    next: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Pending {
    at: Instant,
    seq: u64,
    to: SocketAddr,
    data: Vec<u8>,
}

impl Delivery {
    fn start(socket: Arc<UdpSocket>) -> Self {
        let d = Self {
            queue: Arc::default(),
            wake: Arc::default(),
            next: Arc::default(),
        };
        let q = d.clone();
        tokio::spawn(async move {
            loop {
                let due = lock(&q.queue).peek().map(|p| p.0.at);
                match due {
                    Some(at) if at <= Instant::now() => {
                        let p = lock(&q.queue).pop().unwrap().0;
                        let _ = socket.send_to(&p.data, p.to).await;
                    }
                    Some(at) => {
                        tokio::select! {
                            _ = tokio::time::sleep_until(at.into()) => {}
                            _ = q.wake.notified() => {}
                        }
                    }
                    None => q.wake.notified().await,
                }
            }
        });
        d
    }

    fn push(&self, at: Instant, to: SocketAddr, data: Vec<u8>) {
        let seq = self.next.fetch_add(1, SeqCst);
        lock(&self.queue).push(std::cmp::Reverse(Pending { at, seq, to, data }));
        self.wake.notify_one();
    }
}

/// A UDP relay in front of the host: the viewer connects to it instead.
struct Link {
    addr: SocketAddr,
    state: Arc<Mutex<LinkState>>,
}

impl Link {
    async fn start(host: SocketAddr, up: Impairment, down: Impairment) -> Self {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = socket.local_addr().unwrap();
        let now = Instant::now();
        let dir = |imp| Dir {
            imp,
            next_free: now,
            stats: DirStats::default(),
        };
        let state = Arc::new(Mutex::new(LinkState {
            up: dir(up),
            down: dir(down),
            started: now,
            rng: 0x9E37_79B9_7F4A_7C15,
            cut: false,
            lose_down_until: None,
        }));
        // One delivery task per direction, sending in order of arrival time, so a
        // batch goes out in the order it was sent (as a radio hands it over); only
        // jitter reorders.
        let up_q = Delivery::start(socket.clone());
        let down_q = Delivery::start(socket.clone());
        let st = state.clone();
        tokio::spawn(async move {
            let mut viewer: Option<SocketAddr> = None;
            let mut buf = vec![0u8; 65_536];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                let upward = from != host;
                if upward {
                    viewer = Some(from);
                }
                let Some(to) = (if upward { Some(host) } else { viewer }) else {
                    continue;
                };
                let Some(at) = lock(&st).admit(upward, n, Instant::now()) else {
                    continue;
                };
                let q = if upward { &up_q } else { &down_q };
                q.push(at, to, buf[..n].to_vec());
            }
        });
        Self { addr, state }
    }

    fn cut(&self, cut: bool) {
        lock(&self.state).cut = cut;
    }

    /// Changes what the link does to the host's packets from now on.
    fn set_down(&self, change: impl FnOnce(&mut Impairment)) {
        change(&mut lock(&self.state).down.imp);
    }

    /// Loses everything the host sends for the next `time`.
    fn lose(&self, time: Duration) {
        lock(&self.state).lose_down_until = Some(Instant::now() + time);
    }

    /// The 95th percentile of how long the host's packets waited at the bottleneck,
    /// over the last `n` of them.
    fn queue_p95(&self, n: usize) -> Duration {
        let s = lock(&self.state);
        let q = &s.down.stats.queued;
        p95(&q[q.len().saturating_sub(n)..])
    }

    /// Packets the host sent that never arrived (lost or overflowed).
    fn dropped(&self) -> u64 {
        let s = lock(&self.state);
        s.down.stats.lost + s.down.stats.overflowed
    }

    fn summary(&self) -> String {
        let s = lock(&self.state);
        let d = &s.down.stats;
        format!(
            "down: {} sent, {} lost, {} overflowed",
            d.sent, d.lost, d.overflowed
        )
    }
}

fn p95(v: &[Duration]) -> Duration {
    if v.is_empty() {
        return Duration::ZERO;
    }
    let mut v = v.to_vec();
    v.sort();
    v[(v.len() * 95 / 100).min(v.len() - 1)]
}

// ---------------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------------

/// Whether the screen is still at this time into the session.
type StillScreen = fn(Duration) -> bool;

/// Frames as large as the rate controller's bitrate allows; key frames four times that.
/// With `still`, the screen is still (small frames) whenever it says so.
struct SizedEncoder {
    fps: u32,
    bps: Arc<AtomicU32>,
    /// The picture needs no more than this, when not zero.
    needs_bps: Arc<AtomicU32>,
    key_next: bool,
    still: Option<StillScreen>,
    started: Instant,
}

impl Encoder for SizedEncoder {
    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            dynamic_bitrate: true,
            hardware: true,
            ..EncoderCaps::default()
        }
    }
    fn encode(
        &mut self,
        _: &CapturedFrame,
        id: FrameId,
        o: EncodeOpts,
        out: &mut Vec<u8>,
    ) -> nexa_media::Result<EncodedMeta> {
        let key = o.force_key || self.key_next || id == 0;
        self.key_next = false;
        let needs = self.needs_bps.load(SeqCst);
        let bps = self.bps.load(SeqCst);
        let mut size =
            (if needs > 0 { bps.min(needs) } else { bps } / 8 / self.fps.max(1)) as usize;
        if self
            .still
            .is_some_and(|still| still(self.started.elapsed()))
        {
            size = size.min(800);
        }
        out.clear();
        out.resize(if key { size * 4 } else { size }.max(64), (id % 251) as u8);
        Ok(EncodedMeta {
            frame_id: id,
            key,
            recovery_point: false,
        })
    }
    fn set_bitrate(&mut self, bps: u32) -> nexa_media::Result<()> {
        self.bps.store(bps, SeqCst);
        Ok(())
    }
    fn invalidate_from(&mut self, _: FrameId) -> nexa_media::Result<Recovery> {
        self.key_next = true;
        Ok(Recovery::Idr)
    }
    fn on_frame_acked(&mut self, _: FrameId) {}
    fn force_idr(&mut self) {
        self.key_next = true;
    }
}

/// When each pointer position reached the host.
#[derive(Default)]
struct HostProbe {
    moves: Mutex<Vec<(f64, Instant)>>,
    bps: Arc<AtomicU32>,
    /// See [`SizedEncoder::needs_bps`]; set before connecting.
    needs_bps: Arc<AtomicU32>,
    /// When the screen is still (see [`SizedEncoder::still`]); set before connecting.
    still: Mutex<Option<StillScreen>>,
    started: Mutex<Option<Instant>>,
}

struct MoveSink(Arc<HostProbe>);

impl InputSink for MoveSink {
    fn key(&mut self, _: u32, _: bool, _: bool, _: Instant) {}
    fn move_absolute(&mut self, x: f64, _: f64) {
        lock(&self.0.moves).push((x, Instant::now()));
    }
    fn move_relative(&mut self, _: i32, _: i32) {}
    fn button(&mut self, _: u32, _: bool, _: Option<(f64, f64)>, _: Instant) {}
    fn wheel(&mut self, _: i32, _: i32) {}
    fn text(&mut self, _: &str) {}
    fn apply_key_state(&mut self, _: &[u32], _: Instant) {}
    fn poll_timeout(&mut self, _: Instant) {}
    fn release_all(&mut self, _: Instant) {}
}

struct WeakHost {
    base: FakePlatform,
    probe: Arc<HostProbe>,
}

fn screen() -> ScreenInfo {
    // Points and pixels alike, so a viewer's x is the host's x.
    ScreenInfo {
        display_id: 1,
        origin_points: (0.0, 0.0),
        size_points: (1920.0, 1080.0),
        size_pixels: (1920, 1080),
        refresh_hz: 60,
        name: "weak host".into(),
    }
}

/// The shared fake capturer has a picture every 5 ms; real ones (ScreenCaptureKit,
/// DXGI) deliver at most the stream's frame rate. Without this the host would send
/// three times the frames the rate controller sized.
struct PacedCapturer {
    inner: Box<dyn Capturer>,
    interval: Duration,
    next_at: Instant,
}

impl Capturer for PacedCapturer {
    fn next(&mut self, timeout: Duration) -> nexa_media::Result<nexa_media::CaptureEvent> {
        let now = Instant::now();
        if self.next_at > now {
            let wait = self.next_at - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(nexa_media::CaptureEvent::Idle);
            }
            std::thread::sleep(wait);
        }
        self.next_at = self.next_at.max(now - self.interval) + self.interval;
        self.inner.next(timeout)
    }
}

impl Platform for WeakHost {
    fn screen(&self) -> Result<ScreenInfo, String> {
        Ok(screen())
    }
    fn displays(&self) -> Result<Vec<ScreenInfo>, String> {
        Ok(vec![screen()])
    }
    fn capturer(&self, c: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        Ok(Box::new(PacedCapturer {
            interval: Duration::from_secs_f64(1.0 / f64::from(c.fps.max(1))),
            next_at: Instant::now(),
            inner: self.base.capturer(c)?,
        }))
    }
    fn encoder(&self, c: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        self.probe.bps.store(c.bitrate_bps, SeqCst);
        Ok(Box::new(SizedEncoder {
            fps: c.fps,
            bps: self.probe.bps.clone(),
            needs_bps: self.probe.needs_bps.clone(),
            key_next: true,
            still: *lock(&self.probe.still),
            started: lock(&self.probe.started).unwrap_or_else(Instant::now),
        }))
    }
    fn input_available(&self) -> bool {
        true
    }
    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        Ok(Box::new(MoveSink(self.probe.clone())))
    }
    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        self.base.clipboard()
    }
}

struct Host {
    agent: Agent,
    probe: Arc<HostProbe>,
    addr: SocketAddr,
    _dir: tempfile::TempDir,
}

async fn host() -> Host {
    let dir = tempfile::tempdir().unwrap();
    let identity = DeviceIdentity::generate();
    let store =
        PairingStore::open(dir.path().join("pairings.json"), identity.store_mac_key()).unwrap();
    let probe = Arc::new(HostProbe::default());
    let platform = WeakHost {
        base: FakePlatform::default(),
        probe: probe.clone(),
    };
    let agent = start(
        AgentConfig {
            lan_discovery: false,
            web: None,
            web_tls: None,
            password: Some(Arc::new(nexa_auth::HostPassword::create(PASSWORD).unwrap())),
            listen: "127.0.0.1:0".parse().unwrap(),
            device_name: "weak host".into(),
            os: Os::Macos,
            caps: caps(),
            timeouts: Timeouts::default(),
            signal: None,
            files: nexa_agent::FileSettings {
                downloads: dir.path().join("Downloads"),
                limits: Default::default(),
            },
        },
        identity,
        store,
        Arc::new(platform),
        FakeUi::allowing(perms(true, true, false), false) as Arc<dyn AgentUi>,
    )
    .await
    .unwrap();
    let addr = agent.local_addr();
    Host {
        agent,
        probe,
        addr,
        _dir: dir,
    }
}

// ---------------------------------------------------------------------------------
// Viewer
// ---------------------------------------------------------------------------------

/// When each picture went on screen.
struct TimedPresenter(Arc<Mutex<Vec<Instant>>>);

impl Presenter for TimedPresenter {
    fn resize(&mut self, _: u32, _: u32, _: f32) -> nexa_media::Result<()> {
        Ok(())
    }
    fn set_mode(&mut self, _: PresentMode) -> nexa_media::Result<()> {
        Ok(())
    }
    fn present(&mut self, _: &DecodedFrame) -> nexa_media::Result<PresentStat> {
        lock(&self.0).push(Instant::now());
        Ok(PresentStat::default())
    }
}

struct NoKeys;

impl KeySource for NoKeys {
    fn set_active(&mut self, _: bool) -> Result<(), KeySourceError> {
        Ok(())
    }
}

#[derive(Default)]
struct ViewerProbe {
    decoded: Arc<AtomicU32>,
    shown: Arc<Mutex<Vec<Instant>>>,
    input_ready: AtomicBool,
}

struct WeakDeps {
    probe: Arc<ViewerProbe>,
    dir: std::path::PathBuf,
}

impl Deps for WeakDeps {
    fn os(&self) -> Os {
        Os::Macos
    }
    fn key_store(&self) -> Box<dyn KeyStore> {
        Box::new(FileKeyStore::new(self.dir.join("viewer.key")))
    }
    fn caps(&self) -> Caps {
        caps()
    }
    fn decoder(&self, _: Codec) -> Result<Box<dyn Decoder>, String> {
        Ok(Box::new(FakeDecoder(self.probe.decoded.clone())))
    }
    fn key_source(&self, _: Sender<InputCmd>) -> Box<dyn KeySource> {
        self.probe.input_ready.store(true, SeqCst);
        Box::new(NoKeys)
    }
    fn fallback_key_source(&self, _: Sender<InputCmd>) -> Option<Box<dyn KeySource>> {
        None
    }
    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        Err("no clipboard here".into())
    }
    unsafe fn attach_view(
        &self,
        _: *mut c_void,
        _: u32,
        _: u32,
        _: u32,
        _: f32,
        _: PresentMode,
    ) -> Result<Box<dyn Presenter>, String> {
        Ok(Box::new(TimedPresenter(self.probe.shown.clone())))
    }
    fn link_options(&self) -> nexa_session::rendezvous::link::LinkOptions {
        nexa_session::rendezvous::link::LinkOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            interfaces: nexa_session::rendezvous::link::Interfaces::Fixed(vec![
                "127.0.0.1".parse().unwrap(),
            ]),
            ..Default::default()
        }
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<(Instant, Event)>>);

unsafe extern "C" fn on_event(user: *mut c_void, pb: *const u8, len: usize) {
    // SAFETY: `user` is the Events this test keeps alive, `pb` valid for `len`.
    let (events, bytes) = unsafe {
        (
            &*(user as *const Events),
            std::slice::from_raw_parts(pb, len),
        )
    };
    lock(&events.0).push((Instant::now(), Event::decode(bytes).unwrap()));
}

struct Viewer {
    client: *mut NexaClient,
    session: *mut NexaSession,
    events: Arc<Events>,
    probe: Arc<ViewerProbe>,
    _dir: tempfile::TempDir,
}

// SAFETY: the handles are registry ids, usable from any thread.
unsafe impl Send for Viewer {}

impl Viewer {
    /// Connects through `link` and waits for the picture. Returns how long that took.
    async fn connect(h: &Host, link: &Link) -> (Self, Duration) {
        Self::connect_with(link, ConnectAuth::Code(h.agent.current_code().to_string())).await
    }

    async fn connect_with(link: &Link, auth: ConnectAuth) -> (Self, Duration) {
        Self::connect_in(link, auth, mode_under_test()).await
    }

    async fn connect_in(link: &Link, auth: ConnectAuth, mode: Mode) -> (Self, Duration) {
        let started = Instant::now();
        let dir = tempfile::tempdir().unwrap();
        let probe = Arc::new(ViewerProbe::default());
        let deps = Arc::new(WeakDeps {
            probe: probe.clone(),
            dir: dir.path().to_path_buf(),
        });
        let events = Arc::new(Events::default());
        let cfg = nexa_client::proto::ClientConfig {
            device_name: "weak viewer".into(),
            data_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        }
        .encode_to_vec();
        let mut client = std::ptr::null_mut();
        // SAFETY: valid buffers; `events` outlives the client.
        let status = unsafe {
            abi::init_with_deps(
                deps,
                cfg.as_ptr(),
                cfg.len(),
                Some(on_event),
                Arc::as_ptr(&events) as *mut c_void,
                &mut client,
            )
        };
        assert_eq!(status, abi::NEXA_OK);
        let request = ConnectRequest {
            address: link.addr.to_string(),
            tag: 1,
            mode: mode as i32,
            fps: 0,
            auth: Some(auth),
            signal_server: String::new(),
            signal_secret: String::new(),
        }
        .encode_to_vec();
        let mut session = std::ptr::null_mut();
        // SAFETY: valid buffers and handle.
        let status =
            unsafe { abi::nexa_connect(client, request.as_ptr(), request.len(), &mut session) };
        assert_eq!(status, abi::NEXA_OK);
        let v = Self {
            client,
            session,
            events,
            probe,
            _dir: dir,
        };
        until("connected", WAIT, || {
            v.saw(|b| matches!(b, EventBody::Connected(_)))
        })
        .await;
        // SAFETY: a fake handle: the fake deps never dereference it.
        let status = unsafe {
            abi::nexa_session_attach_view(
                session,
                std::ptr::dangling_mut::<c_void>(),
                abi::NEXA_VIEW_NSVIEW,
                1920,
                1080,
                1.0,
            )
        };
        assert_eq!(status, abi::NEXA_OK);
        until("the first picture", WAIT, || {
            !lock(&v.probe.shown).is_empty()
        })
        .await;
        // SAFETY: handles are registry ids.
        unsafe { abi::nexa_session_set_input_active(v.session, true) };
        until("input", WAIT, || v.probe.input_ready.load(SeqCst)).await;
        (v, started.elapsed())
    }

    fn saw(&self, f: impl Fn(&EventBody) -> bool) -> bool {
        lock(&self.events.0)
            .iter()
            .any(|(_, e)| e.body.as_ref().is_some_and(&f))
    }

    fn saw_since(&self, t: Instant, f: impl Fn(&EventBody) -> bool) -> bool {
        lock(&self.events.0)
            .iter()
            .any(|(at, e)| *at >= t && e.body.as_ref().is_some_and(&f))
    }

    fn pointer(&self, x: f64) {
        let bytes = SessionCommand {
            cmd: Some(Command::Move(PointerMove { x, y: 500.0 })),
        }
        .encode_to_vec();
        // SAFETY: valid buffer and handle.
        unsafe { abi::nexa_session_command(self.session, bytes.as_ptr(), bytes.len()) };
    }

    /// Pictures shown from `t` on.
    fn shown_since(&self, t: Instant) -> Vec<Instant> {
        lock(&self.probe.shown)
            .iter()
            .copied()
            .filter(|s| *s >= t)
            .collect()
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // SAFETY: the handle came from init.
        unsafe { abi::nexa_shutdown(self.client) };
    }
}

// ---------------------------------------------------------------------------------
// Measuring
// ---------------------------------------------------------------------------------

#[derive(Debug)]
struct Measured {
    fps: f64,
    /// The longest time without a new picture.
    freeze: Duration,
    /// How long pointer moves took to reach the host (95th percentile, and the worst).
    input_p95: Duration,
    input_max: Duration,
    /// Moves that never arrived (later ones may have replaced them, which is fine).
    input_lost: usize,
    /// The encoder's bitrate at the end.
    bitrate_mbps: f64,
}

/// Watches the session for `secs`, moving the pointer every 20 ms.
async fn measure(v: &Viewer, h: &Host, secs: u64) -> Measured {
    let start = Instant::now();
    let mut sent = Vec::new();
    let mut x = 1.0;
    while start.elapsed() < Duration::from_secs(secs) {
        x += 1.0;
        v.pointer(x);
        sent.push((x, Instant::now()));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Let the last moves land.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let end = start + Duration::from_secs(secs);
    let shown: Vec<Instant> = v
        .shown_since(start)
        .into_iter()
        .filter(|t| *t <= end)
        .collect();
    let mut freeze = shown
        .first()
        .map_or(Duration::from_secs(secs), |f| *f - start);
    for w in shown.windows(2) {
        freeze = freeze.max(w[1] - w[0]);
    }
    if let Some(last) = shown.last() {
        freeze = freeze.max(end.saturating_duration_since(*last));
    }
    // A move counts as arrived when the host reached its position or a later one.
    let arrived = lock(&h.probe.moves).clone();
    let mut delays = Vec::new();
    let mut lost = 0;
    for (sx, at) in &sent {
        match arrived.iter().find(|(hx, t)| *hx >= *sx && *t >= *at) {
            Some((_, t)) => delays.push(*t - *at),
            None => lost += 1,
        }
    }
    Measured {
        fps: shown.len() as f64 / secs as f64,
        freeze,
        input_p95: p95(&delays),
        input_max: delays.iter().copied().max().unwrap_or_default(),
        input_lost: lost,
        bitrate_mbps: f64::from(h.probe.bps.load(SeqCst)) / 1e6,
    }
}

fn report(name: &str, connect: Duration, m: &Measured, link: &Link) {
    eprintln!(
        "{name:<34} connect {:>5} ms | {:>5.1} fps | freeze {:>5} ms | input p95 {:>4} ms max {:>5} ms lost {:>3} | {:>5.1} Mbit/s | {}",
        connect.as_millis(),
        m.fps,
        m.freeze.as_millis(),
        m.input_p95.as_millis(),
        m.input_max.as_millis(),
        m.input_lost,
        m.bitrate_mbps,
        link.summary(),
    );
}

async fn scenario(
    name: &str,
    up: Impairment,
    down: Impairment,
    secs: u64,
) -> (Host, Link, Viewer, Measured) {
    let h = host().await;
    let link = Link::start(h.addr, up, down).await;
    let (v, connect) = Viewer::connect(&h, &link).await;
    // The rate controller settles first.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let m = measure(&v, &h, secs).await;
    report(name, connect, &m, &link);
    (h, link, v, m)
}

/// The mode the scenarios run in: Balanced, or `NEXA_E2E_MODE=office|game` to run
/// the whole suite in another (its limits were set for Balanced).
fn mode_under_test() -> Mode {
    match std::env::var("NEXA_E2E_MODE").as_deref() {
        Ok("office") => Mode::Office,
        Ok("game") => Mode::Game,
        _ => Mode::Balanced,
    }
}

fn both(imp: Impairment) -> (Impairment, Impairment) {
    (imp, imp)
}

// ---------------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_lan_is_smooth() {
    let (up, down) = both(Impairment {
        delay: Duration::from_millis(1),
        ..Impairment::CLEAN
    });
    let (_h, _l, _v, m) = scenario("clean LAN", up, down, 5).await;
    assert!(m.fps >= 50.0, "{m:?}");
    assert!(m.freeze < Duration::from_millis(200), "{m:?}");
    assert!(m.input_p95 < Duration::from_millis(50), "{m:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_percent_loss_keeps_the_picture_moving() {
    let (up, down) = both(Impairment {
        loss: 0.02,
        delay: Duration::from_millis(20),
        ..Impairment::CLEAN
    });
    let (_h, _l, _v, m) = scenario("2% loss, 40 ms RTT", up, down, 6).await;
    assert!(m.fps >= 40.0, "{m:?}");
    assert!(m.freeze < Duration::from_millis(500), "{m:?}");
    assert!(m.input_p95 < Duration::from_millis(150), "{m:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_percent_loss_is_usable() {
    let (up, down) = both(Impairment {
        loss: 0.10,
        delay: Duration::from_millis(20),
        ..Impairment::CLEAN
    });
    let (_h, _l, v, m) = scenario("10% loss, 40 ms RTT", up, down, 6).await;
    assert!(!v.saw(|b| matches!(b, EventBody::Disconnected(_))));
    assert!(m.fps >= 20.0, "{m:?}");
    assert!(m.freeze < Duration::from_secs(1), "{m:?}");
    assert!(m.input_p95 < Duration::from_millis(300), "{m:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_far_jittery_link_stays_responsive() {
    let (up, down) = both(Impairment {
        delay: Duration::from_millis(150),
        jitter: Duration::from_millis(40),
        loss: 0.01,
        ..Impairment::CLEAN
    });
    let (_h, _l, _v, m) = scenario("300 ms RTT, 40 ms jitter, 1% loss", up, down, 6).await;
    assert!(m.fps >= 30.0, "{m:?}");
    assert!(m.freeze < Duration::from_secs(1), "{m:?}");
    assert!(m.input_p95 < Duration::from_millis(450), "{m:?}");
}

/// A frame lost on a path that reorders is repaired by the next key frame, one round
/// trip later. That key frame used to leave in half a frame interval: at 20 Mbit/s
/// some 140 packets in 8 ms, of which QUIC discarded as duplicates the fifth that 40 ms
/// of jitter delivered more than 128 packet numbers late, so the key frame was lost
/// too, and the next one, for one to four seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_frame_survives_a_path_that_reorders() {
    let (up, down) = both(Impairment {
        delay: Duration::from_millis(150),
        jitter: Duration::from_millis(40),
        ..Impairment::CLEAN
    });
    let h = host().await;
    // A picture that takes 20 Mbit/s: its key frames, four frames each, are 145
    // packets. (The path itself carries no more than some 30: at 3.2 packets a
    // millisecond, 40 ms of jitter is 128 packet numbers.)
    h.probe.needs_bps.store(20_000_000, SeqCst);
    let link = Link::start(h.addr, up, down).await;
    let (v, _) = Viewer::connect(&h, &link).await;
    until("20 Mbit/s", Duration::from_secs(40), || {
        h.probe.bps.load(SeqCst) >= 20_000_000
    })
    .await;
    let mut worst = Duration::ZERO;
    for _ in 0..4 {
        let lost = Instant::now();
        // Most of a frame, whichever one is on its way: more than FEC repairs.
        link.lose(Duration::from_millis(12));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        // The longest time without a new picture, up to now if none came back.
        let mut shown = v.shown_since(lost);
        shown.push(Instant::now());
        let freeze = shown.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        eprintln!(
            "a frame lost at 20 Mbit/s, 300 ms RTT, 40 ms jitter: no picture for {} ms",
            freeze.as_millis()
        );
        worst = worst.max(freeze);
    }
    // One round: the loss noticed, the report up, the key frame down.
    assert!(worst < Duration::from_millis(800), "{worst:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_link_is_not_flooded() {
    // 3 Mbit/s with a deep (bufferbloated) queue: the sender must stay under it, or
    // everything, input included, waits behind its own video.
    let down = Impairment {
        rate_bps: Some(3_000_000),
        queue: Duration::from_millis(800),
        delay: Duration::from_millis(15),
        ..Impairment::CLEAN
    };
    let up = Impairment {
        delay: Duration::from_millis(15),
        ..Impairment::CLEAN
    };
    let (_h, link, _v, m) = scenario("3 Mbit/s, 800 ms buffer", up, down, 8).await;
    let queue = link.queue_p95(300);
    eprintln!(
        "    bottleneck queue p95 over the last packets: {} ms",
        queue.as_millis()
    );
    assert!(m.bitrate_mbps <= 3.0, "{m:?}");
    assert!(queue < Duration::from_millis(250), "queue {queue:?}");
    assert!(m.fps >= 20.0, "{m:?}");
    assert!(m.input_p95 < Duration::from_millis(300), "{m:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wifi_stalls_do_not_end_the_session() {
    // Half a second of nothing every four seconds.
    let (up, down) = both(Impairment {
        bursts: Some((Duration::from_secs(4), Duration::from_millis(500))),
        delay: Duration::from_millis(5),
        ..Impairment::CLEAN
    });
    let (_h, _l, v, m) = scenario("500 ms stall every 4 s", up, down, 9).await;
    assert!(!v.saw(|b| matches!(b, EventBody::Disconnected(_))));
    assert!(m.fps >= 30.0, "{m:?}");
    assert!(m.freeze < Duration::from_millis(1200), "{m:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_short_outage_is_ridden_out() {
    let (up, down) = both(Impairment {
        delay: Duration::from_millis(5),
        ..Impairment::CLEAN
    });
    let h = host().await;
    let link = Link::start(h.addr, up, down).await;
    let (v, _) = Viewer::connect(&h, &link).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    link.cut(true);
    tokio::time::sleep(Duration::from_secs(6)).await;
    let restored = Instant::now();
    link.cut(false);
    until("pictures after the outage", WAIT, || {
        v.shown_since(restored).len() >= 30
    })
    .await;
    let back = v.shown_since(restored)[0] - restored;
    eprintln!(
        "6 s outage: picture back {} ms after the network",
        back.as_millis()
    );
    assert!(!v.saw(|b| matches!(b, EventBody::Disconnected(_))));
    assert!(back < Duration::from_secs(2), "{back:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_outage_reconnects_by_itself() {
    let (up, down) = both(Impairment {
        delay: Duration::from_millis(5),
        ..Impairment::CLEAN
    });
    let h = host().await;
    let link = Link::start(h.addr, up, down).await;
    let (v, _) = Viewer::connect_with(&link, ConnectAuth::Password(PASSWORD.into())).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let cut = Instant::now();
    link.cut(true);
    until("the viewer notices", WAIT, || {
        v.saw_since(cut, |b| matches!(b, EventBody::Reconnecting(_)))
    })
    .await;
    let noticed = cut.elapsed();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let restored = Instant::now();
    link.cut(false);
    until(
        "pictures after reconnecting",
        Duration::from_secs(40),
        || v.shown_since(restored).len() >= 30,
    )
    .await;
    let back = v.shown_since(restored)[0] - restored;
    eprintln!(
        "long outage: noticed after {} ms, picture back {} ms after the network",
        noticed.as_millis(),
        back.as_millis()
    );
    assert!(!v.saw(|b| matches!(b, EventBody::Disconnected(_))));
    assert!(back < Duration::from_secs(10), "{back:?}");
}

/// Light work on a far link (typing, with a larger repaint for 0.3 s of every 2 s),
/// a slow spell of four seconds that cuts the bitrate, and the link as it was again.
/// The recovery probes that find out at once whether the old rate is back did
/// nothing here: their padding rode on small frames that may carry little, the pacer
/// spread what was left like a key frame, and they were judged before a round trip
/// had reported them. The bitrate then crept back only while the screen was busy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn light_work_on_a_far_link_gets_its_bitrate_back() {
    let (up, down) = both(Impairment {
        delay: Duration::from_millis(135),
        jitter: Duration::from_millis(5),
        ..Impairment::CLEAN
    });
    let h = host().await;
    *lock(&h.probe.still) = Some(|t| t.as_millis() % 2000 >= 300);
    *lock(&h.probe.started) = Some(Instant::now());
    let link = Link::start(h.addr, up, down).await;
    let (_v, _) = Viewer::connect(&h, &link).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let before = h.probe.bps.load(SeqCst);
    // Four seconds at 1 Mbit/s: two repaints, whenever in their cycle it begins.
    link.set_down(|imp| imp.rate_bps = Some(1_000_000));
    tokio::time::sleep(Duration::from_secs(4)).await;
    let low = h.probe.bps.load(SeqCst);
    link.set_down(|imp| imp.rate_bps = None);
    let restored = Instant::now();
    let mut back = None;
    // For a failure: the bitrate and what the host sent, every half second.
    let mut seen = String::new();
    let (mut last_sent, mut next_note) = (lock(&link.state).down.stats.sent, Duration::ZERO);
    while restored.elapsed() < Duration::from_secs(20) {
        let bps = h.probe.bps.load(SeqCst);
        if restored.elapsed() >= next_note {
            next_note += Duration::from_millis(500);
            let sent = lock(&link.state).down.stats.sent;
            seen += &format!(" {:.1}/{}", f64::from(bps) / 1e6, sent - last_sent);
            last_sent = sent;
        }
        if f64::from(bps) >= 0.8 * f64::from(before) {
            back = Some(restored.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("    after the slow spell, Mbit/s / packets per half second:{seen}");
    eprintln!(
        "light work, 270 ms round trip, 4 s at 1 Mbit/s: bitrate {:.1} Mbit/s before, {:.1} at the end of the slow spell, back at 80% after {:?}",
        f64::from(before) / 1e6,
        f64::from(low) / 1e6,
        back
    );
    assert!(
        f64::from(low) < 0.5 * f64::from(before),
        "no cut to recover from"
    );
    assert!(
        back.is_some_and(|t| t < Duration::from_secs(8)),
        "the bitrate stayed down: {back:?}"
    );
}

/// Game mode gives a frame a frame interval and a half (25 ms) to arrive, and a
/// phone's hotspot hands packets over every 40 ms: a frame split over two batches
/// was declared lost with all of its shards on the way, the key frame sent to repair
/// it likewise, and the picture stood still for as long as the screen moved (four
/// seconds and more, in 0.1.14 too).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn game_mode_on_a_phone_hotspot_keeps_its_frames() {
    // 5 Mbit/s down, 180 ms round trip, a deep buffer, 40 ms batches; the screen is
    // still for six seconds of every ten and moves for four.
    let down = Impairment {
        rate_bps: Some(5_000_000),
        queue: Duration::from_millis(1000),
        delay: Duration::from_millis(90),
        jitter: Duration::from_millis(5),
        batch: Duration::from_millis(40),
        ..Impairment::CLEAN
    };
    let up = Impairment {
        delay: Duration::from_millis(90),
        batch: Duration::from_millis(40),
        ..Impairment::CLEAN
    };
    let h = host().await;
    *lock(&h.probe.still) = Some(|t| t.as_secs() % 10 < 6);
    *lock(&h.probe.started) = Some(Instant::now());
    let link = Link::start(h.addr, up, down).await;
    let auth = ConnectAuth::Code(h.agent.current_code().to_string());
    let (v, connect) = Viewer::connect_in(&link, auth, Mode::Game).await;
    tokio::time::sleep(Duration::from_secs(18)).await;
    let m = measure(&v, &h, 20).await;
    report("hotspot 5 Mbit/s, game mode", connect, &m, &link);
    let queue = link.queue_p95(4000);
    eprintln!(
        "    bottleneck queue p95 over the measured part: {} ms",
        queue.as_millis()
    );
    assert!(m.freeze < Duration::from_secs(1), "{m:?}");
    assert!(m.fps >= 40.0, "{m:?}");
    assert!(queue < Duration::from_millis(200), "queue {queue:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_hotspot_is_not_flooded_when_the_screen_moves() {
    // 5 Mbit/s down, 180 ms round trip, a deep buffer, 40 ms batches; the screen is
    // still for six seconds of every ten and moves for four.
    let down = Impairment {
        rate_bps: Some(5_000_000),
        queue: Duration::from_millis(1000),
        delay: Duration::from_millis(90),
        jitter: Duration::from_millis(5),
        batch: Duration::from_millis(40),
        ..Impairment::CLEAN
    };
    let up = Impairment {
        delay: Duration::from_millis(90),
        batch: Duration::from_millis(40),
        ..Impairment::CLEAN
    };
    let h = host().await;
    *lock(&h.probe.still) = Some(|t| t.as_secs() % 10 < 6);
    *lock(&h.probe.started) = Some(Instant::now());
    let link = Link::start(h.addr, up, down).await;
    let (v, connect) = Viewer::connect(&h, &link).await;
    // Two cycles to learn the link, then measure two more.
    tokio::time::sleep(Duration::from_secs(18)).await;
    let (dropped0, decoded0) = (link.dropped(), v.probe.decoded.load(SeqCst));
    let m = measure(&v, &h, 20).await;
    eprintln!(
        "    in the measured 20 s: {} packets dropped at the bottleneck, {} frames decoded",
        link.dropped() - dropped0,
        v.probe.decoded.load(SeqCst) - decoded0
    );
    report("hotspot 5 Mbit/s, 180 ms, batches", connect, &m, &link);
    let queue = link.queue_p95(4000);
    eprintln!(
        "    bottleneck queue p95 over the measured part: {} ms",
        queue.as_millis()
    );
    assert!(queue < Duration::from_millis(200), "queue {queue:?}");
    assert!(m.input_p95 < Duration::from_millis(400), "{m:?}");
    assert!(m.freeze < Duration::from_secs(1), "{m:?}");
}
