//! One view-only run of the real pipeline: ScreenCaptureKit → VideoToolbox → QUIC on
//! 127.0.0.1 → VideoToolbox → an offscreen Metal layer. Input and clipboard are fakes,
//! frames go nowhere but the GPU, and no window opens.
//!
//! `cargo test -p nexa-client --test viewonly -- --ignored --nocapture`

#![cfg(target_os = "macos")]

#[path = "../../nexa-agent/tests/common/mod.rs"]
mod common;

use std::{
    ffi::c_void,
    sync::{Arc, Mutex, mpsc::Sender},
    time::{Duration, Instant},
};

use common::{FakeUi, lock, perms};
use nexa_agent::{AgentConfig, AgentUi, Platform, ScreenInfo, start};
use nexa_auth::{DeviceIdentity, FileKeyStore, KeyStore, PairingStore};
use nexa_client::{
    abi,
    deps::{Deps, KeySource, KeySourceError},
    input::InputCmd,
    proto::{ConnectAuth, ConnectRequest, EventBody},
};
use nexa_media::{
    CaptureConfig, Capturer, Decoder, Encoder, EncoderConfig, PresentMode, Presenter,
    macos::{
        DecoderOutput, MetalPresenter, ScreenCapturer, VtDecoder, VtEncoder,
        has_screen_capture_permission, probe_caps,
    },
};
use nexa_session::{
    clip::{ClipPlatform, PromiseResolver},
    handshake::Timeouts,
    input::InputSink,
};
use nexa_wire::control::{Caps, Codec, Mode, Os};
use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID};
use prost::Message;

/// How long frames flow. The whole capture stays under 10 s.
const RUN: Duration = Duration::from_secs(5);

struct RealScreen;

impl Platform for RealScreen {
    fn screen(&self) -> Result<ScreenInfo, String> {
        let id = CGMainDisplayID();
        let mode = CGDisplayCopyDisplayMode(id).ok_or("no display mode")?;
        let (w, h) = (
            CGDisplayMode::pixel_width(Some(&mode)) as u32,
            CGDisplayMode::pixel_height(Some(&mode)) as u32,
        );
        Ok(ScreenInfo {
            display_id: id,
            origin_points: (0.0, 0.0),
            size_points: (f64::from(w) / 2.0, f64::from(h) / 2.0),
            size_pixels: (w, h),
            refresh_hz: 60,
            name: "main".into(),
        })
    }
    fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        Ok(Box::new(ScreenCapturer::new(config)?))
    }
    fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        Ok(Box::new(VtEncoder::new(config)?))
    }
    fn input_available(&self) -> bool {
        false
    }
    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        panic!("a view-only run must never create an injector");
    }
    fn clipboard(&self, _: PromiseResolver) -> Result<Box<dyn ClipPlatform>, String> {
        panic!("a view-only run must never open the pasteboard");
    }
}

struct NoKeys;

impl KeySource for NoKeys {
    fn set_active(&mut self, _: bool) -> Result<(), KeySourceError> {
        panic!("a view-only run must never capture keys");
    }
}

struct ViewerSide {
    dir: std::path::PathBuf,
    caps: Caps,
}

impl Deps for ViewerSide {
    fn os(&self) -> Os {
        Os::Macos
    }
    fn key_store(&self) -> Box<dyn KeyStore> {
        Box::new(FileKeyStore::new(self.dir.join("viewer.key")))
    }
    fn caps(&self) -> Caps {
        self.caps.clone()
    }
    fn decoder(&self, codec: Codec) -> Result<Box<dyn Decoder>, String> {
        VtDecoder::new(codec, DecoderOutput::Display)
            .map(|d| Box::new(d) as Box<dyn Decoder>)
            .map_err(|e| e.to_string())
    }
    fn key_source(&self, _: Sender<InputCmd>) -> Box<dyn KeySource> {
        Box::new(NoKeys)
    }
    fn fallback_key_source(&self, _: Sender<InputCmd>) -> Option<Box<dyn KeySource>> {
        None
    }
    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        panic!("a view-only run must never open the pasteboard");
    }
    unsafe fn attach_view(
        &self,
        _: *mut c_void,
        _: u32,
        w: u32,
        h: u32,
        _: f32,
        _: PresentMode,
    ) -> Result<Box<dyn Presenter>, String> {
        MetalPresenter::offscreen(w, h)
            .map(|p| Box::new(p) as Box<dyn Presenter>)
            .map_err(|e| e.to_string())
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<nexa_client::proto::Event>>);

