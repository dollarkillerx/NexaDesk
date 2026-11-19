//! The MCP side: JSON-RPC 2.0, one JSON message per line (the MCP stdio transport,
//! which `nexa-mcp` relays from the AI app to NexaDesk's local socket), and the tools
//! (docs/MCP.md §3.1, §13.5, §16). One `Server` per MCP client; its sessions end with it.
//!
//! Requests are handled one at a time, in order. Tool failures come back as results
//! with `isError` (the agent can read and act on them); protocol mistakes as JSON-RPC
//! errors.

use std::{
    io::{BufRead, Write},
    sync::Arc,
};

use base64::Engine;
use serde_json::{Value, json};

use nexa_session::agent::{Action, ObserveReply};

use super::{
    keys,
    viewer::{Button, LookFor, Looked, MAX_TEXT, Remote, Target, Viewer},
};

/// Protocol versions this server speaks; the newest is offered to clients asking for
/// one it does not know.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Operate another computer through NexaDesk. nexa_devices lists the \
computers the person here allowed you to operate; connect to one with nexa_connect {device} \
(NexaDesk signs in itself, and the person at that computer is asked to allow it). Read the \
screen with nexa_ui first: the \
frontmost window's elements as text, each one you can act on with a ref like @e3, its role, \
name, value, state and position. Act by ref (nexa_click {ref: \"@e3\"}, nexa_type {ref, text}) \
rather than by coordinates where you can. Refs hold until your next nexa_ui: read again after \
the screen changes. When the elements do not show what you need (pictures, canvases, games, \
some Electron or custom-drawn apps, or to check how something looks), call nexa_screenshot: \
a lossless picture, the pointer drawn in, whose pixels are the coordinates actions take. \
nexa_press and nexa_set_value act on a ref through the accessibility API without moving the \
pointer: when a click does not land, or to fill a field in one go. nexa_screenshot {marks: \
true} boxes and numbers the refs on the picture. \
Each action waits until the remote computer has carried it out and its screen has settled \
(up to 3 s), then says whether the screen changed, what is in front and what has the keyboard \
focus; pass then: \"ui\" or \"screenshot\" to get the new elements or picture with it. Typing \
and keys go to whatever has the focus. Calls run one at a time, in order. Key names are the \
remote computer's own: cmd is the Command key on a Mac and the Windows key on Windows. \
nexa_display lists and switches displays. Where the person there allowed it, nexa_run runs \
commands on its command line (often more reliable than clicking), and nexa_clipboard_get / \
nexa_clipboard_set read and fill its clipboard, nexa_send_file sends a file there and \
nexa_receive_file fetches one from there by its path. The person at the remote computer sees that \
an AI agent is connected and can end it at any time; someone who wants to watch can connect \
with the NexaDesk app as well.";

pub struct Server {
    viewer: Arc<Viewer>,
    /// This MCP client, as the viewer tells its sessions apart.
    client: u64,
}

impl Drop for Server {
    /// The client has gone: so do its sessions.
    fn drop(&mut self) {
        self.viewer.end_client(self.client);
    }
}

impl Server {
    /// Serves MCP client `client` (a number of its own) with `viewer`.
    pub fn new(viewer: Arc<Viewer>, client: u64) -> Self {
        Self { viewer, client }
    }

    pub fn viewer(&self) -> &Viewer {
        &self.viewer
    }

