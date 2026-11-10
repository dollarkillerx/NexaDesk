//! In-session features end to end over loopback QUIC: the host cursor (§6.9), game
//! mode's pointer lock (§7.4), system audio and its permission, and display
//! switching (§6.7). Every device on both sides is a fake: nothing captures the
//! screen or audio, plays sound, injects input, locks the mouse or changes the cursor.

#[path = "../../nexa-agent/tests/common/mod.rs"]
mod common;

use std::{
    ffi::c_void,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::SeqCst},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

use common::{
    FakeClip, FakeDecoder, FakePlatform, FakeUi, NullPresenter, caps, lock, perms, until,
};
use nexa_agent::{Agent, AgentConfig, AgentUi, Platform, ScreenInfo, start};
use nexa_auth::{DeviceIdentity, FileKeyStore, KeyStore, PairingStore};
use nexa_client::{
    abi::{self, NexaClient, NexaSession},
    deps::{
        AudioOutput, AudioRender, CursorDisplay, Deps, KeySource, KeySourceError, LocalCursor,
        PointerLock,
    },
    input::InputCmd,
    proto::{
        Command, ConnectAuth, ConnectRequest, Event, EventBody, PointerButton, PointerMove,
        SelectDisplay, SessionCommand, SetAudio, SetMode,
    },
};
use nexa_media::{
    AudioCapture, AudioChunk, AudioDecoder, AudioEncoder, CaptureConfig, Capturer, CursorImage,
    CursorSample, CursorSource, Decoder, Encoder, EncoderConfig, PresentMode, Presenter,
};
use nexa_session::{
    clip::{ClipPlatform, PromiseResolver},
    handshake::Timeouts,
    input::InputSink,
};
use nexa_wire::control::{Caps, Codec, Mode, Os, Permissions};
use prost::Message;

const WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------
// Host fakes
// ---------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Hosted {
    Abs(f64, f64),
    Rel(i32, i32),
    Button(u32, bool, Option<(f64, f64)>),
    ReleaseAll,
}

#[derive(Default)]
struct HostProbe {
    captures: Mutex<Vec<CaptureConfig>>,
    injected: Mutex<Vec<Hosted>>,
    cursor_sources: AtomicU32,
    cursor: Mutex<CursorSample>,
    audio_started: AtomicU32,
    audio_running: AtomicBool,
    audio_frames: AtomicU64,
}

struct Features {
    base: FakePlatform,
    probe: Arc<HostProbe>,
    displays: Vec<ScreenInfo>,
}

struct RecordingSink(Arc<HostProbe>);

impl InputSink for RecordingSink {
    fn key(&mut self, _: u32, _: bool, _: bool, _: Instant) {}
    fn move_absolute(&mut self, x: f64, y: f64) {
        lock(&self.0.injected).push(Hosted::Abs(x, y));
    }
    fn move_relative(&mut self, dx: i32, dy: i32) {
        lock(&self.0.injected).push(Hosted::Rel(dx, dy));
    }
    fn button(&mut self, b: u32, down: bool, at: Option<(f64, f64)>, _: Instant) {
        lock(&self.0.injected).push(Hosted::Button(b, down, at));
    }
    fn wheel(&mut self, _: i32, _: i32) {}
    fn text(&mut self, _: &str) {}
    fn apply_key_state(&mut self, _: &[u32], _: Instant) {}
    fn poll_timeout(&mut self, _: Instant) {}
    fn release_all(&mut self, _: Instant) {
        lock(&self.0.injected).push(Hosted::ReleaseAll);
    }
}

struct ScriptedCursor(Arc<HostProbe>);

impl CursorSource for ScriptedCursor {
    fn sample(&mut self) -> CursorSample {
        lock(&self.0.cursor).clone()
    }
}

/// 10 ms of a constant level every 10 ms, for under a second at a time in practice.
struct FakeMic(Arc<HostProbe>);

impl AudioCapture for FakeMic {
    fn next(&mut self, _: Duration) -> nexa_media::Result<Option<AudioChunk>> {
        thread::sleep(Duration::from_millis(10));
        Ok(Some(AudioChunk {
            samples: vec![0.25; nexa_media::AUDIO_FRAME_LEN],
            captured_at: Instant::now(),
        }))
    }
}

impl Drop for FakeMic {
    fn drop(&mut self) {
        self.0.audio_running.store(false, SeqCst);
    }
}

