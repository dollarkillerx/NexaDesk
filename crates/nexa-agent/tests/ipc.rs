//! The desktop UI's IPC (§5.3) against a real agent on loopback, with fake devices and
//! a hand-rolled UI client that can misbehave. Nothing here touches the screen, the
//! Keychain, the pasteboard or the input system.

mod common;

use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use common::{FakePlatform, FakeUi, caps, perms, until};
use nexa_agent::{
    AgentConfig, AgentUi, UiHost,
    ipc::{
        IpcConfig, IpcError, IpcListener,
        proto::{
            AgentBody, AgentToUi, AuthKind, Decide, Empty, Hello, IncomingRequest, Permissions,
            RevokePairing, SetAccessPolicy, Status, UiBody, UiToAgent,
        },
    },
    start_with_ui,
};
use nexa_auth::{DeviceIdentity, PairingStore, PeerPolicy, client_config, tls::SERVER_NAME};
use nexa_net::vsock::bind_media_socket;
use nexa_session::{
    SessionError,
    handshake::{Timeouts, ViewerAccepted, ViewerAuth, ViewerConfig, viewer_handshake},
    proto::{CloseReason, ControlBody, RejectReason, StreamRequest},
};
use nexa_wire::{
    control::{Mode, Os},
    framing::{FrameDecoder, encode_frame},
};
use prost::Message;
use quinn::{Connection, Endpoint, EndpointConfig, TokioRuntime};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};
use zeroize::Zeroizing;

const WAIT: Duration = Duration::from_secs(10);
const CONSENT: Duration = Duration::from_secs(5);

fn timeouts() -> Timeouts {
    Timeouts {
        handshake: Duration::from_secs(5),
        consent: CONSENT,
    }
}

struct Rig {
    host: UiHost,
    platform: FakePlatform,
    shutdowns: Arc<AtomicU32>,
    dir: tempfile::TempDir,
}

impl Rig {
    fn socket(&self) -> std::path::PathBuf {
        self.dir.path().join("agent.sock")
    }

    async fn ui(&self) -> TestUi {
        TestUi::connect(&self.socket()).await
    }

    /// A UI that has said hello and read the first Status.
    async fn hello(&self) -> (TestUi, Status) {
        let mut ui = self.ui().await;
        ui.send(UiBody::Hello(Hello {
            version: 1,
            client: "test".into(),
        }))
        .await;
        let status = match ui.recv().await {
            Some(AgentBody::Status(s)) => s,
            other => panic!("expected Status, got {other:?}"),
        };
        (ui, status)
    }
}

async fn rig_with(fallback: Option<Arc<FakeUi>>, tweak: impl FnOnce(&mut IpcConfig)) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let identity = DeviceIdentity::generate();
    let store =
        PairingStore::open(dir.path().join("pairings.json"), identity.store_mac_key()).unwrap();
    let platform = FakePlatform::default();
    let mut ipc = IpcConfig::in_dir(dir.path());
    ipc.hello_timeout = Duration::from_millis(300);
    tweak(&mut ipc);
    let shutdowns = Arc::new(AtomicU32::new(0));
    let s = shutdowns.clone();
    let host = start_with_ui(
        AgentConfig {
            lan_discovery: false,
            listen: "127.0.0.1:0".parse().unwrap(),
            device_name: "fake host".into(),
            os: Os::Macos,
            caps: caps(),
            timeouts: timeouts(),
            signal: None,
            files: nexa_agent::FileSettings {
                downloads: dir.path().join("Downloads/NexaDesk"),
                limits: Default::default(),
            },
        },
        identity,
        store,
        Arc::new(platform.clone()),
        fallback.map(|f| f as Arc<dyn AgentUi>),
        ipc,
        Arc::new(move || {
            s.fetch_add(1, SeqCst);
        }),
    )
    .await
    .unwrap();
    Rig {
        host,
        platform,
        shutdowns,
        dir,
    }
}

async fn rig() -> Rig {
    rig_with(None, |_| {}).await
}

/// A UI client speaking the IPC framing, or not, as the test likes.
struct TestUi {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl TestUi {
    async fn connect(path: &Path) -> Self {
        Self {
            stream: UnixStream::connect(path).await.unwrap(),
            decoder: FrameDecoder::new(1 << 20),
        }
    }

