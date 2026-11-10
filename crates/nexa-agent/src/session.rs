//! One host session, from `SessionAccept` to close.
//!
//! Permissions are enforced by construction: without `input` no [`InjectThread`] is
//! created, without `clipboard` the pasteboard is never opened, and in both cases the
//! matching stream is refused.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig, MediaError};
use nexa_net::rate::RateConfig;
use nexa_session::rendezvous::link::Link;
use nexa_session::{
    SessionError,
    clip::{ClipCallbacks, ClipChannel, ClipSession, PromiseMode},
    framed::{FramedRecv, FramedSend, accept_bi, accept_uni},
    handshake::{ControlChannel, FLUSH_TIMEOUT, HANDSHAKE_TIMEOUT, HostAccepted},
    host::{HostMedia, HostOptions, HostParams, RebuildReason, Rebuilt, StreamRebuilder},
    input::{InjectThread, PointMap, pump_input_stream},
    negotiate::{self, Chosen},
    proto::{CloseReason, ControlBody, ControlMsg, Goodbye, StreamRequest, close},
};
use nexa_wire::{
    control::{Caps, FocusContext, Mode, StreamConfig},
    framing::StreamType,
};
use quinn::Connection;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::{AgentEvent, AgentUi, Platform, ScreenInfo, SessionInfo};

/// Mode switches rebuild the pipeline, so a viewer cannot make that happen in a loop.
const MIN_REBUILD_INTERVAL: Duration = Duration::from_millis(500);
const MIN_BITRATE: u32 = 8_000_000;
const MAX_BITRATE: u32 = 60_000_000;

pub(crate) struct SessionEnv {
    pub platform: Arc<dyn Platform>,
    pub ui: Arc<dyn AgentUi>,
    pub focus: watch::Receiver<Option<FocusContext>>,
    pub caps: Caps,
    pub device: nexa_clip::DeviceId,
}

/// Maps wire pixels to CoreGraphics points (§7.7), following display changes.
struct Geometry {
    state: Mutex<(ScreenInfo, (u32, u32))>,
}

