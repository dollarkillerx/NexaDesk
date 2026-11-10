//! Fakes for both roles: nothing here touches the screen, the Keychain, the pasteboard
//! or the input system. Also included by nexa-client's tests.

#![allow(dead_code)]

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU32, Ordering::SeqCst},
    },
    thread,
    time::{Duration, Instant},
};

use nexa_agent::{Agent, AgentConfig, AgentEvent, AgentUi, Platform, ScreenInfo, start};
use nexa_auth::{DeviceIdentity, PairingStore};
use nexa_clip::{ClipKind, ClipboardBackend, FakeBackend, FormatInfo, Gen, LocalSnapshot};
use nexa_media::{
    CaptureConfig, CaptureEvent, CapturedFrame, Capturer, CpuFrame, DecodedFrame, Decoder,
    DisplayMode, EncodeOpts, EncodedMeta, Encoder, EncoderCaps, EncoderConfig, FrameId, FrameMeta,
    NativeImage, PresentMode, PresentStat, Presenter, Recovery,
};
use nexa_session::{
    clip::{ClipPlatform, PromiseResolver},
    handshake::{Consent, ConsentAnswer, ConsentRequest, Timeouts},
    input::InputSink,
};
use nexa_wire::control::{Caps, Chroma, Codec, CodecCap, Os, Permissions};

pub fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn perms(view: bool, input: bool, clipboard: bool) -> Permissions {
    Permissions {
        view,
        input,
        clipboard,
        file: false,
        audio: false,
    }
}

/// HEVC and H.264 at 4K, with both encode and decode.
pub fn caps() -> Caps {
    let list = |codec: Codec| CodecCap {
        codec: codec as i32,
        chroma: Chroma::Yuv420 as i32,
        max_w: 3840,
        max_h: 2160,
    };
    Caps {
        encode: vec![list(Codec::Hevc), list(Codec::H264)],
        decode: vec![list(Codec::Hevc), list(Codec::H264)],
        rfi: false,
        ltr: true,
        max_fps: 120,
    }
}

pub fn image() -> NativeImage {
    NativeImage::Cpu(Arc::new(CpuFrame {
        width: 1,
        height: 1,
        y: vec![0],
        uv: vec![0],
    }))
}

/// One frame every 5 ms, forever, plus a display change when the test asks for one.
struct FakeCapturer(Arc<FakePlatformState>);

impl Capturer for FakeCapturer {
    fn next(&mut self, _: Duration) -> nexa_media::Result<CaptureEvent> {
        thread::sleep(Duration::from_millis(5));
        if self.0.fire_mode_change.swap(false, SeqCst) {
            let (width, height) = *lock(&self.0.pixels);
            return Ok(CaptureEvent::ModeChanged(DisplayMode {
                width,
                height,
                refresh_hz: 60,
            }));
        }
        Ok(CaptureEvent::Frame(CapturedFrame {
            image: image(),
            captured_at: Instant::now(),
            dirty: vec![],
        }))
    }
}

struct FakeEncoder;

impl Encoder for FakeEncoder {
    fn caps(&self) -> EncoderCaps {
        EncoderCaps::default()
    }
    fn encode(
        &mut self,
        _: &CapturedFrame,
        id: FrameId,
        o: EncodeOpts,
        out: &mut Vec<u8>,
    ) -> nexa_media::Result<EncodedMeta> {
        out.extend_from_slice(&[9; 32]);
        Ok(EncodedMeta {
            frame_id: id,
            key: o.force_key || id == 0,
            recovery_point: false,
        })
    }
    fn set_bitrate(&mut self, _: u32) -> nexa_media::Result<()> {
        Ok(())
    }
    fn invalidate_from(&mut self, _: FrameId) -> nexa_media::Result<Recovery> {
        Ok(Recovery::Idr)
    }
    fn on_frame_acked(&mut self, _: FrameId) {}
    fn force_idr(&mut self) {}
}

#[derive(Clone, Debug, PartialEq)]
pub enum Injected {
    Key {
        hid: u32,
        down: bool,
        at: Instant,
    },
    Move(f64, f64),
    Button {
        button: u32,
        down: bool,
        at: Option<(f64, f64)>,
    },
    Wheel(i32, i32),
    Text(String),
    Sync(Vec<u32>),
    ReleaseAll,
}

#[derive(Clone, Default)]
pub struct InjectLog(pub Arc<Mutex<Vec<Injected>>>);

impl InjectLog {
    pub fn all(&self) -> Vec<Injected> {
        lock(&self.0).clone()
    }
    pub fn keys(&self) -> Vec<(u32, bool)> {
        lock(&self.0)
            .iter()
            .filter_map(|i| match i {
                Injected::Key { hid, down, .. } => Some((*hid, *down)),
                _ => None,
            })
            .collect()
    }
}

