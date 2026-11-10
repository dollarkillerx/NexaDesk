//! The controlling terminal is the only UI for now (§9.3): consent prompts, the
//! running-session line and the way to end a session.
//!
//! It reads `/dev/tty`, never stdin, so `yes | nexa-agent` cannot answer a prompt. With
//! no controlling terminal there is no one to ask: `can_present` is false and every
//! connection is refused.

use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::fd::AsRawFd,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering::Relaxed},
        mpsc::{self, RecvTimeoutError, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use nexa_agent::{AgentEvent, AgentUi};
use nexa_session::handshake::{AuthKind, Consent, ConsentAnswer, ConsentRequest};
use nexa_wire::control::Permissions;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Disconnect,
    RotateCode,
    Quit,
}

struct Terminal {
    out: Mutex<File>,
    fd: i32,
    pending: Mutex<Option<SyncSender<String>>>,
    alive: AtomicBool,
    prompt_lock: Mutex<()>,
}

pub struct TerminalUi {
    terminal: Option<Arc<Terminal>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn describe(perms: &Permissions) -> String {
    let mut parts = Vec::new();
    for (on, name) in [
        (perms.view, "view"),
        (perms.input, "input"),
        (perms.clipboard, "clipboard"),
        (perms.file, "file"),
        (perms.audio, "audio"),
    ] {
        if on {
            parts.push(name);
        }
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(" + ")
    }
}

fn perms(view: bool, input: bool, clipboard: bool) -> Permissions {
    Permissions {
        view,
        input,
        clipboard,
        file: false,
        audio: false,
    }
}

/// Turns the reply to the permission question into a grant. Anything unrecognized denies.
fn parse_grant(line: &str, proposed: &Permissions) -> Option<Permissions> {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(proposed.clone()),
        "v" => Some(perms(true, false, false)),
        "c" => Some(perms(true, false, true)),
        "i" => Some(perms(true, true, false)),
        "a" => Some(perms(true, true, true)),
        _ => None,
    }
}

