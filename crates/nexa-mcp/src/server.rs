//! The MCP side: JSON-RPC 2.0 over stdio, one JSON message per line (the MCP stdio
//! transport), and the tools (docs/MCP.md §3.1).
//!
//! Requests are handled one at a time, in order. Tool failures come back as results
//! with `isError` (the agent can read and act on them); protocol mistakes as JSON-RPC
//! errors.

use std::io::{BufRead, Write};

use base64::Engine;
use serde_json::{Value, json};

use crate::{
    keys,
    viewer::{Button, MAX_TEXT, Remote, Shot, Viewer},
};

/// Protocol versions this server speaks; the newest is offered to clients asking for
/// one it does not know.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Operate another computer through NexaDesk. Connect with nexa_connect \
(the person at that computer may have to allow it), then call nexa_screenshot and act with \
nexa_click, nexa_scroll, nexa_type and nexa_key. Coordinates are pixels in the latest \
screenshot. Each action waits for the screen to settle and returns a new screenshot. Key \
names are the remote computer's own: cmd is the Command key on a Mac and the Windows key on \
Windows. The person at the remote computer sees that a session is running and can end it at \
any time.";

pub struct Server {
    viewer: Viewer,
}

impl Server {
    pub fn new(viewer: Viewer) -> Self {
        Self { viewer }
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
            "initialize" => ok(id, initialize(&params)),
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
            "nexa_connect" => {
                let address = str_arg(args, "address")?.ok_or("`address` is required")?;
                let code = str_arg(args, "code")?;
                let r = v.connect(address, code)?;
                Ok(vec![text(&describe(&r))])
            }
            "nexa_sessions" => {
                let list = v.sessions();
                if list.is_empty() {
                    return Ok(vec![text("No sessions.")]);
                }
                Ok(vec![text(
                    &list
                        .iter()
                        .map(|r| describe(r))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )])
            }
            "nexa_disconnect" => {
                let r = v.remote(session_arg(args)?)?;
                v.disconnect(&r);
                Ok(vec![text(&format!(
                    "Disconnected session {} ({}).",
                    r.id, r.host.host_name
                ))])
            }
            "nexa_screenshot" => {
                let r = v.remote(session_arg(args)?)?;
                let shot = v.screenshot(&r)?;
                Ok(shot_content(&shot, "Screenshot"))
            }
            "nexa_click" => {
                let r = v.remote(session_arg(args)?)?;
                let (x, y) = point_arg(args)?;
                let button = match str_arg(args, "button")?.unwrap_or("left") {
                    "left" => Button::Left,
                    "right" => Button::Right,
                    "middle" => Button::Middle,
                    other => return Err(format!("unknown button {other}: left, right or middle")),
                };
                let count = int_arg(args, "count")?.unwrap_or(1);
                if !(1..=3).contains(&count) {
                    return Err("`count` is 1, 2 (double click) or 3".into());
                }
                let since = v.click(&r, x, y, button, count as u32)?;
                self.after(&r, args, since, &format!("Clicked at ({x}, {y})"))
            }
            "nexa_scroll" => {
                let r = v.remote(session_arg(args)?)?;
                let (x, y) = point_arg(args)?;
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
                let since = v.scroll(&r, x, y, dx, dy)?;
                self.after(&r, args, since, &format!("Scrolled at ({x}, {y})"))
            }
            "nexa_type" => {
                let r = v.remote(session_arg(args)?)?;
                let t = str_arg(args, "text")?.ok_or("`text` is required")?;
                let since = v.type_text(&r, t)?;
                self.after(
                    &r,
                    args,
                    since,
                    &format!("Typed {} characters", t.chars().count()),
                )
            }
            "nexa_key" => {
                let r = v.remote(session_arg(args)?)?;
                let combo = str_arg(args, "keys")?.ok_or("`keys` is required")?;
                let usages = keys::parse(combo)?;
                let since = v.keys(&r, &usages)?;
                self.after(&r, args, since, &format!("Pressed {combo}"))
            }
            _ => Err(format!("unknown tool {name}")),
        }
    }

    /// What an action returns: what was done, and the screen after it unless the
    /// caller said `screenshot: false`.
    fn after(
        &self,
        r: &Remote,
        args: &Value,
        since: std::time::Instant,
        done: &str,
    ) -> Result<Vec<Value>, String> {
        if args.get("screenshot").and_then(Value::as_bool) == Some(false) {
            return Ok(vec![text(&format!("{done}."))]);
        }
        match self.viewer.after(r, since) {
            Ok(shot) => Ok(shot_content(&shot, &format!("{done}. Screenshot"))),
            Err(e) => Ok(vec![text(&format!("{done}, but no screenshot: {e}"))]),
        }
    }
}

