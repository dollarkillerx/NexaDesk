//! The real C ABI against a real agent over loopback QUIC, with fake devices on both
//! sides: nothing here touches the keyboard, the pasteboard or the screen.

#[path = "../../nexa-agent/tests/common/mod.rs"]
mod common;

use std::{
    ffi::c_void,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering::SeqCst},
        mpsc::Sender,
    },
    time::{Duration, Instant},
};

use common::{
    FakeClip, FakeDecoder, FakeUi, Host, Injected, NullPresenter, caps, host, lock, perms, until,
};
use nexa_auth::{FileKeyStore, KeyStore};
use nexa_client::{
    abi::{self, NexaClient, NexaSession},
    deps::{Deps, KeySource, KeySourceError},
    input::InputCmd,
    proto::{
        CaptureState, Command, ConnectAuth, ConnectRequest, DisconnectReason, Event, EventBody,
        PointerButton, PointerMove, SessionCommand,
    },
};
use nexa_clip::ClipKind;
use nexa_input::{PhysKey, hid::key};
use nexa_media::{Decoder, PresentMode, Presenter};
use nexa_session::{clip::ClipPlatform, handshake::Timeouts};
use nexa_wire::control::{Caps, Codec, Mode, Os};
use prost::Message;

const WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct ViewerProbe {
    /// Drops every datagram on the viewer's ID-mode sockets while set: an outage.
    cut: Arc<AtomicBool>,
    active_changes: Mutex<Vec<bool>>,
    key_tx: Mutex<Option<Sender<InputCmd>>>,
    decoded: Arc<AtomicU32>,
    clip: FakeClip,
    panic_in_decoder: AtomicBool,
    views_attached: AtomicU32,
}

struct FakeDeps {
    probe: Arc<ViewerProbe>,
    dir: std::path::PathBuf,
}

struct FakeKeys(Arc<ViewerProbe>);

impl KeySource for FakeKeys {
    fn set_active(&mut self, active: bool) -> Result<(), KeySourceError> {
        lock(&self.0.active_changes).push(active);
        Ok(())
    }
}

impl Deps for FakeDeps {
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
        assert!(
            !self.probe.panic_in_decoder.load(SeqCst),
            "simulated bug in the session"
        );
        Ok(Box::new(FakeDecoder(self.probe.decoded.clone())))
    }
    fn key_source(&self, tx: Sender<InputCmd>) -> Box<dyn KeySource> {
        *lock(&self.probe.key_tx) = Some(tx);
        Box::new(FakeKeys(self.probe.clone()))
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
        self.probe.views_attached.fetch_add(1, SeqCst);
        Ok(Box::new(NullPresenter))
    }
    fn link_options(&self) -> nexa_session::rendezvous::link::LinkOptions {
        let cut = self.probe.cut.clone();
        let factory: nexa_session::rendezvous::link::SocketFactory = Arc::new(move |addr| {
            use quinn::Runtime;
            let std = nexa_net::vsock::bind_media_socket(addr)?;
            let inner = quinn::TokioRuntime.wrap_udp_socket(std)?;
            let s: Arc<dyn quinn::AsyncUdpSocket> = Arc::new(Cuttable {
                inner,
                cut: cut.clone(),
            });
            Ok(s)
        });
        nexa_session::rendezvous::link::LinkOptions {
            socket_factory: Some(factory),
            ..loopback_link()
        }
    }
}

/// A real loopback socket that loses everything while `cut` is set.
#[derive(Debug)]
struct Cuttable {
    inner: Arc<dyn quinn::AsyncUdpSocket>,
    cut: Arc<AtomicBool>,
}

impl quinn::AsyncUdpSocket for Cuttable {
    fn create_io_poller(self: Arc<Self>) -> std::pin::Pin<Box<dyn quinn::UdpPoller>> {
        self.inner.clone().create_io_poller()
    }
    fn try_send(&self, t: &quinn::udp::Transmit<'_>) -> std::io::Result<()> {
        if self.cut.load(SeqCst) {
            return Ok(());
        }
        self.inner.try_send(t)
    }
    fn poll_recv(
        &self,
        cx: &mut std::task::Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> std::task::Poll<std::io::Result<usize>> {
        loop {
            match self.inner.poll_recv(cx, bufs, meta) {
                std::task::Poll::Ready(Ok(_)) if self.cut.load(SeqCst) => continue,
                other => return other,
            }
        }
    }
    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }
    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }
    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }
    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// Session sockets on loopback, so tests never touch a real interface.