struct FakeSink(InjectLog);

impl InputSink for FakeSink {
    fn key(&mut self, hid: u32, down: bool, _: bool, at: Instant) {
        lock(&self.0.0).push(Injected::Key { hid, down, at });
    }
    fn move_absolute(&mut self, x: f64, y: f64) {
        lock(&self.0.0).push(Injected::Move(x, y));
    }
    fn move_relative(&mut self, _: i32, _: i32) {}
    fn button(&mut self, button: u32, down: bool, at: Option<(f64, f64)>, _: Instant) {
        lock(&self.0.0).push(Injected::Button { button, down, at });
    }
    fn wheel(&mut self, dx: i32, dy: i32) {
        lock(&self.0.0).push(Injected::Wheel(dx, dy));
    }
    fn text(&mut self, text: &str) {
        lock(&self.0.0).push(Injected::Text(text.to_owned()));
    }
    fn apply_key_state(&mut self, pressed: &[u32], _: Instant) {
        lock(&self.0.0).push(Injected::Sync(pressed.to_vec()));
    }
    fn poll_timeout(&mut self, _: Instant) {}
    fn release_all(&mut self, _: Instant) {
        lock(&self.0.0).push(Injected::ReleaseAll);
    }
}

/// A [`FakeBackend`] that can be slow, and records when a write completed.
#[derive(Clone, Default)]
pub struct FakeClip {
    pub backend: Arc<Mutex<FakeBackend>>,
    last_seen: Arc<Mutex<u64>>,
    pub write_delay: Arc<Mutex<Duration>>,
    pub written_at: Arc<Mutex<Option<Instant>>>,
    /// Formats that were still promises when written.
    pub promised_on_write: Arc<Mutex<Vec<ClipKind>>>,
    pub resolver: Arc<Mutex<Option<PromiseResolver>>>,
}

impl FakeClip {
    pub fn user_copy(&self, items: &[(ClipKind, &[u8])]) {
        lock(&self.backend).user_copy(items);
    }
    pub fn content(&self, kind: ClipKind) -> Option<Vec<u8>> {
        lock(&self.backend).content(kind).map(|b| b.to_vec())
    }
}

impl ClipboardBackend for FakeClip {
    fn read_formats(&mut self) -> nexa_clip::Result<Vec<FormatInfo>> {
        lock(&self.backend).read_formats()
    }
    fn read(&mut self, kind: ClipKind) -> nexa_clip::Result<Vec<u8>> {
        lock(&self.backend).read(kind)
    }
    fn write_promise(
        &mut self,
        generation: Gen,
        formats: &[FormatInfo],
        inline: &[(ClipKind, bytes::Bytes)],
    ) -> nexa_clip::Result<()> {
        let delay = *lock(&self.write_delay);
        thread::sleep(delay);
        let mut backend = lock(&self.backend);
        let result = backend.write_promise(generation, formats, inline);
        *lock(&self.promised_on_write) = backend.promised_kinds();
        *lock(&self.written_at) = Some(Instant::now());
        result
    }
}

impl ClipPlatform for FakeClip {
    fn poll(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
        let backend = lock(&self.backend);
        let mut seen = lock(&self.last_seen);
        if backend.change_count() == *seen {
            return Ok(None);
        }
        *seen = backend.change_count();
        Ok(Some(backend.snapshot()))
    }
}

pub struct FakePlatformState {
    pub inject_sinks_created: AtomicU32,
    pub clipboards_created: AtomicU32,
    pub encoders_created: AtomicU32,
    pub input_unavailable: AtomicBool,
    pub fire_mode_change: AtomicBool,
    pub pixels: Mutex<(u32, u32)>,
}

impl Default for FakePlatformState {
    fn default() -> Self {
        Self {
            inject_sinks_created: AtomicU32::new(0),
            clipboards_created: AtomicU32::new(0),
            encoders_created: AtomicU32::new(0),
            input_unavailable: AtomicBool::new(false),
            fire_mode_change: AtomicBool::new(false),
            pixels: Mutex::new((1920, 1080)),
        }
    }
}

#[derive(Clone, Default)]
pub struct FakePlatform {
    pub state: Arc<FakePlatformState>,
    pub injected: InjectLog,
    pub clip: FakeClip,
}

