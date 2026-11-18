//! A session with an AI agent (docs/MCP.md §13): no video. The agent looks when it
//! asks, on `Agent` streams: the frontmost window's elements as text, a lossless
//! screenshot, what has focus, whether the screen settled after its last action.
//! Input, the clipboard and the command line work as in any session; pointer input
//! is in the agent's pixels of the display it chose, which `StreamConfig` gives
//! (`codec: Unknown`).
//!
//! Someone who wants to watch the agent connects with the app as well, in a session
//! of their own.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use nexa_session::{
    agent::{
        AgentBody, AgentMsg, CHUNK, ObserveReply, ObserveRequest, REFUSED_UNSUPPORTED, agent_size,
    },
    bulk::{BulkLimiter, UplinkScavenger},
    clip::ClipSession,
    framed::{FramedRecv, FramedSend},
    handshake::{ControlChannel, FLUSH_TIMEOUT, HostAccepted},
    input::{InjectThread, Marks, PointMap, pump_input_stream},
    proto::{CloseReason, ControlBody, ControlMsg, Goodbye, close},
    rendezvous::link::Link,
};
use nexa_wire::{
    control::{Chroma, Codec, Mode, StreamConfig},
    datagram::{self, Clock, Pointer},
    framing::StreamType,
};
use quinn::Connection;
use tokio::sync::{mpsc, watch};

use crate::{
    AgentEvent, ScreenInfo, SessionInfo,
    agent_view::{AgentSpace, AgentView, Frontmost, Rgba, UiOptions, png, render, same_picture},
    session::{
        AbortOnDrop, FileSlot, Geometry, Inbound, PathFollower, SessionEnv, control_writer,
        display_at, spawn_acceptors, start_clipboard,
    },
};

/// The thumbnail compared to tell whether the screen still changes: longer side.
const THUMB_SIDE: u32 = 160;
/// Between thumbnails while waiting for the screen to settle.
const THUMB_EVERY: Duration = Duration::from_millis(100);
/// Pointer moves and the wheel travel as datagrams and may land after a mark: a look
/// waits this much past the mark for them.
const DATAGRAM_GRACE: Duration = Duration::from_millis(100);
/// The longest an agent may make a look wait, whatever it asks.
const MAX_WAIT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_NODES: u32 = 300;
const MAX_NODES: u32 = 2000;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the looks of one session share.
struct Looks {
    view: Option<Arc<dyn AgentView>>,
    geometry: Arc<Geometry>,
    marks: Option<Arc<Marks>>,
    may_view: bool,
    /// The screen at the end of the last look, to tell the agent whether it changed.
    last: Mutex<Last>,
    /// One look at a time: they take screenshots.
    turn: tokio::sync::Mutex<()>,
}

/// How the screen was at the end of a look.
#[derive(Default)]
struct Last {
    picture: Option<Rgba>,
    front: Option<Frontmost>,
}

fn stream_config(display: u32, space: (u32, u32)) -> ControlBody {
    ControlBody::Config(StreamConfig {
        display,
        codec: Codec::Unknown as i32,
        mode: Mode::Office as i32,
        w: space.0,
        h: space.1,
        fps: 0,
        chroma: Chroma::Yuv420 as i32,
    })
}