fn describe(r: &Remote) -> String {
    let perms = r.host.perms.clone().unwrap_or_default();
    let mut allowed = Vec::new();
    if perms.view {
        allowed.push("view");
    }
    if perms.input {
        allowed.push("keyboard and mouse");
    }
    let displays = r
        .host
        .displays
        .iter()
        .map(|d| format!("{} {}×{}", d.name, d.w, d.h))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Session {}: {} ({}) at {}. Allowed: {}. Displays: {}.",
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

fn shot_content(shot: &Shot, what: &str) -> Vec<Value> {
    let note = if shot.settled {
        ""
    } else {
        " The screen was still changing; take another screenshot to see where it ends up."
    };
    vec![
        text(&format!(
            "{what} {}×{} pixels; coordinates for actions are pixels in this image.{note}",
            shot.width, shot.height
        )),
        json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(&shot.png),
            "mimeType": "image/png",
        }),
    ]
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

fn point_arg(args: &Value) -> Result<(f64, f64), String> {
    let num = |k: &str| {
        args.get(k)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("`{k}` is required (a number)"))
    };
    Ok((num("x")?, num("y")?))
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
        "description": "Session number from nexa_connect. May be left out while only one session is open.",
    });
    let screenshot = json!({
        "type": "boolean",
        "description": "Return a screenshot once the screen settles (default true).",
    });
    let x = json!({ "type": "number", "description": "Pixels from the left of the latest screenshot." });
    let y =
        json!({ "type": "number", "description": "Pixels from the top of the latest screenshot." });
    vec![
        json!({
            "name": "nexa_connect",
            "description": "Connect to another computer running NexaDesk. The person there is usually asked to allow it, and sees the session while it runs. A computer paired before connects without a code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "address": { "type": "string", "description": "host:port on the network, or the 9-digit device ID the computer shows." },
                    "code": { "type": "string", "description": "The one-time code the computer shows. Needed the first time, unless connecting by device ID and asking the person there." },
                },
                "required": ["address"],
            },
        }),
        json!({
            "name": "nexa_sessions",
            "description": "List the open sessions.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "nexa_screenshot",
            "description": "See the remote computer's screen. Coordinates of later actions are pixels in this image.",
            "inputSchema": { "type": "object", "properties": { "session": session } },
        }),
        json!({
            "name": "nexa_click",
            "description": "Move the pointer to a point and click there.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session, "x": x, "y": y,
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "Default left." },
                    "count": { "type": "integer", "minimum": 1, "maximum": 3, "description": "2 for a double click. Default 1." },
                    "screenshot": screenshot,
                },
                "required": ["x", "y"],
            },
        }),
        json!({
            "name": "nexa_scroll",
            "description": "Scroll with the pointer at a point.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session, "x": x, "y": y,
                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "amount": { "type": "integer", "minimum": 1, "maximum": 30, "description": "Wheel notches. Default 3." },
                    "screenshot": screenshot,
                },
                "required": ["x", "y", "direction"],
            },
        }),
        json!({
            "name": "nexa_type",
            "description": format!("Type text where the keyboard focus is, as characters (independent of the remote keyboard layout). At most {MAX_TEXT} characters. For Enter, Tab or shortcuts use nexa_key."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "text": { "type": "string" },
                    "screenshot": screenshot,
                },
                "required": ["text"],
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
                    "screenshot": screenshot,
                },
                "required": ["keys"],
            },
        }),
        json!({
            "name": "nexa_disconnect",
            "description": "End a session.",
            "inputSchema": { "type": "object", "properties": { "session": session } },
        }),
    ]
}
