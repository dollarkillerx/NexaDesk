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
