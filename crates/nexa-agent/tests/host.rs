//! The agent over loopback QUIC, driven by a hand-rolled viewer that can misbehave.

mod common;

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use common::{FakeUi, Host, Injected, caps, host, perms, until};
use nexa_agent::AgentEvent;
use nexa_auth::{DeviceIdentity, PeerPolicy, client_config, tls::SERVER_NAME};
use nexa_net::vsock::bind_media_socket;
use nexa_session::{
    SessionError,
    framed::{open_bi, open_uni},
    handshake::{Timeouts, ViewerAccepted, ViewerAuth, ViewerConfig, viewer_handshake},
    proto::{CloseReason, ControlBody, InputBody, InputMsg, RejectReason, StreamRequest},
};
use nexa_wire::{
    control::{ButtonEvent, KeyEvent, KeyStateSync, Mode, Os, StreamConfig, TextInput},
    datagram::Pointer,
    framing::StreamType,
};
use quinn::{Connection, Endpoint, EndpointConfig, TokioRuntime};
use zeroize::Zeroizing;

const WAIT: Duration = Duration::from_secs(10);

fn timeouts() -> Timeouts {
    Timeouts {
        handshake: Duration::from_secs(5),
        consent: Duration::from_secs(5),
    }
}

async fn host_with(ui: Arc<FakeUi>) -> Host {
    host(ui, timeouts()).await
}

/// A viewer endpoint connected to `h` under `policy`.
async fn dial(
    h: &Host,
    me: &DeviceIdentity,
    policy: PeerPolicy,
) -> (Endpoint, Result<Connection, String>) {
    let socket = bind_media_socket("127.0.0.1:0".parse().unwrap()).unwrap();
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        None,
        socket,
        Arc::new(TokioRuntime),
    )
    .unwrap();
    let config = client_config(me, policy).unwrap();
    let connecting = endpoint.connect_with(config, h.addr, SERVER_NAME).unwrap();
    let result = tokio::time::timeout(WAIT, connecting)
        .await
        .expect("connect timed out")
        .map_err(|e| e.to_string());
    (endpoint, result)
}

fn viewer_config(me: &DeviceIdentity, auth: ViewerAuth) -> ViewerConfig {
    ViewerConfig {
        me: me.public_key(),
        device_name: "test viewer".into(),
        os: Os::Macos,
        caps: caps(),
        auth,
        timeouts: timeouts(),
    }
}

fn code_of(h: &Host) -> ViewerAuth {
    ViewerAuth::Code(Zeroizing::new(h.agent.current_code().to_string()))
}

async fn connect_with_code(
    h: &Host,
    me: &DeviceIdentity,
) -> (Endpoint, Connection, Result<ViewerAccepted, SessionError>) {
    let (ep, conn) = dial(h, me, PeerPolicy::AcceptUnknown).await;
    let conn = conn.unwrap();
    let result = viewer_handshake(&conn, viewer_config(me, code_of(h))).await;
    (ep, conn, result)
}

fn rejected(r: Result<ViewerAccepted, SessionError>) -> RejectReason {
    match r {
        Err(SessionError::Rejected { reason, .. }) => reason,
        Err(e) => panic!("expected a rejection, got {e}"),
        Ok(_) => panic!("expected a rejection, the session was accepted"),
    }
}

/// Finishes the opening after `SessionAccept`: asks for a stream and gets its config.
async fn begin_stream(accepted: &mut ViewerAccepted) -> StreamConfig {
    accepted
        .control
        .send(ControlBody::Request(StreamRequest {
            mode: Mode::Balanced as i32,
            fps: 0,
            display: 0,
        }))
        .await
        .unwrap();
    loop {
        match accepted.control.recv().await.unwrap() {
            Some(ControlBody::Config(c)) => return c,
            Some(_) => {}
            None => panic!("control stream ended"),
        }
    }
}