fn loopback_link() -> nexa_session::rendezvous::link::LinkOptions {
    nexa_session::rendezvous::link::LinkOptions {
        bind: "127.0.0.1:0".parse().unwrap(),
        interfaces: nexa_session::rendezvous::link::Interfaces::Fixed(vec![
            "127.0.0.1".parse().unwrap(),
        ]),
        ..Default::default()
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
    events: Arc<Events>,
    probe: Arc<ViewerProbe>,
    _dir: tempfile::TempDir,
}

unsafe impl Send for Viewer {}

impl Viewer {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let probe = Arc::new(ViewerProbe::default());
        let deps = Arc::new(FakeDeps {
            probe: probe.clone(),
            dir: dir.path().to_path_buf(),
        });
        let events = Arc::new(Events::default());
        let cfg = nexa_client::proto::ClientConfig {
            device_name: "fake viewer".into(),
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
        assert!(!client.is_null());
        Self {
            client,
            events,
            probe,
            _dir: dir,
        }
    }

    fn connect(&self, request: ConnectRequest) -> *mut NexaSession {
        let bytes = request.encode_to_vec();
        let mut session = std::ptr::null_mut();
        // SAFETY: valid buffers and handle.
        let status =
            unsafe { abi::nexa_connect(self.client, bytes.as_ptr(), bytes.len(), &mut session) };
        assert_eq!(status, abi::NEXA_OK);
        session
    }

    fn connect_with_code(&self, h: &Host, tag: u64) -> *mut NexaSession {
        self.connect(ConnectRequest {
            address: h.addr.to_string(),
            tag,
            mode: Mode::Balanced as i32,
            fps: 0,
            auth: Some(ConnectAuth::Code(h.agent.current_code().to_string())),
            signal_server: String::new(),
        })
    }

    fn event_matching(&self, f: impl Fn(&EventBody) -> bool) -> Option<EventBody> {
        lock(&self.events.0)
            .iter()
            .filter_map(|e| e.body.as_ref())
            .find(|b| f(b))
            .cloned()
    }

    async fn wait_event(&self, what: &str, f: impl Fn(&EventBody) -> bool + Copy) -> EventBody {
        until(what, WAIT, || self.event_matching(f).is_some()).await;
        self.event_matching(f).unwrap()
    }

    async fn wait_connected(&self) -> nexa_client::proto::Connected {
        match self
            .wait_event("Connected", |b| {
                matches!(b, EventBody::Connected(_) | EventBody::Disconnected(_))
            })
            .await
        {
            EventBody::Connected(c) => c,
            other => panic!("did not connect: {other:?}"),
        }
    }

    fn keys(&self) -> Sender<InputCmd> {
        lock(&self.probe.key_tx)
            .clone()
            .expect("key source created")
    }

    fn press(&self, hid: nexa_input::Hid, down: bool) {
        self.keys()
            .send(InputCmd::Key(PhysKey {
                hid,
                down,
                repeat: false,
            }))
            .unwrap();
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // SAFETY: the handle came from init.
        unsafe { abi::nexa_shutdown(self.client) };
    }
}

fn set_active(s: *mut NexaSession, active: bool) -> i32 {
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_set_input_active(s, active) }
}

async fn make_host(p: nexa_wire::control::Permissions) -> Host {
    host(FakeUi::allowing(p, true), Timeouts::default()).await
}

async fn connected(h: &Host, tag: u64) -> (Viewer, *mut NexaSession) {
    let viewer = Viewer::new();
    let session = viewer.connect_with_code(h, tag);
    viewer.wait_connected().await;
    (viewer, session)
}

