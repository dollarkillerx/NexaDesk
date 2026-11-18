//! One host session, from `SessionAccept` to close.
//!
//! Permissions are enforced by construction: without `input` no [`InjectThread`] is
//! created, without `clipboard` the pasteboard is never opened, and in both cases the
//! matching stream is refused.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig, MediaError};
use nexa_net::rate::RateConfig;
use nexa_session::rendezvous::link::Link;
use nexa_session::{
    SessionError,
    bulk::BulkLimiter,
    clip::{ClipCallbacks, ClipChannel, ClipDirection, ClipProgress, ClipSession, ClipStage},
    cursor::{InjectHistory, StreamGeometry},
    framed::{FramedRecv, FramedSend, accept_bi, accept_uni},
    handshake::{ControlChannel, FLUSH_TIMEOUT, HANDSHAKE_TIMEOUT, HostAccepted},
    host::{HostMedia, HostOptions, HostParams, RebuildReason, Rebuilt, StreamRebuilder},
    input::{InjectThread, PointMap, pump_input_stream},
    negotiate::{self, Chosen},
    proto::{
        CloseReason, ControlBody, ControlMsg, Goodbye, LINES_NATIVE, StreamQuality, StreamRequest,
        close,
    },
    transfer::{self, CONFIRM_TIMEOUT, TransferConfig, Transfers},
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

use crate::{
    AgentEvent, AgentUi, FileSettings, Platform, ScreenInfo, SessionInfo,
    host_audio::AudioSwitch,
    host_cursor::{CursorControl, CursorThread, TrackingSink},
};

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
    pub files: FileSettings,
    /// Starts a shell for a viewer granted the terminal.
    pub shell: crate::shell::Spawner,
}

/// The running session's transfers, for [`crate::Agent::send_files`] and friends.
pub(crate) type FileSlot = Arc<Mutex<Option<Transfers>>>;

/// Maps wire pixels to CoreGraphics points (§7.7), following display changes and
/// switches. The display index is the one being streamed (§3.6: `Display.id`).
pub(crate) struct Geometry {
    pub(crate) state: Mutex<(ScreenInfo, (u32, u32), u32)>,
}

impl PointMap for Geometry {
    fn to_points(&self, display: u32, x: i32, y: i32) -> Option<(f64, f64)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (screen, stream, index) = &*state;
        // Input for a display that is no longer streamed is dropped, not redirected.
        (display == *index).then(|| pixels_to_points(screen, *stream, x, y))
    }
}

impl StreamGeometry for Geometry {
    fn points_to_stream(&self, x: f64, y: f64) -> Option<(u32, i32, i32)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (screen, stream, index) = &*state;
        points_to_pixels(screen, *stream, x, y).map(|(px, py)| (*index, px, py))
    }

    fn stream_pixels_per_point(&self) -> f64 {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (screen, stream, _) = &*state;
        f64::from(stream.0) / screen.size_points.0.max(1.0)
    }
}

/// The inverse of [`pixels_to_points`]; `None` off the display.
fn points_to_pixels(screen: &ScreenInfo, stream: (u32, u32), x: f64, y: f64) -> Option<(i32, i32)> {
    let (rx, ry) = (x - screen.origin_points.0, y - screen.origin_points.1);
    let (w, h) = screen.size_points;
    if !(0.0..w).contains(&rx) || !(0.0..h).contains(&ry) {
        return None;
    }
    let px = (rx * f64::from(stream.0) / w).floor() as i32;
    let py = (ry * f64::from(stream.1) / h).floor() as i32;
    Some((px, py))
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
    /// The platform's id of the streamed display (`CaptureConfig::display`).
    display_id: u32,
}

/// Builds capture and encode for the current display and the viewer's request.
struct Builder {
    platform: Arc<dyn Platform>,
    host: Caps,
    viewer: Caps,
    geometry: Arc<Geometry>,
    /// §6.9: the cursor goes into the video unless the viewer draws it, which it
    /// does only outside game mode and only when its pointer drives this one.
    viewer_draws_cursor: bool,
    cursor: Arc<CursorControl>,
}