/// The app listens on every interface by default (`[::]`, dual-stack). An IPv4
/// viewer must still be shown, and charged pairing guesses, as plain IPv4.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dual_stack_host_sees_ipv4_viewers_as_ipv4() {
    let ui = FakeUi::allowing(perms(true, false, false), false);
    let mut h = common::host_on(ui.clone(), timeouts(), None, "[::]:0".parse().unwrap()).await;
    assert!(h.addr.ip().is_unspecified());
    h.addr = std::net::SocketAddr::new([127, 0, 0, 1].into(), h.addr.port());
    let me = DeviceIdentity::generate();
    let (_ep, _conn, result) = connect_with_code(&h, &me).await;
    result.unwrap();
    let asked = common::lock(&ui.asked);
    assert_eq!(asked[0].remote.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correct_code_with_consent_is_accepted_and_negotiates_a_stream() {
    let ui = FakeUi::allowing(perms(true, false, false), true);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, _conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    assert!(
        accepted.proof.is_some(),
        "the viewer must be able to pin the host"
    );
    assert_eq!(accepted.host, h.agent.public_key());
    assert_eq!(ui.prompts(), 1, "a correct code still needs consent (§9.3)");
    assert_eq!(accepted.accept.perms, Some(perms(true, false, false)));
    assert_eq!(accepted.accept.host_name, "fake host");
    assert_eq!(accepted.accept.displays[0].name, "fake");

    let config = begin_stream(&mut accepted).await;
    assert_eq!((config.w, config.h), (1920, 1080));
    assert_eq!(config.codec, nexa_wire::control::Codec::Hevc as i32);
    assert_eq!(config.fps, 60);
    until("session shown", WAIT, || {
        ui.saw(|e| matches!(e, AgentEvent::SessionStarted(_)))
    })
    .await;
    assert!(h.agent.session_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paired_device_connects_and_needs_consent_unless_unattended() {
    let ui = FakeUi::allowing(perms(true, true, false), true);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    {
        let (_ep, conn, result) = connect_with_code(&h, &me).await;
        result.unwrap();
        conn.close(3u32.into(), b"done");
    }
    until("first session gone", WAIT, || !h.agent.session_active()).await;
    assert_eq!(ui.prompts(), 1);

    let pin = PeerPolicy::Pinned(h.agent.public_key());
    common::lock(&ui.answer).skip_next_time = true;
    {
        let (_ep, conn) = dial(&h, &me, pin).await;
        let conn = conn.unwrap();
        let accepted = viewer_handshake(&conn, viewer_config(&me, ViewerAuth::Known))
            .await
            .unwrap();
        assert!(accepted.proof.is_none());
        assert_eq!(accepted.accept.perms, Some(perms(true, true, false)));
        assert_eq!(
            ui.prompts(),
            2,
            "a paired device is still asked about (§9.3)"
        );
        conn.close(3u32.into(), b"done");
    }
    until("second session gone", WAIT, || !h.agent.session_active()).await;

    // "Don't ask next time" was ticked; decide() now returns Allow, with no prompt.
    *common::lock(&ui.answer) = nexa_session::handshake::ConsentAnswer::deny();
    {
        let (_ep, conn) = dial(&h, &me, pin).await;
        let conn = conn.unwrap();
        let accepted = viewer_handshake(&conn, viewer_config(&me, ViewerAuth::Known))
            .await
            .unwrap();
        assert_eq!(accepted.accept.perms, Some(perms(true, true, false)));
        assert_eq!(ui.prompts(), 2, "an unattended device is not asked about");
        conn.close(3u32.into(), b"done");
    }
    until("third session gone", WAIT, || !h.agent.session_active()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_device_claiming_known_is_refused_without_asking() {
    let ui = FakeUi::allowing(perms(true, true, true), false);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn) = dial(&h, &me, PeerPolicy::AcceptUnknown).await;
    let conn = conn.unwrap();
    let result = viewer_handshake(&conn, viewer_config(&me, ViewerAuth::Known)).await;
    assert_eq!(rejected(result), RejectReason::NotPaired);
    assert_eq!(ui.prompts(), 0);
    assert!(!h.agent.session_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_code_is_refused_without_asking() {
    let ui = FakeUi::allowing(perms(true, true, true), true);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn) = dial(&h, &me, PeerPolicy::AcceptUnknown).await;
    let conn = conn.unwrap();
    let wrong = ViewerAuth::Code(Zeroizing::new("AAAAAAAA".to_owned()));
    let result = viewer_handshake(&conn, viewer_config(&me, wrong)).await;
    assert!(
        matches!(
            result,
            Err(SessionError::Auth(_)) | Err(SessionError::Rejected { .. })
        ),
        "wrong code must fail"
    );
    assert_eq!(ui.prompts(), 0, "no prompt without proof");
    assert!(!h.agent.session_active());
    until("failure reported", WAIT, || {
        ui.saw(|e| matches!(e, AgentEvent::ConnectionFailed { .. }))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consent_denied_is_refused() {
    let ui = FakeUi::denying();
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, _conn, result) = connect_with_code(&h, &me).await;
    assert_eq!(rejected(result), RejectReason::ConsentDenied);
    assert_eq!(ui.prompts(), 1);
    assert!(!h.agent.session_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pin_mismatch_fails_in_tls() {
    let ui = FakeUi::allowing(perms(true, true, true), true);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let someone_else = DeviceIdentity::generate().public_key();
    let (_ep, result) = dial(&h, &me, PeerPolicy::Pinned(someone_else)).await;
    assert!(result.is_err(), "a mismatched pin must not connect");
    assert_eq!(ui.prompts(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nobody_present_refuses_everything() {
    let ui = FakeUi::allowing(perms(true, true, true), true);
    ui.present.store(false, std::sync::atomic::Ordering::SeqCst);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, _conn, result) = connect_with_code(&h, &me).await;
    assert_eq!(rejected(result), RejectReason::ConsentDenied);
    assert_eq!(ui.prompts(), 0, "no terminal means nobody to ask");
    assert!(!h.agent.session_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_peer_times_out() {
    let ui = FakeUi::allowing(perms(true, false, false), false);
    let h = host(
        ui.clone(),
        Timeouts {
            handshake: Duration::from_millis(200),
            consent: Duration::from_secs(5),
        },
    )
    .await;
    let me = DeviceIdentity::generate();
    let (_ep, conn) = dial(&h, &me, PeerPolicy::AcceptUnknown).await;
    let conn = conn.unwrap();
    tokio::time::timeout(WAIT, conn.closed())
        .await
        .expect("host must close a silent connection");
    assert_eq!(ui.prompts(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_viewer_needs_consent_to_take_over() {
    let ui = FakeUi::allowing(perms(true, false, false), false);
    let h = host_with(ui.clone()).await;
    let first = DeviceIdentity::generate();
    let (_ep1, conn1, result) = connect_with_code(&h, &first).await;
    let mut accepted1 = result.unwrap();
    begin_stream(&mut accepted1).await;
    until("first session", WAIT, || h.agent.session_active()).await;

    let second = DeviceIdentity::generate();
    let (_ep2, _conn2, result) = connect_with_code(&h, &second).await;
    result.unwrap();
    assert!(
        lock_prompts_last_takeover(&ui),
        "§5.3: taking over asks first"
    );

    let goodbye = loop {
        match accepted1.control.recv().await {
            Ok(Some(ControlBody::Bye(b))) => break b,
            Ok(Some(_)) => {}
            other => panic!("expected goodbye, got {:?}", other.map(|_| ())),
        }
    };
    assert_eq!(goodbye.reason, CloseReason::Replaced as i32);
    tokio::time::timeout(WAIT, conn1.closed())
        .await
        .expect("old session closed");
}

fn lock_prompts_last_takeover(ui: &FakeUi) -> bool {
    common::lock(&ui.asked).last().is_some_and(|r| r.takeover)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_can_end_the_session() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    begin_stream(&mut accepted).await;
    until("session", WAIT, || h.agent.session_active()).await;
    h.agent.disconnect();
    let reason = loop {
        match accepted.control.recv().await {
            Ok(Some(ControlBody::Bye(b))) => break b.reason,
            Ok(Some(_)) => {}
            other => panic!("expected goodbye, got {:?}", other.map(|_| ())),
        }
    };
    assert_eq!(reason, CloseReason::HostEnded as i32);
    tokio::time::timeout(WAIT, conn.closed())
        .await
        .expect("connection closed");
    until("session ended", WAIT, || !h.agent.session_active()).await;
    assert!(ui.saw(|e| matches!(e, AgentEvent::SessionEnded { .. })));
}

fn send_pointer(conn: &Connection, p: Pointer) {
    conn.send_datagram(Bytes::copy_from_slice(&p.to_bytes()))
        .unwrap();
}

async fn send_all_kinds_of_input(conn: &Connection) {
    let mut input = open_uni(conn, StreamType::Input).await.unwrap();
    for body in [
        InputBody::Key(KeyEvent {
            hid: 7 << 16 | 4,
            down: true,
            repeat: false,
        }),
        InputBody::Button(ButtonEvent {
            button: 0,
            down: true,
            x: 200,
            y: 100,
            display: 0,
        }),
        InputBody::Text(TextInput {
            utf8: "pwned".into(),
        }),
        InputBody::Sync(KeyStateSync {
            pressed: vec![7 << 16 | 4],
            seq: 1,
        }),
    ] {
        // The host may stop the stream, which is the point; ignore write errors.
        if input.send(&InputMsg::from(body)).await.is_err() {
            break;
        }
    }
    send_pointer(
        conn,
        Pointer {
            seq: 1,
            x: 300,
            y: 400,
            wheel_v_total: 240,
            ..Pointer::default()
        },
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_input_permission_nothing_is_injected() {
    let ui = FakeUi::allowing(perms(true, false, true), false);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    assert_eq!(accepted.accept.perms, Some(perms(true, false, true)));
    begin_stream(&mut accepted).await;

    // A viewer that ignores what SessionAccept said.
    send_all_kinds_of_input(&conn).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    send_pointer(
        &conn,
        Pointer {
            seq: 2,
            x: 1,
            y: 1,
            ..Pointer::default()
        },
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(h.agent.session_active(), "the session itself is fine");
    assert_eq!(
        h.platform.injected.all(),
        vec![],
        "input arrived at the injector without permission"
    );
    assert_eq!(
        h.platform
            .state
            .inject_sinks_created
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the injector must not even be created"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_input_permission_events_arrive_in_points_and_are_released_at_the_end() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    begin_stream(&mut accepted).await;

    send_all_kinds_of_input(&conn).await;
    until("all input injected", WAIT, || {
        h.platform.injected.all().len() >= 6
    })
    .await;
    let log = h.platform.injected.all();
    assert!(
        log.iter()
            .any(|i| matches!(i, Injected::Key { down: true, .. }))
    );
    // The display is 1920 pixels across 960 points.
    assert!(log.contains(&Injected::Button {
        button: 0,
        down: true,
        at: Some((100.0, 50.0)),
    }));
    assert!(log.contains(&Injected::Text("pwned".into())));
    assert!(log.contains(&Injected::Sync(vec![7 << 16 | 4])));
    assert!(log.contains(&Injected::Move(150.0, 200.0)));
    assert!(log.contains(&Injected::Wheel(0, 240)));
    assert_eq!(
        h.platform
            .state
            .inject_sinks_created
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    conn.close(3u32.into(), b"bye");
    until("input released", WAIT, || {
        h.platform.injected.all().contains(&Injected::ReleaseAll)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_is_withheld_when_the_os_would_refuse_it() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = host_with(ui.clone()).await;
    h.platform
        .state
        .input_unavailable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let accepted = result.unwrap();
    assert_eq!(accepted.accept.perms, Some(perms(true, false, false)));
    conn.close(3u32.into(), b"bye");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_clipboard_permission_the_pasteboard_is_never_opened() {
    let ui = FakeUi::allowing(perms(true, false, false), false);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    begin_stream(&mut accepted).await;
    let (mut send, _recv) = open_bi(&conn, StreamType::Clipboard).await.unwrap();
    let _ = send
        .send(&nexa_wire::control::KeyEvent {
            hid: 1,
            down: true,
            repeat: false,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.platform
            .state
            .clipboards_created
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    conn.close(3u32.into(), b"bye");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requested_mode_change_rebuilds_and_announces() {
    let ui = FakeUi::allowing(perms(true, false, false), false);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    begin_stream(&mut accepted).await;
    tokio::time::sleep(Duration::from_millis(600)).await; // past the rebuild rate limit
    accepted
        .control
        .send(ControlBody::Request(StreamRequest {
            mode: Mode::Game as i32,
            fps: 0,
            display: 0,
        }))
        .await
        .unwrap();
    let config = loop {
        match tokio::time::timeout(WAIT, accepted.control.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Some(ControlBody::Config(c)) if c.mode == Mode::Game as i32 => break c,
            Some(_) => {}
            None => panic!("control ended"),
        }
    };
    assert_eq!(config.fps, 60, "limited by the fake display's refresh");
    assert!(
        h.platform
            .state
            .encoders_created
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2
    );
    conn.close(3u32.into(), b"bye");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_display_change_rebuilds_and_clicks_follow_the_new_geometry() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    begin_stream(&mut accepted).await;

    *common::lock(&h.platform.state.pixels) = (1280, 720);
    h.platform
        .state
        .fire_mode_change
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let config = loop {
        match tokio::time::timeout(WAIT, accepted.control.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Some(ControlBody::Config(c)) => break c,
            Some(_) => {}
            None => panic!("control ended"),
        }
    };
    assert_eq!((config.w, config.h), (1280, 720));

    let mut input = open_uni(&conn, StreamType::Input).await.unwrap();
    input
        .send(&InputMsg::from(InputBody::Button(ButtonEvent {
            button: 0,
            down: true,
            x: 1280 / 2,
            y: 720 / 2,
            display: 0,
        })))
        .await
        .unwrap();
    until("click injected", WAIT, || {
        // The display is 960 points wide however many pixels it has.
        h.platform.injected.all().contains(&Injected::Button {
            button: 0,
            down: true,
            at: Some((480.0, 270.0)),
        })
    })
    .await;
    conn.close(3u32.into(), b"bye");
}

// ---------------------------------------------------------------------------------
// Files (§3.1 stream 4): only with the `file` permission, enforced here both ways
// ---------------------------------------------------------------------------------

fn with_file(view: bool) -> nexa_wire::control::Permissions {
    nexa_wire::control::Permissions {
        file: true,
        ..perms(view, false, false)
    }
}

type Log = Arc<std::sync::Mutex<Vec<nexa_session::transfer::TransferEvent>>>;

/// The viewer's half of file transfer, receiving into `dir`.
fn viewer_transfers(
    conn: &Connection,
    dir: &std::path::Path,
) -> (nexa_session::transfer::Transfers, Log) {
    use nexa_session::{
        bulk::BulkLimiter,
        transfer::{TransferConfig, Transfers},
    };
    let log: Log = Arc::default();
    let l = log.clone();
    let t = Transfers::new(
        conn.clone(),
        TransferConfig {
            downloads: dir.to_path_buf(),
            limits: Default::default(),
            bulk: BulkLimiter::fixed(400_000_000),
            confirm_timeout: Duration::from_secs(5),
        },
        Arc::new(move |e| l.lock().unwrap().push(e)),
    );
    let (c, accept) = (conn.clone(), t.clone());
    tokio::spawn(async move {
        while let Ok((kind, send, recv)) = nexa_session::framed::accept_bi(&c).await {
            if kind == StreamType::File {
                accept.accept(send, recv);
            }
        }
    });
    (t, log)
}

fn host_transfer_done(ui: &FakeUi, outgoing: bool) -> bool {
    use nexa_session::transfer::{Direction, TransferState};
    ui.saw(|e| {
        matches!(e, AgentEvent::Transfer { event, .. }
            if matches!(event.state, TransferState::Done { .. })
                && (event.direction == Direction::Outgoing) == outgoing)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_the_file_permission_files_go_both_ways() {
    let ui = FakeUi::allowing(with_file(true), false);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    assert!(accepted.accept.perms.as_ref().unwrap().file);
    begin_stream(&mut accepted).await;
    let dir = tempfile::tempdir().unwrap();
    let viewer_downloads = dir.path().join("viewer-downloads");
    let (transfers, log) = viewer_transfers(&conn, &viewer_downloads);

    // Viewer → host.
    let src = dir.path().join("from viewer.txt");
    std::fs::write(&src, b"hello host").unwrap();
    let id = transfers.send_file(src);
    until("received by the host", WAIT, || {
        host_transfer_done(&ui, false)
    })
    .await;
    assert_eq!(
        std::fs::read(h.downloads.join("from viewer.txt")).unwrap(),
        b"hello host"
    );
    until("sender done", WAIT, || {
        log.lock()
            .unwrap()
            .iter()
            .any(|e| e.id == id && e.state.is_final())
    })
    .await;

    // Host → viewer: what the person at the host dropped on the banner.
    let src = dir.path().join("from host.txt");
    std::fs::write(&src, b"hello viewer").unwrap();
    let ids = h.agent.send_files(vec![src]).unwrap();
    assert_eq!(ids.len(), 1);
    until("sent by the host", WAIT, || host_transfer_done(&ui, true)).await;
    until("received by the viewer", WAIT, || {
        viewer_downloads.join("from host.txt").exists()
    })
    .await;
    assert_eq!(
        std::fs::read(viewer_downloads.join("from host.txt")).unwrap(),
        b"hello viewer"
    );
    assert!(h.agent.send_files(vec!["relative.txt".into()]).is_err());
    conn.close(3u32.into(), b"bye");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_file_permission_nothing_is_written_either_way() {
    use nexa_session::transfer::{FileBody, FileChunk, FileMsg, FileOffer};
    let ui = FakeUi::allowing(perms(true, true, true), false);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    assert!(!accepted.accept.perms.as_ref().unwrap().file);
    begin_stream(&mut accepted).await;

    // A viewer that tries anyway is refused before the host reads a byte.
    let (mut send, mut recv) = open_bi(&conn, StreamType::File).await.unwrap();
    let _ = send
        .send(&FileMsg::from(FileBody::Offer(FileOffer {
            name: "x.txt".into(),
            size: 3,
        })))
        .await;
    let _ = send
        .send(&FileMsg::from(FileBody::Chunk(FileChunk {
            offset: 0,
            data: Bytes::from_static(b"abc"),
        })))
        .await;
    let answer = tokio::time::timeout(WAIT, recv.next::<FileMsg>())
        .await
        .unwrap();
    assert!(
        !matches!(answer, Ok(Some(_))),
        "refused without an answer: {answer:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !h.downloads.exists(),
        "nothing written, not even the folder"
    );

    // And the host cannot send either.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.txt");
    std::fs::write(&src, b"a").unwrap();
    assert!(h.agent.send_files(vec![src]).is_err());
    assert!(!ui.saw(|e| matches!(e, AgentEvent::Transfer { .. })));
    conn.close(3u32.into(), b"bye");
}