fn is_yes(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn group_code(code: &str) -> String {
    match code.len() {
        8 => format!("{}-{}", &code[..4], &code[4..]),
        _ => code.to_owned(),
    }
}

impl Terminal {
    /// Text can carry peer-supplied strings (a close reason, say), so no control
    /// characters reach the terminal from here.
    fn say(&self, text: &str) {
        let line: String = text.chars().filter(|c| !c.is_control()).collect();
        let mut out = lock(&self.out);
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }

    /// Reads one reply, or `None` on timeout, a dead terminal or a cancelled request.
    fn reply(
        &self,
        rx: &mpsc::Receiver<String>,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Option<String> {
        loop {
            if cancelled.load(Relaxed) || !self.alive.load(Relaxed) {
                return None;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            match rx.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(line) => return Some(line),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

impl TerminalUi {
    /// Opens the controlling terminal and starts reading it. Commands go to `commands`.
    pub fn open(commands: impl Fn(Command) + Send + 'static) -> Self {
        let Ok(input) = OpenOptions::new().read(true).open("/dev/tty") else {
            return Self { terminal: None };
        };
        let Ok(out) = OpenOptions::new().write(true).open("/dev/tty") else {
            return Self { terminal: None };
        };
        let terminal = Arc::new(Terminal {
            fd: input.as_raw_fd(),
            out: Mutex::new(out),
            pending: Mutex::new(None),
            alive: AtomicBool::new(true),
            prompt_lock: Mutex::new(()),
        });
        let t = terminal.clone();
        let _ = thread::Builder::new()
            .name("terminal".into())
            .spawn(move || {
                for line in BufReader::new(input).lines() {
                    let Ok(line) = line else { break };
                    if let Some(tx) = lock(&t.pending).as_ref() {
                        let _ = tx.try_send(line);
                        continue;
                    }
                    match line.trim() {
                        "d" => commands(Command::Disconnect),
                        "r" => commands(Command::RotateCode),
                        "q" => commands(Command::Quit),
                        "" => {}
                        _ => t.say("commands: d = end session, r = new code, q = quit"),
                    }
                }
                t.alive.store(false, Relaxed);
            });
        Self {
            terminal: Some(terminal),
        }
    }

    pub fn has_terminal(&self) -> bool {
        self.terminal.is_some()
    }
}

impl Consent for TerminalUi {
    fn can_present(&self) -> bool {
        self.terminal
            .as_ref()
            .is_some_and(|t| t.alive.load(Relaxed))
    }

    fn ask(&self, request: &ConsentRequest, timeout: Duration) -> ConsentAnswer {
        let Some(terminal) = &self.terminal else {
            return ConsentAnswer::deny();
        };
        let _one_at_a_time = lock(&terminal.prompt_lock);
        if request.cancelled.load(Relaxed) {
            return ConsentAnswer::deny();
        }
        let deadline = Instant::now() + timeout;
        // Nothing typed before this point may answer the prompt.
        // SAFETY: `fd` stays open for as long as the reader thread holds the terminal.
        unsafe {
            libc::tcflush(terminal.fd, libc::TCIFLUSH);
        }

        let method = match request.auth {
            AuthKind::Paired => "paired device",
            AuthKind::Code => "one-time code",
            AuthKind::Password => "password",
        };
        terminal.say("");
        terminal.say("---------------------------------------------------------");
        terminal.say(&format!(
            "Connection request from \"{}\" (device {}) at {}",
            request.device_name,
            request.peer.device_id(),
            request.remote
        ));
        terminal.say(&format!("Verified by: {method}"));
        if request.takeover {
            terminal.say("Accepting ENDS the session that is running now.");
        }
        terminal.say(&format!(
            "Proposed permissions: {}",
            describe(&request.prompt.perms)
        ));
        terminal.say("  y = allow as proposed   v = view only   c = view + clipboard");
        terminal.say("  i = view + input        a = view + input + clipboard");
        terminal.say("  anything else = deny");
        terminal.say(&format!("Allow? (answer within {} s)", timeout.as_secs()));

        let (tx, rx) = mpsc::sync_channel(4);
        *lock(&terminal.pending) = Some(tx);
        let answer = (|| {
            let line = terminal.reply(&rx, deadline, &request.cancelled)?;
            let granted = parse_grant(&line, &request.prompt.perms)?;
            let mut answer = ConsentAnswer {
                allow: true,
                perms: granted,
                save: false,
                skip_next_time: false,
            };
            if request.prompt.offer_save {
                let default = if request.prompt.save_default {
                    "Y/n"
                } else {
                    "y/N"
                };
                terminal.say(&format!("Save this device for next time? [{default}]"));
                let line = terminal.reply(&rx, deadline, &request.cancelled);
                answer.save = match line.as_deref().map(str::trim) {
                    Some("") | None => request.prompt.save_default,
                    Some(s) => is_yes(s),
                };
            }
            if request.prompt.offer_skip_next_time {
                terminal.say("Connect without asking next time? [y/N]");
                answer.skip_next_time = terminal
                    .reply(&rx, deadline, &request.cancelled)
                    .is_some_and(|l| is_yes(&l));
            }
            Some(answer)
        })();
        *lock(&terminal.pending) = None;
        match answer {
            Some(a) => a,
            None => {
                terminal.say("Denied.");
                ConsentAnswer::deny()
            }
        }
    }
}

impl AgentUi for TerminalUi {
    fn event(&self, event: AgentEvent) {
        let Some(t) = &self.terminal else {
            if let AgentEvent::Listening(addr) = event {
                eprintln!(
                    "nexa-agent: listening on {addr}, but there is no controlling terminal: every connection will be refused"
                );
            }
            return;
        };
        match event {
            AgentEvent::Listening(addr) => t.say(&format!(
                "nexa-agent listening on {addr} (commands: d = end session, r = new code, q = quit)"
            )),
            AgentEvent::CodeChanged(code) => {
                t.say(&format!("One-time pairing code: {}", group_code(&code)));
            }
            AgentEvent::SessionStarted(s) => {
                t.say(&format!(
                    "*** SESSION ACTIVE: \"{}\" (device {}) at {} with {} ***",
                    s.device_name,
                    s.device_id,
                    s.remote,
                    describe(&s.perms)
                ));
                t.say("*** type d and press Enter to end it ***");
                if s.perms.input {
                    t.say("*** this device can control your keyboard and mouse ***");
                }
            }
            AgentEvent::SessionEnded {
                device_name,
                reason,
                ..
            } => t.say(&format!("Session with \"{device_name}\" ended: {reason}")),
            AgentEvent::ConnectionFailed { remote, reason } => {
                t.say(&format!("Connection from {remote} failed: {reason}"));
            }
            AgentEvent::ClipboardNotice(n) => t.say(&format!("Clipboard: {n:?}")),
            AgentEvent::Warning(w) => t.say(&format!("Warning: {w}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants() {
        let proposed = perms(true, false, false);
        assert_eq!(parse_grant("y", &proposed), Some(proposed.clone()));
        assert_eq!(parse_grant(" A ", &proposed), Some(perms(true, true, true)));
        assert_eq!(parse_grant("i", &proposed), Some(perms(true, true, false)));
    }

    #[test]
    fn anything_else_denies() {
        let proposed = perms(true, true, false);
        for line in ["", "n", "yep", "no", "1", " "] {
            assert_eq!(parse_grant(line, &proposed), None);
        }
    }

    #[test]
    fn code_grouping() {
        assert_eq!(group_code("ABCD2345"), "ABCD-2345");
    }
}