    /// Reads requests from `input` until it closes, answering on `output`.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.handle(&msg),
                Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
            };
            if let Some(reply) = reply {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    /// One message; `None` for notifications and responses, which get no reply.
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        let Some(obj) = msg.as_object() else {
            return Some(error(Value::Null, -32600, "expected a JSON-RPC object"));
        };
        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            // A response to something we never sent, or garbage: nothing to answer.
            return id.map(|id| error(id, -32600, "no method"));
        };
        let id = id?; // a notification (`notifications/initialized`, `…/cancelled`)
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        Some(match method {
            "initialize" => {
                if let Some(name) = params
                    .get("clientInfo")
                    .and_then(|c| c.get("name"))
                    .and_then(Value::as_str)
                {
                    self.viewer.name_client(self.client, name);
                }
                ok(id, initialize(&params))
            }
            "ping" => ok(id, json!({})),
            "tools/list" => ok(id, json!({ "tools": tools() })),
            "tools/call" => match params.get("name").and_then(Value::as_str) {
                Some(name) if tools().iter().any(|t| t["name"] == name) => {
                    let args = params.get("arguments").cloned().unwrap_or(json!({}));
                    ok(id, self.call(name, &args))
                }
                Some(name) => error(id, -32602, &format!("unknown tool {name}")),
                None => error(id, -32602, "no tool name"),
            },
            _ => error(id, -32601, &format!("method not found: {method}")),
        })
    }

    fn call(&mut self, name: &str, args: &Value) -> Value {
        match self.run(name, args) {
            Ok(content) => json!({ "content": content, "isError": false }),
            Err(e) => json!({ "content": [text(&e)], "isError": true }),
        }
    }

    fn run(&mut self, name: &str, args: &Value) -> Result<Vec<Value>, String> {
        let v = &self.viewer;
        match name {
            "nexa_devices" => {
                let devices = v.devices();
                if devices.is_empty() {
                    return Ok(vec![text(
                        "No computer may be operated by an AI agent yet. The person here allows \
                         one in NexaDesk: \"Allow AI to operate\" on a saved device.",
                    )]);
                }
                let lines: Vec<String> = devices
                    .iter()
                    .map(|d| {
                        let how = if d.sign_in.is_empty() {
                            "NexaDesk has no way to sign in to it yet".to_owned()
                        } else {
                            format!("signs in with {}", d.sign_in)
                        };
                        format!("\"{}\" ({}): {how}", d.name, d.address)
                    })
                    .collect();
                Ok(vec![text(&format!(
                    "{}\nConnect with nexa_connect {{device: \"<name>\"}}.",
                    lines.join("\n")
                ))])
            }
            "nexa_connect" => {
                let device = str_arg(args, "device")?.ok_or("`device` is required")?;
                let r = v.connect(self.client, device)?;
                Ok(vec![text(&format!(
                    "{} Read the screen with nexa_ui.",
                    describe(v, &r)
                ))])
            }
            "nexa_sessions" => {
                let list = v.sessions(self.client);
                if list.is_empty() {
                    return Ok(vec![text("No sessions.")]);
                }
                Ok(vec![text(
                    &list
                        .iter()
                        .map(|r| describe(v, r))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )])
            }
            "nexa_disconnect" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                v.disconnect(&r);
                Ok(vec![text(&format!(
                    "Disconnected session {} ({}).",
                    r.id, r.host.host_name
                ))])
            }
            "nexa_ui" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
                let max = int_arg(args, "max_nodes")?.unwrap_or(0);
                if !(0..=2000).contains(&max) {
                    return Err("`max_nodes` is 1 to 2000".into());
                }
                let what = LookFor {
                    ui: true,
                    all,
                    max_nodes: max as u32,
                    ..LookFor::default()
                };
                let looked = v.look(&r, 0, &what)?;
                Ok(look_content(&looked, None))
            }
            "nexa_screenshot" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let marks = args.get("marks").and_then(Value::as_bool).unwrap_or(false);
                let what = LookFor {
                    screenshot: true,
                    marks,
                    ..LookFor::default()
                };
                let looked = v.look(&r, 0, &what)?;
                Ok(look_content(&looked, None))
            }
            "nexa_press" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let target = str_arg(args, "ref")?.ok_or("`ref` is required")?;
                v.act(&r, target, Action::Press, "")?;
                self.after(&r, args, 0, &format!("Pressed {target}"))
            }
            "nexa_set_value" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let target = str_arg(args, "ref")?.ok_or("`ref` is required")?;
                let t = str_arg(args, "text")?.ok_or("`text` is required")?;
                if t.chars().count() > MAX_TEXT {
                    return Err(format!("at most {MAX_TEXT} characters at a time"));
                }
                v.act(&r, target, Action::SetValue, t)?;
                self.after(&r, args, 0, &format!("Set the value of {target}"))
            }
            "nexa_click" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let target = target_arg(args, "")?;
                let button = button_arg(args)?;
                let count = int_arg(args, "count")?.unwrap_or(1);
                if !(1..=3).contains(&count) {
                    return Err("`count` is 1, 2 (double click) or 3".into());
                }
                let mark = v.click(&r, &target, button, count as u32)?;
                let verb = match count {
                    2 => "Double-clicked",
                    3 => "Triple-clicked",
                    _ => "Clicked",
                };
                self.after(&r, args, mark, &format!("{verb} {}", target_text(&target)))
            }
            "nexa_scroll" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let target = target_arg(args, "")?;
                let amount = int_arg(args, "amount")?.unwrap_or(3);
                if !(1..=30).contains(&amount) {
                    return Err("`amount` is 1 to 30 notches".into());
                }
                let amount = amount as i32;
                let (dx, dy) = match str_arg(args, "direction")?.ok_or("`direction` is required")? {
                    "up" => (0, -amount),
                    "down" => (0, amount),
                    "left" => (-amount, 0),
                    "right" => (amount, 0),
                    other => return Err(format!("unknown direction {other}")),
                };
                let mark = v.scroll(&r, &target, dx, dy)?;
                self.after(
                    &r,
                    args,
                    mark,
                    &format!("Scrolled at {}", target_text(&target)),
                )
            }
            "nexa_type" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let t = str_arg(args, "text")?.ok_or("`text` is required")?;
                // Into an element: click it first, so it has the focus.
                let into = str_arg(args, "ref")?.map(|s| Target::Ref(s.to_owned()));
                if let Some(target) = &into {
                    v.click(&r, target, Button::Left, 1)?;
                }
                let mark = v.type_text(&r, t)?;
                let typed = match t.chars().count() {
                    1 => "1 character".to_owned(),
                    n => format!("{n} characters"),
                };
                let done = match &into {
                    Some(target) => format!("Clicked {} and typed {typed}", target_text(target)),
                    None => format!("Typed {typed} into whatever has the focus"),
                };
                self.after(&r, args, mark, &done)
            }
            "nexa_key" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let combo = str_arg(args, "keys")?.ok_or("`keys` is required")?;
                let usages = keys::parse(combo)?;
                let mark = v.keys(&r, &usages)?;
                self.after(&r, args, mark, &format!("Pressed {combo}"))
            }
            "nexa_drag" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let from = target_arg(args, "from_")?;
                let to = target_arg(args, "to_")?;
                let button = button_arg(args)?;
                let mark = v.drag(&r, &from, &to, button)?;
                self.after(
                    &r,
                    args,
                    mark,
                    &format!(
                        "Dragged from {} to {}",
                        target_text(&from),
                        target_text(&to)
                    ),
                )
            }
            "nexa_display" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                match int_arg(args, "display")? {
                    None => {
                        let (list, current) = v.displays(&r);
                        if list.is_empty() {
                            return Ok(vec![text("The host did not list its displays.")]);
                        }
                        let current = current.or_else(|| list.first().map(|d| d.id));
                        let lines: Vec<String> = list
                            .iter()
                            .map(|d| {
                                let shown = if Some(d.id) == current {
                                    " (shown now)"
                                } else {
                                    ""
                                };
                                format!("Display {}: {} {}×{}{shown}", d.id, d.name, d.w, d.h)
                            })
                            .collect();
                        Ok(vec![text(&lines.join("\n"))])
                    }
                    Some(id) => {
                        let id = u32::try_from(id).map_err(|_| "`display` must not be negative")?;
                        let (w, h) = v.select_display(&r, id)?;
                        Ok(vec![text(&format!(
                            "Display {id} is shown now; its coordinates are {w}×{h} pixels. Earlier \
                             refs and coordinates no longer hold: read it with nexa_ui."
                        ))])
                    }
                }
            }
            "nexa_run" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let command = str_arg(args, "command")?.ok_or("`command` is required")?;
                let secs = int_arg(args, "timeout")?.unwrap_or(30);
                if !(1..=600).contains(&secs) {
                    return Err("`timeout` is 1 to 600 seconds".into());
                }
                let ran = v.run(&r, command, std::time::Duration::from_secs(secs as u64))?;
                let head = match (ran.timed_out, ran.status) {
                    (true, _) => {
                        format!("Still running after {secs} s, so it was interrupted (Ctrl+C).")
                    }
                    (false, Some(0)) => "Exit status 0.".to_owned(),
                    (false, Some(s)) => format!("Exit status {s}."),
                    (false, None) => "Finished.".to_owned(),
                };
                let cut = if ran.truncated {
                    format!(
                        " Output was long: only its last {} KB is shown.",
                        super::terminal::MAX_OUTPUT >> 10
                    )
                } else {
                    String::new()
                };
                let body = if ran.output.is_empty() {
                    "(no output)".to_owned()
                } else {
                    ran.output
                };
                Ok(vec![text(&format!("{head}{cut}\n{body}"))])
            }
            "nexa_clipboard_get" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                Ok(vec![text(&match v.clipboard_text(&r)? {
                    Some(t) => t,
                    None => "(the clipboard has no text)".to_owned(),
                })])
            }
            "nexa_clipboard_set" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let t = str_arg(args, "text")?.ok_or("`text` is required")?;
                if t.len() > MAX_CLIPBOARD {
                    return Err(format!("at most {} KB at a time", MAX_CLIPBOARD >> 10));
                }
                let confirmed = v.set_clipboard(&r, t)?;
                Ok(vec![text(if confirmed {
                    "The remote computer has it on its clipboard: paste with nexa_key (cmd+v on a Mac, ctrl+v on Windows)."
                } else {
                    "Put on the clipboard; the remote computer did not confirm it yet. Paste with nexa_key in a moment."
                })])
            }
            "nexa_send_file" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let path = str_arg(args, "path")?.ok_or("`path` is required")?;
                let wait = file_wait(args)?;
                let t = v.send_file(&r, std::path::Path::new(path), wait)?;
                let folder = if r.host.host_os == 1 {
                    r"Downloads\NexaDesk in the user's folder"
                } else {
                    "~/Downloads/NexaDesk"
                };
                Ok(vec![text(&format!(
                    "Sent {} ({}). It is in {folder} on the remote computer; a name already there gets a number added, as in \"name (1).ext\". Move it with nexa_run if it belongs elsewhere.",
                    t.name,
                    size_text(t.size)
                ))])
            }
            "nexa_receive_file" => {
                let r = v.remote(self.client, session_arg(args)?)?;
                let path = str_arg(args, "path")?.ok_or("`path` is required")?;
                let save_as = str_arg(args, "save_as")?.map(std::path::PathBuf::from);
                if let Some(to) = &save_as {
                    check_save_as(to)?;
                }
                let wait = file_wait(args)?;
                let t = v.receive_file(&r, path, wait)?;
                let mut at = std::path::PathBuf::from(&t.path);
                if let Some(to) = save_as {
                    move_file(&at, &to).map_err(|e| {
                        format!(
                            "fetched to {}, but it could not be moved to {}: {e}",
                            at.display(),
                            to.display()
                        )
                    })?;
                    at = to;
                }
                Ok(vec![text(&format!(
                    "Fetched {} ({}) to {} on this computer.",
                    t.name,
                    size_text(t.size),
                    at.display()
                ))])
            }
            _ => Err(format!("unknown tool {name}")),
        }
    }

    /// What an action returns: what was done and how the screen took it, and the new
    /// elements or picture if the caller asked (`then`).
    fn after(&self, r: &Remote, args: &Value, mark: u64, done: &str) -> Result<Vec<Value>, String> {
        let (ui, screenshot) = match str_arg(args, "then")?.unwrap_or("none") {
            "none" => (false, false),
            "ui" => (true, false),
            "screenshot" => (false, true),
            other => return Err(format!("`then` is none, ui or screenshot, not {other}")),
        };
        let what = LookFor {
            settle: true,
            ui,
            screenshot,
            ..LookFor::default()
        };
        match self.viewer.look(r, mark, &what) {
            Ok(looked) => Ok(look_content(&looked, Some(done))),
            Err(e) => Ok(vec![text(&format!(
                "{done}, but the screen could not be looked at afterwards: {e}"
            ))]),
        }
    }
}