/// "Encodes" a frame as its first sample scaled to a byte.
struct FakeOpus(Arc<HostProbe>);

impl AudioEncoder for FakeOpus {
    fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> nexa_media::Result<()> {
        self.0.audio_frames.fetch_add(1, SeqCst);
        out.push((pcm[0] * 100.0) as u8);
        Ok(())
    }
}

impl Platform for Features {
    fn screen(&self) -> Result<ScreenInfo, String> {
        Ok(self.displays[0].clone())
    }
    fn displays(&self) -> Result<Vec<ScreenInfo>, String> {
        Ok(self.displays.clone())
    }
    fn capturer(&self, c: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        lock(&self.probe.captures).push(c);
        self.base.capturer(c)
    }
    fn encoder(&self, c: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        self.base.encoder(c)
    }
    fn input_available(&self) -> bool {
        true
    }
    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        Ok(Box::new(RecordingSink(self.probe.clone())))
    }
    fn clipboard(&self, r: PromiseResolver) -> Result<Box<dyn ClipPlatform>, String> {
        self.base.clipboard(r)
    }
    fn cursor_source(&self) -> Option<Box<dyn CursorSource>> {
        self.probe.cursor_sources.fetch_add(1, SeqCst);
        Some(Box::new(ScriptedCursor(self.probe.clone())))
    }
    fn audio_capture(&self, _: u32) -> Result<Box<dyn AudioCapture>, String> {
        self.probe.audio_started.fetch_add(1, SeqCst);
        self.probe.audio_running.store(true, SeqCst);
        Ok(Box::new(FakeMic(self.probe.clone())))
    }
    fn audio_encoder(&self) -> Result<Box<dyn AudioEncoder>, String> {
        Ok(Box::new(FakeOpus(self.probe.clone())))
    }
}

fn display(id: u32, x: f64, w: u32, h: u32) -> ScreenInfo {
    ScreenInfo {
        display_id: id,
        origin_points: (x, 0.0),
        size_points: (f64::from(w) / 2.0, f64::from(h) / 2.0),
        size_pixels: (w, h),
        refresh_hz: 60,
        name: format!("screen {id}"),
    }
}

struct Host {
    agent: Agent,
    probe: Arc<HostProbe>,
    addr: std::net::SocketAddr,
    downloads: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn host(p: Permissions) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let identity = DeviceIdentity::generate();
    let store =
        PairingStore::open(dir.path().join("pairings.json"), identity.store_mac_key()).unwrap();
    let probe = Arc::new(HostProbe::default());
    let platform = Features {
        base: FakePlatform::default(),
        probe: probe.clone(),
        // A 1920×1080 Retina main display and a 1280×720 one to its right.
        displays: vec![display(11, 0.0, 1920, 1080), display(22, 960.0, 1280, 720)],
    };
    let ui = FakeUi::allowing(p, false);
    let agent = start(
        AgentConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            device_name: "feature host".into(),
            os: Os::Macos,
            caps: caps(),
            timeouts: Timeouts::default(),
            signal: None,
            files: nexa_agent::FileSettings {
                downloads: dir.path().join("Downloads/NexaDesk"),
                limits: Default::default(),
            },
        },
        identity,
        store,
        Arc::new(platform),
        ui as Arc<dyn AgentUi>,
    )
    .await
    .unwrap();
    let addr = agent.local_addr();
    Host {
        agent,
        probe,
        addr,
        downloads: dir.path().join("Downloads/NexaDesk"),
        _dir: dir,
    }
}

// ---------------------------------------------------------------------------------
// Viewer fakes
// ---------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Shown {
    Host(u64, f64, f64),
    Hidden,
    Default,
    Warp(f64, f64),
}

#[derive(Default)]
struct ViewerProbe {
    key_tx: Mutex<Option<Sender<InputCmd>>>,
    decoded: Arc<AtomicU32>,
    clip: FakeClip,
    cursor: Arc<Mutex<Vec<Shown>>>,
    locks_taken: AtomicU32,
    locks_held: Arc<AtomicU32>,
    lock_tx: Mutex<Option<Sender<InputCmd>>>,
    outputs_started: AtomicU32,
    outputs_running: Arc<AtomicU32>,
    /// Samples rendered with sound in them.
    heard: Arc<AtomicU64>,
}

struct FakeKeys;

impl KeySource for FakeKeys {
    fn set_active(&mut self, _: bool) -> Result<(), KeySourceError> {
        Ok(())
    }
}