/// Runs an agent's session to its end; returns why it ended.
pub(crate) async fn run(
    conn: Connection,
    accepted: HostAccepted,
    env: Arc<SessionEnv>,
    mut stop: watch::Receiver<Option<CloseReason>>,
    info: SessionInfo,
    link: Option<Arc<Link>>,
    files: FileSlot,
) -> String {
    let HostAccepted { control, perms, .. } = accepted;
    let ControlChannel { send, mut recv } = control;
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(control_writer(send, ctl_rx));
    // §4.5: our candidates after a network change go to the viewer on this stream.
    let _update_pump = AbortOnDrop(link.as_ref().and_then(|l| l.take_candidate_updates()).map(
        |mut updates| {
            let ctl = ctl_tx.clone();
            tokio::spawn(async move {
                while let Some(candidates) = updates.recv().await {
                    let body = ControlBody::Candidates(nexa_session::proto::CandidateUpdate {
                        candidates,
                    });
                    if ctl.send(body).is_err() {
                        break;
                    }
                }
            })
        },
    ));
    let mut path = PathFollower { link, last: None };
    // No files in an agent's session (§13.1).
    *lock(&files) = None;

    let finish = |conn: &Connection, reason: CloseReason, message: &str| {
        conn.close(close::for_reason(reason).into(), message.as_bytes());
    };
    let mut display = 0u32;
    let screen = match display_at(&*env.platform, display) {
        Ok(s) => s,
        Err(e) => {
            finish(&conn, CloseReason::PipelineFailed, "display unavailable");
            return format!("display unavailable: {e}");
        }
    };
    let space = agent_size(screen.size_pixels.0, screen.size_pixels.1);
    let geometry = Arc::new(Geometry {
        state: Mutex::new((screen, space, display)),
    });
    let _ = ctl_tx.send(stream_config(display, space));

    // Visible before any input can flow (§9.3).
    env.ui.event(AgentEvent::SessionStarted(info));

    let inject = perms.input.then(|| {
        let platform = env.platform.clone();
        InjectThread::spawn(
            Box::new(move || platform.input_sink()),
            geometry.clone() as Arc<dyn PointMap>,
        )
    });
    let datagrams = AbortOnDrop(Some(tokio::spawn(read_datagrams(
        conn.clone(),
        inject.as_ref().map(InjectThread::pointer_sink),
    ))));
    let looks = Arc::new(Looks {
        view: env.platform.agent_view(),
        geometry: geometry.clone(),
        marks: inject.as_ref().map(|t| t.marks.clone()),
        may_view: perms.view,
        last: Mutex::default(),
        turn: tokio::sync::Mutex::new(()),
    });
    let bulk = BulkLimiter::new(Arc::new(UplinkScavenger::new(conn.clone())));

    let (in_tx, mut in_rx) = mpsc::unbounded_channel();
    let (uni_task, bi_task) = spawn_acceptors(&conn, in_tx);
    let mut input_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut clipboard: Option<ClipSession> = None;
    let mut tasks: Vec<AbortOnDrop> = Vec::new();

    let (reason, message) = loop {
        tokio::select! {
            changed = stop.changed() => {
                let reason = if changed.is_ok() {
                    stop.borrow().unwrap_or(CloseReason::Shutdown)
                } else {
                    CloseReason::Shutdown
                };
                break (reason, "ended by the host".to_owned());
            }
            e = conn.closed() => break (CloseReason::Unspecified, format!("connection closed: {e}")),
            msg = recv.next::<ControlMsg>() => match msg {
                Ok(Some(ControlMsg { body: Some(ControlBody::Request(r)) })) => {
                    // Only the display matters: there is no picture to shape.
                    if r.display != display
                        && let Ok(screen) = display_at(&*env.platform, r.display)
                    {
                        display = r.display;
                        let space = agent_size(screen.size_pixels.0, screen.size_pixels.1);
                        *lock(&geometry.state) = (screen, space, display);
                        *lock(&looks.last) = Last::default();
                        let _ = ctl_tx.send(stream_config(display, space));
                    }
                }
                Ok(Some(ControlMsg { body: Some(ControlBody::Bye(_)) })) => {
                    break (CloseReason::Unspecified, "the viewer ended the session".to_owned());
                }
                Ok(Some(ControlMsg { body: Some(ControlBody::PathSwitch(p)) })) => {
                    path.apply(p.path_id);
                }
                Ok(Some(ControlMsg { body: Some(ControlBody::Candidates(c)) })) => path.candidates(&c),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break (CloseReason::Unspecified, "the control stream ended".to_owned()),
            },
            Some(inbound) = in_rx.recv() => match inbound {
                Inbound::Uni(StreamType::Input, recv_input) => match (&inject, &input_task) {
                    (Some(thread), None) => {
                        let tx = thread.sender();
                        input_task = Some(tokio::spawn(async move {
                            let _ = pump_input_stream(recv_input, tx).await;
                        }));
                    }
                    _ => recv_input.stop(),
                },
                Inbound::Uni(_, other) => other.stop(),
                Inbound::Bi(StreamType::Clipboard, send, clip_recv) => {
                    if perms.clipboard && clipboard.is_none() {
                        clipboard = start_clipboard(&env, send, clip_recv, bulk.clone());
                    } else {
                        clip_recv.stop();
                        drop(send);
                    }
                }
                Inbound::Bi(StreamType::Terminal, send, term_recv) => {
                    let task = if perms.terminal {
                        tokio::spawn(crate::shell::serve(send, term_recv, env.shell.clone()))
                    } else {
                        tokio::spawn(crate::shell::refuse(send, term_recv))
                    };
                    tasks.push(AbortOnDrop(Some(task)));
                }
                Inbound::Bi(StreamType::Agent, send, look_recv) => {
                    tasks.retain(|t| t.0.as_ref().is_some_and(|t| !t.is_finished()));
                    tasks.push(AbortOnDrop(Some(tokio::spawn(serve_look(
                        looks.clone(),
                        send,
                        look_recv,
                    )))));
                }
                Inbound::Bi(_, send, other) => {
                    other.stop();
                    drop(send);
                }
            },
        }
    };

    let local_end = !matches!(reason, CloseReason::Unspecified);
    if local_end {
        let _ = ctl_tx.send(ControlBody::Bye(Goodbye {
            reason: reason as i32,
            message: message.clone(),
        }));
    }
    drop(ctl_tx);
    let _ = tokio::time::timeout(FLUSH_TIMEOUT * 2, writer).await;
    uni_task.abort();
    bi_task.abort();
    drop(tasks);
    drop(datagrams);
    if let Some(task) = input_task.take() {
        task.abort();
    }
    conn.close(close::for_reason(reason).into(), message.as_bytes());
    // Input is released last, so nothing can arrive after the release.
    let _ = tokio::task::spawn_blocking(move || {
        if let Some(c) = clipboard {
            c.stop();
        }
        if let Some(thread) = inject {
            let _ = thread.stop();
        }
    })
    .await;
    message
}