impl PointMap for Geometry {
    fn to_points(&self, display: u32, x: i32, y: i32) -> Option<(f64, f64)> {
        if display != 0 {
            return None;
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (screen, stream) = &*state;
        Some(pixels_to_points(screen, *stream, x, y))
    }
}

fn pixels_to_points(screen: &ScreenInfo, stream: (u32, u32), x: i32, y: i32) -> (f64, f64) {
    let (sw, sh) = (f64::from(stream.0.max(1)), f64::from(stream.1.max(1)));
    // Clamped so a bad coordinate cannot reach another display.
    let (px, py) = (
        f64::from(x).clamp(0.0, sw - 1.0),
        f64::from(y).clamp(0.0, sh - 1.0),
    );
    (
        screen.origin_points.0 + px * screen.size_points.0 / sw,
        screen.origin_points.1 + py * screen.size_points.1 / sh,
    )
}

struct Pipeline {
    capturer: Box<dyn Capturer>,
    encoder: Box<dyn Encoder>,
    config: StreamConfig,
}

/// Builds capture and encode for the current display and the viewer's request.
struct Builder {
    platform: Arc<dyn Platform>,
    host: Caps,
    viewer: Caps,
    geometry: Arc<Geometry>,
}

fn bitrate_for(w: u32, h: u32, fps: u32) -> u32 {
    let bits = f64::from(w) * f64::from(h) * f64::from(fps) * 0.1;
    (bits as u32).clamp(MIN_BITRATE, MAX_BITRATE)
}

fn media_err(e: String) -> MediaError {
    MediaError::Other(e)
}

impl Builder {
    fn build(&self, mode: Mode, fps: u32) -> nexa_media::Result<Pipeline> {
        let Chosen {
            codec,
            chroma,
            max_w,
            max_h,
        } = negotiate::choose(&self.host, &self.viewer, mode)
            .ok_or(MediaError::Unsupported("no common codec"))?;
        let screen = self.platform.screen().map_err(media_err)?;
        let (w, h) = negotiate::fit_size(screen.size_pixels.0, screen.size_pixels.1, max_w, max_h);
        let fps = negotiate::choose_fps(
            fps,
            mode,
            self.host.max_fps,
            self.viewer.max_fps,
            screen.refresh_hz,
        );
        let capturer = self.platform.capturer(CaptureConfig {
            display: screen.display_id,
            width: w,
            height: h,
            fps,
            // §6.9 (client-drawn cursor) is not done, so the cursor stays in the video.
            show_cursor: true,
            chroma,
        })?;
        let encoder = self.platform.encoder(EncoderConfig {
            codec,
            chroma,
            mode,
            width: w,
            height: h,
            fps,
            bitrate_bps: bitrate_for(w, h, fps),
        })?;
        let config = StreamConfig {
            display: 0,
            codec: codec as i32,
            mode: mode as i32,
            w,
            h,
            fps,
            chroma: chroma as i32,
        };
        *self
            .geometry
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (screen, (w, h));
        Ok(Pipeline {
            capturer,
            encoder,
            config,
        })
    }
}

/// Runs on the capture thread when the display changes (§6.7).
struct Rebuilder {
    builder: Arc<Builder>,
    mode: Mode,
    fps: u32,
    control: mpsc::UnboundedSender<ControlBody>,
    failed: mpsc::UnboundedSender<String>,
}

impl StreamRebuilder for Rebuilder {
    fn rebuild(&mut self, reason: RebuildReason) -> nexa_media::Result<Rebuilt> {
        if let RebuildReason::Requested { mode, fps } = reason {
            self.mode = mode;
            self.fps = fps;
        }
        match self.builder.build(self.mode, self.fps) {
            Ok(p) => {
                let (mode, fps) = (self.mode, p.config.fps);
                let _ = self.control.send(ControlBody::Config(p.config));
                Ok(Rebuilt {
                    capturer: p.capturer,
                    encoder: p.encoder,
                    mode,
                    fps,
                })
            }
            Err(e) => {
                let _ = self.failed.send(e.to_string());
                Err(e)
            }
        }
    }
}

enum Inbound {
    Uni(StreamType, FramedRecv),
    Bi(StreamType, FramedSend, FramedRecv),
}

fn spawn_acceptors(
    conn: &Connection,
    tx: mpsc::UnboundedSender<Inbound>,
) -> (JoinHandle<()>, JoinHandle<()>) {
    let (c, t) = (conn.clone(), tx.clone());
    let uni = tokio::spawn(async move {
        loop {
            match accept_uni(&c).await {
                Ok((kind, recv)) => {
                    if t.send(Inbound::Uni(kind, recv)).is_err() {
                        return;
                    }
                }
                Err(SessionError::Connection(_)) => return,
                Err(_) => {} // one bad stream is not the connection's end
            }
        }
    });
    let c = conn.clone();
    let bi = tokio::spawn(async move {
        loop {
            match accept_bi(&c).await {
                Ok((kind, send, recv)) => {
                    if tx.send(Inbound::Bi(kind, send, recv)).is_err() {
                        return;
                    }
                }
                Err(SessionError::Connection(_)) => return,
                Err(_) => {}
            }
        }
    });
    (uni, bi)
}

async fn control_writer(mut send: FramedSend, mut rx: mpsc::UnboundedReceiver<ControlBody>) {
    while let Some(body) = rx.recv().await {
        if send.send(&ControlMsg::from(body)).await.is_err() {
            return;
        }
    }
    send.finish_and_flush(FLUSH_TIMEOUT).await;
}

/// §4.5 on the host: follow the viewer's `PathSwitch`, which only ever arrives on
/// this authenticated stream. Sessions on the listening address have no link and
/// ignore it.
struct PathFollower {
    link: Option<Arc<Link>>,
    last: Option<u32>,
}

impl PathFollower {
    /// True when the path really changed (not the initial choice).
    fn apply(&mut self, path_id: u32) -> bool {
        let Some(link) = &self.link else { return false };
        link.apply_remote_switch(path_id);
        let changed = self.last.is_some_and(|l| l != path_id);
        self.last = Some(path_id);
        changed
    }
}

/// What the viewer asked for before streaming starts.
async fn first_request(
    recv: &mut FramedRecv,
    path: &mut PathFollower,
) -> Result<StreamRequest, &'static str> {
    loop {
        match recv.next::<ControlMsg>().await {
            Ok(Some(ControlMsg {
                body: Some(ControlBody::Request(r)),
            })) => return Ok(r),
            Ok(Some(ControlMsg {
                body: Some(ControlBody::PathSwitch(p)),
            })) => {
                path.apply(p.path_id);
            }
            Ok(Some(ControlMsg {
                body: Some(ControlBody::Bye(_)),
            })) => return Err("the viewer left"),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return Err("the viewer went away"),
        }
    }
}