/// What the viewer asked of the picture beyond mode, frame rate and display (§6.7):
/// how good it may be, and how many lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct View {
    quality: StreamQuality,
    lines: u32,
    window: (u32, u32),
}

/// The smallest window a picture is fitted to: below it, text would be lost.
const SMALLEST_WINDOW: (u32, u32) = (640, 360);

impl View {
    fn of(r: &StreamRequest) -> Self {
        Self {
            quality: StreamQuality::try_from(r.quality).unwrap_or(StreamQuality::Auto),
            lines: r.lines,
            window: (r.window_w, r.window_h),
        }
    }

    /// The box the picture must fit in, on top of what the codec allows.
    fn bound(&self) -> (u32, u32) {
        const ANY: (u32, u32) = (u32::MAX, u32::MAX);
        match (self.lines, self.quality) {
            (LINES_NATIVE, _) => ANY,
            (n, _) if n > LINES_NATIVE => (u32::MAX, n),
            (_, StreamQuality::Clear) => ANY,
            (_, StreamQuality::Weak) => (u32::MAX, 720),
            // As large as the viewer's window shows it; no window said, the host's own.
            _ if self.window.0 == 0 || self.window.1 == 0 => ANY,
            _ => (
                self.window.0.max(SMALLEST_WINDOW.0),
                self.window.1.max(SMALLEST_WINDOW.1),
            ),
        }
    }

    /// What decides the picture's size: a change in it means a new pipeline.
    fn size_key(&self) -> (u32, u32) {
        self.bound()
    }

    /// The highest frame rate this quality wants; 0 for no limit.
    fn fps_cap(&self) -> u32 {
        if self.quality == StreamQuality::Weak {
            30
        } else {
            0
        }
    }

    /// The most the rate controller may send.
    fn ceiling_bps(&self) -> u64 {
        match self.quality {
            StreamQuality::Auto | StreamQuality::Clear => MAX_RATE_BPS,
            StreamQuality::Balanced => 12_000_000,
            StreamQuality::Weak => 2_500_000,
        }
    }
}

/// The rate controller's ceiling when the viewer sets none.
const MAX_RATE_BPS: u64 = 200_000_000;

fn bitrate_for(w: u32, h: u32, fps: u32) -> u32 {
    let bits = f64::from(w) * f64::from(h) * f64::from(fps) * 0.1;
    (bits as u32).clamp(MIN_BITRATE, MAX_BITRATE)
}

fn media_err(e: String) -> MediaError {
    MediaError::Other(e)
}

/// Display `index` of the platform's list (§3.6: `Display.id` is the index).
pub(crate) fn display_at(platform: &dyn Platform, index: u32) -> nexa_media::Result<ScreenInfo> {
    let mut displays = platform.displays().map_err(media_err)?;
    if (index as usize) < displays.len() {
        Ok(displays.swap_remove(index as usize))
    } else {
        Err(MediaError::Other(format!("there is no display {index}")))
    }
}

impl Builder {
    fn build(
        &self,
        mode: Mode,
        fps: u32,
        display: u32,
        view: View,
    ) -> nexa_media::Result<Pipeline> {
        let Chosen {
            codec,
            chroma,
            max_w,
            max_h,
        } = negotiate::choose(&self.host, &self.viewer, mode)
            .ok_or(MediaError::Unsupported("no common codec"))?;
        let screen = display_at(&*self.platform, display)?;
        let (bound_w, bound_h) = view.bound();
        let (w, h) = negotiate::fit_size(
            screen.size_pixels.0,
            screen.size_pixels.1,
            max_w.min(bound_w),
            max_h.min(bound_h),
        );
        let fps = negotiate::choose_fps(
            fps,
            mode,
            self.host.max_fps,
            self.viewer.max_fps,
            screen.refresh_hz,
        );
        let fps = match view.fps_cap() {
            0 => fps,
            cap => fps.min(cap),
        };
        let viewer_cursor = self.viewer_draws_cursor && mode != Mode::Game;
        let capturer = self.platform.capturer(CaptureConfig {
            display: screen.display_id,
            width: w,
            height: h,
            fps,
            show_cursor: !viewer_cursor,
            chroma,
        })?;
        let encoder = self.platform.encoder(EncoderConfig {
            codec,
            chroma,
            mode,
            width: w,
            height: h,
            fps,
            bitrate_bps: bitrate_for(w, h, fps).min(view.ceiling_bps() as u32),
        })?;
        let config = StreamConfig {
            display,
            codec: codec as i32,
            mode: mode as i32,
            w,
            h,
            fps,
            chroma: chroma as i32,
        };
        let display_id = screen.display_id;
        *self
            .geometry
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (screen, (w, h), display);
        self.cursor.set_tracking(viewer_cursor);
        Ok(Pipeline {
            capturer,
            encoder,
            config,
            display_id,
        })
    }
}