unsafe extern "C" fn on_event(user: *mut c_void, pb: *const u8, len: usize) {
    // SAFETY: as in e2e.rs.
    let (events, bytes) = unsafe {
        (
            &*(user as *const Events),
            std::slice::from_raw_parts(pb, len),
        )
    };
    lock(&events.0).push(nexa_client::proto::Event::decode(bytes).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real screen capture: run explicitly with --ignored"]
async fn view_only_real_pipeline_on_loopback() {
    if !has_screen_capture_permission() {
        println!("skipped: this process does not have Screen Recording permission");
        return;
    }
    let started = Instant::now();
    let probed = probe_caps();
    println!("probe_caps took {:?}: {:?}", started.elapsed(), probed);

    let dir = tempfile::tempdir().unwrap();
    let identity = DeviceIdentity::generate();
    let store =
        PairingStore::open(dir.path().join("pairings.json"), identity.store_mac_key()).unwrap();
    let ui = FakeUi::allowing(perms(true, false, false), false);
    let agent = start(
        AgentConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            device_name: "view-only test".into(),
            os: Os::Macos,
            caps: probed.clone(),
            timeouts: Timeouts::default(),
        },
        identity,
        store,
        Arc::new(RealScreen),
        ui as Arc<dyn AgentUi>,
    )
    .await
    .unwrap();
    assert!(agent.local_addr().ip().is_loopback());

    let events = Arc::new(Events::default());
    let cfg = nexa_client::proto::ClientConfig {
        device_name: "view-only viewer".into(),
        data_dir: dir.path().join("viewer").to_string_lossy().into_owned(),
    }
    .encode_to_vec();
    let mut client = std::ptr::null_mut();
    // SAFETY: valid buffers; `events` outlives the client.
    let status = unsafe {
        abi::init_with_deps(
            Arc::new(ViewerSide {
                dir: dir.path().to_path_buf(),
                caps: probed,
            }),
            cfg.as_ptr(),
            cfg.len(),
            Some(on_event),
            Arc::as_ptr(&events) as *mut c_void,
            &mut client,
        )
    };
    assert_eq!(status, abi::NEXA_OK);

    let request = ConnectRequest {
        address: agent.local_addr().to_string(),
        tag: 1,
        mode: Mode::Balanced as i32,
        fps: 60,
        auth: Some(ConnectAuth::Code(agent.current_code().to_string())),
    }
    .encode_to_vec();
    let mut session = std::ptr::null_mut();
    // SAFETY: valid buffers and handle.
    let status =
        unsafe { abi::nexa_connect(client, request.as_ptr(), request.len(), &mut session) };
    assert_eq!(status, abi::NEXA_OK);

    let capture_began = Instant::now();
    // SAFETY: the test deps ignore the handle.
    let status = unsafe {
        abi::nexa_session_attach_view(
            session,
            std::ptr::dangling_mut::<c_void>(),
            abi::NEXA_VIEW_NSVIEW,
            1280,
            720,
            1.0,
        )
    };
    assert_eq!(status, abi::NEXA_OK);

    tokio::time::sleep(RUN).await;

    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
    common::until("host stopped streaming", Duration::from_secs(3), || {
        !agent.session_active()
    })
    .await;
    let elapsed = capture_began.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "capture ran for {elapsed:?}"
    );
    agent.shutdown();

    let log = lock(&events.0);
    for event in log.iter() {
        match &event.body {
            Some(EventBody::Stream(s)) => println!(
                "stream: codec={} {}x{} @ {} fps mode={}",
                s.codec, s.w, s.h, s.fps, s.mode
            ),
            Some(EventBody::Disconnected(d)) => println!("ended: {:?} {}", d.reason, d.message),
            _ => {}
        }
    }
    let last = log.iter().rev().find_map(|e| match &e.body {
        Some(EventBody::Stats(s)) => Some(s.clone()),
        _ => None,
    });
    let stats = last.expect("no stats event");
    let frames = stats.presented as f64 / RUN.as_secs_f64();
    println!(
        "stats after {:?}: complete={} lost={} fec_recovered={} decoded={} decode_errors={} presented={} ({frames:.0} fps) rtt={}us",
        elapsed,
        stats.complete,
        stats.lost,
        stats.fec_recovered,
        stats.decoded,
        stats.decode_errors,
        stats.presented,
        stats.rtt_us
    );
    assert!(stats.decoded > 0, "nothing decoded");
    assert_eq!(stats.decode_errors, 0, "decoder errors");
    drop(log);
    // SAFETY: the handle came from init.
    unsafe { abi::nexa_shutdown(client) };
}
