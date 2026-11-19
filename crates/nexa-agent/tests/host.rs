//! The agent over loopback QUIC, driven by a hand-rolled viewer that can misbehave.

mod common;

use std::{
    sync::{Arc, atomic::Ordering::SeqCst},
    time::{Duration, Instant},
};

use bytes::Bytes;
use common::{FakeUi, Host, Injected, caps, host, perms, until};
use nexa_agent::AgentEvent;
use nexa_auth::{DeviceIdentity, PeerPolicy, client_config, tls::SERVER_NAME};
use nexa_net::vsock::bind_media_socket;
use nexa_session::{
    SessionError,
    framed::{open_bi, open_uni},
    handshake::{AuthKind, Timeouts, ViewerAccepted, ViewerAuth, ViewerConfig, viewer_handshake},
    proto::{
        CloseReason, ControlBody, InputBody, InputMsg, LINES_NATIVE, RejectReason, StreamQuality,
        StreamRequest,
    },
    terminal::{EXIT_DENIED, TermBody, TermMsg, TermSize},
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
        agent: false,
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
            ..Default::default()
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

/// The next stream config, after asking for `request`.
async fn ask(accepted: &mut ViewerAccepted, request: StreamRequest) -> StreamConfig {
    accepted
        .control
        .send(ControlBody::Request(request))
        .await
        .unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            match accepted.control.recv().await.unwrap() {
                Some(ControlBody::Config(c)) => return c,
                Some(_) => {}
                None => panic!("control stream ended"),
            }
        }
    })
    .await
    .expect("a new stream config")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quality_and_resolution_shape_the_picture() {
    let ui = FakeUi::allowing(perms(true, false, false), true);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, _conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    let first = begin_stream(&mut accepted).await;
    assert_eq!((first.w, first.h, first.fps), (1920, 1080, 60));
    let balanced = StreamRequest {
        mode: Mode::Balanced as i32,
        ..Default::default()
    };

    // A weak network: at most 720 lines and 30 fps.
    let weak = ask(
        &mut accepted,
        StreamRequest {
            quality: StreamQuality::Weak as i32,
            ..balanced.clone()
        },
    )
    .await;
    assert_eq!((weak.w, weak.h, weak.fps), (1280, 720, 30));
    // Auto fits the viewer's window.
    let fitted = ask(
        &mut accepted,
        StreamRequest {
            window_w: 1600,
            window_h: 1000,
            ..balanced.clone()
        },
    )
    .await;
    assert_eq!((fitted.w, fitted.h, fitted.fps), (1600, 900, 60));
    // Native: the host's own pixels, whatever the window.
    let native = ask(
        &mut accepted,
        StreamRequest {
            lines: LINES_NATIVE,
            window_w: 1600,
            window_h: 1000,
            ..balanced.clone()
        },
    )
    .await;
    assert_eq!((native.w, native.h), (1920, 1080));
}