/// Runs on the capture thread when the display changes or the viewer asks (§6.7).
struct Rebuilder {
    builder: Arc<Builder>,
    mode: Mode,
    fps: u32,
    display: u32,
    /// Quality and size: set by the session as the viewer asks, read at each rebuild.
    view: Arc<std::sync::Mutex<View>>,
    /// The streamed display's platform id, for audio capture.
    display_id: Arc<AtomicU32>,
    control: mpsc::UnboundedSender<ControlBody>,
    failed: mpsc::UnboundedSender<String>,
}

impl StreamRebuilder for Rebuilder {
    fn rebuild(&mut self, reason: RebuildReason) -> nexa_media::Result<Rebuilt> {
        let previous = (self.mode, self.fps, self.display);
        if let RebuildReason::Requested { mode, fps, display } = reason {
            self.mode = mode;
            self.fps = fps;
            self.display = display;
        }
        let view = *self.view.lock().unwrap_or_else(|e| e.into_inner());
        let mut built = self.builder.build(self.mode, self.fps, self.display, view);
        if built.is_err() && (self.mode, self.fps, self.display) != previous {
            // The viewer asked for something this host cannot do (a display that
            // went away, say): stay on what worked rather than end the session.
            (self.mode, self.fps, self.display) = previous;
            built = self.builder.build(self.mode, self.fps, self.display, view);
        }
        match built {
            Ok(p) => {
                let (mode, fps) = (self.mode, p.config.fps);
                self.display_id.store(p.display_id, Relaxed);
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

pub(crate) enum Inbound {
    Uni(StreamType, FramedRecv),
    Bi(StreamType, FramedSend, FramedRecv),
}

pub(crate) fn spawn_acceptors(
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

pub(crate) async fn control_writer(
    mut send: FramedSend,
    mut rx: mpsc::UnboundedReceiver<ControlBody>,
) {
    while let Some(body) = rx.recv().await {
        if send.send(&ControlMsg::from(body)).await.is_err() {
            return;
        }
    }
    send.finish_and_flush(FLUSH_TIMEOUT).await;
}

/// Every [`STREAM_LOG_EVERY`], one line: frames encoded and skipped per second, the
/// mean encode time, Mbit/s sent, and the rate controller's target and link estimate.
async fn log_stream(media: Arc<HostMedia>, session: u64, (w, h, fps): (u32, u32, u32)) {
    use std::sync::atomic::Ordering::Relaxed;
    let s = media.stats.clone();
    let read = || {
        (
            s.encoded.load(Relaxed),
            s.skipped_no_room.load(Relaxed),
            s.encode_us.load(Relaxed),
            s.bytes_sent.load(Relaxed),
            // What the viewer had to ask for: a frame lost beyond FEC, repaired from
            // the next frame on, and a key frame when that did not arrive either.
            // Each is a round trip or more of standing picture at the viewer.
            s.recoveries.load(Relaxed),
            s.idrs.load(Relaxed),
        )
    };
    let mut last = read();
    let mut last_stalls = 0;
    let mut tick = tokio::time::interval(STREAM_LOG_EVERY);
    tick.tick().await;
    loop {
        tick.tick().await;
        let now = read();
        let secs = STREAM_LOG_EVERY.as_secs_f64();
        let (frames, skipped) = (now.0 - last.0, now.1 - last.1);
        let encode_ms = if frames > 0 {
            (now.2 - last.2) as f64 / frames as f64 / 1e3
        } else {
            0.0
        };
        let sent = (now.3 - last.3) as f64 * 8.0 / secs / 1e6;
        let rate = media.rate_snapshot();
        let link = rate.link_bps.map_or("unknown".to_owned(), |l| {
            format!("{:.1} Mbit/s", l as f64 / 1e6)
        });
        eprintln!(
            "nexa-agent: session {session} stream {w}x{h}@{fps}: {:.1} frames/s, {:.1} skipped/s, encode {encode_ms:.1} ms, sent {sent:.2} Mbit/s, target {:.2} Mbit/s, encoder {:.2} Mbit/s, link {link}, queue {:.0} ms, loss {:.1}%, {} radio stalls, {} repairs, {} key frames asked, round trip {} ms",
            frames as f64 / secs,
            skipped as f64 / secs,
            rate.target_bps as f64 / 1e6,
            s.encoder_bps.load(Relaxed) as f64 / 1e6,
            rate.queue_ms,
            rate.loss * 100.0,
            rate.stalls - last_stalls,
            now.4 - last.4,
            now.5 - last.5,
            media.round_trip().as_millis(),
        );
        last = now;
        last_stalls = rate.stalls;
    }
}

const STREAM_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) struct AbortOnDrop(pub(crate) Option<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            t.abort();
        }
    }
}

/// §4.5 on the host: follow the viewer's `PathSwitch`, which only ever arrives on
/// this authenticated stream. Sessions on the listening address have no link and
/// ignore it.
pub(crate) struct PathFollower {
    pub(crate) link: Option<Arc<Link>>,
    pub(crate) last: Option<u32>,
}

impl PathFollower {
    /// §4.5: the viewer's candidates after a network change, from this stream.
    pub(crate) fn candidates(&self, c: &nexa_session::proto::CandidateUpdate) {
        if let Some(link) = &self.link {
            link.apply_candidate_update(&c.candidates);
        }
    }

    /// True when the path really changed (not the initial choice).
    pub(crate) fn apply(&mut self, path_id: u32) -> bool {
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
                body: Some(ControlBody::Candidates(c)),
            })) => path.candidates(&c),
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
    files: FileSlot,
) -> String {
    // An AI agent's session has no video: it looks when it asks (docs/MCP.md §13).
    if accepted.agent {
        return crate::agent_session::run(conn, accepted, env, stop, info, link, files).await;
    }
    let HostAccepted {
        control,
        perms,
        viewer_caps,
        ..
    } = accepted;
    let ControlChannel { send, mut recv } = control;
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(control_writer(send, ctl_rx));
    // §4.5: our candidates after a network change go to the viewer on this stream.
    let update_pump = link
        .as_ref()
        .and_then(|l| l.take_candidate_updates())
        .map(|mut updates| {
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
        });
    let _update_pump = AbortOnDrop(update_pump);

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

    // A display the viewer cannot have (unplugged since SessionAccept) means the first.
    let display = if display_at(&*env.platform, request.display).is_ok() {
        request.display
    } else {
        0
    };
    let screen = match display_at(&*env.platform, display) {
        Ok(s) => s,
        Err(e) => {
            finish(&conn, CloseReason::PipelineFailed, "display unavailable");
            return format!("display unavailable: {e}");
        }
    };
    let geometry = Arc::new(Geometry {
        state: Mutex::new((screen, (1, 1), display)),
    });
    // §6.9: the host's cursor is only worth tracking when the viewer's pointer drives it.
    let cursor_source = if perms.input {
        env.platform.cursor_source()
    } else {
        None
    };
    let cursor_control = Arc::new(CursorControl::default());
    let builder = Arc::new(Builder {
        platform: env.platform.clone(),
        host: env.caps.clone(),
        viewer: viewer_caps,
        geometry: geometry.clone(),
        viewer_draws_cursor: cursor_source.is_some(),
        cursor: cursor_control.clone(),
    });

    let mut current = (mode_of(&request), request.fps, display);
    let view = Arc::new(std::sync::Mutex::new(View::of(&request)));
    // The size the running pipeline was built for.
    let mut built_key = View::of(&request).size_key();
    let pipeline = {
        let (b, mode, fps, v) = (builder.clone(), current.0, current.1, View::of(&request));
        tokio::task::spawn_blocking(move || b.build(mode, fps, display, v)).await
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
    let session_id = info.id;
    env.ui.event(AgentEvent::SessionStarted(info));

    // Input is only ever wired up when SessionAccept granted it.
    let injected = Arc::new(InjectHistory::default());
    let inject = perms.input.then(|| {
        let (platform, injected) = (env.platform.clone(), injected.clone());
        InjectThread::spawn(
            Box::new(move || {
                platform
                    .input_sink()
                    .map(|sink| TrackingSink::wrap(sink, injected))
            }),
            geometry.clone() as Arc<dyn PointMap>,
        )
    });
    let cursor_thread = cursor_source.map(|source| {
        CursorThread::spawn(
            source,
            geometry.clone() as Arc<dyn StreamGeometry>,
            injected.clone(),
            cursor_control.clone(),
            ctl_tx.clone(),
        )
    });
    let on_pointer: Box<dyn Fn(nexa_wire::datagram::Pointer) + Send> = match &inject {
        Some(thread) => thread.pointer_sink(),
        None => Box::new(|_| {}),
    };

    let (failed_tx, mut failed_rx) = mpsc::unbounded_channel();
    let display_id = Arc::new(AtomicU32::new(pipeline.display_id));
    let media = Arc::new(HostMedia::spawn_with(
        conn.clone(),
        pipeline.capturer,
        pipeline.encoder,
        HostParams {
            stream: 0,
            fps: config.fps,
            mode: current.0,
            rate: RateConfig {
                initial_bps: u64::from(bitrate_for(config.w, config.h, config.fps))
                    .min(View::of(&request).ceiling_bps()),
                min_bps: 300_000,
                max_bps: View::of(&request).ceiling_bps(),
            },
        },
        on_pointer,
        HostOptions {
            rebuilder: Some(Box::new(Rebuilder {
                builder: builder.clone(),
                mode: current.0,
                fps: request.fps,
                display,
                view: view.clone(),
                display_id: display_id.clone(),
                control: ctl_tx.clone(),
                failed: failed_tx,
            })),
            thread_init: env.platform.thread_init(),
        },
    ));
    // What the stream does, in the log every few seconds: what was encoded, skipped
    // for want of room and sent, against the rate controller's target. A blurry or
    // slow picture can then be traced to its cause after the fact.
    let _stream_log = AbortOnDrop(Some(tokio::spawn(log_stream(
        media.clone(),
        session_id,
        (config.w, config.h, config.fps),
    ))));
    // §4.9: one bulk budget for the clipboard and every file transfer.
    let bulk = BulkLimiter::new(Arc::new(media.bulk_rate()));
    // Files exist only for sessions granted them, in both directions.
    let transfers = perms.file.then(|| {
        let (ui, session) = (env.ui.clone(), session_id);
        Transfers::new(
            conn.clone(),
            TransferConfig {
                downloads: env.files.downloads.clone(),
                limits: env.files.limits,
                bulk: bulk.clone(),
                confirm_timeout: CONFIRM_TIMEOUT,
            },
            Arc::new(move |event| ui.event(AgentEvent::Transfer { session, event })),
        )
    });
    *files.lock().unwrap_or_else(|e| e.into_inner()) = transfers.clone();

    // Audio exists only for sessions granted it; even then it waits for the viewer.
    let audio = perms.audio.then(|| {
        AudioSwitch::spawn(
            env.platform.clone(),
            env.ui.clone(),
            media.clone(),
            display_id.clone(),
        )
    });
    // A mode or display request that came too soon after the last rebuild waits for
    // its turn instead of being lost (the viewer's toolbar would show it otherwise).
    let mut deferred: Option<(Mode, u32, u32)> = None;

    let (in_tx, mut in_rx) = mpsc::unbounded_channel();
    let (uni_task, bi_task) = spawn_acceptors(&conn, in_tx);
    let mut input_task: Option<JoinHandle<()>> = None;
    let mut clipboard: Option<ClipSession> = None;
    let mut shells: Vec<AbortOnDrop> = Vec::new();
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
                    let next = (mode_of(&r), r.fps, r.display);
                    let asked = View::of(&r);
                    // The ceiling at once; the size, if it changed, with the next rebuild.
                    media.set_max_bps(asked.ceiling_bps());
                    *view.lock().unwrap_or_else(|e| e.into_inner()) = asked;
                    let resize = asked.size_key() != built_key;
                    deferred = (next != current || resize).then_some(next);
                }
                Ok(Some(ControlMsg { body: Some(ControlBody::Audio(a)) })) => {
                    // Without the audio permission there is nothing to switch on.
                    if let Some(audio) = &audio {
                        audio.set(a.enabled);
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
                Ok(Some(ControlMsg { body: Some(ControlBody::Candidates(c)) })) => path.candidates(&c),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break (CloseReason::Unspecified, "the control stream ended".to_owned()),
            },
            Some(reason) = failed_rx.recv() => {
                break (CloseReason::PipelineFailed, format!("capture failed: {reason}"));
            }
            _ = tokio::time::sleep_until((last_rebuild + MIN_REBUILD_INTERVAL).into()), if deferred.is_some() => {
                if let Some(next) = deferred.take() {
                    current = next;
                    built_key = view.lock().unwrap_or_else(|e| e.into_inner()).size_key();
                    last_rebuild = Instant::now();
                    media.request_rebuild(RebuildReason::Requested {
                        mode: next.0,
                        fps: next.1,
                        display: next.2,
                    });
                }
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
                        clipboard = start_clipboard(&env, send, clip_recv, bulk.clone());
                    } else {
                        clip_recv.stop();
                        drop(send);
                    }
                }
                Inbound::Bi(StreamType::File, send, file_recv) => match &transfers {
                    Some(t) => t.accept(send, file_recv),
                    // Refused unread: nothing is written without the permission.
                    None => transfer::refuse(send, file_recv),
                },
                Inbound::Bi(StreamType::Terminal, send, term_recv) => {
                    let task = if perms.terminal {
                        tokio::spawn(crate::shell::serve(send, term_recv, env.shell.clone()))
                    } else {
                        tokio::spawn(crate::shell::refuse(send, term_recv))
                    };
                    // Aborted with the session: the shells end with it.
                    shells.push(AbortOnDrop(Some(task)));
                }
                Inbound::Bi(_, send, other) => {
                    other.stop();
                    drop(send);
                }
            },
        }
    };

    // No transfer outlives its session; partial files are deleted by their tasks.
    *files.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if let Some(t) = &transfers {
        t.shutdown();
    }
    // Nothing more about the cursor after Goodbye, and no audio once the session ends.
    if let Some(c) = cursor_thread {
        let _ = tokio::task::spawn_blocking(move || c.stop()).await;
    }
    if let Some(a) = audio {
        a.shutdown().await;
    }
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
        media.shutdown();
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