impl Platform for FakePlatform {
    fn screen(&self) -> Result<ScreenInfo, String> {
        Ok(ScreenInfo {
            display_id: 1,
            origin_points: (0.0, 0.0),
            size_points: (960.0, 540.0),
            size_pixels: *lock(&self.state.pixels),
            refresh_hz: 60,
            name: "fake".into(),
        })
    }
    fn capturer(&self, _: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        Ok(Box::new(FakeCapturer(self.state.clone())))
    }
    fn encoder(&self, _: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        self.state.encoders_created.fetch_add(1, SeqCst);
        Ok(Box::new(FakeEncoder))
    }
    fn input_available(&self) -> bool {
        !self.state.input_unavailable.load(SeqCst)
    }
    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        self.state.inject_sinks_created.fetch_add(1, SeqCst);
        Ok(Box::new(FakeSink(self.injected.clone())))
    }
    fn clipboard(&self, resolver: PromiseResolver) -> Result<Box<dyn ClipPlatform>, String> {
        self.state.clipboards_created.fetch_add(1, SeqCst);
        *lock(&self.clip.resolver) = Some(resolver);
        Ok(Box::new(self.clip.clone()))
    }
}

/// Answers consent prompts as instructed and records everything.
pub struct FakeUi {
    pub answer: Mutex<ConsentAnswer>,
    pub asked: Mutex<Vec<ConsentRequest>>,
    pub events: Mutex<Vec<AgentEvent>>,
    pub present: AtomicBool,
}

impl FakeUi {
    pub fn allowing(p: Permissions, save: bool) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(ConsentAnswer {
                allow: true,
                perms: p,
                save,
                skip_next_time: false,
            }),
            asked: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            present: AtomicBool::new(true),
        })
    }

    pub fn denying() -> Arc<Self> {
        let ui = Self::allowing(perms(false, false, false), false);
        *lock(&ui.answer) = ConsentAnswer::deny();
        ui
    }

    pub fn prompts(&self) -> usize {
        lock(&self.asked).len()
    }

    pub fn saw(&self, f: impl Fn(&AgentEvent) -> bool) -> bool {
        lock(&self.events).iter().any(f)
    }
}

impl Consent for FakeUi {
    fn can_present(&self) -> bool {
        self.present.load(SeqCst)
    }
    fn ask(&self, request: &ConsentRequest, _: Duration) -> ConsentAnswer {
        lock(&self.asked).push(request.clone());
        lock(&self.answer).clone()
    }
}

impl AgentUi for FakeUi {
    fn event(&self, event: AgentEvent) {
        lock(&self.events).push(event);
    }
}

/// An agent on loopback with fakes for every device.
pub struct Host {
    pub agent: Agent,
    pub platform: FakePlatform,
    pub ui: Arc<FakeUi>,
    pub addr: SocketAddr,
    /// Where files sent to this host land: inside the temporary directory.
    pub downloads: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

pub async fn host(ui: Arc<FakeUi>, timeouts: Timeouts) -> Host {
    host_with(ui, timeouts, None).await
}

/// An agent that is also reachable by ID through `signal`.
pub async fn host_with(
    ui: Arc<FakeUi>,
    timeouts: Timeouts,
    signal: Option<nexa_agent::SignalSetup>,
) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let identity = DeviceIdentity::generate();
    let store =
        PairingStore::open(dir.path().join("pairings.json"), identity.store_mac_key()).unwrap();
    let platform = FakePlatform::default();
    let agent = start(
        AgentConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            device_name: "fake host".into(),
            os: Os::Macos,
            caps: caps(),
            timeouts,
            signal,
            files: nexa_agent::FileSettings {
                downloads: dir.path().join("Downloads/NexaDesk"),
                limits: Default::default(),
            },
        },
        identity,
        store,
        Arc::new(platform.clone()),
        ui.clone() as Arc<dyn AgentUi>,
    )
    .await
    .unwrap();
    let addr = agent.local_addr();
    Host {
        agent,
        platform,
        ui,
        addr,
        downloads: dir.path().join("Downloads/NexaDesk"),
        _dir: dir,
    }
}

pub async fn until(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub struct FakeDecoder(pub Arc<AtomicU32>);

impl Decoder for FakeDecoder {
    fn decode(&mut self, _: &[u8], meta: FrameMeta) -> nexa_media::Result<DecodedFrame> {
        self.0.fetch_add(1, SeqCst);
        Ok(DecodedFrame {
            image: image(),
            meta,
        })
    }
}

pub struct NullPresenter;

impl Presenter for NullPresenter {
    fn resize(&mut self, _: u32, _: u32, _: f32) -> nexa_media::Result<()> {
        Ok(())
    }
    fn set_mode(&mut self, _: PresentMode) -> nexa_media::Result<()> {
        Ok(())
    }
    fn present(&mut self, _: &DecodedFrame) -> nexa_media::Result<PresentStat> {
        Ok(PresentStat::default())
    }
}
