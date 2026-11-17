//! The host's command line in the app (`nexa_session::terminal`): one `Terminal`
//! stream of the session, its bytes handed to the UI's terminal control (MySSH's
//! native Avalonia `TerminalView`, the same on macOS and Windows) through a callback,
//! and the control's keys and size sent back.
//!
//! The stream runs on the session's runtime. Output goes to the callback from that
//! runtime's threads; [`close`] waits for a callback in progress and allows no more,
//! so the UI can free what the callback uses as soon as it returns.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

use nexa_session::{
    framed::open_bi,
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
}

/// Where output goes. `None` once the terminal was closed here.
type Sink = Arc<Mutex<Option<Box<dyn FnMut(TermOut<'_>) + Send>>>>;

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
pub fn start(size: TermSize, out: Box<dyn FnMut(TermOut<'_>) + Send>) -> (u64, TermLink) {
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

fn emit(sink: &Sink, out: TermOut<'_>) {
    if let Some(f) = sink.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        f(out);
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
        Err(e) => return emit(&sink, TermOut::Exit(&e.to_string())),
    };
    if let Err(e) = send.send(&TermMsg::from(TermBody::Open(size))).await {
        return emit(&sink, TermOut::Exit(&e.to_string()));
    }
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(TermCmd::Data(d)) => {
                    for piece in d.chunks(CHUNK) {
                        if send.send(&TermMsg::from(TermBody::Data(piece.to_vec()))).await.is_err() {
                            break;
                        }
                    }
                }
                Some(TermCmd::Resize(s)) => {
                    let _ = send.send(&TermMsg::from(TermBody::Resize(s))).await;
                }
                // The terminal was closed here.
                None => break,
            },
            msg = recv.next::<TermMsg>() => match msg {
                Ok(Some(TermMsg { body: Some(TermBody::Data(d)) })) => emit(&sink, TermOut::Data(&d)),
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
                s2.lock().unwrap().push(match o {
                    TermOut::Data(d) => String::from_utf8_lossy(d).into_owned(),
                    TermOut::Exit(why) => format!("exit: {why}"),
                })
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
