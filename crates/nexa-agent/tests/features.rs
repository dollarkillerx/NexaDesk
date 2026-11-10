//! The agent's in-session features on the wire, driven by a hand-rolled viewer:
//! audio and its permission, the shared transport sequence, and cursor messages.
//! All devices are fakes: no capture, no sound, no input.

mod common;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering::SeqCst},
    },
    thread,
    time::{Duration, Instant},
};

use common::{FakePlatform, FakeUi, caps, lock, perms};
use nexa_agent::{Agent, AgentConfig, AgentUi, Platform, ScreenInfo, start};
use nexa_auth::{DeviceIdentity, PairingStore, PeerPolicy, client_config, tls::SERVER_NAME};
use nexa_media::{
    AudioCapture, AudioChunk, AudioEncoder, CaptureConfig, Capturer, CursorImage, CursorSample,
    CursorSource, Encoder, EncoderConfig,
};
use nexa_net::vsock::bind_media_socket;
use nexa_session::{
    clip::{ClipPlatform, PromiseResolver},
    handshake::{Timeouts, ViewerAccepted, ViewerAuth, ViewerConfig, viewer_handshake},
    input::InputSink,
    proto::{AudioControl, ControlBody, StreamRequest},
};
use nexa_wire::{
    control::{Mode, Os, Permissions},
    datagram::{self, AudioHeader},
};
use quinn::{Connection, Endpoint, EndpointConfig, TokioRuntime};
use zeroize::Zeroizing;

const WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Probe {
    audio_started: AtomicU32,
    audio_running: AtomicBool,
    cursor: Mutex<CursorSample>,
    captures: Mutex<Vec<CaptureConfig>>,
}

struct Rig {
    base: FakePlatform,
    probe: Arc<Probe>,
}

struct Tone(Arc<Probe>);

impl AudioCapture for Tone {
    fn next(&mut self, _: Duration) -> nexa_media::Result<Option<AudioChunk>> {
        thread::sleep(Duration::from_millis(10));
        Ok(Some(AudioChunk {
            samples: vec![0.5; nexa_media::AUDIO_FRAME_LEN],
            captured_at: Instant::now(),
        }))
    }
}

impl Drop for Tone {
    fn drop(&mut self) {
        self.0.audio_running.store(false, SeqCst);
    }
}

struct Bytes3;

impl AudioEncoder for Bytes3 {
    fn encode(&mut self, _: &[f32], out: &mut Vec<u8>) -> nexa_media::Result<()> {
        out.extend_from_slice(&[1, 2, 3]);
        Ok(())
    }
}

struct Scripted(Arc<Probe>);

impl CursorSource for Scripted {
    fn sample(&mut self) -> CursorSample {
        lock(&self.0.cursor).clone()
    }
}

impl Platform for Rig {
    fn screen(&self) -> Result<ScreenInfo, String> {
        self.base.screen()
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
        self.base.input_sink()
    }
    fn clipboard(&self, r: PromiseResolver) -> Result<Box<dyn ClipPlatform>, String> {
        self.base.clipboard(r)
    }
    fn cursor_source(&self) -> Option<Box<dyn CursorSource>> {
        Some(Box::new(Scripted(self.probe.clone())))
    }
    fn audio_capture(&self, _: u32) -> Result<Box<dyn AudioCapture>, String> {
        self.probe.audio_started.fetch_add(1, SeqCst);
        self.probe.audio_running.store(true, SeqCst);
        Ok(Box::new(Tone(self.probe.clone())))
    }
    fn audio_encoder(&self) -> Result<Box<dyn AudioEncoder>, String> {
        Ok(Box::new(Bytes3))
    }
}

struct Host {
    agent: Agent,
    probe: Arc<Probe>,
    _dir: tempfile::TempDir,
}