struct FakeCursorDisplay(Arc<Mutex<Vec<Shown>>>);

impl CursorDisplay for FakeCursorDisplay {
    fn show(&mut self, c: Option<&LocalCursor>) {
        lock(&self.0).push(match c {
            Some(c) => Shown::Host(c.id, c.width_pt, c.height_pt),
            None => Shown::Hidden,
        });
    }
    fn reset(&mut self) {
        lock(&self.0).push(Shown::Default);
    }
    fn warp(&mut self, x: f64, y: f64) {
        lock(&self.0).push(Shown::Warp(x, y));
    }
}

struct FakeLock(Arc<AtomicU32>);

impl PointerLock for FakeLock {}

impl Drop for FakeLock {
    fn drop(&mut self) {
        self.0.fetch_sub(1, SeqCst);
    }
}

struct FakeAudioDecoder;

impl AudioDecoder for FakeAudioDecoder {
    fn decode(&mut self, p: Option<&[u8]>, out: &mut [f32]) -> nexa_media::Result<()> {
        let p = p.ok_or(nexa_media::MediaError::Unsupported("no concealment"))?;
        out.fill(f32::from(p[0]) / 100.0);
        Ok(())
    }
    fn reset(&mut self) {}
}

/// Pulls 10 ms every 10 ms into a buffer nobody hears: offline rendering.
struct NullSpeaker {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicU32>,
}

impl AudioOutput for NullSpeaker {}

impl Drop for NullSpeaker {
    fn drop(&mut self) {
        self.stop.store(true, SeqCst);
        self.running.fetch_sub(1, SeqCst);
    }
}

struct FeatureDeps {
    probe: Arc<ViewerProbe>,
    dir: std::path::PathBuf,
}

