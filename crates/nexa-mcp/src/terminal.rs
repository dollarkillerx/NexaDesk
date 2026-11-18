//! Commands on the host's command line (docs/MCP.md §3.5): one shell per session, kept
//! between commands so the directory and variables carry over.
//!
//! Each command is followed by a line that prints an end marker with its exit status.
//! The marker is printed from pieces (`printf '%s_%s' __NEXA_END <nonce>`), so it
//! appears whole only in the output, never in an echo of what was typed. On a Mac or
//! Linux host the shell is told not to echo or prompt, and the command's standard
//! input is empty, so a command that would wait for typing ends instead. On Windows
//! (PowerShell, which echoes) the echoed lines and prompts are taken out.

use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use nexa_client::{
    client::Session,
    terminal::{self, TermOut},
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Output kept while a command runs; older output is dropped beyond it.
const KEEP: usize = 4 << 20;
/// Output returned for one command; beyond it, its end is kept.
pub const MAX_OUTPUT: usize = 64 << 10;
/// After a timeout and Ctrl+C, how long the shell gets to come back.
const AFTER_INTERRUPT: Duration = Duration::from_secs(3);
const MARK: &str = "__NEXA_END";

#[derive(Default)]
struct Out {
    buf: Vec<u8>,
    exited: Option<String>,
}

type Shared = Arc<(Mutex<Out>, Condvar)>;

pub struct Shell {
    id: u64,
    out: Shared,
    windows: bool,
}

/// How a command went.
#[derive(Debug, PartialEq, Eq)]
pub struct Ran {
    pub output: String,
    /// The exit status; `None` when it timed out and the shell did not say.
    pub status: Option<i32>,
    pub timed_out: bool,
    /// Output was longer than [`MAX_OUTPUT`]; its end is kept.
    pub truncated: bool,
}

impl Shell {
    /// Opens a shell on the host and waits for it to answer. `windows`: the host is
    /// Windows, whose shell is PowerShell.
    pub fn open(session: &Session, windows: bool, ready_within: Duration) -> Result<Self, String> {
        let out: Shared = Arc::new((Mutex::new(Out::default()), Condvar::new()));
        let sink = out.clone();
        let (id, link) = terminal::start(
            terminal::size(200, 50),
            Box::new(move |o| {
                let (m, cv) = &*sink;
                let mut out = lock(m);
                match o {
                    TermOut::Data(d) => {
                        out.buf.extend_from_slice(d);
                        if out.buf.len() > KEEP {
                            let cut = out.buf.len() - KEEP;
                            out.buf.drain(..cut);
                        }
                    }
                    TermOut::Exit(why) => out.exited = Some(why.to_owned()),
                    TermOut::Backlog => {}
                }
                cv.notify_all();
                0
            }),
        );
        session.open_terminal(link);
        let shell = Self { id, out, windows };
        if !windows {
            // No echo, no line editor (which echoes by itself), no prompts, no colours.
            shell.write(
                " stty -echo 2>/dev/null; set +o emacs 2>/dev/null; set +o vi 2>/dev/null; \
                 unsetopt zle 2>/dev/null; export TERM=dumb PS1='' PS2='' PROMPT='' RPROMPT='' \
                 PROMPT_EOL_MARK='' PROMPT_COMMAND='' NO_COLOR=1\r",
            );
        }
        match shell.run(if windows { "$null" } else { "true" }, ready_within) {
            Ok(r) if !r.timed_out => Ok(shell),
            Ok(_) => {
                shell.close();
                Err("the host's command line did not answer".into())
            }
            Err(e) => {
                shell.close();
                Err(e)
            }
        }
    }

    pub fn exited(&self) -> bool {
        lock(&self.out.0).exited.is_some()
    }

    fn write(&self, text: &str) {
        terminal::write(self.id, text.as_bytes());
    }

    /// Runs `command` and returns its output once its end marker arrives, or after
    /// `timeout`, when it is interrupted with Ctrl+C.
    pub fn run(&self, command: &str, timeout: Duration) -> Result<Ran, String> {
        let nonce = nonce();
        let marker = format!("{MARK}_{nonce} ");
        {
            let mut out = lock(&self.out.0);
            if let Some(why) = &out.exited {
                return Err(exit_text(why));
            }
            out.buf.clear();
        }
        self.write(&wrap(command, &nonce, self.windows));
        let deadline = Instant::now() + timeout;
        if let Some(done) = self.wait_for(&marker, deadline)? {
            return Ok(done);
        }
        // Still running: interrupt it. The marker line, already typed, then runs.
        self.write("\x03");
        let late = self.wait_for(&marker, Instant::now() + AFTER_INTERRUPT)?;
        let (output, truncated) = match late {
            Some(r) => (r.output, r.truncated),
            None => clean(
                &String::from_utf8_lossy(&lock(&self.out.0).buf),
                self.windows,
            ),
        };
        Ok(Ran {
            output,
            status: None,
            timed_out: true,
            truncated,
        })
    }

    /// The command's result once `marker` and a status follow it, `None` at `deadline`.
    fn wait_for(&self, marker: &str, deadline: Instant) -> Result<Option<Ran>, String> {
        let (m, cv) = &*self.out;
        let mut out = lock(m);
        loop {
            let text = String::from_utf8_lossy(&out.buf).into_owned();
            if let Some(r) = finished(&text, marker, self.windows) {
                return Ok(Some(r));
            }
            if let Some(why) = &out.exited {
                return Err(exit_text(why));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            out = cv
                .wait_timeout(out, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Ends the shell (and what it started) on the host.
    pub fn close(&self) {
        terminal::close(self.id);
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.close();
    }
}

fn exit_text(why: &str) -> String {
    if why == nexa_session::terminal::EXIT_DENIED {
        return "the host did not allow its command line (ask the person there to allow it)".into();
    }
    format!("the host's command line ended: {why}")
}

/// The line typed for `command`, followed by the marker line.
fn wrap(command: &str, nonce: &str, windows: bool) -> String {
    if windows {
        format!(
            "& {{ {command} }}; $nexaOk = $?; Write-Output ('{MARK}' + '_{nonce} ' + \
             $(if ($nexaOk) {{ 0 }} else {{ 1 }}))\r"
        )
    } else {
        // Braces keep `cd` and variables in this shell; stdin is empty.
        format!("{{ {command}\r}} </dev/null; printf '\\n%s_%s %d\\n' {MARK} {nonce} \"$?\"\r")
    }
}

/// The result if the output holds the marker and a status after it.
fn finished(text: &str, marker: &str, windows: bool) -> Option<Ran> {
    let at = text.find(marker)?;
    let rest = &text[at + marker.len()..];
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    // The status is complete once something follows it.
    if digits.is_empty() || rest.len() == digits.len() {
        return None;
    }
    let (output, truncated) = clean(&text[..at], windows);
    Some(Ran {
        output,
        status: digits.parse().ok(),
        timed_out: false,
        truncated,
    })
}

/// Plain text: no terminal sequences, no carriage returns, no echo or prompts, at most
/// [`MAX_OUTPUT`] bytes (the end kept).
fn clean(raw: &str, windows: bool) -> (String, bool) {
    let text = strip_ansi(raw).replace('\r', "");
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| {
            // What the shell echoed of the typed lines, and PowerShell's prompts.
            !(l.contains(&format!("{MARK}' +"))
                || l.contains(&format!("printf '\\n%s_%s %d\\n' {MARK}"))
                || (windows && l.starts_with("PS ") && l.trim_end().ends_with('>')))
        })
        .collect();
    let joined = lines.join("\n");
    let trimmed = joined.trim_matches('\n').to_owned();
    if trimmed.len() <= MAX_OUTPUT {
        return (trimmed, false);
    }
    let mut cut = trimmed.len() - MAX_OUTPUT;
    while !trimmed.is_char_boundary(cut) {
        cut += 1;
    }
    (trimmed[cut..].to_owned(), true)
}

/// Removes CSI, OSC and other escape sequences.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            if !c.is_control() || c == '\n' || c == '\t' || c == '\r' {
                out.push(c);
            }
            continue;
        }
        match chars.next() {
            // CSI: parameters, then a final byte in @..~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: to BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Charset selection and the like: one more character.
            Some('(' | ')' | '#' | '%') => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    let n = COUNT.fetch_add(1, Relaxed);
    format!(
        "{:x}",
        t ^ n.rotate_left(32) ^ u64::from(std::process::id())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_is_found_only_whole_and_with_a_status() {
        let m = format!("{MARK}_abc ");
        assert!(finished("output\n", &m, false).is_none());
        // The echo of the typed line has the pieces apart.
        assert!(finished("printf '\\n%s_%s %d\\n' __NEXA_END abc \"$?\"\n", &m, false).is_none());
        assert!(
            finished(&format!("out\n{m}0"), &m, false).is_none(),
            "status not complete yet"
        );
        let r = finished(&format!("hello\nworld\n\n{m}3\n"), &m, false).unwrap();
        assert_eq!(r.output, "hello\nworld");
        assert_eq!(r.status, Some(3));
        assert!(!r.timed_out);
    }

    #[test]
    fn windows_echo_and_prompts_are_taken_out() {
        let m = format!("{MARK}_n1 ");
        let raw = format!(
            "\x1b[?25lPS C:\\Users\\me> & {{ dir }}; $nexaOk = $?; Write-Output ('{MARK}' + '_n1 ' + 0)\r\n\
             \x1b[32mfile.txt\x1b[0m\r\n{m}0\r\nPS C:\\Users\\me> "
        );
        let r = finished(&raw, &m, true).unwrap();
        assert_eq!(r.output, "file.txt");
        assert_eq!(r.status, Some(0));
    }

    #[test]
    fn the_wrapped_line_keeps_the_shell_and_empties_stdin() {
        let unix = wrap("cd /tmp", "n2", false);
        assert!(unix.starts_with("{ cd /tmp\r} </dev/null; printf"));
        assert!(unix.contains("__NEXA_END n2 \"$?\""));
        assert!(
            !unix.contains("__NEXA_END_n2"),
            "the marker is never typed whole"
        );
        let win = wrap("Get-Location", "n3", true);
        assert!(win.contains("& { Get-Location }"));
        assert!(!win.contains("__NEXA_END_n3"));
    }

    #[test]
    fn long_output_keeps_its_end() {
        let raw = "x".repeat(MAX_OUTPUT + 10) + "END";
        let (out, truncated) = clean(&raw, false);
        assert!(truncated);
        assert_eq!(out.len(), MAX_OUTPUT);
        assert!(out.ends_with("END"));
    }

    #[test]
    fn escapes_go_and_text_stays() {
        assert_eq!(
            strip_ansi("\x1b]0;title\x07a\x1b[1;31mb\x1b[0mc\x1b(Bd 中文\x08"),
            "abcd 中文"
        );
    }
}