async fn host(p: Permissions) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let identity = DeviceIdentity::generate();
    let store =
        PairingStore::open(dir.path().join("pairings.json"), identity.store_mac_key()).unwrap();
    let probe = Arc::new(Probe::default());
    let agent = start(
        AgentConfig {
            lan_discovery: false,
            web: None,
            listen: "127.0.0.1:0".parse().unwrap(),
            device_name: "rig".into(),
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
        Arc::new(Rig {
            base: FakePlatform::default(),
            probe: probe.clone(),
        }),
        FakeUi::allowing(p, false) as Arc<dyn AgentUi>,
    )
    .await
    .unwrap();
    Host {
        agent,
        probe,
        _dir: dir,
    }
}

async fn viewer(h: &Host, mode: Mode) -> (Endpoint, Connection, ViewerAccepted) {
    let me = DeviceIdentity::generate();
    let socket = bind_media_socket("127.0.0.1:0".parse().unwrap()).unwrap();
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        None,
        socket,
        Arc::new(TokioRuntime),
    )
    .unwrap();
    let config = client_config(&me, PeerPolicy::AcceptUnknown).unwrap();
    let conn = endpoint
        .connect_with(config, h.agent.local_addr(), SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    let mut accepted = viewer_handshake(
        &conn,
        ViewerConfig {
            me: me.public_key(),
            device_name: "raw viewer".into(),
            os: Os::Macos,
            caps: caps(),
            auth: ViewerAuth::Code(Zeroizing::new(h.agent.current_code().to_string())),
            timeouts: Timeouts::default(),
        },
    )
    .await
    .unwrap();
    accepted
        .control
        .send(ControlBody::Request(StreamRequest {
            mode: mode as i32,
            fps: 0,
            display: 0,
        }))
        .await
        .unwrap();
    (endpoint, conn, accepted)
}

