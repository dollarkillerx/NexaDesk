//! The MCP server against a real agent over loopback QUIC, driven by JSON-RPC
//! messages as an MCP client sends them. Fake devices on both sides: nothing here
//! touches the screen, keyboard, pasteboard or Keychain of this machine.

#[path = "../../nexa-agent/tests/common/mod.rs"]
mod common;

use std::{
    ffi::c_void,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::Engine;
use common::{FakeUi, Host, Injected, caps, host, lock, perms, until};
use nexa_auth::{FileKeyStore, KeyStore};
use nexa_client::{
    deps::{Deps, KeySource, KeySourceError},
    input::InputCmd,
};
use nexa_input::Hid;
use nexa_mcp::{server::Server, viewer::Viewer};
use nexa_media::{CpuFrame, DecodedFrame, Decoder, FrameMeta, NativeImage, PresentMode, Presenter};
use nexa_session::{clip::ClipPlatform, handshake::Timeouts};
use nexa_wire::control::{Caps, Codec, Os};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(10);
/// What the fake decoder hands out: the screenshots' size.
const FRAME: (u32, u32) = (784, 441);

struct TestDeps {
    dir: std::path::PathBuf,
    clipboard: Arc<nexa_client::memclip::MemoryClipboard>,
}

struct NoKeys;

impl KeySource for NoKeys {
    fn set_active(&mut self, _: bool) -> Result<(), KeySourceError> {
        Ok(())
    }
}

/// A mid-gray picture of [`FRAME`], whatever arrives.
struct GrayDecoder;

impl Decoder for GrayDecoder {
    fn decode(&mut self, _: &[u8], meta: FrameMeta) -> nexa_media::Result<DecodedFrame> {
        let (w, h) = FRAME;
        Ok(DecodedFrame {
            image: NativeImage::Cpu(Arc::new(CpuFrame {
                width: w,
                height: h,
                y: vec![126; (w * h) as usize],
                uv: vec![128; (w.div_ceil(2) * 2 * h.div_ceil(2)) as usize],
            })),
            meta,
        })
    }
}

impl Deps for TestDeps {
    fn os(&self) -> Os {
        Os::Macos
    }
    fn key_store(&self) -> Box<dyn KeyStore> {
        Box::new(FileKeyStore::new(self.dir.join("device.key")))
    }
    fn caps(&self) -> Caps {
        caps()
    }
    fn decoder(&self, _: Codec) -> Result<Box<dyn Decoder>, String> {
        Ok(Box::new(GrayDecoder))
    }
    fn key_source(&self, _: std::sync::mpsc::Sender<InputCmd>) -> Box<dyn KeySource> {
        Box::new(NoKeys)
    }
    fn rewrites_keys(&self) -> bool {
        false
    }
    fn is_agent(&self) -> bool {
        true
    }
    fn fallback_key_source(
        &self,
        _: std::sync::mpsc::Sender<InputCmd>,
    ) -> Option<Box<dyn KeySource>> {
        None
    }
    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        Ok(self.clipboard.platform())
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
        Err("no views".into())
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

/// The server, used from blocking threads as stdio would.
struct Agent {
    server: Arc<Mutex<Server>>,
    next: Mutex<u64>,
    _dir: tempfile::TempDir,
}

impl Agent {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clipboard = nexa_client::memclip::MemoryClipboard::new();
        let deps = Arc::new(TestDeps {
            dir: dir.path().to_path_buf(),
            clipboard: clipboard.clone(),
        });
        let viewer = Viewer::new(
            deps,
            clipboard,
            dir.path().to_path_buf(),
            "test agent".into(),
        )
        .unwrap();
        Self {
            server: Arc::new(Mutex::new(Server::new(viewer))),
            next: Mutex::new(1),
            _dir: dir,
        }
    }

    async fn request(&self, method: &str, params: Value) -> Value {
        let id = {
            let mut n = lock(&self.next);
            *n += 1;
            *n
        };
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let server = self.server.clone();
        let reply = tokio::task::spawn_blocking(move || lock(&server).handle(&msg))
            .await
            .unwrap()
            .expect("a request gets a reply");
        assert_eq!(reply["id"], id);
        reply
    }

    async fn call(&self, tool: &str, args: Value) -> Value {
        let reply = self
            .request("tools/call", json!({ "name": tool, "arguments": args }))
            .await;
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }
}

fn texts(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["type"] == "text")
        .map(|c| c["text"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn png_size(result: &Value) -> (u32, u32) {
    let image = result["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "image")
        .unwrap_or_else(|| panic!("no image in {result}"));
    assert_eq!(image["mimeType"], "image/png");
    let png = base64::engine::general_purpose::STANDARD
        .decode(image["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    (
        u32::from_be_bytes(png[16..20].try_into().unwrap()),
        u32::from_be_bytes(png[20..24].try_into().unwrap()),
    )
}

async fn connect(agent: &Agent, h: &Host) -> Value {
    agent
        .call(
            "nexa_connect",
            json!({ "address": h.addr.to_string(), "code": h.agent.current_code().to_string() }),
        )
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_connects_looks_and_acts() {
    let h = host(
        FakeUi::allowing(perms(true, true, false), true),
        Timeouts::default(),
    )
    .await;
    h.agent.set_allow_agents(true);
    let agent = Agent::new();

    let init = agent
        .request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {} }),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "nexadesk");
    let list = agent.request("tools/list", json!({})).await;
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for t in [
        "nexa_connect",
        "nexa_screenshot",
        "nexa_click",
        "nexa_type",
        "nexa_key",
    ] {
        assert!(names.contains(&t), "{t} missing from {names:?}");
    }

    let connected = connect(&agent, &h).await;
    assert_eq!(connected["isError"], false, "{connected}");
    assert!(texts(&connected).contains("Session 1"), "{connected}");
    assert!(
        texts(&connected).contains("keyboard and mouse"),
        "{connected}"
    );
    assert!(
        texts(&connected).contains("display 0 = fake 1920×1080, shown now"),
        "displays are numbered as nexa_display takes them: {connected}"
    );

    let shot = agent.call("nexa_screenshot", json!({})).await;
    assert_eq!(shot["isError"], false, "{shot}");
    assert_eq!(png_size(&shot), FRAME);

    // The middle of the screenshot is the middle of the host's 960×540-point screen.
    let clicked = agent
        .call("nexa_click", json!({ "x": 392, "y": 220.5 }))
        .await;
    assert_eq!(clicked["isError"], false, "{clicked}");
    assert_eq!(png_size(&clicked), FRAME, "an action returns a screenshot");
    until("click injected", WAIT, || {
        h.platform.injected.all().iter().any(|i| {
            matches!(i, Injected::Button { button: 0, down: false, at: Some((x, y)) }
                if (x - 480.0).abs() <= 1.0 && (y - 270.0).abs() <= 1.0)
        })
    })
    .await;

    let typed = agent
        .call(
            "nexa_type",
            json!({ "text": "héllo 你好", "screenshot": false }),
        )
        .await;
    assert_eq!(typed["isError"], false, "{typed}");
    assert!(
        !typed["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == "image")
    );
    until("text injected", WAIT, || {
        h.platform
            .injected
            .all()
            .contains(&Injected::Text("héllo 你好".into()))
    })
    .await;

    // Keys as named for the host, with no ⌘/Ctrl rules in between.
    let pressed = agent
        .call("nexa_key", json!({ "keys": "ctrl+c", "screenshot": false }))
        .await;
    assert_eq!(pressed["isError"], false, "{pressed}");
    let (ctrl, c) = (Hid::key(0xE0).0, Hid::key(0x06).0);
    until("keys injected", WAIT, || {
        h.platform.injected.keys().len() >= 4
    })
    .await;
    assert_eq!(
        h.platform.injected.keys(),
        vec![(ctrl, true), (c, true), (c, false), (ctrl, false)]
    );

    let outside = agent.call("nexa_click", json!({ "x": 5000, "y": 1 })).await;
    assert_eq!(outside["isError"], true);
    assert!(
        texts(&outside).contains("outside the screenshot"),
        "{outside}"
    );

    let bad_key = agent.call("nexa_key", json!({ "keys": "hyper+q" })).await;
    assert_eq!(bad_key["isError"], true);

    let bye = agent.call("nexa_disconnect", json!({})).await;
    assert_eq!(bye["isError"], false, "{bye}");
    let none = agent.call("nexa_screenshot", json!({})).await;
    assert_eq!(none["isError"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paired_host_connects_again_without_a_code() {
    let h = host(
        FakeUi::allowing(perms(true, true, false), true),
        Timeouts::default(),
    )
    .await;
    h.agent.set_allow_agents(true);
    let agent = Agent::new();
    let first = connect(&agent, &h).await;
    assert_eq!(first["isError"], false, "{first}");
    agent.call("nexa_disconnect", json!({})).await;

    let again = agent
        .call("nexa_connect", json!({ "address": h.addr.to_string() }))
        .await;
    assert_eq!(again["isError"], false, "{again}");
    assert!(texts(&again).contains("Session 2"), "{again}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unpaired_address_without_a_code_is_explained() {
    let h = host(
        FakeUi::allowing(perms(true, true, false), false),
        Timeouts::default(),
    )
    .await;
    h.agent.set_allow_agents(true);
    let agent = Agent::new();
    let r = agent
        .call("nexa_connect", json!({ "address": h.addr.to_string() }))
        .await;
    assert_eq!(r["isError"], true);
    assert!(texts(&r).contains("one-time code"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_view_only_session_refuses_actions() {
    let h = host(
        FakeUi::allowing(perms(true, false, false), false),
        Timeouts::default(),
    )
    .await;
    h.agent.set_allow_agents(true);
    let agent = Agent::new();
    let connected = connect(&agent, &h).await;
    assert_eq!(connected["isError"], false, "{connected}");
    let shot = agent.call("nexa_screenshot", json!({})).await;
    assert_eq!(shot["isError"], false, "{shot}");
    let click = agent.call("nexa_click", json!({ "x": 10, "y": 10 })).await;
    assert_eq!(click["isError"], true);
    assert!(texts(&click).contains("viewing only"), "{click}");
    assert!(
        h.platform
            .injected
            .all()
            .iter()
            .all(|i| !matches!(i, Injected::Button { .. }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_mistakes_get_json_rpc_errors() {
    let agent = Agent::new();
    let unknown = agent.request("resources/list", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601);
    let tool = agent
        .request(
            "tools/call",
            json!({ "name": "nexa_format_disk", "arguments": {} }),
        )
        .await;
    assert_eq!(tool["error"]["code"], -32602);
    // A notification gets no reply at all.
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert!(lock(&agent.server).handle(&note).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_that_does_not_accept_agents_says_so() {
    let h = host(
        FakeUi::allowing(perms(true, true, false), false),
        Timeouts::default(),
    )
    .await;
    let agent = Agent::new();
    let r = connect(&agent, &h).await;
    assert_eq!(r["isError"], true);
    assert!(texts(&r).contains("does not accept AI agents"), "{r}");
}

fn granted(terminal: bool, clipboard: bool) -> nexa_wire::control::Permissions {
    nexa_wire::control::Permissions {
        terminal,
        ..perms(true, true, clipboard)
    }
}

async fn connected_to(p: nexa_wire::control::Permissions) -> (Host, Agent) {
    let h = host(FakeUi::allowing(p, false), Timeouts::default()).await;
    h.agent.set_allow_agents(true);
    let agent = Agent::new();
    let r = connect(&agent, &h).await;
    assert_eq!(r["isError"], false, "{r}");
    let shot = agent.call("nexa_screenshot", json!({})).await;
    assert_eq!(shot["isError"], false, "{shot}");
    (h, agent)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drag_presses_moves_and_lets_go_at_the_other_end() {
    let (h, agent) = connected_to(granted(false, false)).await;
    // Screenshot (784×441) corners to the host's 960×540-point screen.
    let r = agent
        .call(
            "nexa_drag",
            json!({ "from_x": 98, "from_y": 55.1, "to_x": 588, "to_y": 330.7, "screenshot": false }),
        )
        .await;
    assert_eq!(r["isError"], false, "{r}");
    until("drag injected", WAIT, || {
        h.platform
            .injected
            .all()
            .iter()
            .any(|i| matches!(i, Injected::Button { down: false, .. }))
    })
    .await;
    let all = h.platform.injected.all();
    let at = |down: bool| {
        all.iter()
            .find_map(|i| match i {
                Injected::Button {
                    button: 0,
                    down: d,
                    at: Some(p),
                } if *d == down => Some(*p),
                _ => None,
            })
            .unwrap()
    };
    let (start, end) = (at(true), at(false));
    assert!(
        (start.0 - 120.0).abs() <= 1.5 && (start.1 - 67.5).abs() <= 1.5,
        "{start:?}"
    );
    assert!(
        (end.0 - 720.0).abs() <= 1.5 && (end.1 - 405.0).abs() <= 1.5,
        "{end:?}"
    );
    let moves_between = all
        .iter()
        .skip_while(|i| !matches!(i, Injected::Button { down: true, .. }))
        .take_while(|i| !matches!(i, Injected::Button { down: false, .. }))
        .filter(|i| matches!(i, Injected::Move(..)))
        .count();
    assert!(moves_between >= 3, "the pointer moves while held: {all:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn displays_are_listed_and_a_missing_one_is_named() {
    let (_h, agent) = connected_to(granted(false, false)).await;
    let list = agent.call("nexa_display", json!({})).await;
    assert_eq!(list["isError"], false, "{list}");
    assert!(texts(&list).contains("(shown now)"), "{list}");
    let same = agent.call("nexa_display", json!({ "display": 0 })).await;
    assert_eq!(same["isError"], false, "{same}");
    assert_eq!(png_size(&same), FRAME);
    let missing = agent.call("nexa_display", json!({ "display": 7 })).await;
    assert_eq!(missing["isError"], true);
    assert!(texts(&missing).contains("no display 7"), "{missing}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_run_in_one_shell_with_their_status() {
    let (_h, agent) = connected_to(granted(true, false)).await;
    let r = agent
        .call("nexa_run", json!({ "command": "echo $((6*7))nexa" }))
        .await;
    assert_eq!(r["isError"], false, "{r}");
    let t = texts(&r);
    assert!(t.starts_with("Exit status 0."), "{t}");
    assert!(t.contains("42nexa"), "{t}");
    assert!(!t.contains("__NEXA_END"), "no marker in the output: {t}");
    assert!(!t.contains("echo"), "no echo of the command: {t}");
    // The directory carries over to the next command.
    agent.call("nexa_run", json!({ "command": "cd /" })).await;
    let pwd = agent.call("nexa_run", json!({ "command": "pwd" })).await;
    assert!(texts(&pwd).ends_with("\n/"), "{pwd}");
    let failed = agent.call("nexa_run", json!({ "command": "false" })).await;
    assert!(texts(&failed).starts_with("Exit status 1."), "{failed}");
    // Something waiting for typing gets nothing and ends.
    let read = agent
        .call(
            "nexa_run",
            json!({ "command": "read x; echo got-$x", "timeout": 5 }),
        )
        .await;
    assert!(texts(&read).contains("got-"), "{read}");
    // Too slow: interrupted, and the shell is still usable after.
    let slow = agent
        .call("nexa_run", json!({ "command": "sleep 30", "timeout": 1 }))
        .await;
    assert!(texts(&slow).contains("interrupted"), "{slow}");
    let after = agent
        .call("nexa_run", json!({ "command": "echo still-here" }))
        .await;
    assert!(texts(&after).contains("still-here"), "{after}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_line_not_allowed_is_explained() {
    let (_h, agent) = connected_to(granted(false, false)).await;
    let r = agent
        .call("nexa_run", json!({ "command": "echo hi" }))
        .await;
    assert_eq!(r["isError"], true);
    assert!(texts(&r).contains("did not allow its command line"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_clipboard_goes_both_ways_without_this_machines_own() {
    let (h, agent) = connected_to(granted(false, true)).await;
    let set = agent
        .call("nexa_clipboard_set", json!({ "text": "来自 AI 的文字" }))
        .await;
    assert_eq!(set["isError"], false, "{set}");
    until("host has it", WAIT, || {
        h.platform
            .clip
            .content(nexa_clip::ClipKind::Text)
            .as_deref()
            == Some("来自 AI 的文字".as_bytes())
    })
    .await;
    h.platform
        .clip
        .user_copy(&[(nexa_clip::ClipKind::Text, b"copied on the host")]);
    let mut got = String::new();
    for _ in 0..200 {
        got = texts(&agent.call("nexa_clipboard_get", json!({})).await);
        if got == "copied on the host" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(got, "copied on the host");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clipboard_not_allowed_is_explained() {
    let (_h, agent) = connected_to(granted(false, false)).await;
    let r = agent.call("nexa_clipboard_get", json!({})).await;
    assert_eq!(r["isError"], true);
    assert!(texts(&r).contains("did not allow the clipboard"), "{r}");
}