/// The datagrams of an agent's session: pointer moves and the wheel, and clock
/// probes, which are answered. There is no video to report on.
async fn read_datagrams(conn: Connection, on_pointer: Option<Box<dyn Fn(Pointer) + Send>>) {
    let base = Instant::now();
    while let Ok(d) = conn.read_datagram().await {
        match d.first() {
            Some(&datagram::TYPE_POINTER) => {
                if let (Ok(p), Some(sink)) = (Pointer::parse(&d), on_pointer.as_ref()) {
                    sink(p);
                }
            }
            Some(&datagram::TYPE_CLOCK) => {
                if let Ok(Clock::Ping { t1 }) = Clock::parse(&d) {
                    let t = base.elapsed().as_micros() as u64;
                    let _ = conn.send_datagram(bytes::Bytes::copy_from_slice(
                        &Clock::Pong { t1, t2: t, t3: t }.to_bytes(),
                    ));
                }
            }
            _ => {}
        }
    }
}

/// One look: the request, then the reply and the PNG in pieces.
async fn serve_look(looks: Arc<Looks>, mut send: FramedSend, mut recv: FramedRecv) {
    let request = match recv.next::<AgentMsg>().await {
        Ok(Some(AgentMsg {
            body: Some(AgentBody::Observe(r)),
        })) => r,
        _ => return,
    };
    recv.stop();
    let refusal = if !looks.may_view {
        Some("the host did not allow viewing")
    } else if looks.view.is_none() {
        Some(REFUSED_UNSUPPORTED)
    } else {
        None
    };
    if let Some(why) = refusal {
        let _ = send
            .send(&AgentMsg::from(AgentBody::Refused(why.to_owned())))
            .await;
        send.finish_and_flush(FLUSH_TIMEOUT).await;
        return;
    }
    let _turn = looks.turn.lock().await;
    let l = looks.clone();
    let Ok((reply, png)) = tokio::task::spawn_blocking(move || look(&l, &request)).await else {
        return;
    };
    if send
        .send(&AgentMsg::from(AgentBody::Reply(reply)))
        .await
        .is_err()
    {
        return;
    }
    for piece in png.chunks(CHUNK) {
        if send
            .send(&AgentMsg::from(AgentBody::Png(piece.to_vec())))
            .await
            .is_err()
        {
            return;
        }
    }
    send.finish_and_flush(FLUSH_TIMEOUT * 4).await;
}

/// `space` scaled down to a thumbnail, keeping its shape.
fn thumb_size(space: (u32, u32)) -> (u32, u32) {
    let k = f64::from(THUMB_SIDE) / f64::from(space.0.max(space.1).max(1));
    let k = k.min(1.0);
    (
        ((f64::from(space.0) * k).round() as u32).max(1),
        ((f64::from(space.1) * k).round() as u32).max(1),
    )
}