/// Opens a terminal stream and reads what comes back until `done` says so.
async fn terminal_until(
    conn: &Connection,
    keys: &[u8],
    done: impl Fn(&str, Option<&str>) -> bool,
) -> (String, Option<String>) {
    let (mut send, mut recv) = open_bi(conn, StreamType::Terminal).await.unwrap();
    send.send(&TermMsg::from(TermBody::Open(TermSize {
        cols: 80,
        rows: 24,
    })))
    .await
    .unwrap();
    send.send(&TermMsg::from(TermBody::Data(keys.to_vec())))
        .await
        .unwrap();
    let (mut out, mut exit) = (String::new(), None);
    tokio::time::timeout(WAIT, async {
        while !done(&out, exit.as_deref()) {
            match recv.next::<TermMsg>().await {
                Ok(Some(TermMsg {
                    body: Some(TermBody::Data(d)),
                })) => out.push_str(&String::from_utf8_lossy(&d)),
                Ok(Some(TermMsg {
                    body: Some(TermBody::Exit(why)),
                })) => exit = Some(why),
                Ok(Some(_)) => {}
                _ => break,
            }
        }
    })
    .await
    .expect("terminal answered");
    (out, exit)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_needs_its_permission() {
    let ui = FakeUi::allowing(perms(true, true, false), true);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    begin_stream(&mut accepted).await;
    let (out, exit) = terminal_until(&conn, b"echo nope\r", |_, e| e.is_some()).await;
    assert_eq!(exit.as_deref(), Some(EXIT_DENIED));
    assert!(out.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_granted_terminal_runs_a_shell() {
    let granted = nexa_wire::control::Permissions {
        terminal: true,
        ..perms(true, true, false)
    };
    let ui = FakeUi::allowing(granted, true);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn, result) = connect_with_code(&h, &me).await;
    let mut accepted = result.unwrap();
    begin_stream(&mut accepted).await;
    assert!(accepted.accept.perms.as_ref().is_some_and(|p| p.terminal));
    // The answer is computed, so it cannot be the echo of what was typed.
    let (out, _) = terminal_until(&conn, b"echo $((6*7))nexa\r", |o, e| {
        o.contains("42nexa") || e.is_some()
    })
    .await;
    assert!(out.contains("42nexa"), "shell output: {out:?}");
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
async fn the_fixed_password_lets_in_without_asking_and_the_code_still_asks() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = common::host_with_password(ui.clone(), timeouts(), "correct horse battery").await;
    assert!(h.agent.has_password());

    // The right password: in without anyone clicking Allow, and still shown as a session.
    let me = DeviceIdentity::generate();
    let (_ep, conn) = dial(&h, &me, PeerPolicy::AcceptUnknown).await;
    let auth = ViewerAuth::Password(Zeroizing::new("correct horse battery".to_owned()));
    let accepted = viewer_handshake(&conn.unwrap(), viewer_config(&me, auth)).await;
    assert!(accepted.is_ok(), "{:?}", accepted.err());
    assert_eq!(
        ui.prompts(),
        0,
        "the password is the owner's: nobody is asked"
    );
    // And it may do everything: a view-only owner cannot use the mouse.
    let granted = accepted.as_ref().unwrap().accept.perms.clone().unwrap();
    assert!(
        granted.view
            && granted.input
            && granted.clipboard
            && granted.file
            && granted.audio
            && granted.terminal,
        "{granted:?}"
    );
    until("session running", WAIT, || h.agent.session_active()).await;
    drop(accepted);
    until("session over", WAIT, || !h.agent.session_active()).await;

    // A wrong one: refused without asking.
    let other = DeviceIdentity::generate();
    let (_ep2, conn) = dial(&h, &other, PeerPolicy::AcceptUnknown).await;
    let auth = ViewerAuth::Password(Zeroizing::new("wrong horse battery".to_owned()));
    let result = viewer_handshake(&conn.unwrap(), viewer_config(&other, auth)).await;
    assert!(result.is_err(), "a wrong password must fail");
    assert_eq!(ui.prompts(), 0, "no prompt without proof");

    // The one-time code still works too, and asks.
    let third = DeviceIdentity::generate();
    let (_ep3, _conn, result) = connect_with_code(&h, &third).await;
    assert!(result.is_ok(), "{:?}", result.err());
    assert_eq!(ui.prompts(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_without_a_secret_asks_the_person_and_says_it_is_unverified() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = host_with(ui.clone()).await;
    let me = DeviceIdentity::generate();
    let (_ep, conn) = dial(&h, &me, PeerPolicy::AcceptUnknown).await;
    let accepted = viewer_handshake(&conn.unwrap(), viewer_config(&me, ViewerAuth::Request)).await;
    assert!(accepted.is_ok(), "{:?}", accepted.err());
    assert_eq!(ui.prompts(), 1);
    {
        let asked = ui.asked.lock().unwrap();
        assert_eq!(asked[0].auth, AuthKind::Request);
        assert!(
            !asked[0].prompt.offer_save,
            "an unverified device is never saved"
        );
    }
    drop(accepted);
    until("session over", WAIT, || !h.agent.session_active()).await;

    // Turned down: refused.
    let no = FakeUi::denying();
    let h = host_with(no.clone()).await;
    let other = DeviceIdentity::generate();
    let (_ep2, conn) = dial(&h, &other, PeerPolicy::AcceptUnknown).await;
    let result = viewer_handshake(&conn.unwrap(), viewer_config(&other, ViewerAuth::Request)).await;
    assert!(result.is_err());
    assert_eq!(no.prompts(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_password_set_password_login_is_refused() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = host_with(ui.clone()).await;
    assert!(!h.agent.has_password());
    let me = DeviceIdentity::generate();
    let (_ep, conn) = dial(&h, &me, PeerPolicy::AcceptUnknown).await;
    let auth = ViewerAuth::Password(Zeroizing::new("anything goes".to_owned()));
    let result = viewer_handshake(&conn.unwrap(), viewer_config(&me, auth)).await;
    assert!(result.is_err());
    assert_eq!(ui.prompts(), 0);
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
async fn several_viewers_share_the_host_and_leave_one_at_a_time() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = host_with(ui.clone()).await;
    let first = DeviceIdentity::generate();
    let (_ep1, conn1, result) = connect_with_code(&h, &first).await;
    let mut accepted1 = result.unwrap();
    begin_stream(&mut accepted1).await;
    until("first session", WAIT, || {
        h.agent.active_sessions().len() == 1
    })
    .await;

    // A second person joins: asked about on their own, nobody is taken over.
    let second = DeviceIdentity::generate();
    let (_ep2, conn2, result) = connect_with_code(&h, &second).await;
    let mut accepted2 = result.unwrap();
    begin_stream(&mut accepted2).await;
    assert_eq!(ui.prompts(), 2);
    assert!(
        !common::lock(&ui.asked).iter().any(|r| r.takeover),
        "joining is not taking over"
    );
    until("both sessions", WAIT, || {
        h.agent.active_sessions().len() == 2
    })
    .await;
    assert!(conn1.close_reason().is_none(), "the first is still there");

    // The first leaves: only its session ends, at once.
    conn1.close(0u32.into(), b"bye");
    until("first gone", WAIT, || h.agent.active_sessions().len() == 1).await;
    assert!(conn2.close_reason().is_none());

    // The host ends the second by its id.
    let id = h.agent.active_sessions()[0].id;
    h.agent.disconnect_session(id);
    let goodbye = loop {
        match accepted2.control.recv().await {
            Ok(Some(ControlBody::Bye(b))) => break b,
            Ok(Some(_)) => {}
            other => panic!("expected goodbye, got {:?}", other.map(|_| ())),
        }
    };
    assert_eq!(goodbye.reason, CloseReason::HostEnded as i32);
    until("none left", WAIT, || !h.agent.session_active()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_more_than_four_people_at_once() {
    let ui = FakeUi::allowing(perms(true, false, false), false);
    let h = host_with(ui.clone()).await;
    let mut kept = Vec::new();
    for _ in 0..4 {
        let me = DeviceIdentity::generate();
        let (ep, conn, result) = connect_with_code(&h, &me).await;
        let mut accepted = result.unwrap();
        begin_stream(&mut accepted).await;
        kept.push((ep, conn, accepted));
    }
    until("four sessions", WAIT, || {
        h.agent.active_sessions().len() == 4
    })
    .await;
    let fifth = DeviceIdentity::generate();
    let (_ep, _conn, result) = connect_with_code(&h, &fifth).await;
    assert!(result.is_err(), "a fifth person is turned away");
    assert_eq!(ui.prompts(), 4, "without asking anyone");
}

/// What Windows hosts did with DXGI desktop duplication, one per display per process:
/// each viewer's stream opened its own, and the second viewer's session ended at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_capture_per_display_unshared_ends_the_second_viewer() {
    let ui = FakeUi::allowing(perms(true, false, false), false);
    let h = host_with(ui.clone()).await;
    h.platform.state.exclusive_capture.store(true, SeqCst);
    let first = DeviceIdentity::generate();
    let (_ep1, conn1, result) = connect_with_code(&h, &first).await;
    let mut accepted1 = result.unwrap();
    begin_stream(&mut accepted1).await;

    let second = DeviceIdentity::generate();
    let (_ep2, conn2, result) = connect_with_code(&h, &second).await;
    let mut accepted2 = result.unwrap();
    accepted2
        .control
        .send(ControlBody::Request(StreamRequest {
            mode: Mode::Balanced as i32,
            ..Default::default()
        }))
        .await
        .unwrap();
    match tokio::time::timeout(WAIT, conn2.closed())
        .await
        .expect("the second session ends")
    {
        quinn::ConnectionError::ApplicationClosed(c) => {
            assert_eq!(&c.reason[..], b"capture failed")
        }
        other => panic!("expected the host to end it, got {other}"),
    }
    assert!(conn1.close_reason().is_none(), "the first goes on");
}

/// As Windows hosts do now: every viewer of a display reads its one capture, each
/// with a stream of its own, and one rebuilding its stream does not disturb the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewers_of_one_display_share_its_one_capture() {
    let ui = FakeUi::allowing(perms(true, false, false), false);
    let h = host_with(ui.clone()).await;
    let state = h.platform.state.clone();
    state.exclusive_capture.store(true, SeqCst);
    state.shared_capture.store(true, SeqCst);
    let pictures = |stream: usize| {
        common::lock(&state.stream_pictures)
            .get(stream)
            .map_or(0, |n| n.load(SeqCst))
    };
    let mut viewers = Vec::new();
    for _ in 0..2 {
        let me = DeviceIdentity::generate();
        let (ep, conn, result) = connect_with_code(&h, &me).await;
        let mut accepted = result.unwrap();
        begin_stream(&mut accepted).await;
        viewers.push((ep, conn, accepted));
    }
    until("both streams get pictures", WAIT, || {
        pictures(0) > 5 && pictures(1) > 5
    })
    .await;
    assert_eq!(state.capturers_live.load(SeqCst), 1, "one capture for both");
    assert_eq!(h.agent.active_sessions().len(), 2);

    // The second switches to game mode: its stream is rebuilt, old stages first.
    tokio::time::sleep(Duration::from_millis(600)).await; // past the rebuild rate limit
    let accepted = &mut viewers[1].2;
    accepted
        .control
        .send(ControlBody::Request(StreamRequest {
            mode: Mode::Game as i32,
            ..Default::default()
        }))
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(WAIT, accepted.control.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Some(ControlBody::Config(c)) if c.mode == Mode::Game as i32 => break,
            Some(_) => {}
            None => panic!("control ended"),
        }
    }
    let before = pictures(0);
    until("the rebuilt stream gets pictures", WAIT, || pictures(2) > 5).await;
    assert!(pictures(0) > before, "the first viewer's stream went on");
    assert_eq!(state.capturers_live.load(SeqCst), 1);
    assert_eq!(h.agent.active_sessions().len(), 2);

    // Once both have gone, so has the capture.
    for (_, conn, _) in &viewers {
        conn.close(0u32.into(), b"bye");
    }
    until("the capture let go", WAIT, || {
        state.capturers_live.load(SeqCst) == 0
    })
    .await;
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
    // As on Windows: the old capture must be gone before the new one starts.
    h.platform
        .state
        .exclusive_capture
        .store(true, std::sync::atomic::Ordering::SeqCst);
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
            ..Default::default()
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
    // As on Windows: the old capture must be gone before the new one starts.
    h.platform
        .state
        .exclusive_capture
        .store(true, std::sync::atomic::Ordering::SeqCst);
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

// ---------------------------------------------------------------------------------
// AI agents (docs/MCP.md §4)
// ---------------------------------------------------------------------------------

fn agent_config(me: &DeviceIdentity, auth: ViewerAuth) -> ViewerConfig {
    ViewerConfig {
        agent: true,
        ..viewer_config(me, auth)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_are_refused_unless_the_host_allows_them() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = host_with(ui.clone()).await;
    assert!(!h.agent.allow_agents(), "off when the agent starts");
    let me = DeviceIdentity::generate();
    let (_ep, conn) = dial(&h, &me, PeerPolicy::AcceptUnknown).await;
    let result = viewer_handshake(&conn.unwrap(), agent_config(&me, code_of(&h))).await;
    match result {
        Err(SessionError::Rejected { reason, message }) => {
            assert_eq!(reason, RejectReason::ConsentDenied);
            assert!(message.contains("AI agents"), "{message}");
        }
        Err(e) => panic!("expected a refusal, got {e}"),
        Ok(_) => panic!("an agent got in while agents are off"),
    }
    assert_eq!(
        ui.prompts(),
        0,
        "nobody is asked about an agent that may not come"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_allowed_agent_is_put_to_the_person_as_one_and_acknowledged() {
    let ui = FakeUi::allowing(perms(true, true, false), true);
    let h = host_with(ui.clone()).await;
    h.agent.set_allow_agents(true);
    let me = DeviceIdentity::generate();
    let (_ep, conn) = dial(&h, &me, PeerPolicy::AcceptUnknown).await;
    let mut accepted = viewer_handshake(&conn.unwrap(), agent_config(&me, code_of(&h)))
        .await
        .unwrap();
    assert!(accepted.accept.agent_ack);
    let asked = common::lock(&ui.asked)[0].clone();
    assert!(asked.agent);
    let proposed = &asked.prompt.perms;
    assert!(proposed.view && proposed.input, "{proposed:?}");
    assert!(
        !proposed.clipboard && !proposed.file && !proposed.audio && !proposed.terminal,
        "{proposed:?}"
    );
    assert!(!asked.prompt.offer_skip_next_time);
    begin_stream(&mut accepted).await;
    until("the session is shown as an agent's", WAIT, || {
        common::lock(&ui.events)
            .iter()
            .any(|e| matches!(e, AgentEvent::SessionStarted(s) if s.agent))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_persons_app_is_not_acknowledged_as_an_agent() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = host_with(ui.clone()).await;
    h.agent.set_allow_agents(true);
    let me = DeviceIdentity::generate();
    let (_ep, _conn, result) = connect_with_code(&h, &me).await;
    let accepted = result.unwrap();
    assert!(!accepted.accept.agent_ack);
    assert!(!common::lock(&ui.asked)[0].agent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fixed_password_does_not_let_an_agent_in_unasked() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = common::host_with_password(ui.clone(), timeouts(), "correct horse battery").await;
    h.agent.set_allow_agents(true);
    let me = DeviceIdentity::generate();
    let (_ep, conn) = dial(&h, &me, PeerPolicy::AcceptUnknown).await;
    let auth = ViewerAuth::Password(Zeroizing::new("correct horse battery".to_owned()));
    let accepted = viewer_handshake(&conn.unwrap(), agent_config(&me, auth))
        .await
        .unwrap();
    assert_eq!(ui.prompts(), 1, "an agent is always put to the person");
    let granted = accepted.accept.perms.unwrap();
    assert!(!granted.terminal, "not the owner's everything: {granted:?}");
}

// ---------------------------------------------------------------------------------
// An agent's session: no video, looks on request (docs/MCP.md §13)
// ---------------------------------------------------------------------------------

use nexa_session::{
    agent::{ActReply, ActRequest, Action, AgentBody, AgentMsg, ObserveReply, ObserveRequest},
    proto::InputMark,
};

async fn agent_session(h: &Host) -> (Endpoint, Connection, ViewerAccepted) {
    h.agent.set_allow_agents(true);
    let me = DeviceIdentity::generate();
    let (ep, conn) = dial(h, &me, PeerPolicy::AcceptUnknown).await;
    let conn = conn.unwrap();
    let accepted = viewer_handshake(&conn, agent_config(&me, code_of(h)))
        .await
        .unwrap();
    (ep, conn, accepted)
}

/// The first `StreamConfig` of the session.
async fn first_config(accepted: &mut ViewerAccepted) -> StreamConfig {
    loop {
        match tokio::time::timeout(WAIT, accepted.control.recv()).await {
            Ok(Ok(Some(ControlBody::Config(c)))) => return c,
            Ok(Ok(Some(_))) => {}
            other => panic!("no stream config: {:?}", other.map(|r| r.map(|_| ()))),
        }
    }
}

enum Answer {
    Look(Box<ObserveReply>, Vec<u8>),
    Refused(String),
}

async fn look(conn: &Connection, request: ObserveRequest) -> Answer {
    let (mut send, mut recv) = open_bi(conn, StreamType::Agent).await.unwrap();
    send.send(&AgentMsg::from(AgentBody::Observe(request)))
        .await
        .unwrap();
    let first = tokio::time::timeout(WAIT, recv.next::<AgentMsg>())
        .await
        .expect("an answer")
        .unwrap();
    let reply = match first.and_then(|m| m.body) {
        Some(AgentBody::Reply(r)) => r,
        Some(AgentBody::Refused(why)) => return Answer::Refused(why),
        other => panic!("unexpected {other:?}"),
    };
    let mut png = Vec::new();
    while png.len() < reply.png_len as usize {
        match recv.next::<AgentMsg>().await.unwrap().and_then(|m| m.body) {
            Some(AgentBody::Png(p)) => png.extend_from_slice(&p),
            other => panic!("unexpected {other:?}"),
        }
    }
    Answer::Look(Box::new(reply), png)
}

fn ui_look() -> ObserveRequest {
    ObserveRequest {
        ui: true,
        interactive_only: true,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agents_session_has_no_video_and_names_its_pixels() {
    let ui = FakeUi::allowing(perms(true, true, false), false);
    let h = host_with(ui.clone()).await;
    let (_ep, _conn, mut accepted) = agent_session(&h).await;
    let config = first_config(&mut accepted).await;
    assert_eq!(config.codec, nexa_wire::control::Codec::Unknown as i32);
    // The fake's 1920×1080 pixels, scaled to a longer side of 1568.
    assert_eq!((config.w, config.h, config.fps), (1568, 882, 0));
    until("the session is shown", WAIT, || {
        common::lock(&ui.events)
            .iter()
            .any(|e| matches!(e, AgentEvent::SessionStarted(s) if s.agent))
    })
    .await;
    assert_eq!(
        h.platform.state.encoders_created.load(SeqCst),
        0,
        "no encoder"
    );
    assert_eq!(
        h.platform.state.capturers_live.load(SeqCst),
        0,
        "no capture"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_look_gives_elements_with_refs_and_a_lossless_screenshot() {
    let h = host_with(FakeUi::allowing(perms(true, true, false), false)).await;
    let (_ep, conn, mut accepted) = agent_session(&h).await;
    first_config(&mut accepted).await;
    let Answer::Look(reply, png) = look(
        &conn,
        ObserveRequest {
            screenshot: true,
            ..ui_look()
        },
    )
    .await
    else {
        panic!("refused")
    };
    assert_eq!((reply.w, reply.h), (1568, 882));
    assert_eq!(reply.app, "Fake app");
    assert_eq!(reply.focused, "textfield \"Name\"");
    assert_eq!((reply.pointer_x, reply.pointer_y), (784, 441));
    // 100,100 100×40 points of 960×540, in 1568×882 pixels.
    assert_eq!(
        reply.ui,
        "window \"Fake window\" (0,0 1568×882)\n\
         \x20\x20@e1 button \"OK\" (163,163 163×65)\n\
         \x20\x20@e2 textfield \"Name\" value=\"\" (163,327 327×49)\n"
    );
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 1568);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 882);

    h.platform.view.unreadable.store(true, SeqCst);
    let Answer::Look(reply, png) = look(&conn, ui_look()).await else {
        panic!("refused")
    };
    assert!(reply.ui.is_empty());
    assert_eq!(reply.ui_error, "this app shows no elements");
    assert!(png.is_empty(), "no screenshot unless asked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_look_waits_for_the_agents_last_action() {
    let h = host_with(FakeUi::allowing(perms(true, true, false), false)).await;
    let (_ep, conn, mut accepted) = agent_session(&h).await;
    first_config(&mut accepted).await;
    let mut input = open_uni(&conn, StreamType::Input).await.unwrap();
    for body in [
        InputBody::Key(KeyEvent {
            hid: 7 << 16 | 4,
            down: true,
            repeat: false,
        }),
        InputBody::Key(KeyEvent {
            hid: 7 << 16 | 4,
            down: false,
            repeat: false,
        }),
        InputBody::Mark(InputMark { id: 1 }),
    ] {
        input.send(&InputMsg::from(body)).await.unwrap();
    }
    let Answer::Look(reply, _) = look(
        &conn,
        ObserveRequest {
            after_mark: 1,
            max_ms: 3000,
            ..Default::default()
        },
    )
    .await
    else {
        panic!("refused")
    };
    assert_eq!(
        h.platform.injected.keys().len(),
        2,
        "both keys were in before the look answered"
    );
    assert!(reply.settled, "nothing asked to settle");

    // A mark that never comes holds the look only as long as it may wait.
    let started = Instant::now();
    let Answer::Look(..) = look(
        &conn,
        ObserveRequest {
            after_mark: 99,
            max_ms: 300,
            ..Default::default()
        },
    )
    .await
    else {
        panic!("refused")
    };
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_look_waits_for_the_screen_to_settle_and_says_it_changed() {
    let h = host_with(FakeUi::allowing(perms(true, true, false), false)).await;
    let (_ep, conn, mut accepted) = agent_session(&h).await;
    first_config(&mut accepted).await;
    // A first look, for the next to compare with.
    let Answer::Look(..) = look(&conn, ObserveRequest::default()).await else {
        panic!("refused")
    };
    let view = h.platform.view.clone();
    let changer = std::thread::spawn(move || {
        for i in 0..10u32 {
            view.shade.store(40 + i * 20, SeqCst);
            std::thread::sleep(Duration::from_millis(40));
        }
    });
    let started = Instant::now();
    let Answer::Look(reply, _) = look(
        &conn,
        ObserveRequest {
            quiet_ms: 250,
            max_ms: 3000,
            ..Default::default()
        },
    )
    .await
    else {
        panic!("refused")
    };
    changer.join().unwrap();
    assert!(reply.settled);
    assert!(reply.changed);
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(500), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    // Still now: the same picture as the last look.
    let Answer::Look(again, _) = look(
        &conn,
        ObserveRequest {
            quiet_ms: 100,
            max_ms: 1000,
            ..Default::default()
        },
    )
    .await
    else {
        panic!("refused")
    };
    assert!(again.settled && !again.changed);
    // Typing into the focused field: the picture is the same, the field's text is not.
    *h.platform.view.typed.lock().unwrap() = "x".into();
    let Answer::Look(typed, _) = look(
        &conn,
        ObserveRequest {
            quiet_ms: 100,
            max_ms: 1000,
            ..Default::default()
        },
    )
    .await
    else {
        panic!("refused")
    };
    assert!(typed.settled && typed.changed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_that_cannot_show_an_agent_its_screen_says_so() {
    let h = host_with(FakeUi::allowing(perms(true, true, false), false)).await;
    h.platform.view.absent.store(true, SeqCst);
    let (_ep, conn, mut accepted) = agent_session(&h).await;
    first_config(&mut accepted).await;
    match look(&conn, ui_look()).await {
        Answer::Refused(why) => assert_eq!(why, nexa_session::agent::REFUSED_UNSUPPORTED),
        Answer::Look(..) => panic!("a host without a view answered a look"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agents_session_keeps_the_display_awake_until_it_ends() {
    let h = host_with(FakeUi::allowing(perms(true, true, false), false)).await;
    let (_ep, conn, mut accepted) = agent_session(&h).await;
    first_config(&mut accepted).await;
    // Nothing is captured between looks: only the session keeps the display on.
    assert_eq!(h.platform.view.awake.load(SeqCst), 1);
    conn.close(0u32.into(), b"done");
    until("the display released", Duration::from_secs(5), || {
        h.platform.view.awake.load(SeqCst) == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_screen_that_cannot_be_watched_says_why() {
    let h = host_with(FakeUi::allowing(perms(true, true, false), false)).await;
    let (_ep, conn, mut accepted) = agent_session(&h).await;
    first_config(&mut accepted).await;
    h.platform.view.blind.store(true, SeqCst);
    let Answer::Look(reply, _) = look(
        &conn,
        ObserveRequest {
            quiet_ms: 100,
            max_ms: 1000,
            ..Default::default()
        },
    )
    .await
    else {
        panic!("refused")
    };
    assert_eq!(reply.watch_error, "screen recording is not allowed");
    assert!(!reply.settled && !reply.changed);
    assert!(reply.waited_ms < 500, "{}", reply.waited_ms);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_look_gives_each_refs_centre() {
    let h = host_with(FakeUi::allowing(perms(true, true, false), false)).await;
    let (_ep, conn, mut accepted) = agent_session(&h).await;
    first_config(&mut accepted).await;
    let Answer::Look(reply, _) = look(&conn, ui_look()).await else {
        panic!("refused")
    };
    // OK at (163,163 163×65) and Name at (163,327 327×49), as the text has them.
    let centres: Vec<(i32, i32)> = reply.refs.iter().map(|r| (r.x, r.y)).collect();
    assert_eq!(centres, vec![(244, 195), (326, 351)]);
}

/// An action on a ref: `Ok` when done, the host's reason otherwise.
async fn act(conn: &Connection, request: ActRequest) -> Result<(), String> {
    let (mut send, mut recv) = open_bi(conn, StreamType::Agent).await.unwrap();
    send.send(&AgentMsg::from(AgentBody::Act(request)))
        .await
        .unwrap();
    match tokio::time::timeout(WAIT, recv.next::<AgentMsg>())
        .await
        .expect("an answer")
        .unwrap()
        .and_then(|m| m.body)
    {
        Some(AgentBody::Acted(ActReply { error })) if error.is_empty() => Ok(()),
        Some(AgentBody::Acted(ActReply { error })) => Err(error),
        Some(AgentBody::Refused(why)) => Err(why),
        other => panic!("unexpected {other:?}"),
    }
}

fn press(tree: u64, r: u32) -> ActRequest {
    ActRequest {
        tree,
        r#ref: r,
        action: Action::Press as i32,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refs_are_acted_on_without_the_pointer_while_their_tree_is_the_last() {
    let h = host_with(FakeUi::allowing(perms(true, true, false), false)).await;
    let (_ep, conn, mut accepted) = agent_session(&h).await;
    first_config(&mut accepted).await;
    let Answer::Look(first, _) = look(&conn, ui_look()).await else {
        panic!("refused")
    };
    assert_ne!(first.tree, 0);
    act(&conn, press(first.tree, 1)).await.unwrap();
    act(
        &conn,
        ActRequest {
            tree: first.tree,
            r#ref: 2,
            action: Action::SetValue as i32,
            text: "hello".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        *h.platform.view.acted.lock().unwrap(),
        vec!["OK press".to_owned(), "Name set hello".to_owned()]
    );
    // What the platform says no to comes back as it said it.
    let e = act(
        &conn,
        ActRequest {
            tree: first.tree,
            r#ref: 1,
            action: Action::SetValue as i32,
            text: "x".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(e, "a button has no value");
    assert!(
        act(&conn, press(first.tree, 3))
            .await
            .unwrap_err()
            .contains("no @e3")
    );
    // A newer tree: the first one's refs no longer hold.
    let Answer::Look(second, _) = look(&conn, ui_look()).await else {
        panic!("refused")
    };
    assert!(second.tree > first.tree);
    let e = act(&conn, press(first.tree, 1)).await.unwrap_err();
    assert!(e.contains("call nexa_ui again"), "{e}");
    // No pointer moved for any of it.
    assert!(h.platform.injected.all().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acting_needs_the_keyboard_and_mouse_permission() {
    let h = host_with(FakeUi::allowing(perms(true, false, false), false)).await;
    let (_ep, conn, mut accepted) = agent_session(&h).await;
    first_config(&mut accepted).await;
    let Answer::Look(reply, _) = look(&conn, ui_look()).await else {
        panic!("refused")
    };
    let e = act(&conn, press(reply.tree, 1)).await.unwrap_err();
    assert_eq!(e, "the host did not allow keyboard and mouse");
    assert!(h.platform.view.acted.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marked_screenshot_boxes_and_numbers_the_refs() {
    let h = host_with(FakeUi::allowing(perms(true, true, false), false)).await;
    let (_ep, conn, mut accepted) = agent_session(&h).await;
    first_config(&mut accepted).await;
    let plain = ObserveRequest {
        screenshot: true,
        ..Default::default()
    };
    let Answer::Look(_, unmarked) = look(&conn, plain).await else {
        panic!("refused")
    };
    let Answer::Look(reply, marked) = look(
        &conn,
        ObserveRequest {
            marks: true,
            ..plain
        },
    )
    .await
    else {
        panic!("refused")
    };
    // The tree comes with it, and its refs are the ones marked.
    assert_eq!(reply.refs.len(), 2);
    assert!(reply.ui.contains("@e1 button \"OK\""), "{}", reply.ui);
    let pixels = |png: &[u8]| {
        let decoder = png::Decoder::new(std::io::Cursor::new(png.to_vec()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut buf).unwrap();
        buf
    };
    let (a, b) = (pixels(&unmarked), pixels(&marked));
    // OK at (163,163 163×65): its box's left edge is drawn, its middle is not.
    let at = |buf: &[u8], x: usize, y: usize| {
        let i = (y * 1568 + x) * 3;
        [buf[i], buf[i + 1], buf[i + 2]]
    };
    assert_ne!(at(&a, 163, 190), at(&b, 163, 190));
    assert_eq!(at(&a, 244, 195), at(&b, 244, 195));
}