impl Deps for FeatureDeps {
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
    fn key_source(&self, tx: Sender<InputCmd>) -> Box<dyn KeySource> {
        *lock(&self.probe.key_tx) = Some(tx);
        Box::new(FakeKeys)
    }
    fn fallback_key_source(&self, _: Sender<InputCmd>) -> Option<Box<dyn KeySource>> {
        None
    }
    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        Ok(Box::new(self.probe.clip.clone()))
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
        Ok(Box::new(NullPresenter))
    }
    unsafe fn cursor_display(&self, _: *mut c_void, _: u32) -> Box<dyn CursorDisplay> {
        Box::new(FakeCursorDisplay(self.probe.cursor.clone()))
    }
    fn pointer_lock(&self, tx: Sender<InputCmd>) -> Result<Box<dyn PointerLock>, String> {
        self.probe.locks_taken.fetch_add(1, SeqCst);
        self.probe.locks_held.fetch_add(1, SeqCst);
        *lock(&self.probe.lock_tx) = Some(tx);
        Ok(Box::new(FakeLock(self.probe.locks_held.clone())))
    }
    fn audio_decoder(&self) -> Result<Box<dyn AudioDecoder>, String> {
        Ok(Box::new(FakeAudioDecoder))
    }
    fn audio_output(&self, mut render: AudioRender) -> Result<Box<dyn AudioOutput>, String> {
        self.probe.outputs_started.fetch_add(1, SeqCst);
        self.probe.outputs_running.fetch_add(1, SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let (s, heard) = (stop.clone(), self.probe.heard.clone());
        thread::spawn(move || {
            let mut buf = vec![0.0f32; nexa_media::AUDIO_FRAME_LEN];
            while !s.load(SeqCst) {
                render(&mut buf, Instant::now());
                heard.fetch_add(buf.iter().filter(|v| **v != 0.0).count() as u64, SeqCst);
                thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(Box::new(NullSpeaker {
            stop,
            running: self.probe.outputs_running.clone(),
        }))
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
struct Events(Mutex<Vec<Event>>);

unsafe extern "C" fn on_event(user: *mut c_void, pb: *const u8, len: usize) {
    // SAFETY: `user` is the Events this test keeps alive, `pb` valid for `len`.
    let (events, bytes) = unsafe {
        (
            &*(user as *const Events),
            std::slice::from_raw_parts(pb, len),
        )
    };
    lock(&events.0).push(Event::decode(bytes).unwrap());
}

struct Viewer {
    client: *mut NexaClient,
    session: *mut NexaSession,
    events: Arc<Events>,
    probe: Arc<ViewerProbe>,
    dir: tempfile::TempDir,
}

unsafe impl Send for Viewer {}

impl Viewer {
    async fn connect(h: &Host, mode: Mode) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let probe = Arc::new(ViewerProbe::default());
        let deps = Arc::new(FeatureDeps {
            probe: probe.clone(),
            dir: dir.path().to_path_buf(),
        });
        let events = Arc::new(Events::default());
        let cfg = nexa_client::proto::ClientConfig {
            device_name: "feature viewer".into(),
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
            address: h.addr.to_string(),
            tag: 1,
            mode: mode as i32,
            fps: 0,
            auth: Some(ConnectAuth::Code(h.agent.current_code().to_string())),
            signal_server: String::new(),
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
            dir,
        };
        v.wait_event("Connected", |b| matches!(b, EventBody::Connected(_)))
            .await;
        // SAFETY: a fake handle: the fake deps never dereference it.
        let status = unsafe {
            abi::nexa_session_attach_view(
                session,
                std::ptr::dangling_mut::<c_void>(),
                abi::NEXA_VIEW_NSVIEW,
                1920,
                1080,
                2.0,
            )
        };
        assert_eq!(status, abi::NEXA_OK);
        v
    }

    fn event_matching(&self, f: impl Fn(&EventBody) -> bool) -> Option<EventBody> {
        lock(&self.events.0)
            .iter()
            .rev()
            .filter_map(|e| e.body.as_ref())
            .find(|b| f(b))
            .cloned()
    }

    async fn wait_event(&self, what: &str, f: impl Fn(&EventBody) -> bool + Copy) -> EventBody {
        until(what, WAIT, || self.event_matching(f).is_some()).await;
        self.event_matching(f).unwrap()
    }

    async fn wait_stream(
        &self,
        what: &str,
        f: impl Fn(&nexa_client::proto::StreamChanged) -> bool + Copy,
    ) {
        self.wait_event(what, |b| matches!(b, EventBody::Stream(s) if f(s)))
            .await;
    }

    fn command(&self, cmd: Command) -> i32 {
        let bytes = SessionCommand { cmd: Some(cmd) }.encode_to_vec();
        // SAFETY: valid buffer and handle.
        unsafe { abi::nexa_session_command(self.session, bytes.as_ptr(), bytes.len()) }
    }

    fn set_active(&self, active: bool) {
        // SAFETY: handles are registry ids.
        assert_eq!(
            unsafe { abi::nexa_session_set_input_active(self.session, active) },
            abi::NEXA_OK
        );
    }

    fn input(&self) -> Sender<InputCmd> {
        lock(&self.probe.key_tx)
            .clone()
            .expect("input thread started")
    }

    fn cursor_calls(&self) -> Vec<Shown> {
        lock(&self.probe.cursor).clone()
    }

    fn close(&self) {
        // SAFETY: handles are registry ids.
        unsafe { abi::nexa_session_close(self.session) };
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // SAFETY: the handle came from init.
        unsafe { abi::nexa_shutdown(self.client) };
    }
}

fn arrow(tint: u8) -> Arc<CursorImage> {
    let (w, h) = (32u32, 48u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for p in rgba.as_chunks_mut::<4>().0.iter_mut().take(40) {
        *p = [tint, 0, 0, 255];
    }
    Arc::new(CursorImage {
        width: w,
        height: h,
        rgba,
        hot_x: 2.0,
        hot_y: 2.0,
        scale: 2.0,
    })
}

fn host_cursor(h: &Host, image: &Arc<CursorImage>, visible: bool, at: Option<(f64, f64)>) {
    *lock(&h.probe.cursor) = CursorSample {
        image: Some(image.clone()),
        visible,
        position: at,
    };
}

fn shown_hosts(v: &Viewer) -> Vec<u64> {
    v.cursor_calls()
        .iter()
        .filter_map(|c| match c {
            Shown::Host(id, ..) => Some(*id),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------
// Cursor (§6.9)
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_cursor_becomes_the_local_one_and_leaves_the_video() {
    let h = host(perms(true, true, false)).await;
    host_cursor(&h, &arrow(1), true, None);
    let v = Viewer::connect(&h, Mode::Office).await;
    v.wait_stream("office stream", |s| s.mode == Mode::Office as i32)
        .await;
    assert!(
        !lock(&h.probe.captures)[0].show_cursor,
        "office mode: the viewer draws the cursor, so the video must not"
    );
    until("first shape shown", WAIT, || shown_hosts(&v).len() == 1).await;
    // 32×48 Retina pixels are 16×24 points on the host; the stream is the display's
    // native 1920×1080 on a 1920×1080 view at 2×, so 16×24 points here too.
    let first = v.cursor_calls();
    assert!(
        first
            .iter()
            .any(|c| matches!(c, Shown::Host(_, w, h) if *w == 16.0 && *h == 24.0)),
        "{first:?}"
    );
    let a = shown_hosts(&v)[0];

    host_cursor(&h, &arrow(2), true, None);
    until("second shape", WAIT, || shown_hosts(&v).len() == 2).await;
    host_cursor(&h, &arrow(1), true, None);
    until("back to the first", WAIT, || shown_hosts(&v).len() == 3).await;
    let ids = shown_hosts(&v);
    assert_eq!(ids[2], a, "the same image keeps its id: {ids:?}");
    assert_ne!(ids[1], a);

    host_cursor(&h, &arrow(1), false, None);
    until("hidden", WAIT, || {
        v.cursor_calls().last() == Some(&Shown::Hidden)
    })
    .await;
    v.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_moves_the_viewer_did_not_make_come_back() {
    let h = host(perms(true, true, false)).await;
    host_cursor(&h, &arrow(1), true, None);
    let v = Viewer::connect(&h, Mode::Office).await;
    v.wait_stream("stream", |_| true).await;
    v.set_active(true);
    until("input thread", WAIT, || lock(&v.probe.key_tx).is_some()).await;
    // The viewer moves to the middle of the view: stream (960, 540), host points (480, 270).
    assert_eq!(
        v.command(Command::Move(PointerMove { x: 960.0, y: 540.0 })),
        abi::NEXA_OK
    );
    until("move injected", WAIT, || {
        lock(&h.probe.injected).contains(&Hosted::Abs(480.0, 270.0))
    })
    .await;
    // The cursor is where the viewer put it: nothing comes back.
    host_cursor(&h, &arrow(1), true, Some((480.0, 270.0)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !v.cursor_calls()
            .iter()
            .any(|c| matches!(c, Shown::Warp(..)))
    );
    // A program on the host warps it to (100, 50) points: the viewer follows.
    host_cursor(&h, &arrow(1), true, Some((100.0, 50.0)));
    until("viewer warped", WAIT, || {
        v.cursor_calls()
            .iter()
            .any(|c| matches!(c, Shown::Warp(..)))
    })
    .await;
    let warp = v
        .cursor_calls()
        .into_iter()
        .find(|c| matches!(c, Shown::Warp(..)))
        .unwrap();
    // Stream pixel (200, 100), its middle, in view points at 2×.
    assert_eq!(warp, Shown::Warp(100.25, 50.25));
    // The next click is where the cursor now is, not where the viewer last moved it.
    v.command(Command::Button(PointerButton {
        button: 0,
        down: true,
    }));
    until("click at the warped position", WAIT, || {
        lock(&h.probe.injected).contains(&Hosted::Button(0, true, Some((100.0, 50.0))))
    })
    .await;
    v.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_input_the_cursor_stays_in_the_video_and_is_not_tracked() {
    let h = host(perms(true, false, false)).await;
    host_cursor(&h, &arrow(1), true, None);
    let v = Viewer::connect(&h, Mode::Office).await;
    v.wait_stream("stream", |_| true).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(lock(&h.probe.captures)[0].show_cursor);
    assert_eq!(h.probe.cursor_sources.load(SeqCst), 0);
    assert!(shown_hosts(&v).is_empty());
    v.close();
}

// ---------------------------------------------------------------------------------
// Game mode (§7.4, DESIGN §8.5)
// ---------------------------------------------------------------------------------

fn relative_total(h: &Host) -> (i32, i32) {
    lock(&h.probe.injected)
        .iter()
        .filter_map(|i| match i {
            Hosted::Rel(x, y) => Some((*x, *y)),
            _ => None,
        })
        .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn game_mode_locks_the_pointer_sends_totals_and_every_exit_releases_it() {
    let h = host(perms(true, true, false)).await;
    host_cursor(&h, &arrow(1), true, None);
    let v = Viewer::connect(&h, Mode::Office).await;
    v.wait_stream("office", |s| s.mode == Mode::Office as i32)
        .await;
    v.set_active(true);
    until("input thread", WAIT, || lock(&v.probe.key_tx).is_some()).await;
    assert_eq!(
        v.probe.locks_taken.load(SeqCst),
        0,
        "no lock outside game mode"
    );

    // The toolbar's mode switch: the host rebuilds with the cursor in the video,
    // and the viewer takes the lock when the host confirms game mode.
    assert_eq!(
        v.command(Command::Mode(SetMode {
            mode: Mode::Game as i32,
            fps: 0
        })),
        abi::NEXA_OK
    );
    v.wait_stream("game", |s| s.mode == Mode::Game as i32).await;
    until("host shows the cursor in the video", WAIT, || {
        lock(&h.probe.captures)
            .last()
            .is_some_and(|c| c.show_cursor)
    })
    .await;
    until("locked", WAIT, || v.probe.locks_held.load(SeqCst) == 1).await;
    until("local cursor hidden", WAIT, || {
        v.cursor_calls().last() == Some(&Shown::Hidden)
    })
    .await;

    // Motion arrives as the lock's running totals; the host gets the differences.
    let lock_tx = lock(&v.probe.lock_tx).clone().unwrap();
    for (x, y) in [(5, 0), (12, -3), (30, -10)] {
        lock_tx.send(InputCmd::Relative(x, y)).unwrap();
    }
    until("relative motion", WAIT, || relative_total(&h) == (30, -10)).await;
    // Absolute moves are ignored while locked (the cursor does not move), and the
    // trailing resends at 20 and 60 ms repeat the same sequence number, so nothing
    // is applied twice.
    let absolute = |h: &Host| {
        lock(&h.probe.injected)
            .iter()
            .filter(|i| matches!(i, Hosted::Abs(..)))
            .count()
    };
    let before = absolute(&h);
    v.command(Command::Move(PointerMove { x: 5.0, y: 5.0 }));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(relative_total(&h), (30, -10));
    assert_eq!(absolute(&h), before);

    // A click carries the totals, so it lands where the motion left the pointer.
    v.command(Command::Button(PointerButton {
        button: 0,
        down: true,
    }));
    until("click in relative mode", WAIT, || {
        lock(&h.probe.injected).contains(&Hosted::Button(0, true, None))
    })
    .await;

    // The host key always releases the lock.
    v.input().send(InputCmd::HostKey).unwrap();
    until("released by the host key", WAIT, || {
        v.probe.locks_held.load(SeqCst) == 0
    })
    .await;
    v.wait_event("host key event", |b| matches!(b, EventBody::HostKey(_)))
        .await;
    // It stays released until capture is switched off and on again (Resume).
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(v.probe.locks_held.load(SeqCst), 0);
    v.set_active(false);
    v.set_active(true);
    until("locked again", WAIT, || {
        v.probe.locks_held.load(SeqCst) == 1
    })
    .await;
    // A new lock counts from zero; the host's totals carry on without a jump.
    let lock_tx = lock(&v.probe.lock_tx).clone().unwrap();
    lock_tx.send(InputCmd::Relative(4, 4)).unwrap();
    until("motion after relock", WAIT, || {
        relative_total(&h) == (34, -6)
    })
    .await;

    // Losing focus releases it.
    v.set_active(false);
    until("released on deactivate", WAIT, || {
        v.probe.locks_held.load(SeqCst) == 0
    })
    .await;
    v.set_active(true);
    until("locked once more", WAIT, || {
        v.probe.locks_held.load(SeqCst) == 1
    })
    .await;

    // Leaving game mode releases it and the host draws no cursor in the video again.
    v.command(Command::Mode(SetMode {
        mode: Mode::Balanced as i32,
        fps: 0,
    }));
    v.wait_stream("balanced", |s| s.mode == Mode::Balanced as i32)
        .await;
    until("released on mode switch", WAIT, || {
        v.probe.locks_held.load(SeqCst) == 0
    })
    .await;
    until("cursor out of the video", WAIT, || {
        lock(&h.probe.captures)
            .last()
            .is_some_and(|c| !c.show_cursor)
    })
    .await;
    until("host cursor shown again", WAIT, || {
        matches!(v.cursor_calls().last(), Some(Shown::Host(..)))
    })
    .await;

    // And closing the session releases it too.
    v.command(Command::Mode(SetMode {
        mode: Mode::Game as i32,
        fps: 0,
    }));
    until("locked for the last time", WAIT, || {
        v.probe.locks_held.load(SeqCst) == 1
    })
    .await;
    v.close();
    until("released on close", WAIT, || {
        v.probe.locks_held.load(SeqCst) == 0
    })
    .await;
    until("default cursor after the session", WAIT, || {
        v.cursor_calls().last() == Some(&Shown::Default)
    })
    .await;
    assert!(v.probe.locks_taken.load(SeqCst) >= 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_mode_switches_are_deferred_not_lost() {
    let h = host(perms(true, true, false)).await;
    let v = Viewer::connect(&h, Mode::Office).await;
    v.wait_stream("office", |s| s.mode == Mode::Office as i32)
        .await;
    for mode in [Mode::Game, Mode::Balanced, Mode::Game] {
        v.command(Command::Mode(SetMode {
            mode: mode as i32,
            fps: 0,
        }));
    }
    // The host's rate limit holds them back, but the last one still arrives.
    v.wait_stream("game after the burst", |s| s.mode == Mode::Game as i32)
        .await;
    v.close();
}

// ---------------------------------------------------------------------------------
// Audio and its permission
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_plays_with_the_permission_and_mute_stops_it_at_the_host() {
    let h = host(Permissions {
        audio: true,
        ..perms(true, false, false)
    })
    .await;
    let v = Viewer::connect(&h, Mode::Balanced).await;
    let connected = v
        .wait_event("connected", |b| matches!(b, EventBody::Connected(_)))
        .await;
    assert!(
        matches!(connected, EventBody::Connected(c) if c.perms.as_ref().is_some_and(|p| p.audio))
    );
    until("capture started", WAIT, || {
        h.probe.audio_running.load(SeqCst)
    })
    .await;
    until("sound reaches the output", WAIT, || {
        v.probe.heard.load(SeqCst) > 20_000
    })
    .await;
    let stats = v
        .wait_event(
            "audio stats",
            |b| matches!(b, EventBody::Stats(s) if s.audio_played > 10),
        )
        .await;
    if let EventBody::Stats(s) = stats {
        assert!(
            s.audio_buffer_us < 200_000,
            "{} µs in the buffer",
            s.audio_buffer_us
        );
    }

    // Mute: the output stops here and capture stops at the host.
    assert_eq!(
        v.command(Command::Audio(SetAudio { enabled: false })),
        abi::NEXA_OK
    );
    until("output stopped", WAIT, || {
        v.probe.outputs_running.load(SeqCst) == 0
    })
    .await;
    until("capture stopped", WAIT, || {
        !h.probe.audio_running.load(SeqCst)
    })
    .await;
    let frames = h.probe.audio_frames.load(SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        h.probe.audio_frames.load(SeqCst),
        frames,
        "nothing encoded while muted"
    );

    // Unmute starts both again.
    v.command(Command::Audio(SetAudio { enabled: true }));
    until("capture again", WAIT, || h.probe.audio_running.load(SeqCst)).await;
    assert_eq!(v.probe.outputs_started.load(SeqCst), 2);
    assert_eq!(h.probe.audio_started.load(SeqCst), 2);

    v.close();
    until("capture stops with the session", WAIT, || {
        !h.probe.audio_running.load(SeqCst)
    })
    .await;
    until("output stops with the session", WAIT, || {
        v.probe.outputs_running.load(SeqCst) == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_permission_there_is_no_audio_at_all() {
    let h = host(perms(true, true, true)).await;
    let v = Viewer::connect(&h, Mode::Balanced).await;
    v.wait_stream("stream", |_| true).await;
    // The viewer cannot switch on what it was not given (a viewer that sends
    // AudioControl anyway is covered in nexa-agent's tests/features.rs).
    assert_ne!(
        v.command(Command::Audio(SetAudio { enabled: true })),
        abi::NEXA_OK
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.probe.audio_started.load(SeqCst), 0);
    assert_eq!(v.probe.outputs_started.load(SeqCst), 0);
    v.close();
}

// ---------------------------------------------------------------------------------
// Displays (§6.7)
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn displays_are_listed_and_switching_rebuilds_capture_for_the_new_one() {
    let h = host(perms(true, true, false)).await;
    let v = Viewer::connect(&h, Mode::Balanced).await;
    let connected = v
        .wait_event("connected", |b| matches!(b, EventBody::Connected(_)))
        .await;
    let EventBody::Connected(c) = connected else {
        unreachable!()
    };
    assert_eq!(c.host_name, "feature host");
    let listed: Vec<(u32, String, u32, u32)> = c
        .displays
        .iter()
        .map(|d| (d.id, d.name.clone(), d.w, d.h))
        .collect();
    assert_eq!(
        listed,
        vec![
            (0, "screen 11".to_owned(), 1920, 1080),
            (1, "screen 22".to_owned(), 1280, 720),
        ]
    );
    v.wait_stream("first display", |s| s.display == 0 && s.w == 1920)
        .await;
    assert_eq!(lock(&h.probe.captures)[0].display, 11);

    assert_eq!(
        v.command(Command::Display(SelectDisplay { display: 1 })),
        abi::NEXA_OK
    );
    v.wait_stream("second display", |s| {
        s.display == 1 && (s.w, s.h) == (1280, 720)
    })
    .await;
    assert_eq!(lock(&h.probe.captures).last().unwrap().display, 22);

    // Input now lands on the second display: its origin is 960 points to the right.
    v.set_active(true);
    until("input thread", WAIT, || lock(&v.probe.key_tx).is_some()).await;
    v.command(Command::Move(PointerMove { x: 960.0, y: 540.0 }));
    until("move on display 2", WAIT, || {
        lock(&h.probe.injected).contains(&Hosted::Abs(960.0 + 320.0, 180.0))
    })
    .await;

    // A display that does not exist leaves the stream where it was.
    let builds = lock(&h.probe.captures).len();
    v.command(Command::Display(SelectDisplay { display: 9 }));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        h.agent.session_active(),
        "a bad request does not end the session"
    );
    assert!(lock(&h.probe.captures).len() > builds, "it was tried");
    assert_eq!(lock(&h.probe.captures).last().unwrap().display, 22);
    v.wait_stream("still the second display", |s| s.display == 1)
        .await;

    // Mode switches keep the display.
    v.command(Command::Mode(SetMode {
        mode: Mode::Office as i32,
        fps: 0,
    }));
    v.wait_stream("office on display 2", |s| {
        s.mode == Mode::Office as i32 && s.display == 1
    })
    .await;
    v.close();
}

// ---------------------------------------------------------------------------------
// Files (§3.1 stream 4)
// ---------------------------------------------------------------------------------

fn transfer_done(b: &EventBody, outgoing: bool) -> bool {
    matches!(b, EventBody::Transfer(t)
        if t.outgoing == outgoing && t.state == nexa_client::proto::TransferState::Done as i32)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_go_both_ways_through_the_abi_with_the_permission() {
    let h = host(Permissions {
        file: true,
        ..perms(true, true, false)
    })
    .await;
    let v = Viewer::connect(&h, Mode::Balanced).await;
    v.wait_stream("stream", |_| true).await;
    let src = v.dir.path().join("notes.md");
    std::fs::write(&src, b"# hi").unwrap();
    assert_eq!(
        v.command(Command::SendFiles(nexa_client::proto::SendFiles {
            paths: vec![src.to_string_lossy().into_owned()],
        })),
        abi::NEXA_OK
    );
    v.wait_event("sent", |b| transfer_done(b, true)).await;
    until("at the host", WAIT, || {
        h.downloads.join("notes.md").exists()
    })
    .await;
    assert_eq!(
        std::fs::read(h.downloads.join("notes.md")).unwrap(),
        b"# hi"
    );

    let back = v.dir.path().join("reply.txt");
    std::fs::write(&back, b"ok").unwrap();
    h.agent.send_files(vec![back]).unwrap();
    let got = v.wait_event("received", |b| transfer_done(b, false)).await;
    let EventBody::Transfer(t) = got else {
        unreachable!()
    };
    // Tests keep received files inside the viewer's data directory.
    let expected = v.dir.path().join("Downloads/reply.txt");
    assert_eq!(t.path, expected.to_string_lossy());
    assert_eq!(std::fs::read(expected).unwrap(), b"ok");
    v.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_file_permission_the_abi_refuses_to_send() {
    let h = host(perms(true, true, true)).await;
    let v = Viewer::connect(&h, Mode::Balanced).await;
    v.wait_stream("stream", |_| true).await;
    let src = v.dir.path().join("a.txt");
    std::fs::write(&src, b"a").unwrap();
    assert_ne!(
        v.command(Command::SendFiles(nexa_client::proto::SendFiles {
            paths: vec![src.to_string_lossy().into_owned()],
        })),
        abi::NEXA_OK
    );
    assert!(h.agent.send_files(vec![src]).is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!h.downloads.exists());
    assert!(!v.dir.path().join("Downloads").exists());
    v.close();
}