/// What the agent asked to see, once its last action went through and the screen
/// stopped changing (or the wait ran out). Blocks.
fn look(looks: &Looks, req: &ObserveRequest) -> (ObserveReply, Vec<u8>) {
    let view = looks.view.as_ref().expect("checked by the caller");
    let (screen, space, display): (ScreenInfo, (u32, u32), u32) =
        lock(&looks.geometry.state).clone();
    let max = Duration::from_millis(u64::from(req.max_ms)).min(MAX_WAIT);
    let asked = Instant::now();
    // From when the action went through: everything typed before the mark is in.
    let mut start = asked;
    if req.after_mark != 0
        && let Some(marks) = &looks.marks
    {
        match marks.wait(req.after_mark, max) {
            Some(_) => {
                std::thread::sleep(DATAGRAM_GRACE);
                start = Instant::now();
            }
            None => start = Instant::now(),
        }
    }
    let thumb = thumb_size(space);
    let quiet = Duration::from_millis(u64::from(req.quiet_ms));
    let deadline = asked + max;
    let mut settled = req.quiet_ms == 0;
    let mut current: Option<Rgba> = None;
    if req.quiet_ms > 0 {
        let mut still_since = Instant::now();
        while let Ok(now_shot) = view.screenshot(&screen, thumb) {
            let now = Instant::now();
            match &current {
                Some(prev) if same_picture(prev, &now_shot) => {
                    if now.duration_since(still_since) >= quiet {
                        settled = true;
                        current = Some(now_shot);
                        break;
                    }
                }
                _ => still_since = now,
            }
            current = Some(now_shot);
            if now >= deadline {
                break;
            }
            std::thread::sleep(THUMB_EVERY.min(deadline.saturating_duration_since(now)));
        }
    } else {
        current = view.screenshot(&screen, thumb).ok();
    }
    let waited = start.elapsed();
    let front = view.frontmost();
    // Changed: the picture, or what is in front and has the focus (typing a character
    // or two hardly moves the picture, but changes the focused field's value).
    let changed = {
        let mut last = lock(&looks.last);
        let picture = match (last.picture.as_ref(), current.as_ref()) {
            (Some(a), Some(b)) => !same_picture(a, b),
            _ => false,
        };
        let front_changed = last.front.as_ref().is_some_and(|f| *f != front);
        if current.is_some() {
            last.picture = current;
        }
        last.front = Some(front.clone());
        picture || front_changed
    };
    let at = AgentSpace {
        screen: &screen,
        space,
    };
    let pointer = view.pointer().and_then(|(x, y)| at.point(x, y));
    let mut reply = ObserveReply {
        display,
        w: space.0,
        h: space.1,
        settled,
        changed,
        waited_ms: waited.as_millis().min(u128::from(u32::MAX)) as u32,
        app: front.app,
        window: front.window,
        focused: front.focused,
        has_pointer: pointer.is_some(),
        pointer_x: pointer.map_or(0, |p| p.0),
        pointer_y: pointer.map_or(0, |p| p.1),
        ..Default::default()
    };
    if req.ui {
        let max_nodes = match req.max_nodes {
            0 => DEFAULT_MAX_NODES,
            n => n.min(MAX_NODES),
        };
        match view.ui(&UiOptions {
            interactive_only: req.interactive_only,
            max_nodes: max_nodes as usize * 4,
        }) {
            Ok(nodes) => reply.ui = render(&nodes, at, max_nodes as usize).0,
            Err(e) => reply.ui_error = e,
        }
    }
    let mut png_bytes = Vec::new();
    if req.screenshot {
        match view.screenshot(&screen, space).and_then(|s| png(&s)) {
            Ok(p) if p.len() <= nexa_session::agent::MAX_PNG => png_bytes = p,
            Ok(p) => {
                reply.screenshot_error = format!("the screenshot is too large ({} bytes)", p.len())
            }
            Err(e) => reply.screenshot_error = e,
        }
    }
    reply.png_len = png_bytes.len() as u32;
    (reply, png_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_keep_the_shape() {
        assert_eq!(thumb_size((1568, 980)), (160, 100));
        assert_eq!(thumb_size((882, 1568)), (90, 160));
        assert_eq!(thumb_size((100, 50)), (100, 50));
    }
}
