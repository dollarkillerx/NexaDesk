//! The MCP server against a real agent over loopback QUIC, driven by JSON-RPC
//! messages as an MCP client sends them. Fake devices on both sides, and a made-up
//! directory of the devices an agent may operate (docs/MCP.md §16): nothing here
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
    mcp::{
        server::Server,
        viewer::{Device, Directory, SignIn, Viewer},
    },
    proto::ConnectAuth,
};
use nexa_input::Hid;
use nexa_media::{Decoder, PresentMode, Presenter};
use nexa_session::{clip::ClipPlatform, handshake::Timeouts};
use nexa_wire::control::{Caps, Codec, Os};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(10);
/// The fake host's 1920×1080 pixels as an agent's: its coordinates and screenshots.
const SPACE: (u32, u32) = (1568, 882);

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
        Err("an agent's session has no video".into())
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

/// The devices the person allowed AI agents to operate, made up: each signs in with
/// the code its fake host shows (the app's would use a saved password; the agent
/// never sees either).
#[derive(Default)]
struct FakeDirectory {
    devices: Mutex<Vec<(Device, Option<String>)>>,
}

impl FakeDirectory {
    /// Allows `h` as "Test host", signing in with its current code.
    fn allow(&self, h: &Host) {
        let device = Device {
            name: "Test host".into(),
            address: h.addr.to_string(),
            sign_in: "a one-time code".into(),
        };
        let mut devices = lock(&self.devices);
        devices.retain(|(d, _)| d.address != device.address);
        devices.push((device, Some(h.agent.current_code().to_string())));
    }

    /// The person took away the way to sign in to every device.
    fn forget_sign_ins(&self) {
        for (d, code) in lock(&self.devices).iter_mut() {
            d.sign_in.clear();
            *code = None;
        }
    }
}

impl Directory for FakeDirectory {
    fn devices(&self) -> Vec<Device> {
        lock(&self.devices).iter().map(|(d, _)| d.clone()).collect()
    }
    fn sign_in(&self, device: &Device) -> Result<SignIn, String> {
        let devices = lock(&self.devices);
        let code = devices
            .iter()
            .find(|(d, _)| d.address == device.address)
            .and_then(|(_, c)| c.clone())
            .ok_or_else(|| format!("NexaDesk has no way to sign in to {}", device.name))?;
        Ok(SignIn {
            auth: ConnectAuth::Code(code),
            signal_server: String::new(),
            signal_secret: String::new(),
        })
    }
}