fn describe(v: &Viewer, r: &Remote) -> String {
    let perms = r.host.perms.clone().unwrap_or_default();
    let mut allowed = Vec::new();
    if perms.view {
        allowed.push("view");
    }
    if perms.input {
        allowed.push("keyboard and mouse");
    }
    if perms.clipboard {
        allowed.push("clipboard");
    }
    if perms.file {
        allowed.push("files");
    }
    let (list, current) = v.displays(r);
    let current = current.or_else(|| list.first().map(|d| d.id));
    let displays = list
        .iter()
        .map(|d| {
            let shown = if Some(d.id) == current {
                ", shown now"
            } else {
                ""
            };
            format!("display {} = {} {}×{}{shown}", d.id, d.name, d.w, d.h)
        })
        .collect::<Vec<_>>()
        .join("; ");
    let (w, h) = r.space();
    format!(
        "Session {}: {} ({}) at {}. Allowed: {}. Displays (numbered for nexa_display): {}. \
         Coordinates are {w}×{h} pixels of the display shown.",
        r.id,
        r.host.host_name,
        r.os_name(),
        r.address,
        if allowed.is_empty() {
            "nothing".into()
        } else {
            allowed.join(", ")
        },
        if displays.is_empty() {
            "unknown".into()
        } else {
            displays
        },
    )
}

