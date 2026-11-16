//! A native viewer's terminal stream (`nexa_session::terminal`): a shell for it, its
//! output back, its keys and size in. The shell ends with the stream or the session.

use std::{io::Read, sync::Arc};

use nexa_session::{
    framed::{FramedRecv, FramedSend},
    handshake::FLUSH_TIMEOUT,
    terminal::{CHUNK, EXIT_DENIED, EXIT_EXITED, TermBody, TermMsg, TermSize},
};
use tokio::sync::mpsc;

/// Starts shells. The agent's starts the user's login shell; tests start their own.
pub(crate) type Spawner =
    Arc<dyn Fn(u16, u16) -> std::io::Result<nexa_term::Pty> + Send + Sync + 'static>;

pub(crate) fn login_shell() -> Spawner {
    Arc::new(|cols, rows| {
        nexa_term::Pty::spawn(&nexa_term::Spawn {
            cols,
            rows,
            ..Default::default()
        })
    })
}

fn size(s: TermSize) -> (u16, u16) {
    nexa_term::clamp_size(
        s.cols.min(u32::from(u16::MAX)) as u16,
        s.rows.min(u32::from(u16::MAX)) as u16,
    )
}

/// Says no and ends the stream: the terminal was not granted.
pub(crate) async fn refuse(mut send: FramedSend, recv: FramedRecv) {
    recv.stop();
    let _ = send
        .send(&TermMsg::from(TermBody::Exit(EXIT_DENIED.into())))
        .await;
    send.finish_and_flush(FLUSH_TIMEOUT).await;
}

/// Runs one shell for one stream, to whichever end comes first.
pub(crate) async fn serve(mut send: FramedSend, mut recv: FramedRecv, spawn: Spawner) {
    let (cols, rows) = match recv.next::<TermMsg>().await {
        Ok(Some(TermMsg {
            body: Some(TermBody::Open(s)),
        })) => size(s),
        _ => return,
    };
    let pty = match spawn(cols, rows).and_then(|p| p.reader().map(|r| (p, r))) {
        Ok(p) => p,
        Err(e) => {
            let why = format!("could not start a shell: {e}");
            let _ = send.send(&TermMsg::from(TermBody::Exit(why))).await;
            send.finish_and_flush(FLUSH_TIMEOUT).await;
            return;
        }
    };
    let (pty, mut reader) = pty;
    // The pty is read on a thread of its own; `None` when the shell is gone.
    let (out_tx, mut out_rx) = mpsc::channel::<Option<Vec<u8>>>(16);
    let _ = std::thread::Builder::new()
        .name("shell-output".into())
        .spawn(move || {
            let mut buf = vec![0u8; CHUNK];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || out_tx.blocking_send(Some(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
            let _ = out_tx.blocking_send(None);
        });
    let exit = loop {
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(Some(data)) => {
                    if send.send(&TermMsg::from(TermBody::Data(data))).await.is_err() {
                        break None;
                    }
                }
                _ => break Some(EXIT_EXITED),
            },
            msg = recv.next::<TermMsg>() => match msg {
                Ok(Some(TermMsg { body: Some(TermBody::Data(d)) })) => {
                    let _ = pty.write(&d);
                }
                Ok(Some(TermMsg { body: Some(TermBody::Resize(s)) })) => {
                    let (c, r) = size(s);
                    let _ = pty.resize(c, r);
                }
                Ok(Some(_)) => {}
                // The viewer closed its terminal.
                Ok(None) | Err(_) => break None,
            },
        }
    };
    if let Some(why) = exit {
        let _ = send.send(&TermMsg::from(TermBody::Exit(why.into()))).await;
    }
    send.finish_and_flush(FLUSH_TIMEOUT).await;
    // Dropping the pty ends the shell and its process group.
    drop(pty);
}
