//! The real agent and IPC server with fake devices, for the desktop UI's cross-language
//! test (ui/NexaDesk.Tests/CrossLanguageTests.cs). Nothing here touches the screen, the
//! Keychain, the pasteboard or the input system.
//!
//! Usage: `ipc_fake_host <dir>`. Prints `READY`, waits for a UI to attach, then has a
//! viewer connect with the current code and prints what happened to it:
//! `VIEWER accepted ...`, `VIEWER rejected ...`, then `VIEWER bye ...` when the
//! session ends. Exits when the UI goes away or after 30 s.

#[path = "../tests/common/mod.rs"]
mod common;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use common::{FakePlatform, caps};
use nexa_agent::{AgentConfig, ipc::IpcConfig, start_with_ui};
use nexa_auth::{DeviceIdentity, PairingStore, PeerPolicy, client_config, tls::SERVER_NAME};
use nexa_net::vsock::bind_media_socket;
use nexa_session::{
    SessionError,
    handshake::{Timeouts, ViewerAuth, ViewerConfig, viewer_handshake},
    proto::{CloseReason, ControlBody, StreamRequest},
};
use nexa_wire::control::{Mode, Os};
use quinn::{Endpoint, EndpointConfig, TokioRuntime};
use zeroize::Zeroizing;

async fn wait(mut f: impl FnMut() -> bool, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while !f() {
        if Instant::now() > end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let dir =
        std::path::PathBuf::from(std::env::args().nth(1).expect("usage: ipc_fake_host <dir>"));
    let identity = DeviceIdentity::generate();
    let store = PairingStore::open(dir.join("pairings.json"), identity.store_mac_key()).unwrap();
    let timeouts = Timeouts {
        handshake: Duration::from_secs(5),
        consent: Duration::from_secs(20),
    };
    let host = start_with_ui(
        AgentConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            device_name: "fake host".into(),
            os: Os::Macos,
            caps: caps(),
            timeouts,
            signal: None,
        },
        identity,
        store,
        Arc::new(FakePlatform::default()),
        None,
        IpcConfig::in_dir(&dir),
        Arc::new(|| {}),
    )
    .await
    .unwrap();
    println!("READY");

    if !wait(|| host.hub.ui_attached(), Duration::from_secs(30)).await {
        println!("NO UI");
        return;
    }

    let me = DeviceIdentity::generate();
    let socket = bind_media_socket("127.0.0.1:0".parse().unwrap()).unwrap();
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        None,
        socket,
        Arc::new(TokioRuntime),
    )
    .unwrap();
    let conn = endpoint
        .connect_with(
            client_config(&me, PeerPolicy::AcceptUnknown).unwrap(),
            host.agent.local_addr(),
            SERVER_NAME,
        )
        .unwrap()
        .await
        .unwrap();
    let result = viewer_handshake(
        &conn,
        ViewerConfig {
            me: me.public_key(),
            device_name: "cross-language viewer".into(),
            os: Os::Macos,
            caps: caps(),
            auth: ViewerAuth::Code(Zeroizing::new(host.agent.current_code().to_string())),
            timeouts,
        },
    )
    .await;
    let mut accepted = match result {
        Ok(a) => a,
        Err(SessionError::Rejected { reason, .. }) => {
            println!("VIEWER rejected {reason:?}");
            return;
        }
        Err(e) => {
            println!("VIEWER error {e}");
            return;
        }
    };
    let p = accepted.accept.perms.clone().unwrap_or_default();
    println!(
        "VIEWER accepted view={} input={} clipboard={}",
        p.view, p.input, p.clipboard
    );
    accepted
        .control
        .send(ControlBody::Request(StreamRequest {
            mode: Mode::Balanced as i32,
            fps: 0,
        }))
        .await
        .unwrap();
    let deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            msg = accepted.control.recv() => match msg {
                Ok(Some(ControlBody::Bye(b))) => {
                    let reason = CloseReason::try_from(b.reason).unwrap_or(CloseReason::Unspecified);
                    println!("VIEWER bye {reason:?}");
                    break;
                }
                Ok(Some(_)) => {}
                _ => {
                    println!("VIEWER closed");
                    break;
                }
            },
            _ = &mut deadline => {
                println!("VIEWER timeout");
                break;
            }
        }
    }
    wait(|| !host.hub.ui_attached(), Duration::from_secs(10)).await;
}