/// One line on how the screen is: after an action (`done`), whether it changed and
/// settled; always, what is in front, what has the focus and where the pointer is.
fn context(rep: &ObserveReply, done: Option<&str>) -> String {
    let mut s = String::new();
    if let Some(done) = done {
        s.push_str(done);
        s.push('.');
        let secs = f64::from(rep.waited_ms) / 1000.0;
        if !rep.watch_error.is_empty() {
            s.push_str(&format!(
                " Whether the screen changed is unknown: it could not be watched ({}).",
                rep.watch_error.trim_end_matches('.')
            ));
        } else if rep.settled {
            s.push_str(&format!(
                " The screen {} and settled after {secs:.1} s.",
                if rep.changed {
                    "changed"
                } else {
                    "did not change"
                }
            ));
        } else {
            s.push_str(&format!(
                " The screen was still changing after {secs:.1} s (something on it may move all \
                 the time)."
            ));
        }
        s.push(' ');
    }
    if !rep.app.is_empty() {
        s.push_str(&format!("In front: {}", rep.app));
        if !rep.window.is_empty() {
            s.push_str(&format!(" — \"{}\"", rep.window));
        }
        s.push_str(". ");
    }
    if !rep.focused.is_empty() {
        s.push_str(&format!("Keyboard focus: {}. ", rep.focused));
    }
    if rep.has_pointer {
        s.push_str(&format!(
            "Pointer at ({}, {}). ",
            rep.pointer_x, rep.pointer_y
        ));
    }
    s.trim_end().to_owned()
}