async fn activate(v: &Viewer, s: *mut NexaSession) {
    assert_eq!(set_active(s, true), abi::NEXA_OK);
    until("key source activated", WAIT, || {
        lock(&v.probe.active_changes).last() == Some(&true)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn video_flows_and_the_view_can_attach_at_any_time() {
    let h = make_host(perms(true, false, false)).await;
    let (viewer, session) = connected(&h, 7).await;
    until("frames decoded", WAIT, || {
        viewer.probe.decoded.load(SeqCst) > 5
    })
    .await;
    // SAFETY: a fake handle: the fake `attach_view` never dereferences it.
    let status = unsafe {
        abi::nexa_session_attach_view(
            session,
            std::ptr::dangling_mut::<c_void>(),
            abi::NEXA_VIEW_NSVIEW,
            800,
            600,
            2.0,
        )
    };
    assert_eq!(status, abi::NEXA_OK);
    assert_eq!(viewer.probe.views_attached.load(SeqCst), 1);
    let stream = viewer
        .wait_event("stream", |b| matches!(b, EventBody::Stream(_)))
        .await;
    assert!(matches!(stream, EventBody::Stream(s) if (s.w, s.h) == (1920, 1080)));
    let ready = viewer
        .wait_event(
            "pairing recorded",
            |b| matches!(b, EventBody::Ready(r) if !r.hosts.is_empty()),
        )
        .await;
    assert!(matches!(ready, EventBody::Ready(r) if r.hosts[0].name == "fake host"));
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
    until("session ends at the host", WAIT, || {
        !h.agent.session_active()
    })
    .await;
    viewer
        .wait_event("disconnected", |b| {
            matches!(b, EventBody::Disconnected(d) if d.reason == DisconnectReason::ClosedLocally as i32)
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paired_viewer_reconnects_with_its_pin_and_a_wrong_pin_is_named() {
    let h = make_host(perms(true, false, false)).await;
    let (viewer, session) = connected(&h, 1).await;
    let key = h.agent.public_key().to_hex();
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
    until("first session gone", WAIT, || !h.agent.session_active()).await;

    let session = viewer.connect(ConnectRequest {
        address: h.addr.to_string(),
        tag: 2,
        mode: Mode::Office as i32,
        fps: 0,
        auth: Some(ConnectAuth::KnownHost(key)),
        signal_server: String::new(),
    });
    until("second Connected", WAIT, || {
        lock(&viewer.events.0)
            .iter()
            .any(|e| e.tag == 2 && matches!(e.body, Some(EventBody::Connected(_))))
    })
    .await;
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
    until("second session gone", WAIT, || !h.agent.session_active()).await;

    let viewer = Viewer::new();
    let other = nexa_auth::DeviceIdentity::generate().public_key().to_hex();
    viewer.connect(ConnectRequest {
        address: h.addr.to_string(),
        tag: 3,
        mode: Mode::Office as i32,
        fps: 0,
        auth: Some(ConnectAuth::KnownHost(other)),
        signal_server: String::new(),
    });
    let ended = viewer
        .wait_event("refused", |b| matches!(b, EventBody::Disconnected(_)))
        .await;
    assert!(
        matches!(&ended, EventBody::Disconnected(d) if d.reason == DisconnectReason::NotPaired as i32),
        "an unpaired host must not be dialled at all: {ended:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_with_a_different_key_is_reported_as_a_pin_mismatch() {
    let first = make_host(perms(true, false, false)).await;
    let (viewer, session) = connected(&first, 1).await;
    let pinned = first.agent.public_key().to_hex();
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
    until("session gone", WAIT, || !first.agent.session_active()).await;

    // Something else is now answering at an address the viewer trusts.
    let impostor = make_host(perms(true, true, true)).await;
    viewer.connect(ConnectRequest {
        address: impostor.addr.to_string(),
        tag: 2,
        mode: Mode::Office as i32,
        fps: 0,
        auth: Some(ConnectAuth::KnownHost(pinned)),
        signal_server: String::new(),
    });
    let ended = viewer
        .wait_event("refused", |b| matches!(b, EventBody::Disconnected(d) if d.reason != DisconnectReason::ClosedLocally as i32))
        .await;
    assert!(
        matches!(&ended, EventBody::Disconnected(d) if d.reason == DisconnectReason::PinMismatch as i32),
        "{ended:?}"
    );
    assert_eq!(impostor.ui.prompts(), 0);
    assert!(!impostor.agent.session_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_cannot_capture_input_it_was_not_granted() {
    let h = make_host(perms(true, false, false)).await;
    let (viewer, session) = connected(&h, 1).await;
    assert_eq!(set_active(session, true), abi::NEXA_E_NOT_PERMITTED);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*lock(&viewer.probe.active_changes), Vec::<bool>::new());
    assert!(lock(&viewer.probe.key_tx).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_reach_the_host_in_order_and_capture_stops_with_the_session() {
    let h = make_host(perms(true, true, false)).await;
    let (viewer, session) = connected(&h, 1).await;
    activate(&viewer, session).await;
    assert!(
        viewer
            .event_matching(|b| matches!(b, EventBody::InputCapture(c) if c.state == CaptureState::Active as i32))
            .is_some()
    );
    for (hid, down) in [
        (key::A, true),
        (key::A, false),
        (key::B, true),
        (key::B, false),
    ] {
        viewer.press(hid, down);
    }
    until("keys injected", WAIT, || {
        h.platform.injected.keys().len() == 4
    })
    .await;
    assert_eq!(
        h.platform.injected.keys(),
        vec![
            (key::A.0, true),
            (key::A.0, false),
            (key::B.0, true),
            (key::B.0, false)
        ]
    );

    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
    until("capture stopped", WAIT, || {
        lock(&viewer.probe.active_changes).last() == Some(&false)
    })
    .await;
    until("remote keys released", WAIT, || {
        h.platform.injected.all().contains(&Injected::ReleaseAll)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_stops_when_the_host_goes_away() {
    let h = make_host(perms(true, true, false)).await;
    let (viewer, session) = connected(&h, 1).await;
    activate(&viewer, session).await;
    h.agent.disconnect();
    until("capture stopped", WAIT, || {
        lock(&viewer.probe.active_changes).last() == Some(&false)
    })
    .await;
    viewer
        .wait_event("host ended", |b| {
            matches!(b, EventBody::Disconnected(d) if d.reason == DisconnectReason::HostEnded as i32)
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pointer_commands_map_view_pixels_and_clicks_carry_position() {
    let h = make_host(perms(true, true, false)).await;
    let (viewer, session) = connected(&h, 1).await;
    // SAFETY: a fake handle: the fake `attach_view` never dereferences it.
    unsafe {
        abi::nexa_session_attach_view(
            session,
            std::ptr::dangling_mut::<c_void>(),
            abi::NEXA_VIEW_NSVIEW,
            1920,
            1080,
            1.0,
        );
    }
    until("stream known", WAIT, || {
        viewer
            .event_matching(|b| matches!(b, EventBody::Stream(_)))
            .is_some()
    })
    .await;
    activate(&viewer, session).await;
    let send = |cmd: Command| {
        let bytes = SessionCommand { cmd: Some(cmd) }.encode_to_vec();
        // SAFETY: valid buffer and handle.
        assert_eq!(
            unsafe { abi::nexa_session_command(session, bytes.as_ptr(), bytes.len()) },
            abi::NEXA_OK
        );
    };
    send(Command::Move(PointerMove { x: 960.0, y: 540.0 }));
    send(Command::Button(PointerButton {
        button: 0,
        down: true,
    }));
    send(Command::Button(PointerButton {
        button: 0,
        down: false,
    }));
    until("click injected", WAIT, || {
        h.platform.injected.all().contains(&Injected::Button {
            button: 0,
            down: false,
            at: Some((480.0, 270.0)),
        })
    })
    .await;
    assert!(
        h.platform
            .injected
            .all()
            .contains(&Injected::Move(480.0, 270.0))
    );
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
}

/// Copy on the viewer, paste at once, and a slow host pasteboard: the V must not reach
/// the host until the host has the clipboard (§8.3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_paste_key_waits_for_the_clipboard() {
    let h = make_host(perms(true, true, true)).await;
    let (viewer, session) = connected(&h, 1).await;
    until("host clipboard up", WAIT, || {
        h.platform.state.clipboards_created.load(SeqCst) == 1
    })
    .await;
    activate(&viewer, session).await;
    *lock(&h.platform.clip.write_delay) = Duration::from_millis(250);

    viewer
        .probe
        .clip
        .user_copy(&[(ClipKind::Text, b"copied just now")]);
    let pressed = Instant::now();
    viewer.press(key::LGUI, true);
    viewer.press(key::V, true);
    viewer.press(key::V, false);
    viewer.press(key::LGUI, false);

    until("paste injected", WAIT, || {
        h.platform.injected.keys().contains(&(key::V.0, true))
    })
    .await;
    let written = lock(&h.platform.clip.written_at).expect("host clipboard written");
    let v_down = h
        .platform
        .injected
        .all()
        .into_iter()
        .find_map(|i| match i {
            Injected::Key {
                hid,
                down: true,
                at,
            } if hid == key::V.0 => Some(at),
            _ => None,
        })
        .unwrap();
    assert!(
        v_down >= written,
        "V reached the host before its clipboard was written"
    );
    assert!(
        written.duration_since(pressed) >= Duration::from_millis(200),
        "the write was slow"
    );
    assert_eq!(
        h.platform.clip.content(ClipKind::Text).as_deref(),
        Some(&b"copied just now"[..])
    );

    let keys = h.platform.injected.keys();
    let at = |k: (u32, bool)| keys.iter().position(|x| *x == k).unwrap();
    assert!(at((key::V.0, true)) < at((key::V.0, false)));
    assert!(
        at((key::LGUI.0, true)) < at((key::V.0, true)),
        "Cmd stays down around V"
    );
    assert!(at((key::V.0, false)) < at((key::LGUI.0, false)));

    // A key-state sync must not overtake the held paste: if it did, Cmd would be
    // released at the host before V arrived and the paste would become a plain "v".
    until("sync after release", WAIT, || {
        let log = h.platform.injected.all();
        let cmd_up = log.iter().position(
            |i| matches!(i, Injected::Key { hid, down: false, .. } if *hid == key::LGUI.0),
        );
        cmd_up.is_some_and(|c| {
            log[c..]
                .iter()
                .any(|i| matches!(i, Injected::Sync(p) if p.is_empty()))
        })
    })
    .await;
    let log = h.platform.injected.all();
    let index = |hid: u32, down: bool| {
        log.iter()
            .position(
                |i| matches!(i, Injected::Key { hid: h, down: d, .. } if *h == hid && *d == down),
            )
            .unwrap()
    };
    let (cmd_down, v_index) = (index(key::LGUI.0, true), index(key::V.0, true));
    assert!(
        !log[cmd_down..v_index]
            .iter()
            .any(|i| matches!(i, Injected::Sync(p) if !p.contains(&key::LGUI.0))),
        "a sync dropped Cmd before the held V was released: {:?}",
        &log[cmd_down..v_index]
    );

    viewer
        .wait_event(
            "gate report",
            |b| matches!(b, EventBody::PasteGate(p) if !p.timed_out && p.waited_ms > 0),
        )
        .await;
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
}

/// If the host never confirms, the paste still happens after the deadline and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_paste_gives_up_at_the_deadline() {
    let h = make_host(perms(true, true, true)).await;
    let (viewer, session) = connected(&h, 1).await;
    until("host clipboard up", WAIT, || {
        h.platform.state.clipboards_created.load(SeqCst) == 1
    })
    .await;
    activate(&viewer, session).await;
    *lock(&h.platform.clip.write_delay) = Duration::from_millis(1500);

    viewer.probe.clip.user_copy(&[(ClipKind::Text, b"slow")]);
    let pressed = Instant::now();
    viewer.press(key::LGUI, true);
    viewer.press(key::V, true);
    viewer.press(key::V, false);
    viewer.press(key::LGUI, false);

    until("paste released by deadline", WAIT, || {
        h.platform.injected.keys().contains(&(key::V.0, true))
    })
    .await;
    let waited = pressed.elapsed();
    assert!(
        waited >= Duration::from_millis(700),
        "released too early: {waited:?}"
    );
    assert!(
        waited < Duration::from_millis(1400),
        "should not have waited for the write: {waited:?}"
    );
    assert!(
        lock(&h.platform.clip.written_at).is_none(),
        "the write was still in progress"
    );
    viewer
        .wait_event(
            "timeout report",
            |b| matches!(b, EventBody::PasteGate(p) if p.timed_out),
        )
        .await;
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
}

fn big_image() -> Vec<u8> {
    (0..200 * 1024u32).map(|i| (i % 251) as u8).collect()
}

/// §8.6: the viewer's pasteboard gets real data, never a promise that a paste would
/// block the UI thread on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_image_is_fetched_before_the_viewer_pasteboard_is_written() {
    let h = make_host(perms(true, false, true)).await;
    let (viewer, session) = connected(&h, 1).await;
    until("host clipboard up", WAIT, || {
        h.platform.state.clipboards_created.load(SeqCst) == 1
    })
    .await;
    let image = big_image();
    h.platform
        .clip
        .user_copy(&[(ClipKind::Text, b"caption"), (ClipKind::Png, &image)]);
    until("image on the viewer pasteboard", WAIT, || {
        viewer.probe.clip.content(ClipKind::Png).as_deref() == Some(&image[..])
    })
    .await;
    assert_eq!(
        viewer.probe.clip.content(ClipKind::Text).as_deref(),
        Some(&b"caption"[..])
    );
    assert_eq!(
        *lock(&viewer.probe.clip.promised_on_write),
        Vec::<ClipKind>::new(),
        "a promise on the viewer would block the UI thread on a slow fetch"
    );
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
}

/// The host stays lazy, so that a paste gate is not held up by a large transfer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_image_is_promised_on_the_host_and_fetched_on_demand() {
    let h = make_host(perms(true, false, true)).await;
    let (viewer, session) = connected(&h, 1).await;
    until("host clipboard up", WAIT, || {
        h.platform.state.clipboards_created.load(SeqCst) == 1
    })
    .await;
    let image = big_image();
    viewer.probe.clip.user_copy(&[(ClipKind::Png, &image)]);
    until("host promise", WAIT, || {
        lock(&h.platform.clip.backend).owner().is_some()
    })
    .await;
    assert_eq!(
        *lock(&h.platform.clip.promised_on_write),
        vec![ClipKind::Png]
    );
    let generation = lock(&h.platform.clip.backend).owner().unwrap();
    let resolver = lock(&h.platform.clip.resolver).clone().unwrap();
    let fetched = tokio::task::spawn_blocking(move || resolver(generation, ClipKind::Png))
        .await
        .unwrap();
    assert_eq!(fetched, Some(image));
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn garbage_and_stale_handles_return_errors() {
    let viewer = Viewer::new();
    let junk = [0xFFu8; 8];
    let mut session = std::ptr::null_mut();
    // SAFETY: valid buffers and handle.
    let status =
        unsafe { abi::nexa_connect(viewer.client, junk.as_ptr(), junk.len(), &mut session) };
    assert_eq!(status, abi::NEXA_E_PARSE);
    assert!(session.is_null());

    let session = viewer.connect(ConnectRequest {
        address: "127.0.0.1:1".into(),
        tag: 9,
        mode: 0,
        fps: 0,
        auth: None,
        signal_server: String::new(),
    });
    viewer
        .wait_event("invalid", |b| {
            matches!(b, EventBody::Disconnected(d) if d.reason == DisconnectReason::InvalidRequest as i32)
        })
        .await;
    // SAFETY: handles are registry ids, and closing twice is defined.
    unsafe {
        abi::nexa_session_close(session);
        abi::nexa_session_close(session);
        assert_eq!(
            abi::nexa_session_detach_view(session),
            abi::NEXA_E_BAD_HANDLE
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_in_the_session_ends_it_and_releases_capture() {
    let h = make_host(perms(true, true, false)).await;
    let viewer = Viewer::new();
    viewer.probe.panic_in_decoder.store(true, SeqCst);
    let session = viewer.connect_with_code(&h, 1);
    assert_eq!(set_active(session, true), abi::NEXA_OK);
    viewer
        .wait_event("internal error", |b| {
            matches!(b, EventBody::Disconnected(d) if d.reason == DisconnectReason::Internal as i32)
        })
        .await;
    // The ABI still works afterwards.
    let mut written = 0usize;
    // SAFETY: valid pointers.
    assert_eq!(
        unsafe { abi::nexa_last_error(std::ptr::null_mut(), 0, &mut written) },
        abi::NEXA_OK
    );
}

// ---------------------------------------------------------------------------------
// By device ID, through a signaling server on 127.0.0.1 (§4.3)
// ---------------------------------------------------------------------------------

/// A plain-ws signaling server, STUN and relay, all on 127.0.0.1.
async fn signal_server() -> nexa_signal::Server {
    let ws = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stun = nexa_net::udp::UdpIo::bind_tokio("127.0.0.1:0".parse().unwrap()).unwrap();
    let relay = nexa_net::udp::UdpIo::bind_tokio("127.0.0.1:0".parse().unwrap()).unwrap();
    let (stun_addr, relay_addr) = (stun.local_addr(), relay.local_addr());
    nexa_signal::start(
        nexa_signal::Settings {
            tls: None,
            server_names: vec!["127.0.0.1".into()],
            stun_advertise: vec![stun_addr],
            relay_advertise: Some(relay_addr),
            relay: nexa_signal::relay::RelayLimits {
                bandwidth_bps: 100_000_000,
                max_sessions: 16,
            },
            relay_token_ttl_secs: 600,
            limits: nexa_signal::config::Limits {
                register_per_ip: (100.0, 100.0),
                lookup_per_ip: (100.0, 100.0),
                connect_per_ip: (100.0, 100.0),
                ..Default::default()
            },
        },
        nexa_signal::Sockets {
            ws,
            stun: vec![std::sync::Arc::new(stun)],
            relay: Some(std::sync::Arc::new(relay)),
        },
    )
    .await
    .unwrap()
}

fn signal_url(s: &nexa_signal::Server) -> String {
    format!("ws://127.0.0.1:{}", s.ws_addr.port())
}

async fn host_by_id(
    server: &nexa_signal::Server,
    p: nexa_wire::control::Permissions,
    relay_only: bool,
) -> (Host, String) {
    let setup = nexa_agent::SignalSetup {
        server: nexa_session::rendezvous::signal::SignalConfig::new(signal_url(server)),
        bind_ip: "127.0.0.1".parse().unwrap(),
        interfaces: None,
        paired_only: false,
        relay_only,
        system_services: false,
    };
    let h = common::host_with(FakeUi::allowing(p, true), Timeouts::default(), Some(setup)).await;
    until("registered", WAIT, || {
        h.agent.signal_status().short_id.is_some()
    })
    .await;
    let id = h.agent.signal_status().short_id.unwrap();
    assert_eq!(
        h.agent.signal_status().state,
        nexa_agent::SignalState::Online
    );
    (h, id)
}

fn by_id(id: &str, server: &nexa_signal::Server, tag: u64, auth: ConnectAuth) -> ConnectRequest {
    ConnectRequest {
        // As typed by a person, with spaces.
        address: nexa_wire::signal::group_short_id(id),
        tag,
        mode: Mode::Balanced as i32,
        fps: 0,
        auth: Some(auth),
        signal_server: signal_url(server),
    }
}

fn last_path(v: &Viewer, tag: u64) -> Option<(i32, String)> {
    lock(&v.events.0).iter().rev().find_map(|e| match &e.body {
        Some(EventBody::Stats(s)) if e.tag == tag => Some((s.path, s.path_detail.clone())),
        _ => None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connects_by_id_with_pairing_and_consent_unchanged() {
    let server = signal_server().await;
    let (h, id) = host_by_id(&server, perms(true, false, false), false).await;
    let viewer = Viewer::new();
    let session = viewer.connect(by_id(
        &id,
        &server,
        1,
        ConnectAuth::Code(h.agent.current_code().to_string()),
    ));
    let c = viewer.wait_connected().await;
    assert_eq!(c.host_key, h.agent.public_key().to_hex());
    assert!(c.paired_now);
    // Consent was asked exactly as for ip:port, and the remote shown is the real path.
    assert_eq!(h.ui.prompts(), 1);
    let info = h.agent.active_session().unwrap();
    assert!(info.remote.ip().is_loopback(), "{}", info.remote);
    assert_ne!(info.remote, nexa_net::vsock::VIRTUAL_PEER);
    until("frames decoded", WAIT, || {
        viewer.probe.decoded.load(SeqCst) > 5
    })
    .await;
    until("path stats", WAIT, || {
        last_path(&viewer, 1).is_some_and(|(k, _)| k != 0)
    })
    .await;
    let (kind, detail) = last_path(&viewer, 1).unwrap();
    assert_eq!(
        kind,
        nexa_client::proto::PathKind::Direct as i32,
        "{detail}"
    );
    let stages: Vec<i32> = lock(&viewer.events.0)
        .iter()
        .filter_map(|e| match &e.body {
            Some(EventBody::Connecting(c)) => Some(c.stage),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        vec![1, 1, 2, 3, 0],
        "resolving, gathering, checking, QUIC"
    );
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
    until("first session gone", WAIT, || !h.agent.session_active()).await;

    // Paired now: by ID with the pin, and the host's paired-device rules apply.
    let session = viewer.connect(by_id(
        &id,
        &server,
        2,
        ConnectAuth::KnownHost(h.agent.public_key().to_hex()),
    ));
    until("second Connected", WAIT, || {
        lock(&viewer.events.0)
            .iter()
            .any(|e| e.tag == 2 && matches!(e.body, Some(EventBody::Connected(_))))
    })
    .await;
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(session) };
    until("second session gone", WAIT, || !h.agent.session_active()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_code_by_id_is_refused_like_by_address() {
    let server = signal_server().await;
    let (h, id) = host_by_id(&server, perms(true, true, false), false).await;
    let viewer = Viewer::new();
    let _s = viewer.connect(by_id(&id, &server, 1, ConnectAuth::Code("AAAA2222".into())));
    let d = viewer
        .wait_event("Disconnected", |b| matches!(b, EventBody::Disconnected(_)))
        .await;
    let EventBody::Disconnected(d) = d else {
        unreachable!()
    };
    assert_eq!(
        d.reason,
        DisconnectReason::AuthFailed as i32,
        "{}",
        d.message
    );
    assert_eq!(h.ui.prompts(), 0, "nobody was asked");
    assert!(!h.agent.session_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_only_host_is_reached_through_the_relay() {
    let server = signal_server().await;
    let (h, id) = host_by_id(&server, perms(true, false, false), true).await;
    let viewer = Viewer::new();
    let _s = viewer.connect(by_id(
        &id,
        &server,
        1,
        ConnectAuth::Code(h.agent.current_code().to_string()),
    ));
    viewer.wait_connected().await;
    until("frames decoded", WAIT, || {
        viewer.probe.decoded.load(SeqCst) > 5
    })
    .await;
    until("path stats", WAIT, || {
        last_path(&viewer, 1).is_some_and(|(k, _)| k != 0)
    })
    .await;
    let (kind, detail) = last_path(&viewer, 1).unwrap();
    assert_eq!(kind, nexa_client::proto::PathKind::Relay as i32, "{detail}");
    // QUIC, not just the relay check, went through it (fake frames are tiny).
    assert!(
        server.relay_stats().forwarded > 20,
        "{:?}",
        server.relay_stats()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_ids_and_missing_servers_are_named() {
    let server = signal_server().await;
    let viewer = Viewer::new();
    let _s = viewer.connect(by_id(
        "123456789",
        &server,
        1,
        ConnectAuth::Code("AAAA2222".into()),
    ));
    let d = viewer
        .wait_event("Disconnected", |b| matches!(b, EventBody::Disconnected(_)))
        .await;
    let EventBody::Disconnected(d) = d else {
        unreachable!()
    };
    assert_eq!(d.reason, DisconnectReason::Offline as i32, "{}", d.message);

    let mut req = by_id(
        "123456789",
        &server,
        2,
        ConnectAuth::Code("AAAA2222".into()),
    );
    req.signal_server.clear();
    let _s = viewer.connect(req);
    until("second Disconnected", WAIT, || {
        lock(&viewer.events.0).iter().any(|e| {
            e.tag == 2
                && matches!(&e.body, Some(EventBody::Disconnected(d)) if d.reason == DisconnectReason::InvalidRequest as i32)
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_direct_address_mode_still_reports_its_path() {
    let h = make_host(perms(true, false, false)).await;
    let (viewer, _session) = connected(&h, 3).await;
    until("path stats", WAIT, || last_path(&viewer, 3).is_some()).await;
    let (kind, detail) = last_path(&viewer, 3).unwrap();
    assert_eq!(kind, nexa_client::proto::PathKind::Direct as i32);
    assert_eq!(detail, h.addr.to_string());
}

fn count(v: &Viewer, tag: u64, f: impl Fn(&EventBody) -> bool) -> usize {
    lock(&v.events.0)
        .iter()
        .filter(|e| e.tag == tag && e.body.as_ref().is_some_and(&f))
        .count()
}

/// Cuts the viewer's network until it starts reconnecting (§4.4), then gives
/// it back, and waits for the session to come back on the same tag.
async fn drop_and_recover(viewer: &Viewer, tag: u64) {
    viewer.probe.cut.store(true, SeqCst);
    until("Reconnecting", Duration::from_secs(20), || {
        count(viewer, tag, |b| matches!(b, EventBody::Reconnecting(_))) > 0
    })
    .await;
    // The window stays: nothing said the session ended.
    assert_eq!(
        count(viewer, tag, |b| matches!(b, EventBody::Disconnected(_))),
        0
    );
    viewer.probe.cut.store(false, SeqCst);
    until("Connected again", Duration::from_secs(30), || {
        count(viewer, tag, |b| matches!(b, EventBody::Connected(_))) == 2
            || count(viewer, tag, |b| matches!(b, EventBody::Disconnected(_))) > 0
    })
    .await;
    let ended = lock(&viewer.events.0).iter().find_map(|e| match &e.body {
        Some(EventBody::Disconnected(d)) if e.tag == tag => Some(d.clone()),
        _ => None,
    });
    assert!(ended.is_none(), "{ended:?}");
}

/// Pairs by code, then connects as a known host; `unattended` answers the
/// second prompt with "don't ask next time".
async fn paired_by_id(unattended: bool) -> (nexa_signal::Server, Host, String, Viewer) {
    let server = signal_server().await;
    let (h, id) = host_by_id(&server, perms(true, false, false), false).await;
    let viewer = Viewer::new();
    let s = viewer.connect(by_id(
        &id,
        &server,
        1,
        ConnectAuth::Code(h.agent.current_code().to_string()),
    ));
    assert!(viewer.wait_connected().await.paired_now);
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(s) };
    until("first session gone", WAIT, || !h.agent.session_active()).await;
    lock(&h.ui.answer).skip_next_time = unattended;
    let s = viewer.connect(by_id(
        &id,
        &server,
        2,
        ConnectAuth::KnownHost(h.agent.public_key().to_hex()),
    ));
    until("second Connected", WAIT, || {
        count(&viewer, 2, |b| matches!(b, EventBody::Connected(_))) == 1
    })
    .await;
    // SAFETY: handles are registry ids.
    unsafe { abi::nexa_session_close(s) };
    until("second session gone", WAIT, || !h.agent.session_active()).await;
    assert_eq!(h.ui.prompts(), 2);
    (server, h, id, viewer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_session_reconnects_without_asking_an_unattended_host() {
    let (server, h, id, viewer) = paired_by_id(true).await;
    let key = h.agent.public_key().to_hex();
    let _s = viewer.connect(by_id(&id, &server, 3, ConnectAuth::KnownHost(key.clone())));
    until("third Connected", WAIT, || {
        count(&viewer, 3, |b| matches!(b, EventBody::Connected(_))) == 1
    })
    .await;
    assert_eq!(h.ui.prompts(), 2, "unattended: nobody asked");
    drop_and_recover(&viewer, 3).await;
    assert_eq!(h.ui.prompts(), 2, "the reconnect asked nobody either");
    let again = lock(&viewer.events.0)
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            Some(EventBody::Connected(c)) if e.tag == 3 => Some(c.clone()),
            _ => None,
        });
    assert_eq!(again.unwrap().host_key, key, "the same device");
    let before = viewer.probe.decoded.load(SeqCst);
    until("frames after the reconnect", WAIT, || {
        viewer.probe.decoded.load(SeqCst) > before + 5
    })
    .await;
    assert!(h.agent.session_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_session_of_a_device_that_needs_consent_asks_again() {
    let (server, h, id, viewer) = paired_by_id(false).await;
    let _s = viewer.connect(by_id(
        &id,
        &server,
        3,
        ConnectAuth::KnownHost(h.agent.public_key().to_hex()),
    ));
    until("third Connected", WAIT, || {
        count(&viewer, 3, |b| matches!(b, EventBody::Connected(_))) == 1
    })
    .await;
    assert_eq!(h.ui.prompts(), 3);
    drop_and_recover(&viewer, 3).await;
    assert_eq!(h.ui.prompts(), 4, "the person at the host was asked again");
    let last = lock(&h.ui.asked).last().cloned().unwrap();
    assert!(
        !last.takeover,
        "its own dropped session is not someone else's"
    );
}