fn mode_of(request: &StreamRequest) -> Mode {
    Mode::try_from(request.mode).unwrap_or(Mode::Balanced)
}

/// Runs a session to its end and returns a description of why it ended.
pub(crate) async fn run(
    conn: Connection,
    accepted: HostAccepted,
    env: Arc<SessionEnv>,
    mut stop: watch::Receiver<Option<CloseReason>>,
    info: SessionInfo,
    link: Option<Arc<Link>>,
) -> String {
    let HostAccepted {
        control,
        perms,
        viewer_caps,
        ..
    } = accepted;
    let ControlChannel { send, mut recv } = control;
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(control_writer(send, ctl_rx));

    let finish = |conn: &Connection, reason: CloseReason, message: &str| {
        conn.close(close::for_reason(reason).into(), message.as_bytes());
    };

    let mut path = PathFollower { link, last: None };
    let request =
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, first_request(&mut recv, &mut path)).await {
            Ok(Ok(r)) => r,
            Ok(Err(why)) => {
                finish(&conn, CloseReason::Unspecified, why);
                return why.to_owned();
            }
            Err(_) => {
                finish(&conn, CloseReason::Unspecified, "no stream request");
                return "the viewer sent no stream request".to_owned();
            }
        };

    let screen = match env.platform.screen() {
        Ok(s) => s,
        Err(e) => {
            finish(&conn, CloseReason::PipelineFailed, "display unavailable");
            return format!("display unavailable: {e}");
        }
    };
    let geometry = Arc::new(Geometry {
        state: Mutex::new((screen, (1, 1))),
    });
    let builder = Arc::new(Builder {
        platform: env.platform.clone(),
        host: env.caps.clone(),
        viewer: viewer_caps,
        geometry: geometry.clone(),
    });

    let mut current = (mode_of(&request), request.fps);
    let pipeline = {
        let (b, mode, fps) = (builder.clone(), current.0, current.1);
        tokio::task::spawn_blocking(move || b.build(mode, fps)).await
    };
    let pipeline = match pipeline {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            finish(&conn, CloseReason::PipelineFailed, "capture failed");
            return format!("capture could not start: {e}");
        }
        Err(_) => {
            finish(&conn, CloseReason::PipelineFailed, "capture failed");
            return "capture could not start".to_owned();
        }
    };
    let config = pipeline.config.clone();
    let _ = ctl_tx.send(ControlBody::Config(config.clone()));

    // Visible before any frame or input can flow (§9.3).
    env.ui.event(AgentEvent::SessionStarted(info));

    // Input is only ever wired up when SessionAccept granted it.
    let inject = perms.input.then(|| {
        let platform = env.platform.clone();
        InjectThread::spawn(
            Box::new(move || platform.input_sink()),
            geometry.clone() as Arc<dyn PointMap>,
        )
    });
    let on_pointer: Box<dyn Fn(nexa_wire::datagram::Pointer) + Send> = match &inject {
        Some(thread) => thread.pointer_sink(),
        None => Box::new(|_| {}),
    };

    let (failed_tx, mut failed_rx) = mpsc::unbounded_channel();
    let media = HostMedia::spawn_with(
        conn.clone(),
        pipeline.capturer,
        pipeline.encoder,
        HostParams {
            stream: 0,
            fps: config.fps,
            mode: current.0,
            rate: RateConfig {
                initial_bps: u64::from(bitrate_for(config.w, config.h, config.fps)),
                min_bps: 300_000,
                max_bps: 200_000_000,
            },
        },
        on_pointer,
        HostOptions {
            rebuilder: Some(Box::new(Rebuilder {
                builder: builder.clone(),
                mode: current.0,
                fps: request.fps,
                control: ctl_tx.clone(),
                failed: failed_tx,
            })),
            thread_init: env.platform.thread_init(),
        },
    );

    let (in_tx, mut in_rx) = mpsc::unbounded_channel();
    let (uni_task, bi_task) = spawn_acceptors(&conn, in_tx);
    let mut input_task: Option<JoinHandle<()>> = None;
    let mut clipboard: Option<ClipSession> = None;
    let mut focus = env.focus.clone();
    if let Some(f) = focus.borrow_and_update().clone() {
        let _ = ctl_tx.send(ControlBody::Focus(f));
    }
    let mut last_rebuild = Instant::now();

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
                    let next = (mode_of(&r), r.fps);
                    if next != current && last_rebuild.elapsed() >= MIN_REBUILD_INTERVAL {
                        current = next;
                        last_rebuild = Instant::now();
                        media.request_rebuild(RebuildReason::Requested { mode: next.0, fps: next.1 });
                    }
                }
                Ok(Some(ControlMsg { body: Some(ControlBody::Bye(_)) })) => {
                    break (CloseReason::Unspecified, "the viewer ended the session".to_owned());
                }
                Ok(Some(ControlMsg { body: Some(ControlBody::PathSwitch(p)) })) => {
                    if path.apply(p.path_id) {
                        media.path_switched();
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break (CloseReason::Unspecified, "the control stream ended".to_owned()),
            },
            Some(reason) = failed_rx.recv() => {
                break (CloseReason::PipelineFailed, format!("capture failed: {reason}"));
            }
            Ok(()) = focus.changed() => {
                if let Some(f) = focus.borrow_and_update().clone() {
                    let _ = ctl_tx.send(ControlBody::Focus(f));
                }
            }
            Some(inbound) = in_rx.recv() => match inbound {
                Inbound::Uni(StreamType::Input, recv_input) => {
                    match (&inject, &input_task) {
                        (Some(thread), None) => {
                            let tx = thread.sender();
                            input_task = Some(tokio::spawn(async move {
                                let _ = pump_input_stream(recv_input, tx).await;
                            }));
                        }
                        _ => recv_input.stop(),
                    }
                }
                Inbound::Uni(_, recv_other) => recv_other.stop(),
                Inbound::Bi(StreamType::Clipboard, send, clip_recv) => {
                    if perms.clipboard && clipboard.is_none() {
                        clipboard = start_clipboard(&env, send, clip_recv);
                    } else {
                        clip_recv.stop();
                        drop(send);
                    }
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
    if let Some(task) = input_task.take() {
        task.abort();
    }
    conn.close(close::for_reason(reason).into(), message.as_bytes());

    // These block until their threads finish, and input is released last so that
    // nothing can arrive after the release.
    let _ = tokio::task::spawn_blocking(move || {
        media.stop();
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

fn start_clipboard(
    env: &Arc<SessionEnv>,
    send: FramedSend,
    recv: FramedRecv,
) -> Option<ClipSession> {
    let channel = ClipChannel::default();
    let backend = match env.platform.clipboard(channel.resolver()) {
        Ok(b) => b,
        Err(e) => {
            env.ui
                .event(AgentEvent::Warning(format!("clipboard unavailable: {e}")));
            return None;
        }
    };
    let ui = env.ui.clone();
    Some(channel.spawn(
        backend,
        env.device,
        // The agent has no UI whose main thread a slow promise could freeze.
        PromiseMode::Lazy,
        (send, recv),
        ClipCallbacks {
            on_gate_release: Box::new(|_| {}),
            on_notice: Box::new(move |n| ui.event(AgentEvent::ClipboardNotice(n))),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> ScreenInfo {
        ScreenInfo {
            display_id: 7,
            origin_points: (100.0, 50.0),
            size_points: (1512.0, 982.0),
            size_pixels: (3024, 1964),
            refresh_hz: 120,
            name: "test".into(),
        }
    }

    #[test]
    fn retina_pixels_halve_and_offset_by_the_origin() {
        assert_eq!(
            pixels_to_points(&screen(), (3024, 1964), 300, 100),
            (250.0, 100.0)
        );
    }

    #[test]
    fn scaled_streams_map_to_the_whole_display() {
        let (x, y) = pixels_to_points(&screen(), (1512, 982), 756, 491);
        assert_eq!((x, y), (856.0, 541.0));
    }

    #[test]
    fn out_of_range_is_clamped_to_the_display() {
        assert_eq!(
            pixels_to_points(&screen(), (3024, 1964), -500, -500),
            (100.0, 50.0)
        );
        let (x, y) = pixels_to_points(&screen(), (3024, 1964), 99_999, 99_999);
        assert!(x < 100.0 + 1512.0 && y < 50.0 + 982.0);
    }

    #[test]
    fn bitrate_stays_in_range() {
        assert_eq!(bitrate_for(100, 100, 1), MIN_BITRATE);
        assert_eq!(bitrate_for(7680, 4320, 120), MAX_BITRATE);
    }
}