fn look_content(looked: &Looked, done: Option<&str>) -> Vec<Value> {
    let rep = &looked.reply;
    let mut parts = vec![context(rep, done)];
    if !rep.ui.is_empty() {
        parts.push(rep.ui.trim_end().to_owned());
        parts.push(if looked.refs == 0 {
            "Nothing to act on in this window.".to_owned()
        } else {
            format!(
                "{} refs: act with ref, e.g. nexa_click {{ref: \"@e1\"}}. They hold until the next \
                 nexa_ui. Coordinates are {}×{} pixels.",
                looked.refs, rep.w, rep.h
            )
        });
    } else if !rep.ui_error.is_empty() {
        parts.push(format!(
            "The elements could not be read: {}. Use nexa_screenshot instead.",
            rep.ui_error
        ));
    }
    if !rep.screenshot_error.is_empty() {
        parts.push(format!("No screenshot: {}.", rep.screenshot_error));
    }
    let mut content = Vec::new();
    if !looked.png.is_empty() {
        parts.push(format!(
            "Screenshot {}×{} pixels, the pointer drawn in; coordinates for actions are pixels in it.",
            rep.w, rep.h
        ));
    }
    content.push(text(&parts.join("\n")));
    if !looked.png.is_empty() {
        content.push(json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(&looked.png),
            "mimeType": "image/png",
        }));
    }
    content
}

/// The host's clipboard limit for text (`nexa_clip`, 192 KiB), with room to spare.
const MAX_CLIPBOARD: usize = 128 << 10;

fn button_arg(args: &Value) -> Result<Button, String> {
    match str_arg(args, "button")?.unwrap_or("left") {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        other => Err(format!("unknown button {other}: left, right or middle")),
    }
}