    async fn raw(&mut self, bytes: &[u8]) {
        let _ = self.stream.write_all(bytes).await;
    }

    async fn send(&mut self, body: UiBody) {
        let mut out = bytes::BytesMut::new();
        encode_frame(&UiToAgent::from(body).encode_to_vec(), &mut out);
        self.raw(&out).await;
    }

    /// The next message, or `None` once the agent has closed the connection.
    async fn recv(&mut self) -> Option<AgentBody> {
        let mut buf = [0u8; 4096];
        loop {
            if let Some(frame) = self.decoder.next_frame().unwrap() {
                return AgentToUi::decode(frame).unwrap().body;
            }
            let n = tokio::time::timeout(WAIT, self.stream.read(&mut buf))
                .await
                .expect("the agent neither answered nor closed")
                .unwrap_or(0);
            if n == 0 {
                return None;
            }
            self.decoder.push(&buf[..n]);
        }
    }

    async fn until<T>(&mut self, what: &str, mut f: impl FnMut(AgentBody) -> Option<T>) -> T {
        loop {
            match self.recv().await {
                Some(body) => {
                    if let Some(t) = f(body) {
                        return t;
                    }
                }
                None => panic!("connection closed while waiting for {what}"),
            }
        }
    }

    async fn incoming(&mut self) -> IncomingRequest {
        self.until("IncomingRequest", |b| match b {
            AgentBody::Incoming(r) => Some(r),
            _ => None,
        })
        .await
    }

