//! The host's command line in the app (`nexa_session::terminal`): one `Terminal`
//! stream of the session, its bytes handed to the UI's terminal control (MySSH's
//! native Avalonia `TerminalView`, the same on macOS and Windows) through a callback,
//! and the control's keys and size sent back.
//!
//! The stream runs on the session's runtime. Output goes to the callback from that
//! runtime's threads; [`close`] waits for a callback in progress and allows no more,
//! so the UI can free what the callback uses as soon as it returns.
//!
//! A flood of output (`yes`, `cat` of a large file) is held back at the host: past
//! [`MAX_PENDING`] bytes waiting in the control, the stream is not read until it has
//! caught up, so the host slows down instead of memory filling here. Keys still go out
//! meanwhile, so Ctrl+C reaches the shell.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::Duration,
};

use nexa_session::{
    framed::{FramedSend, open_bi},
    handshake::FLUSH_TIMEOUT,
    terminal::{CHUNK, TermBody, TermMsg, TermSize},
};
use nexa_wire::framing::StreamType;
use quinn::Connection;
use tokio::sync::mpsc;

/// From the control to the stream.
pub enum TermCmd {
    Data(Vec<u8>),
    Resize(TermSize),
}

/// From the stream to the control.
pub enum TermOut<'a> {
    Data(&'a [u8]),
    /// The shell ended or the stream closed; nothing follows.
    Exit(&'a str),
    /// Nothing to show: only asks how much output the control has not shown yet.
    Backlog,
}

/// Output waiting in the control past which the stream is not read (as since 0.1.4).
pub const MAX_PENDING: usize = 1 << 20;

/// What the control does with output: returns the bytes it has not shown yet.
pub type Output = Box<dyn FnMut(TermOut<'_>) -> usize + Send>;

/// Where output goes. `None` once the terminal was closed here.
type Sink = Arc<Mutex<Option<Output>>>;

/// A terminal the control is ready for: the session opens its stream.
pub struct TermLink {
    pub size: TermSize,
    pub cmds: mpsc::UnboundedReceiver<TermCmd>,
    sink: Sink,
}

struct Open {
    cmds: mpsc::UnboundedSender<TermCmd>,
    sink: Sink,
}

fn terminals() -> &'static Mutex<HashMap<u64, Open>> {
    static OPEN: OnceLock<Mutex<HashMap<u64, Open>>> = OnceLock::new();
    OPEN.get_or_init(Default::default)
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The size a control asked for, kept within what a shell can use.
pub fn size(cols: u32, rows: u32) -> TermSize {
    TermSize {
        cols: cols.clamp(2, 1000),
        rows: rows.clamp(1, 500),
    }
}

/// Registers a terminal: `out` receives its output until [`close`]. Returns its id and
/// the link the session's stream task takes.
pub fn start(size: TermSize, out: Output) -> (u64, TermLink) {
    let id = NEXT_ID.fetch_add(1, Relaxed);
    let (tx, rx) = mpsc::unbounded_channel();
    let sink: Sink = Arc::new(Mutex::new(Some(out)));
    terminals()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            id,
            Open {
                cmds: tx,
                sink: sink.clone(),
            },
        );
    (
        id,
        TermLink {
            size,
            cmds: rx,
            sink,
        },
    )
}

/// Keys (or pasted text) typed into the terminal. False if it is not open.
pub fn write(id: u64, data: &[u8]) -> bool {
    send(id, TermCmd::Data(data.to_vec()))
}

/// The control's new size in cells. False if the terminal is not open.
pub fn resize(id: u64, cols: u32, rows: u32) -> bool {
    send(id, TermCmd::Resize(size(cols, rows)))
}

fn send(id: u64, cmd: TermCmd) -> bool {
    terminals()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .is_some_and(|t| t.cmds.send(cmd).is_ok())
}

/// Takes the terminal away; its shell ends. When this returns, the output callback is
/// not running and will not run again.
pub fn close(id: u64) {
    let open = terminals()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    if let Some(open) = open {
        // Waits for a callback in progress, then turns the sink off.
        *open.sink.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // Dropping the sender ends the stream task.
        drop(open.cmds);
    }
}

/// Hands `out` to the control; the bytes it has not shown yet (0 once closed).
fn emit(sink: &Sink, out: TermOut<'_>) -> usize {
    sink.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .map_or(0, |f| f(out))
}

/// Sends what the control asked for. False when the terminal was closed here.
async fn command(send: &mut FramedSend, cmd: Option<TermCmd>) -> bool {
    match cmd {
        Some(TermCmd::Data(d)) => {
            for piece in d.chunks(CHUNK) {
                if send
                    .send(&TermMsg::from(TermBody::Data(piece.to_vec())))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            true
        }
        Some(TermCmd::Resize(s)) => {
            let _ = send.send(&TermMsg::from(TermBody::Resize(s))).await;
            true
        }
        // The terminal was closed here.
        None => false,
    }
}

/// The stream for one terminal, on the session's runtime, until either side ends it.
pub(crate) async fn run(conn: Connection, link: TermLink) {
    let TermLink {
        size,
        mut cmds,
        sink,
    } = link;
    let (mut send, mut recv) = match open_bi(&conn, StreamType::Terminal).await {
        Ok(s) => s,
        Err(e) => {
            emit(&sink, TermOut::Exit(&e.to_string()));
            return;
        }
    };
    if let Err(e) = send.send(&TermMsg::from(TermBody::Open(size))).await {
        emit(&sink, TermOut::Exit(&e.to_string()));
        return;
    }
    'stream: loop {
        tokio::select! {
            cmd = cmds.recv() => {
                if !command(&mut send, cmd).await {
                    break;
                }
            }
            msg = recv.next::<TermMsg>() => match msg {
                Ok(Some(TermMsg { body: Some(TermBody::Data(d)) })) => {
                    if emit(&sink, TermOut::Data(&d)) > MAX_PENDING {
                        // The control is behind: let it catch up before reading on,
                        // still sending what is typed (Ctrl+C included).
                        while emit(&sink, TermOut::Backlog) > MAX_PENDING / 2 {
                            tokio::select! {
                                cmd = cmds.recv() => {
                                    if !command(&mut send, cmd).await {
                                        break 'stream;
                                    }
                                }
                                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                            }
                        }
                    }
                }
                Ok(Some(TermMsg { body: Some(TermBody::Exit(why)) })) => {
                    emit(&sink, TermOut::Exit(&why));
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    emit(&sink, TermOut::Exit("closed"));
                    break;
                }
                Err(e) => {
                    emit(&sink, TermOut::Exit(&e.to_string()));
                    break;
                }
            },
        }
    }
    send.finish_and_flush(FLUSH_TIMEOUT).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_stops_output_and_commands() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s2 = seen.clone();
        let (id, link) = start(
            size(80, 24),
            Box::new(move |o| {
                match o {
                    TermOut::Data(d) => s2
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(d).into_owned()),
                    TermOut::Exit(why) => s2.lock().unwrap().push(format!("exit: {why}")),
                    TermOut::Backlog => {}
                }
                0
            }),
        );
        assert!(write(id, b"ls\r"));
        assert!(resize(id, 100, 30));
        emit(&link.sink, TermOut::Data(b"hello"));
        close(id);
        emit(&link.sink, TermOut::Exit("late"));
        assert_eq!(*seen.lock().unwrap(), vec!["hello".to_owned()]);
        assert!(!write(id, b"x"), "closed");
        let mut cmds = link.cmds;
        assert!(matches!(cmds.try_recv(), Ok(TermCmd::Data(d)) if d == b"ls\r"));
        assert!(matches!(
            cmds.try_recv(),
            Ok(TermCmd::Resize(TermSize {
                cols: 100,
                rows: 30
            }))
        ));
    }

    #[test]
    fn sizes_stay_within_what_a_shell_takes() {
        assert_eq!(size(0, 0), TermSize { cols: 2, rows: 1 });
        assert_eq!(
            size(5000, 5000),
            TermSize {
                cols: 1000,
                rows: 500
            }
        );
    }
}