/// How long a file may take: `timeout` seconds, 5 minutes by default.
fn file_wait(args: &Value) -> Result<std::time::Duration, String> {
    let secs = int_arg(args, "timeout")?.unwrap_or(300);
    if !(1..=3600).contains(&secs) {
        return Err("`timeout` is 1 to 3600 seconds".into());
    }
    Ok(std::time::Duration::from_secs(secs as u64))
}

fn size_text(bytes: u64) -> String {
    match bytes {
        b if b < 1 << 10 => format!("{b} bytes"),
        b if b < 1 << 20 => format!("{:.1} KB", b as f64 / 1024.0),
        b if b < 1 << 30 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b => format!("{:.2} GB", b as f64 / (1 << 30) as f64),
    }
}

/// Where a fetched file may go: an absolute path, not taken yet, in a folder that
/// exists. Checked before fetching, so a bad one costs no transfer.
fn check_save_as(to: &std::path::Path) -> Result<(), String> {
    if !to.is_absolute() {
        return Err("`save_as` must be an absolute path on this computer".into());
    }
    if to.symlink_metadata().is_ok() {
        return Err(format!(
            "{} already exists; NexaDesk does not replace files",
            to.display()
        ));
    }
    match to.parent() {
        Some(dir) if dir.is_dir() => Ok(()),
        _ => Err(format!("the folder for {} does not exist", to.display())),
    }
}

/// Moves `from` to `to`, which must not exist: a rename, or a copy across disks.
fn move_file(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    check_save_as(to).map_err(std::io::Error::other)?;
    if std::fs::hard_link(from, to).is_ok() {
        return std::fs::remove_file(from);
    }
    // Another disk: copy into a new file (never over one), then drop the original.
    let mut src = std::fs::File::open(from)?;
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    if let Err(e) = std::io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all()) {
        drop(dst);
        let _ = std::fs::remove_file(to);
        return Err(e);
    }
    std::fs::remove_file(from)
}

fn text(s: &str) -> Value {
    json!({ "type": "text", "text": s })
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn int_arg(args: &Value, key: &str) -> Result<Option<i64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a whole number")),
    }
}

fn session_arg(args: &Value) -> Result<Option<u64>, String> {
    int_arg(args, "session")?
        .map(|n| u64::try_from(n).map_err(|_| "`session` must be positive".to_owned()))
        .transpose()
}

/// `ref`, or `x` and `y`: where an action happens. `prefix` is `""`, `"from_"` or `"to_"`.
fn target_arg(args: &Value, prefix: &str) -> Result<Target, String> {
    if let Some(r) = str_arg(args, &format!("{prefix}ref"))? {
        return Ok(Target::Ref(r.to_owned()));
    }
    let num = |k: String| {
        args.get(&k)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("`{prefix}ref` or `{prefix}x` and `{prefix}y` are required"))
    };
    Ok(Target::Point(
        num(format!("{prefix}x"))?,
        num(format!("{prefix}y"))?,
    ))
}