    /// Reads until the agent closes, returning what it said first.
    async fn closed(&mut self) -> Vec<AgentBody> {
        let mut said = Vec::new();
        while let Some(b) = self.recv().await {
            said.push(b);
        }
        said
    }
}

fn view_input() -> Permissions {
    Permissions {
        view: true,
        input: true,
        ..Permissions::default()
    }
}

async fn dial(addr: std::net::SocketAddr, me: &DeviceIdentity) -> (Endpoint, Connection) {
    let socket = bind_media_socket("127.0.0.1:0".parse().unwrap()).unwrap();
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        None,
        socket,
        Arc::new(TokioRuntime),
    )
    .unwrap();
    let config = client_config(me, PeerPolicy::AcceptUnknown).unwrap();
    let conn = tokio::time::timeout(
        WAIT,
        endpoint.connect_with(config, addr, SERVER_NAME).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    (endpoint, conn)
}

/// Starts a viewer that connects with the current code; the handshake runs in the
/// background so the test can play the UI.
fn viewer(
    rig: &Rig,
    me: DeviceIdentity,
) -> tokio::task::JoinHandle<(Endpoint, Connection, Result<ViewerAccepted, SessionError>)> {
    let addr = rig.host.agent.local_addr();
    let code = rig.host.agent.current_code().to_string();
    tokio::spawn(async move {
        let (ep, conn) = dial(addr, &me).await;
        let result = viewer_handshake(
            &conn,
            ViewerConfig {
                me: me.public_key(),
                device_name: "test viewer".into(),
                os: Os::Macos,
                caps: caps(),
                auth: ViewerAuth::Code(Zeroizing::new(code)),
                timeouts: timeouts(),
            },
        )
        .await;
        (ep, conn, result)
    })
}

fn rejected(r: &Result<ViewerAccepted, SessionError>) -> RejectReason {
    match r {
        Err(SessionError::Rejected { reason, .. }) => *reason,
        Err(e) => panic!("expected a rejection, got {e}"),
        Ok(_) => panic!("expected a rejection, the session was accepted"),
    }
}

fn allow(req: &IncomingRequest, perms: Permissions, save: bool) -> UiBody {
    UiBody::Decide(Decide {
        req_id: req.req_id,
        allow: true,
        perms: Some(perms),
        save,
        skip_next_time: false,
    })
}

async fn begin_stream(accepted: &mut ViewerAccepted) {
    accepted
        .control
        .send(ControlBody::Request(StreamRequest {
            mode: Mode::Balanced as i32,
            fps: 0,
            display: 0,
        }))
        .await
        .unwrap();
}

async fn goodbye(accepted: &mut ViewerAccepted) -> i32 {
    loop {
        match tokio::time::timeout(WAIT, accepted.control.recv())
            .await
            .expect("no goodbye")
        {
            Ok(Some(ControlBody::Bye(b))) => return b.reason,
            Ok(Some(_)) => {}
            other => panic!("expected goodbye, got {:?}", other.map(|_| ())),
        }
    }
}

/// A connected UI, a viewer that it allowed, and the session running.
async fn running_session(rig: &Rig, perms: Permissions) -> (TestUi, ViewerAccepted, Connection) {
    let (mut ui, _) = rig.hello().await;
    let v = viewer(rig, DeviceIdentity::generate());
    let req = ui.incoming().await;
    ui.send(allow(&req, perms, true)).await;
    let (ep, conn, result) = v.await.unwrap();
    std::mem::forget(ep); // keep the endpoint alive for the session
    let mut accepted = result.unwrap();
    begin_stream(&mut accepted).await;
    ui.until("SessionStarted", |b| match b {
        AgentBody::SessionStarted(s) => Some(s),
        _ => None,
    })
    .await;
    (ui, accepted, conn)
}

// ---------------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_gets_the_status_and_the_socket_is_private() {
    let rig = rig().await;
    let mode = std::fs::metadata(rig.socket())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "socket mode {mode:o}");

    let (_ui, status) = rig.hello().await;
    assert_eq!(status.version, 1);
    assert_eq!(status.code, *rig.host.agent.current_code());
    assert_eq!(status.listen, rig.host.agent.local_addr().to_string());
    assert_eq!(status.device_name, "fake host");
    assert_eq!(status.device_key, rig.host.agent.public_key().to_hex());
    assert!(status.session.is_none());
    assert!(status.input_available);
    until("attached", WAIT, || rig.host.hub.ui_attached()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_with_another_uid_gets_nothing_and_cannot_consent() {
    let rig = rig_with(None, |c| c.expected_uid = c.expected_uid.wrapping_add(1)).await;
    let mut ui = rig.ui().await;
    ui.send(UiBody::Hello(Hello {
        version: 1,
        client: "intruder".into(),
    }))
    .await;
    assert!(ui.closed().await.is_empty(), "not a single byte for them");
    assert!(!rig.host.hub.ui_attached());

    // So nobody is present, and a correct code is still refused without a prompt.
    let (_ep, _conn, result) = viewer(&rig, DeviceIdentity::generate()).await.unwrap();
    assert_eq!(rejected(&result), RejectReason::ConsentDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_clients_are_dropped_and_the_next_one_is_served() {
    let rig = rig().await;
    let mut oversize = bytes::BytesMut::new();
    nexa_wire::framing::put_varint(1 << 20, &mut oversize);
    let not_proto = {
        let mut b = bytes::BytesMut::new();
        encode_frame(&[0xFF, 0xFF, 0xFF], &mut b);
        b.to_vec()
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("endless varint", vec![0xFF; 12]),
        ("oversize frame", oversize.to_vec()),
        ("not protobuf", not_proto),
        ("silent", vec![]),
    ];
    for (what, bytes) in cases {
        let mut ui = rig.ui().await;
        ui.raw(&bytes).await;
        assert!(
            ui.closed().await.is_empty(),
            "{what}: expected a silent close"
        );
        assert!(!rig.host.hub.ui_attached(), "{what}");
    }

    // Saying something other than hello first, or a version from the future.
    let mut ui = rig.ui().await;
    ui.send(UiBody::GetStatus(Empty {})).await;
    assert!(ui.closed().await.is_empty());
    let mut ui = rig.ui().await;
    ui.send(UiBody::Hello(Hello {
        version: 99,
        client: String::new(),
    }))
    .await;
    let said = ui.closed().await;
    assert!(
        matches!(said.as_slice(), [AgentBody::Refused(_)]),
        "{said:?}"
    );

    // After all of that, a well-behaved UI is served.
    let (_ui, status) = rig.hello().await;
    assert_eq!(status.code.len(), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_ui_is_refused_while_one_is_attached() {
    let rig = rig().await;
    let (first, _) = rig.hello().await;
    let mut second = rig.ui().await;
    second
        .send(UiBody::Hello(Hello {
            version: 1,
            client: "second".into(),
        }))
        .await;
    let said = second.closed().await;
    assert!(
        matches!(said.as_slice(), [AgentBody::Refused(_)]),
        "{said:?}"
    );
    drop(first);
    until("first detached", WAIT, || !rig.host.hub.ui_attached()).await;
    let (_third, _) = rig.hello().await;
}

// ---------------------------------------------------------------------------------
// Consent
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ui_allows_with_the_permissions_it_chose() {
    let rig = rig().await;
    let (mut ui, _) = rig.hello().await;
    let me = DeviceIdentity::generate();
    let key = me.public_key().to_hex();
    let v = viewer(&rig, me);

    let req = ui.incoming().await;
    assert_eq!(req.device_name, "test viewer");
    assert_eq!(
        req.device_key, key,
        "the fingerprint is the requester's key"
    );
    assert_eq!(req.auth, AuthKind::Code as i32);
    assert_eq!(
        req.perms,
        Some(Permissions {
            view: true,
            ..Permissions::default()
        }),
        "new devices are proposed view-only"
    );
    assert!(req.offer_save && !req.save_default && !req.takeover);
    assert_eq!(req.timeout_ms, CONSENT.as_millis() as u32);

    ui.send(allow(&req, view_input(), true)).await;
    let (_ep, _conn, result) = v.await.unwrap();
    let mut accepted = result.unwrap();
    assert_eq!(accepted.accept.perms, Some(perms(true, true, false)));
    begin_stream(&mut accepted).await;

    let started = ui
        .until("SessionStarted", |b| match b {
            AgentBody::SessionStarted(s) => s.session,
            _ => None,
        })
        .await;
    assert_eq!(started.device_key, key);
    assert_eq!(started.perms, Some(view_input()));
    ui.send(UiBody::GetStatus(Empty {})).await;
    let status = ui
        .until("Status", |b| match b {
            AgentBody::Status(s) if s.session.is_some() => Some(s),
            _ => None,
        })
        .await;
    assert_eq!(status.session.unwrap().device_name, "test viewer");

    ui.send(UiBody::ListPairings(Empty {})).await;
    let pairings = ui
        .until("Pairings", |b| match b {
            AgentBody::Pairings(p) => Some(p),
            _ => None,
        })
        .await;
    assert_eq!(pairings.devices.len(), 1, "saved as asked");
    assert_eq!(pairings.devices[0].device_key, key);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ui_denies() {
    let rig = rig().await;
    let (mut ui, _) = rig.hello().await;
    let v = viewer(&rig, DeviceIdentity::generate());
    let req = ui.incoming().await;
    ui.send(UiBody::Decide(Decide {
        req_id: req.req_id,
        allow: false,
        perms: Some(view_input()),
        save: true,
        skip_next_time: true,
    }))
    .await;
    let (_ep, _conn, result) = v.await.unwrap();
    assert_eq!(rejected(&result), RejectReason::ConsentDenied);
    assert!(!rig.host.agent.session_active());
    assert!(rig.host.agent.pairings().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ui_that_drops_during_a_pending_request_means_deny() {
    let rig = rig().await;
    let (mut ui, _) = rig.hello().await;
    let v = viewer(&rig, DeviceIdentity::generate());
    let _req = ui.incoming().await;
    assert!(rig.host.hub.question_open());
    let dropped_at = Instant::now();
    drop(ui);

    let (_ep, _conn, result) = v.await.unwrap();
    assert_eq!(rejected(&result), RejectReason::ConsentDenied);
    assert!(
        dropped_at.elapsed() < CONSENT / 2,
        "denied when the UI went away, not at the timeout ({:?})",
        dropped_at.elapsed()
    );
    assert!(!rig.host.agent.session_active());
    assert!(!rig.host.hub.question_open());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_ui_and_no_terminal_everything_is_refused() {
    let rig = rig().await;
    let (_ep, _conn, result) = viewer(&rig, DeviceIdentity::generate()).await.unwrap();
    assert_eq!(rejected(&result), RejectReason::ConsentDenied);
    assert!(!rig.host.agent.session_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_stale_or_viewless_decisions_never_allow() {
    let rig = rig().await;
    let (mut ui, _) = rig.hello().await;
    let v = viewer(&rig, DeviceIdentity::generate());
    let req = ui.incoming().await;

    // An answer to some other question is refused and changes nothing.
    ui.send(UiBody::Decide(Decide {
        req_id: req.req_id ^ 0xFFFF,
        allow: true,
        perms: Some(view_input()),
        ..Decide::default()
    }))
    .await;
    ui.until("Refused", |b| match b {
        AgentBody::Refused(r) => Some(r),
        _ => None,
    })
    .await;
    assert!(rig.host.hub.question_open());

    // "Allow" without saying what is allowed is a denial.
    ui.send(UiBody::Decide(Decide {
        req_id: req.req_id,
        allow: true,
        perms: None,
        ..Decide::default()
    }))
    .await;
    let (_ep, _conn, result) = v.await.unwrap();
    assert_eq!(rejected(&result), RejectReason::ConsentDenied);

    // And a Decide with every field at its default denies the next one.
    let v = viewer(&rig, DeviceIdentity::generate());
    let req = ui.incoming().await;
    ui.send(UiBody::Decide(Decide {
        req_id: req.req_id,
        ..Decide::default()
    }))
    .await;
    let (_ep, _conn, result) = v.await.unwrap();
    assert_eq!(rejected(&result), RejectReason::ConsentDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswered_request_times_out_as_a_denial() {
    let rig = rig().await;
    let (mut ui, _) = rig.hello().await;
    let v = viewer(&rig, DeviceIdentity::generate());
    let req = ui.incoming().await;
    let closed = ui
        .until("RequestClosed", |b| match b {
            AgentBody::RequestClosed(c) => Some(c),
            _ => None,
        })
        .await;
    assert_eq!(closed.req_id, req.req_id);
    assert_eq!(closed.reason, "timed out");
    let (_ep, _conn, result) = v.await.unwrap();
    assert_eq!(rejected(&result), RejectReason::ConsentDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_terminal_is_asked_only_while_no_ui_is_attached() {
    let terminal = FakeUi::allowing(perms(true, false, false), false);
    let rig = rig_with(Some(terminal.clone()), |_| {}).await;

    let (_ep, conn, result) = viewer(&rig, DeviceIdentity::generate()).await.unwrap();
    result.unwrap();
    assert_eq!(terminal.prompts(), 1, "no UI, so the terminal decides");
    conn.close(3u32.into(), b"done");
    until("session gone", WAIT, || !rig.host.agent.session_active()).await;

    let (mut ui, _) = rig.hello().await;
    let v = viewer(&rig, DeviceIdentity::generate());
    let req = ui.incoming().await;
    ui.send(UiBody::Decide(Decide {
        req_id: req.req_id,
        ..Decide::default()
    }))
    .await;
    let (_ep, _conn, result) = v.await.unwrap();
    assert_eq!(rejected(&result), RejectReason::ConsentDenied);
    assert_eq!(
        terminal.prompts(),
        1,
        "the UI answered; the terminal was not asked"
    );
}

// ---------------------------------------------------------------------------------
// Running sessions
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_the_ui_ends_a_running_session() {
    let rig = rig().await;
    let (ui, mut accepted, _conn) = running_session(&rig, view_input()).await;
    assert!(rig.host.agent.session_active());
    drop(ui);
    assert_eq!(goodbye(&mut accepted).await, CloseReason::HostEnded as i32);
    until("session ended", WAIT, || !rig.host.agent.session_active()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_the_ui_keeps_a_session_the_terminal_still_shows() {
    let terminal = FakeUi::allowing(perms(true, false, false), false);
    let rig = rig_with(Some(terminal.clone()), |_| {}).await;
    let (ui, _accepted, _conn) = running_session(&rig, view_input()).await;
    drop(ui);
    until("detached", WAIT, || !rig.host.hub.ui_attached()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(rig.host.agent.session_active());
    assert!(terminal.saw(|e| matches!(e, nexa_agent::AgentEvent::SessionStarted(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reattached_ui_sees_the_running_session() {
    let terminal = FakeUi::allowing(perms(true, false, false), false);
    let rig = rig_with(Some(terminal), |_| {}).await;
    let (ui, _accepted, _conn) = running_session(&rig, view_input()).await;
    drop(ui);
    until("detached", WAIT, || !rig.host.hub.ui_attached()).await;
    let (_ui, status) = rig.hello().await;
    let session = status.session.expect("the banner must come back");
    assert_eq!(session.device_name, "test viewer");
    assert_eq!(session.perms, Some(view_input()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ui_disconnects_and_rotates_the_code() {
    let rig = rig().await;
    let (mut ui, mut accepted, _conn) = running_session(&rig, view_input()).await;

    let before = rig.host.agent.current_code().to_string();
    ui.send(UiBody::RotateCode(Empty {})).await;
    let status = ui
        .until("new code", |b| match b {
            AgentBody::Status(s) if s.code != before => Some(s),
            _ => None,
        })
        .await;
    assert_eq!(status.code, *rig.host.agent.current_code());

    ui.send(UiBody::Disconnect(Empty {})).await;
    assert_eq!(goodbye(&mut accepted).await, CloseReason::HostEnded as i32);
    ui.until("SessionEnded", |b| match b {
        AgentBody::SessionEnded(e) => Some(e),
        _ => None,
    })
    .await;
    assert!(!rig.host.agent.session_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_forgets_the_device_and_ends_its_session() {
    let rig = rig().await;
    let (mut ui, mut accepted, _conn) = running_session(&rig, view_input()).await;
    let key = rig.host.agent.pairings()[0].pubkey.to_hex();
    ui.send(UiBody::RevokePairing(RevokePairing { device_key: key }))
        .await;
    let pairings = ui
        .until("Pairings", |b| match b {
            AgentBody::Pairings(p) => Some(p),
            _ => None,
        })
        .await;
    assert!(pairings.devices.is_empty());
    assert_eq!(goodbye(&mut accepted).await, CloseReason::HostEnded as i32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_policy_can_only_narrow() {
    let rig = rig().await;
    let (mut ui, mut accepted, _conn) = running_session(
        &rig,
        Permissions {
            view: true,
            input: true,
            clipboard: true,
            ..Permissions::default()
        },
    )
    .await;
    let record = rig.host.agent.pairings()[0].clone();
    let key = record.pubkey.to_hex();

    // Switching "connect without asking" on is the consent dialog's job alone.
    ui.send(UiBody::SetAccessPolicy(SetAccessPolicy {
        device_key: key.clone(),
        unattended: true,
        perms: Some(view_input()),
    }))
    .await;
    ui.until("Refused", |b| match b {
        AgentBody::Refused(r) => Some(r),
        _ => None,
    })
    .await;
    assert!(!rig.host.agent.pairings()[0].unattended);

    // Removing input narrows the record and ends the session that had it.
    ui.send(UiBody::SetAccessPolicy(SetAccessPolicy {
        device_key: key,
        unattended: false,
        perms: Some(Permissions {
            view: true,
            input: false,
            clipboard: true,
            file: true,
            audio: true,
        }),
    }))
    .await;
    let pairings = ui
        .until("Pairings", |b| match b {
            AgentBody::Pairings(p) => Some(p),
            _ => None,
        })
        .await;
    let p = pairings.devices[0].perms.clone().unwrap();
    assert!(p.view && !p.input && !p.file && !p.audio);
    assert_eq!(p.clipboard, record.perms.clipboard);
    assert_eq!(goodbye(&mut accepted).await, CloseReason::HostEnded as i32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_reaches_the_agent() {
    let rig = rig().await;
    let (mut ui, _) = rig.hello().await;
    ui.send(UiBody::Shutdown(Empty {})).await;
    until("shutdown", WAIT, || rig.shutdowns.load(SeqCst) == 1).await;
    let _ = &rig.platform;
}

// ---------------------------------------------------------------------------------
// The socket file
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_socket_is_not_taken_over_and_a_stale_one_is_replaced() {
    let rig = rig().await;
    let config = IpcConfig::in_dir(rig.dir.path());
    assert!(matches!(
        IpcListener::bind(config.clone()),
        Err(IpcError::AlreadyRunning(_))
    ));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists(), "a dead agent leaves its socket behind");
    let listener = IpcListener::bind(IpcConfig::in_dir(dir.path())).unwrap();
    drop(listener);

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("agent.sock"), b"not a socket").unwrap();
    assert!(matches!(
        IpcListener::bind(IpcConfig::in_dir(dir.path())),
        Err(IpcError::NotOurSocket(_))
    ));

    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        IpcListener::bind(IpcConfig::in_dir(dir.path())),
        Err(IpcError::UnsafeDirectory(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_server_removes_the_socket() {
    let rig = rig().await;
    let path = rig.socket();
    assert!(path.exists());
    let Rig { host, dir, .. } = rig;
    host.agent.shutdown();
    drop(host);
    assert!(!path.exists());
    drop(dir);
}