pub(crate) fn start_clipboard(
    env: &Arc<SessionEnv>,
    send: FramedSend,
    recv: FramedRecv,
    bulk: Arc<BulkLimiter>,
) -> Option<ClipSession> {
    let channel = ClipChannel::default();
    let backend = match env.platform.clipboard() {
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
        (send, recv),
        ClipCallbacks {
            on_gate_release: Box::new(|_| {}),
            on_gate_progress: Box::new(|_| {}),
            on_notice: Box::new(move |n| ui.event(AgentEvent::ClipboardNotice(n))),
            // Nobody sits at the host to read a toolbar; the viewer shows both ways.
            // The log gets the copies that took a while, so "the paste waited" can be
            // told from what crossed and how long it took (the lines are timestamped).
            on_progress: Box::new(|p| eprintln!("{}", clipboard_log_line(&p))),
        },
        bulk,
    ))
}

/// One line of agent.log for a large copy: sizes and stages only, never content.
fn clipboard_log_line(p: &ClipProgress) -> String {
    let way = match p.direction {
        ClipDirection::Receiving => "from the viewer",
        ClipDirection::Sending => "to the viewer",
    };
    let stage = match p.stage {
        ClipStage::Moving => "on its way",
        ClipStage::Done => "across",
        ClipStage::Dropped => "given up",
    };
    format!(
        "nexa-agent: clipboard: {:?} {way} {stage}: {} of {} KiB",
        p.kind,
        p.done / 1024,
        p.size / 1024
    )
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
    fn host_points_map_back_to_stream_pixels_and_only_on_the_display() {
        let s = screen();
        for (px, py) in [(0, 0), (300, 100), (3023, 1963)] {
            let (x, y) = pixels_to_points(&s, (3024, 1964), px, py);
            assert_eq!(points_to_pixels(&s, (3024, 1964), x, y), Some((px, py)));
        }
        assert_eq!(
            points_to_pixels(&s, (1512, 982), 856.0, 541.0),
            Some((756, 491))
        );
        assert_eq!(points_to_pixels(&s, (3024, 1964), 99.0, 60.0), None);
        assert_eq!(points_to_pixels(&s, (3024, 1964), 1612.0, 60.0), None);
    }

    #[test]
    fn geometry_follows_the_streamed_display() {
        let g = Geometry {
            state: Mutex::new((screen(), (1512, 982), 1)),
        };
        assert_eq!(
            g.to_points(0, 10, 10),
            None,
            "input for another display is dropped"
        );
        assert_eq!(g.to_points(1, 0, 0), Some((100.0, 50.0)));
        assert_eq!(g.points_to_stream(101.0, 51.0), Some((1, 1, 1)));
        assert_eq!(g.stream_pixels_per_point(), 1.0);
    }

    fn view(quality: StreamQuality, lines: u32, window: (u32, u32)) -> View {
        View {
            quality,
            lines,
            window,
        }
    }

    #[test]
    fn the_picture_follows_the_viewer_s_choices() {
        const ANY: (u32, u32) = (u32::MAX, u32::MAX);
        let window = (1440, 900);
        // Auto fits the window; with no window said, the host's own pixels.
        assert_eq!(view(StreamQuality::Auto, 0, window).bound(), window);
        assert_eq!(view(StreamQuality::Auto, 0, (0, 0)).bound(), ANY);
        // A tiny window still gets a readable picture.
        assert_eq!(
            view(StreamQuality::Auto, 0, (300, 200)).bound(),
            SMALLEST_WINDOW
        );
        // Clear is the host's own pixels, Weak at most 720 lines, whatever the window.
        assert_eq!(view(StreamQuality::Clear, 0, window).bound(), ANY);
        assert_eq!(
            view(StreamQuality::Weak, 0, window).bound(),
            (u32::MAX, 720)
        );
        // A resolution chosen wins over the quality's.
        assert_eq!(view(StreamQuality::Weak, LINES_NATIVE, window).bound(), ANY);
        assert_eq!(
            view(StreamQuality::Clear, 1080, window).bound(),
            (u32::MAX, 1080)
        );
    }

    #[test]
    fn quality_sets_the_ceiling_and_weak_the_frame_rate() {
        let at = |q| view(q, 0, (0, 0));
        assert_eq!(at(StreamQuality::Auto).ceiling_bps(), MAX_RATE_BPS);
        assert_eq!(at(StreamQuality::Clear).ceiling_bps(), MAX_RATE_BPS);
        assert!(at(StreamQuality::Balanced).ceiling_bps() < MAX_RATE_BPS);
        assert!(at(StreamQuality::Weak).ceiling_bps() < at(StreamQuality::Balanced).ceiling_bps());
        assert_eq!(at(StreamQuality::Weak).fps_cap(), 30);
        assert_eq!(at(StreamQuality::Auto).fps_cap(), 0);
    }

    #[test]
    fn bitrate_stays_in_range() {
        assert_eq!(bitrate_for(100, 100, 1), MIN_BITRATE);
        assert_eq!(bitrate_for(7680, 4320, 120), MAX_BITRATE);
    }
}