fn target_text(t: &Target) -> String {
    match t {
        Target::Ref(r) => r.clone(),
        Target::Point(x, y) => format!("({x}, {y})"),
    }
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "nexadesk", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tools() -> Vec<Value> {
    let session = json!({
        "type": "integer",
        "description": "Session number from nexa_connect. May be left out while only one session is open; with several, leaving it out is an error that says so.",
    });
    let then = json!({
        "type": "string",
        "enum": ["none", "ui", "screenshot"],
        "description": "What else to return once the action is done and the screen has settled: none (default: a line on what changed, what is in front and what has focus), ui (the new elements and refs) or screenshot.",
    });
    let r = json!({ "type": "string", "description": "A ref from the last nexa_ui, like @e3: where to act. Or give x and y." });
    let x = json!({ "type": "number", "description": "Pixels from the left (as nexa_ui positions and nexa_screenshot give them)." });
    let y = json!({ "type": "number", "description": "Pixels from the top." });
    vec![
        json!({
            "name": "nexa_devices",
            "description": "List the computers the person here allowed you to operate, by name, with how NexaDesk signs in to each.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "nexa_connect",
            "description": "Connect to one of the computers nexa_devices lists. NexaDesk signs in itself (you never handle a password or code); the person at that computer is asked to allow it and sees that an AI agent is connected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device": { "type": "string", "description": "Its name as nexa_devices gives it (or its address or device ID)." },
                },
                "required": ["device"],
            },
        }),
        json!({
            "name": "nexa_sessions",
            "description": "List the open sessions.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "nexa_ui",
            "description": "Read the frontmost window: one line per element you can act on, with a ref (@e1, @e2…), its role, name, value, state (focused, checked, disabled…) and position (x,y w×h in pixels); plus what app is in front and what has the keyboard focus. Start here; act by ref. Refs hold until the next nexa_ui.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "all": { "type": "boolean", "description": "Also list text and headings, not only what can be acted on (default false)." },
                    "max_nodes": { "type": "integer", "minimum": 1, "maximum": 2000, "description": "At most this many lines (default 300)." },
                },
            },
        }),
        json!({
            "name": "nexa_screenshot",
            "description": "A lossless picture of the remote screen, the pointer drawn in. For what the elements do not show: pictures, canvases, games, apps whose elements cannot be read, or to check how something looks. Its pixels are the coordinates actions take.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "marks": { "type": "boolean", "description": "Box and number every element you can act on, from a fresh element tree whose refs (@e1 is box 1) replace the last ones; the tree comes too. For telling which ref is which on the screen." },
                },
            },
        }),
        json!({
            "name": "nexa_click",
            "description": "Click an element (ref) or a point (x, y). Waits until it is done and the screen has settled.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session, "ref": r, "x": x, "y": y,
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "Default left." },
                    "count": { "type": "integer", "minimum": 1, "maximum": 3, "description": "2 for a double click. Default 1." },
                    "then": then,
                },
            },
        }),
        json!({
            "name": "nexa_type",
            "description": format!("Type text, as characters (independent of the remote keyboard layout). With ref, click that field first; otherwise it goes to whatever has the focus. At most {MAX_TEXT} characters. For Enter, Tab or shortcuts use nexa_key; for long text nexa_clipboard_set and a paste is faster."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "text": { "type": "string" },
                    "ref": { "type": "string", "description": "A field's ref from the last nexa_ui, to click before typing." },
                    "then": then,
                },
                "required": ["text"],
            },
        }),
        json!({
            "name": "nexa_press",
            "description": "Do an element's own action through the remote computer's accessibility API: press a button, toggle a checkbox, choose a row or menu item, open a pop-up. The pointer does not move and the element need not be uncovered. For when a click does not land, or the pointer should stay where it is; elements an app draws itself may not have one: click those.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "ref": { "type": "string", "description": "A ref from the last nexa_ui, like @e3." },
                    "then": then,
                },
                "required": ["ref"],
            },
        }),
        json!({
            "name": "nexa_set_value",
            "description": format!("Replace a text field's whole text with `text` through the accessibility API, in one go: no typing, no keyboard layout or input method in the way, the pointer does not move. At most {MAX_TEXT} characters. Some fields only notice typing (search-as-you-type, forms that check each key): use nexa_type for those."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "ref": { "type": "string", "description": "A field's ref from the last nexa_ui." },
                    "text": { "type": "string" },
                    "then": then,
                },
                "required": ["ref", "text"],
            },
        }),
        json!({
            "name": "nexa_key",
            "description": "Press a key or a combination, named for the remote system: \"enter\", \"esc\", \"tab\", \"cmd+c\" (Mac), \"ctrl+c\" (Windows), \"alt+f4\", \"cmd+shift+4\", \"pagedown\", \"f5\". Modifiers: ctrl, shift, alt (option), cmd (Command on a Mac, the Windows key on Windows).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "keys": { "type": "string" },
                    "then": then,
                },
                "required": ["keys"],
            },
        }),
        json!({
            "name": "nexa_scroll",
            "description": "Scroll with the pointer over an element (ref) or a point.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session, "ref": r, "x": x, "y": y,
                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "amount": { "type": "integer", "minimum": 1, "maximum": 30, "description": "Wheel notches. Default 3." },
                    "then": then,
                },
                "required": ["direction"],
            },
        }),
        json!({
            "name": "nexa_drag",
            "description": "Press at one element or point, move to another in small steps (about 0.2 s in all) and let go there: drag and drop, select text, move a window by its title bar.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "from_ref": { "type": "string", "description": "Where to press: a ref. Or from_x and from_y." },
                    "from_x": { "type": "number" }, "from_y": { "type": "number" },
                    "to_ref": { "type": "string", "description": "Where to let go: a ref. Or to_x and to_y." },
                    "to_x": { "type": "number" }, "to_y": { "type": "number" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "Default left." },
                    "then": then,
                },
            },
        }),
        json!({
            "name": "nexa_display",
            "description": "Without `display`: list the remote computer's displays, numbered from 0, and which is shown. With it: show that display instead (one at a time); its coordinates may differ, and earlier refs no longer hold.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "display": { "type": "integer", "minimum": 0, "description": "A display number from the list (the first display is 0)." },
                },
            },
        }),
        json!({
            "name": "nexa_run",
            "description": "Run a command on the remote computer's command line and return its output and exit status: the user's login shell on a Mac or Linux, PowerShell on Windows. One shell per session, so the directory and variables carry over. Standard input is empty, so commands that would wait for typing end instead. Needs the person there to have allowed the command line.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "command": { "type": "string" },
                    "timeout": { "type": "integer", "minimum": 1, "maximum": 600, "description": "Seconds before it is interrupted. Default 30." },
                },
                "required": ["command"],
            },
        }),
        json!({
            "name": "nexa_clipboard_get",
            "description": "The text on the remote computer's clipboard (what was copied there last). Needs the clipboard to have been allowed.",
            "inputSchema": { "type": "object", "properties": { "session": session } },
        }),
        json!({
            "name": "nexa_clipboard_set",
            "description": "Put text on the remote computer's clipboard; then paste it with nexa_key (cmd+v on a Mac, ctrl+v on Windows). Faster than nexa_type for long text. Needs the clipboard to have been allowed.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "text": { "type": "string" } },
                "required": ["text"],
            },
        }),
        json!({
            "name": "nexa_send_file",
            "description": "Send a file from this computer to the remote one, and wait until it is there. It lands in the NexaDesk folder in Downloads there (~/Downloads/NexaDesk on a Mac); move it with nexa_run if it belongs elsewhere. A file over 256 MB waits for the person there to accept it. Needs files to have been allowed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "path": { "type": "string", "description": "Absolute path of a file on this computer." },
                    "timeout": { "type": "integer", "minimum": 1, "maximum": 3600, "description": "Seconds before it is cancelled. Default 300." },
                },
                "required": ["path"],
            },
        }),
        json!({
            "name": "nexa_receive_file",
            "description": "Fetch a file from the remote computer to this one, and wait until it is here. Give its absolute path there (find it with nexa_run if needed). Needs both files and the command line to have been allowed: the command line reads every file there anyway. The file is checked against a checksum before it is kept.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "path": { "type": "string", "description": "Absolute path of a file on the remote computer, e.g. /Users/me/report.pdf or C:\\Users\\me\\report.pdf." },
                    "save_as": { "type": "string", "description": "Absolute path on this computer to save it as; must not exist yet. Default: NexaDesk's folder for AI downloads (the result says where)." },
                    "timeout": { "type": "integer", "minimum": 1, "maximum": 3600, "description": "Seconds before it is cancelled. Default 300." },
                },
                "required": ["path"],
            },
        }),
        json!({
            "name": "nexa_disconnect",
            "description": "End a session.",
            "inputSchema": { "type": "object", "properties": { "session": session } },
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_context_line_says_what_changed_and_what_is_in_front() {
        let rep = ObserveReply {
            settled: true,
            changed: true,
            waited_ms: 600,
            app: "TextEdit".into(),
            window: "Untitled".into(),
            focused: "textarea".into(),
            has_pointer: true,
            pointer_x: 10,
            pointer_y: 20,
            ..Default::default()
        };
        assert_eq!(
            context(&rep, Some("Clicked @e2")),
            "Clicked @e2. The screen changed and settled after 0.6 s. In front: TextEdit — \"Untitled\". \
             Keyboard focus: textarea. Pointer at (10, 20)."
        );
        let moving = ObserveReply {
            settled: false,
            waited_ms: 3000,
            ..Default::default()
        };
        assert!(context(&moving, Some("Pressed enter")).contains("still changing after 3.0 s"));
        assert_eq!(context(&ObserveReply::default(), None), "");
    }
}