/// The server, used from blocking threads as the app's socket would.
struct Agent {
    server: Arc<Mutex<Server>>,
    directory: Arc<FakeDirectory>,
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
        let directory = Arc::new(FakeDirectory::default());
        let viewer = Viewer::new(
            deps,
            clipboard,
            dir.path().to_path_buf(),
            "test agent".into(),
            directory.clone(),
        )
        .unwrap();
        Self {
            server: Arc::new(Mutex::new(Server::new(Arc::new(viewer), 1))),
            directory,
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

/// Allows `h` for AI agents (as the person would in NexaDesk) and connects to it by
/// name.
async fn connect(agent: &Agent, h: &Host) -> Value {
    agent.directory.allow(h);
    agent
        .call("nexa_connect", json!({ "device": "Test host" }))
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

    assert!(
        texts(&connected).contains("Coordinates are 1568×882 pixels"),
        "{connected}"
    );

    // Refs come from nexa_ui.
    let early = agent.call("nexa_click", json!({ "ref": "@e1" })).await;
    assert_eq!(early["isError"], true);
    assert!(texts(&early).contains("call nexa_ui first"), "{early}");

    let ui = agent.call("nexa_ui", json!({})).await;
    assert_eq!(ui["isError"], false, "{ui}");
    let ui_text = texts(&ui);
    assert!(
        ui_text.contains("In front: Fake app — \"Fake window\""),
        "{ui_text}"
    );
    assert!(
        ui_text.contains("Keyboard focus: textfield \"Name\""),
        "{ui_text}"
    );
    assert!(
        ui_text.contains("@e1 button \"OK\" (163,163 163×65)"),
        "{ui_text}"
    );
    assert!(ui_text.contains("@e2 textfield \"Name\""), "{ui_text}");
    assert!(ui_text.contains("2 refs"), "{ui_text}");
    assert!(
        !ui["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == "image")
    );

    // By ref: the middle of the OK button, 100,100 100×40 points.
    let by_ref = agent.call("nexa_click", json!({ "ref": "@e1" })).await;
    assert_eq!(by_ref["isError"], false, "{by_ref}");
    let said = texts(&by_ref);
    assert!(said.starts_with("Clicked @e1. The screen "), "{said}");
    assert!(said.contains("In front: Fake app"), "{said}");
    until("the OK button clicked", WAIT, || {
        h.platform.injected.all().iter().any(|i| {
            matches!(i, Injected::Button { button: 0, down: false, at: Some((x, y)) }
                if (x - 150.0).abs() <= 1.5 && (y - 120.0).abs() <= 1.5)
        })
    })
    .await;
    let stale = agent.call("nexa_click", json!({ "ref": "@e9" })).await;
    assert_eq!(stale["isError"], true);
    assert!(texts(&stale).contains("there is no @e9"), "{stale}");

    // By point: the middle of the agent's pixels is the middle of the 960×540 points,
    // with the screenshot asked for.
    let clicked = agent
        .call(
            "nexa_click",
            json!({ "x": 784, "y": 441, "then": "screenshot" }),
        )
        .await;
    assert_eq!(clicked["isError"], false, "{clicked}");
    assert_eq!(
        png_size(&clicked),
        SPACE,
        "a lossless picture of the agent's pixels"
    );
    until("click injected", WAIT, || {
        h.platform.injected.all().iter().any(|i| {
            matches!(i, Injected::Button { button: 0, down: false, at: Some((x, y)) }
                if (x - 480.0).abs() <= 1.0 && (y - 270.0).abs() <= 1.0)
        })
    })
    .await;

    let shot = agent.call("nexa_screenshot", json!({})).await;
    assert_eq!(shot["isError"], false, "{shot}");
    assert_eq!(png_size(&shot), SPACE);

    let typed = agent
        .call("nexa_type", json!({ "text": "héllo 你好" }))
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
    let pressed = agent.call("nexa_key", json!({ "keys": "ctrl+c" })).await;
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
        texts(&outside).contains("outside the screen, which is 1568×882 pixels"),
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

    // The directory could no longer sign in: the agent's own pairing does.
    agent.directory.forget_sign_ins();
    let again = agent
        .call("nexa_connect", json!({ "device": "Test host" }))
        .await;
    assert_eq!(again["isError"], false, "{again}");
    assert!(texts(&again).contains("Session 2"), "{again}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_devices_the_person_allowed_can_be_reached() {
    let h = host(
        FakeUi::allowing(perms(true, true, false), false),
        Timeouts::default(),
    )
    .await;
    h.agent.set_allow_agents(true);
    let agent = Agent::new();
    // Nothing allowed yet: nothing to list, nothing to connect to, whatever the agent
    // knows of the address.
    let none = agent.call("nexa_devices", json!({})).await;
    assert!(
        texts(&none).contains("No computer may be operated"),
        "{none}"
    );
    let r = agent
        .call("nexa_connect", json!({ "device": h.addr.to_string() }))
        .await;
    assert_eq!(r["isError"], true);
    assert!(texts(&r).contains("no device may be operated"), "{r}");

    agent.directory.allow(&h);
    let listed = agent.call("nexa_devices", json!({})).await;
    let t = texts(&listed);
    assert!(
        t.contains(&format!(
            "\"Test host\" ({}): signs in with a one-time code",
            h.addr
        )),
        "{t}"
    );
    let r = agent
        .call("nexa_connect", json!({ "device": "Elsewhere" }))
        .await;
    assert_eq!(r["isError"], true);
    assert!(
        texts(&r).contains(
            "there is no device \"Elsewhere\" among those you may operate: \"Test host\""
        ),
        "{r}"
    );
    // No way to sign in: said, not tried with something the agent made up.
    agent.directory.forget_sign_ins();
    let r = agent
        .call("nexa_connect", json!({ "device": "test HOST" }))
        .await;
    assert_eq!(r["isError"], true);
    assert!(texts(&r).contains("no way to sign in to Test host"), "{r}");
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
    (h, agent)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drag_presses_moves_and_lets_go_at_the_other_end() {
    let (h, agent) = connected_to(granted(false, false)).await;
    // The agent's 1568×882 pixels of the host's 960×540 points.
    let r = agent
        .call(
            "nexa_drag",
            json!({ "from_x": 196, "from_y": 110.25, "to_x": 1176, "to_y": 661.5 }),
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
    assert!(
        texts(&same).contains("its coordinates are 1568×882 pixels"),
        "{same}"
    );
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
async fn the_agent_is_told_whether_the_command_line_was_allowed() {
    for terminal in [true, false] {
        let h = host(
            FakeUi::allowing(granted(terminal, false), false),
            Timeouts::default(),
        )
        .await;
        h.agent.set_allow_agents(true);
        let agent = Agent::new();
        let r = connect(&agent, &h).await;
        assert_eq!(r["isError"], false, "{r}");
        assert_eq!(texts(&r).contains("command line"), terminal, "{r}");
    }
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

fn with_files(terminal: bool) -> nexa_wire::control::Permissions {
    nexa_wire::control::Permissions {
        file: true,
        terminal,
        ..perms(true, true, false)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_go_to_the_host_and_come_back_by_their_path() {
    let (h, agent) = connected_to(with_files(true)).await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("报告.txt");
    std::fs::write(&src, b"from the agent").unwrap();
    let sent = agent.call("nexa_send_file", json!({ "path": src })).await;
    assert_eq!(sent["isError"], false, "{sent}");
    assert!(
        texts(&sent).starts_with("Sent 报告.txt (14 bytes)"),
        "{sent}"
    );
    let there = h.downloads.join("报告.txt");
    assert_eq!(std::fs::read(&there).unwrap(), b"from the agent");

    // Back by its path there, saved where the agent says.
    let back = dir.path().join("back.txt");
    let got = agent
        .call(
            "nexa_receive_file",
            json!({ "path": there, "save_as": back }),
        )
        .await;
    assert_eq!(got["isError"], false, "{got}");
    assert!(texts(&got).contains(&back.display().to_string()), "{got}");
    assert_eq!(std::fs::read(&back).unwrap(), b"from the agent");
    // Never over a file that is there.
    let again = agent
        .call(
            "nexa_receive_file",
            json!({ "path": there, "save_as": back }),
        )
        .await;
    assert_eq!(again["isError"], true);
    assert!(texts(&again).contains("already exists"), "{again}");
    // Without save_as: into the AI's own downloads, named as there.
    let plain = agent
        .call("nexa_receive_file", json!({ "path": there }))
        .await;
    assert_eq!(plain["isError"], false, "{plain}");
    assert!(texts(&plain).contains("报告.txt"), "{plain}");
    // Something that is not a file there.
    for path in [
        h.downloads.join("missing.txt").display().to_string(),
        h.downloads.display().to_string(),
        "报告.txt".to_owned(),
    ] {
        let r = agent
            .call("nexa_receive_file", json!({ "path": path }))
            .await;
        assert_eq!(r["isError"], true, "{path}");
        assert!(texts(&r).contains("no such file"), "{r}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetching_needs_the_command_line_and_either_way_needs_files() {
    let (h, agent) = connected_to(with_files(false)).await;
    std::fs::create_dir_all(&h.downloads).unwrap();
    let there = h.downloads.join("secret.txt");
    std::fs::write(&there, b"secret").unwrap();
    let r = agent
        .call("nexa_receive_file", json!({ "path": there }))
        .await;
    assert_eq!(r["isError"], true);
    assert!(
        texts(&r).contains("only when the command line was allowed"),
        "{r}"
    );

    let (_h, agent) = connected_to(granted(true, false)).await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.txt");
    std::fs::write(&src, b"a").unwrap();
    for (tool, path) in [
        ("nexa_send_file", src.display().to_string()),
        ("nexa_receive_file", there.display().to_string()),
    ] {
        let r = agent.call(tool, json!({ "path": path })).await;
        assert_eq!(r["isError"], true, "{tool}");
        assert!(texts(&r).contains("did not allow files"), "{r}");
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refs_do_not_outlive_a_tree_that_could_not_be_read() {
    let (h, agent) = connected_to(perms(true, true, false)).await;
    let r = agent.call("nexa_ui", json!({})).await;
    assert_eq!(r["isError"], false, "{r}");
    assert!(texts(&r).contains("@e1 button \"OK\""), "{r}");
    // Another app in front, whose elements cannot be read: @e1 is not the OK button.
    h.platform
        .view
        .unreadable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let r = agent.call("nexa_ui", json!({})).await;
    assert!(texts(&r).contains("could not be read"), "{r}");
    let r = agent.call("nexa_click", json!({ "ref": "@e1" })).await;
    assert_eq!(r["isError"], true, "{r}");
    assert!(texts(&r).contains("could not read the elements"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_action_on_a_screen_that_cannot_be_watched_says_why() {
    let (h, agent) = connected_to(perms(true, true, false)).await;
    h.platform
        .view
        .blind
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let r = agent
        .call("nexa_click", json!({ "x": 100, "y": 100 }))
        .await;
    assert_eq!(r["isError"], false, "{r}");
    let t = texts(&r);
    assert!(
        t.contains("Whether the screen changed is unknown: it could not be watched (screen recording is not allowed)"),
        "{t}"
    );
    assert!(!t.contains("still changing"), "{t}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refs_are_pressed_and_filled_without_the_pointer() {
    let (h, agent) = connected_to(perms(true, true, false)).await;
    let r = agent.call("nexa_press", json!({ "ref": "@e1" })).await;
    assert_eq!(r["isError"], true, "{r}");
    assert!(texts(&r).contains("call nexa_ui first"), "{r}");

    // Boxes and numbers on the picture, with the tree whose refs they are.
    let r = agent
        .call("nexa_screenshot", json!({ "marks": true }))
        .await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(png_size(&r), SPACE);
    assert!(texts(&r).contains("@e2 textfield \"Name\""), "{r}");

    let r = agent
        .call("nexa_press", json!({ "ref": "@e1", "then": "ui" }))
        .await;
    assert_eq!(r["isError"], false, "{r}");
    let t = texts(&r);
    assert!(t.starts_with("Pressed @e1."), "{t}");
    assert!(t.contains("@e1 button \"OK\""), "{t}");
    let r = agent
        .call("nexa_set_value", json!({ "ref": "@e2", "text": "hello" }))
        .await;
    assert_eq!(r["isError"], false, "{r}");
    assert!(texts(&r).starts_with("Set the value of @e2."), "{r}");
    let r = agent
        .call("nexa_set_value", json!({ "ref": "@e1", "text": "x" }))
        .await;
    assert_eq!(r["isError"], true, "{r}");
    assert!(texts(&r).contains("a button has no value"), "{r}");
    assert_eq!(
        *h.platform.view.acted.lock().unwrap(),
        vec!["OK press".to_owned(), "Name set hello".to_owned()]
    );
    // None of it moved the pointer or pressed a key.
    assert!(
        h.platform.injected.all().is_empty(),
        "{:?}",
        h.platform.injected.all()
    );
}

/// One JSON-RPC exchange over the app's socket, as `nexa-mcp` relays it.
#[cfg(unix)]
fn exchange(
    conn: &mut std::os::unix::net::UnixStream,
    reader: &mut std::io::BufReader<std::os::unix::net::UnixStream>,
    msg: Value,
) -> Value {
    use std::io::{BufRead, Write};
    writeln!(conn, "{msg}").unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ai_app_reaches_the_app_through_its_socket_and_its_sessions_end_with_it() {
    use nexa_client::mcp::{Service, listen};

    let h = host(
        FakeUi::allowing(perms(true, true, false), false),
        Timeouts::default(),
    )
    .await;
    h.agent.set_allow_agents(true);
    let dir = tempfile::tempdir().unwrap();
    let directory = Arc::new(FakeDirectory::default());
    directory.allow(&h);
    let endpoint = dir.path().join("mcp.sock").to_string_lossy().into_owned();
    let service = Service::start_with(
        &endpoint,
        dir.path().join("ai"),
        "test agent".into(),
        directory,
    )
    .unwrap();
    // Only this user may open it.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&endpoint).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let endpoint2 = endpoint.clone();
    let service = Arc::new(service);
    let s = service.clone();
    let session = tokio::task::spawn_blocking(move || {
        let mut conn = listen::connect(&endpoint2).unwrap();
        let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
        let init = exchange(
            &mut conn,
            &mut reader,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": { "name": "claude-code", "version": "1" } } }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "nexadesk");
        let r = exchange(
            &mut conn,
            &mut reader,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": { "name": "nexa_connect", "arguments": { "device": "Test host" } } }),
        );
        assert_eq!(r["result"]["isError"], false, "{r}");
        // The app sees who operates what.
        let shown = s.sessions();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].client, "claude-code");
        assert_eq!(shown[0].device, "Test host");
        // The AI app goes away: its session ends with it.
        drop(reader);
        drop(conn);
    });
    session.await.unwrap();
    until("the AI's session ended", WAIT, || {
        service.sessions().is_empty()
    })
    .await;
    drop(service);
    assert!(!std::path::Path::new(&endpoint).exists());
}