/// Datagrams for `d`: (type, tx_seq) in arrival order, plus parsed audio headers.
async fn collect(conn: &Connection, d: Duration) -> (Vec<(u8, u32)>, Vec<AudioHeader>) {
    let end = Instant::now() + d;
    let (mut all, mut audio) = (Vec::new(), Vec::new());
    while let Ok(Ok(dg)) = tokio::time::timeout(
        end.saturating_duration_since(Instant::now()),
        conn.read_datagram(),
    )
    .await
    {
        if dg.len() >= 8 && matches!(dg[0], datagram::TYPE_VIDEO | datagram::TYPE_AUDIO) {
            all.push((dg[0], u32::from_le_bytes([dg[4], dg[5], dg[6], dg[7]])));
        }
        if dg[0] == datagram::TYPE_AUDIO {
            audio.push(AudioHeader::parse(&dg).unwrap().0);
        }
    }
    (all, audio)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_without_the_audio_permission_gets_none_even_if_it_asks() {
    let h = host(perms(true, true, true)).await;
    let (_ep, conn, mut accepted) = viewer(&h, Mode::Balanced).await;
    assert!(!accepted.accept.perms.as_ref().unwrap().audio);
    accepted
        .control
        .send(ControlBody::Audio(AudioControl { enabled: true }))
        .await
        .unwrap();
    let (all, audio) = collect(&conn, Duration::from_millis(600)).await;
    assert!(!all.is_empty(), "video flows");
    assert!(audio.is_empty());
    assert_eq!(
        h.probe.audio_started.load(SeqCst),
        0,
        "capture never started"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_starts_only_when_asked_and_shares_the_transport_sequence() {
    let h = host(Permissions {
        audio: true,
        ..perms(true, false, false)
    })
    .await;
    let (_ep, conn, mut accepted) = viewer(&h, Mode::Balanced).await;
    assert!(accepted.accept.perms.as_ref().unwrap().audio);
    // Granted is not on: nothing is captured until the viewer unmutes.
    let (_, audio) = collect(&conn, Duration::from_millis(300)).await;
    assert!(audio.is_empty());
    assert_eq!(h.probe.audio_started.load(SeqCst), 0);

    accepted
        .control
        .send(ControlBody::Audio(AudioControl { enabled: true }))
        .await
        .unwrap();
    let (all, audio) = collect(&conn, Duration::from_millis(800)).await;
    assert!(audio.len() > 20, "{} audio packets", audio.len());
    assert_eq!(
        audio[0].flags & datagram::audio_flags::RESTART,
        datagram::audio_flags::RESTART
    );
    assert!(
        audio.windows(2).all(|w| w[1].seq == w[0].seq + 1),
        "audio numbered without gaps"
    );
    // Video and audio draw from one tx_seq, in send order, so feedback sees no holes.
    let seqs: Vec<u32> = all.iter().map(|(_, s)| *s).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "one sequence: {:?}",
        &seqs[..20.min(seqs.len())]
    );
    assert!(all.iter().any(|(t, _)| *t == datagram::TYPE_VIDEO));

    accepted
        .control
        .send(ControlBody::Audio(AudioControl { enabled: false }))
        .await
        .unwrap();
    common::until("capture stopped", WAIT, || {
        !h.probe.audio_running.load(SeqCst)
    })
    .await;
    let (_, audio) = collect(&conn, Duration::from_millis(300)).await;
    assert!(
        audio.len() <= 2,
        "at most what was in flight: {}",
        audio.len()
    );
}

fn image(tint: u8) -> Arc<CursorImage> {
    let mut rgba = vec![0u8; 16 * 16 * 4];
    rgba[..4].copy_from_slice(&[tint, tint, tint, 255]);
    Arc::new(CursorImage {
        width: 16,
        height: 16,
        rgba,
        hot_x: 0.0,
        hot_y: 0.0,
        scale: 1.0,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_cursor_shape_crosses_the_wire_once_and_not_in_game_mode() {
    let h = host(perms(true, true, false)).await;
    let set = |tint| {
        *lock(&h.probe.cursor) = CursorSample {
            image: Some(image(tint)),
            visible: true,
            position: None,
        }
    };
    set(1);
    let (_ep, _conn, mut accepted) = viewer(&h, Mode::Office).await;
    let mut shapes = Vec::new();
    let mut sets = Vec::new();
    let mut script = [
        (Duration::from_millis(250), 2u8),
        (Duration::from_millis(500), 1),
        (Duration::from_millis(750), 2),
    ]
    .into_iter()
    .peekable();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1000) {
        if let Some((at, tint)) = script.peek().copied()
            && start.elapsed() >= at
        {
            set(tint);
            script.next();
        }
        if let Ok(Ok(Some(body))) =
            tokio::time::timeout(Duration::from_millis(20), accepted.control.recv()).await
        {
            match body {
                ControlBody::CursorShape(s) => shapes.push(s.id),
                ControlBody::CursorSet(s) => sets.push(s.id),
                _ => {}
            }
        }
    }
    assert_eq!(shapes.len(), 2, "two images, two shapes: {shapes:?}");
    assert_eq!(sets.len(), 4, "every change is a set: {sets:?}");
    assert_eq!(sets, vec![shapes[0], shapes[1], shapes[0], shapes[1]]);
    assert!(!lock(&h.probe.captures)[0].show_cursor);

    // Game mode: the cursor goes into the video and the messages stop.
    accepted
        .control
        .send(ControlBody::Request(StreamRequest {
            mode: Mode::Game as i32,
            fps: 0,
            display: 0,
        }))
        .await
        .unwrap();
    let mut after = 0;
    let mut saw_game = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1500) {
        if saw_game && start.elapsed() > Duration::from_millis(800) {
            set(if start.elapsed().as_millis().is_multiple_of(2) {
                3
            } else {
                4
            });
        }
        if let Ok(Ok(Some(body))) =
            tokio::time::timeout(Duration::from_millis(20), accepted.control.recv()).await
        {
            match body {
                ControlBody::Config(c) if c.mode == Mode::Game as i32 => saw_game = true,
                ControlBody::CursorShape(_) | ControlBody::CursorSet(_) if saw_game => after += 1,
                _ => {}
            }
        }
    }
    assert!(saw_game);
    assert_eq!(after, 0, "no cursor messages in game mode");
    assert!(lock(&h.probe.captures).last().unwrap().show_cursor);
}
