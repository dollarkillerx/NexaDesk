//! Host media path (§1.1). Capture and encode run inline on one thread; a second
//! thread packetizes and paces; a third handles everything arriving from the viewer.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed},
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use nexa_media::{
    CaptureEvent, CapturedFrame, Capturer, DisplayMode, EncodeOpts, Encoder, MediaError,
    NativeImage,
};
use nexa_net::{
    rate::{EncoderThrottle, RateConfig, RateController, SentPacket},
    recovery::{SenderAction, SenderRecovery},
    shard::{FrameInfo, Packetizer},
};
use nexa_wire::{
    control::Mode,
    datagram::{self, Clock, Pointer, ReportKind, TransportFeedback, VideoReport, flags},
};
use quinn::Connection;

use crate::{
    audio::{AudioCapture, AudioEncoder, AudioPacketizer, Framer},
    wait_until,
};

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub frame_id: u32,
    pub flags: u8,
    pub ref_epoch: u8,
    pub capture_ts_us: u32,
    pub data: Vec<u8>,
}

/// Where encoded frames go. QUIC is the native path; a browser transport can be
/// added as another sink without touching capture or encode.
pub trait FrameSink: Send {
    /// `false` means the next frame must not be encoded (§1.3: drop only before encoding).
    fn has_room(&self) -> bool;
    fn push(&mut self, frame: EncodedFrame);
}

#[derive(Debug, Clone)]
pub struct HostParams {
    pub stream: u8,
    pub fps: u32,
    pub mode: Mode,
    pub rate: RateConfig,
}

#[derive(Default, Debug)]
pub struct HostStats {
    pub encoded: AtomicU64,
    pub skipped_no_room: AtomicU64,
    pub idle_refreshes: AtomicU64,
    pub packets: AtomicU64,
    pub recoveries: AtomicU64,
    pub idrs: AtomicU64,
    pub encoder_errors: AtomicU64,
    pub malformed: AtomicU64,
    pub capture_lost: AtomicU64,
}

enum Cmd {
    Report(VideoReport),
    /// §4.5: the path changed under QUIC.
    PathSwitched,
    SetBitrate(u32),
    Rebuild(RebuildReason),
}

/// Why the capture and encode stages must be recreated (§6.7, §13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildReason {
    /// The display changed resolution or refresh rate.
    ModeChanged(DisplayMode),
    /// The capturer stopped delivering (screen locked, permission change, ...).
    AccessLost,
    /// The viewer asked for a different mode, frame rate or display (§6.7).
    Requested { mode: Mode, fps: u32, display: u32 },
}

pub struct Rebuilt {
    pub capturer: Box<dyn Capturer>,
    pub encoder: Box<dyn Encoder>,
    pub mode: Mode,
    pub fps: u32,
}

/// Recreates the pipeline on the capture thread. It also tells the viewer: the new
/// `StreamConfig` must be sent before this returns, so it precedes the first frame.
pub trait StreamRebuilder: Send {
    fn rebuild(&mut self, reason: RebuildReason) -> nexa_media::Result<Rebuilt>;
}

/// Optional behaviour for [`HostMedia::spawn_with`].
#[derive(Default)]
pub struct HostOptions {
    pub rebuilder: Option<Box<dyn StreamRebuilder>>,
    /// Runs first on every media thread; the assembly layer sets priorities here (§1.1).
    pub thread_init: Option<fn()>,
}

/// Repeats the last frame after the screen goes quiet (§6.8). Besides sharpening
/// the picture this guarantees the viewer can notice a lost final frame, which it
/// otherwise never could: nothing follows it.
const IDLE_REFRESH_AFTER: [Duration; 4] = [
    Duration::from_millis(100),
    Duration::from_millis(300),
    Duration::from_millis(600),
    Duration::from_millis(1000),
];

const MAX_QUEUED_FRAMES: usize = 2;

/// Sends media datagrams and gives each its `tx_seq` at the moment it is sent, so
/// sequence order is send order even with video and audio on different threads. The
/// viewer's feedback would otherwise report a shard still being paced as lost when an
/// audio packet numbered after it arrived first.
struct DatagramTx {
    conn: Connection,
    next_seq: Mutex<u32>,
}

impl DatagramTx {
    /// Stamps `tx_seq` (bytes 4..8 in both VideoShard and AudioPacket) and sends.
    fn send(
        &self,
        packet: Bytes,
        frame_id: Option<u32>,
        rate: &Mutex<RateController>,
    ) -> Result<(), quinn::SendDatagramError> {
        let mut packet = packet
            .try_into_mut()
            .unwrap_or_else(|shared| BytesMut::from(&shared[..]));
        let mut seq = self.next_seq.lock().unwrap_or_else(|e| e.into_inner());
        let tx_seq = *seq;
        packet[4..8].copy_from_slice(&tx_seq.to_le_bytes());
        let bytes = packet.len() as u32;
        self.conn.send_datagram(packet.freeze())?;
        *seq = seq.wrapping_add(1);
        drop(seq);
        rate.lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_packet_sent(
                SentPacket {
                    tx_seq,
                    frame_id,
                    bytes,
                },
                Instant::now(),
            );
        Ok(())
    }
}

#[derive(Default, Debug)]
pub struct AudioStats {
    pub frames: AtomicU64,
    pub packets: AtomicU64,
    /// Not sent because the datagram queue was full: late audio is useless.
    pub dropped: AtomicU64,
    pub encode_errors: AtomicU64,
}

struct AudioThread {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

pub struct HostMedia {
    stop: Arc<AtomicBool>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    cmds: Mutex<Sender<Cmd>>,
    tx: Arc<DatagramTx>,
    rate: Arc<Mutex<RateController>>,
    base: Instant,
    thread_init: Option<fn()>,
    audio: Mutex<Option<AudioThread>>,
    pub stats: Arc<HostStats>,
    pub audio_stats: Arc<AudioStats>,
}

impl HostMedia {
    /// `on_pointer` runs on the receive thread and must not block.
    pub fn spawn(
        conn: Connection,
        capturer: Box<dyn Capturer>,
        encoder: Box<dyn Encoder>,
        params: HostParams,
        on_pointer: Box<dyn Fn(Pointer) + Send>,
    ) -> Self {
        Self::spawn_with(
            conn,
            capturer,
            encoder,
            params,
            on_pointer,
            HostOptions::default(),
        )
    }

    /// [`Self::spawn`], with rebuilding after display changes and thread setup.
    pub fn spawn_with(
        conn: Connection,
        capturer: Box<dyn Capturer>,
        encoder: Box<dyn Encoder>,
        params: HostParams,
        on_pointer: Box<dyn Fn(Pointer) + Send>,
        options: HostOptions,
    ) -> Self {
        let HostOptions {
            rebuilder,
            thread_init,
        } = options;
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(HostStats::default());
        let rate = Arc::new(Mutex::new(RateController::new(
            params.rate,
            params.mode,
            Instant::now(),
        )));
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::sync_channel(MAX_QUEUED_FRAMES);
        let depth = Arc::new(AtomicUsize::new(0));
        let fps = Arc::new(AtomicU32::new(params.fps));
        let base = Instant::now();
        let tx = Arc::new(DatagramTx {
            conn: conn.clone(),
            next_seq: Mutex::new(0),
        });
        let sink = Box::new(QuicSink {
            tx: frame_tx,
            depth: depth.clone(),
        });

        let mut threads = Vec::new();
        {
            let pipeline = Pipeline {
                capturer,
                encoder,
                rebuilder,
                rate: rate.clone(),
                fps: fps.clone(),
            };
            let (stop, stats) = (stop.clone(), stats.clone());
            threads.push(spawn("capture-encode", thread_init, move || {
                run_encoder(pipeline, sink, cmd_rx, base, stop, stats)
            }));
        }
        {
            let (c, r, cmds, p, stop, stats) = (
                conn.clone(),
                rate.clone(),
                cmd_tx.clone(),
                params.clone(),
                stop.clone(),
                stats.clone(),
            );
            let (fps, tx) = (fps.clone(), tx.clone());
            threads.push(spawn("media-tx", thread_init, move || {
                run_tx(c, tx, frame_rx, depth, r, cmds, p, fps, stop, stats)
            }));
        }
        {
            let (stop, stats) = (stop.clone(), stats.clone());
            let (cmds, rate) = (cmd_tx.clone(), rate.clone());
            threads.push(spawn("media-back", thread_init, move || {
                run_back(conn, rate, cmds, on_pointer, base, stop, stats)
            }));
        }
        Self {
            stop,
            threads: Mutex::new(threads),
            cmds: Mutex::new(cmd_tx),
            tx,
            rate,
            base,
            thread_init,
            audio: Mutex::new(None),
            stats,
            audio_stats: Arc::new(AudioStats::default()),
        }
    }

    /// Starts sending system audio from `capture`, replacing any audio already running.
    /// The caller decides whether the session may: this only does what it is told.
    pub fn start_audio(&self, capture: Box<dyn AudioCapture>, encoder: Box<dyn AudioEncoder>) {
        self.stop_audio();
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rate, stats, base) = (
            self.tx.clone(),
            self.rate.clone(),
            self.audio_stats.clone(),
            self.base,
        );
        let (s, session_stop) = (stop.clone(), self.stop.clone());
        let join = spawn("audio", self.thread_init, move || {
            run_audio(capture, encoder, tx, rate, base, s, session_stop, stats)
        });
        *self.audio.lock().unwrap_or_else(|e| e.into_inner()) = Some(AudioThread { stop, join });
    }

    /// Stops audio capture and waits for it to end. Harmless when none is running.
    pub fn stop_audio(&self) {
        let running = self.audio.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(a) = running {
            a.stop.store(true, Relaxed);
            let _ = a.join.join();
        }
    }

    pub fn audio_running(&self) -> bool {
        self.audio
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// The session's bulk allocation (§4.9), for file and clipboard transfers.
    pub fn bulk_rate(&self) -> crate::bulk::HostBulkRate {
        crate::bulk::HostBulkRate(self.rate.clone())
    }

    /// §4.5 side effects of a path switch: the rate restarts at 70% and the next
    /// frame is a key frame, since packets in flight on the old path may be lost.
    pub fn path_switched(&self) {
        self.send_cmd(Cmd::PathSwitched);
    }

    /// Asks the capture thread to rebuild the pipeline (§6.7).
    pub fn request_rebuild(&self, reason: RebuildReason) {
        self.send_cmd(Cmd::Rebuild(reason));
    }

    pub fn stop(self) {
        self.shutdown();
    }

    /// [`Self::stop`] for a shared handle. Later calls return at once.
    pub fn shutdown(&self) {
        self.stop_audio();
        self.stop.store(true, Relaxed);
        let threads = std::mem::take(&mut *self.threads.lock().unwrap_or_else(|e| e.into_inner()));
        for t in threads {
            let _ = t.join();
        }
    }

    fn send_cmd(&self, cmd: Cmd) {
        let _ = self
            .cmds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(cmd);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_audio(
    mut capture: Box<dyn AudioCapture>,
    mut encoder: Box<dyn AudioEncoder>,
    tx: Arc<DatagramTx>,
    rate: Arc<Mutex<RateController>>,
    base: Instant,
    stop: Arc<AtomicBool>,
    session_stop: Arc<AtomicBool>,
    stats: Arc<AudioStats>,
) {
    let mut framer = Framer::default();
    let mut packetizer = AudioPacketizer::new(base);
    let (mut opus, mut datagram) = (Vec::with_capacity(1500), Vec::with_capacity(1500));
    while !stop.load(Relaxed) && !session_stop.load(Relaxed) {
        let chunk = match capture.next(Duration::from_millis(50)) {
            Ok(Some(c)) => c,
            Ok(None) => continue,
            Err(_) => return,
        };
        let mut failed = false;
        framer.push(&chunk, |pcm, at| {
            if failed {
                return;
            }
            stats.frames.fetch_add(1, Relaxed);
            opus.clear();
            if encoder.encode(pcm, &mut opus).is_err() {
                stats.encode_errors.fetch_add(1, Relaxed);
                return;
            }
            if opus.is_empty() {
                return; // the codec is still filling its look-ahead
            }
            datagram.clear();
            packetizer.packet(&opus, at, &mut datagram);
            if tx.conn.datagram_send_buffer_space() < datagram.len() {
                stats.dropped.fetch_add(1, Relaxed);
                return;
            }
            match tx.send(Bytes::copy_from_slice(&datagram), None, &rate) {
                Ok(()) => {
                    stats.packets.fetch_add(1, Relaxed);
                }
                Err(_) => failed = true,
            }
        });
        if failed {
            return;
        }
    }
}

fn spawn(name: &str, init: Option<fn()>, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            if let Some(init) = init {
                init();
            }
            f()
        })
        .expect("spawn thread")
}

struct QuicSink {
    tx: SyncSender<EncodedFrame>,
    depth: Arc<AtomicUsize>,
}

impl FrameSink for QuicSink {
    fn has_room(&self) -> bool {
        self.depth.load(Relaxed) < MAX_QUEUED_FRAMES
    }
    fn push(&mut self, frame: EncodedFrame) {
        self.depth.fetch_add(1, Relaxed);
        // Only one producer and `has_room` passed, so this cannot block for long.
        let _ = self.tx.send(frame);
    }
}

fn frame_interval(fps: u32) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(fps.max(1)))
}

struct Pipeline {
    capturer: Box<dyn Capturer>,
    encoder: Box<dyn Encoder>,
    rebuilder: Option<Box<dyn StreamRebuilder>>,
    rate: Arc<Mutex<RateController>>,
    fps: Arc<AtomicU32>,
}

fn run_encoder(
    pipeline: Pipeline,
    mut sink: Box<dyn FrameSink>,
    cmds: Receiver<Cmd>,
    base: Instant,
    stop: Arc<AtomicBool>,
    stats: Arc<HostStats>,
) {
    let Pipeline {
        mut capturer,
        mut encoder,
        mut rebuilder,
        rate,
        fps,
    } = pipeline;
    let mut interval = frame_interval(fps.load(Relaxed));
    let mut pending_rebuild: Option<RebuildReason> = None;
    let mut recovery = SenderRecovery::new();
    let mut next_id = 0u32;
    let mut last: Option<(NativeImage, Instant)> = None;
    let mut recovery_pending = false;
    let mut force_key = false;
    let mut refresh_step = IDLE_REFRESH_AFTER.len();
    let mut last_content = Instant::now();
    let mut buf = Vec::with_capacity(1 << 20);

    while !stop.load(Relaxed) {
        while let Ok(cmd) = cmds.try_recv() {
            match cmd {
                Cmd::SetBitrate(bps) => {
                    if encoder.set_bitrate(bps).is_err() {
                        stats.encoder_errors.fetch_add(1, Relaxed);
                    }
                }
                Cmd::Rebuild(reason) => pending_rebuild = Some(reason),
                Cmd::PathSwitched => {
                    rate.lock().unwrap().on_path_switch(Instant::now());
                    force_key = true;
                    recovery_pending = true;
                }
                Cmd::Report(r) if r.kind == ReportKind::FrameAck => {
                    encoder.on_frame_acked(r.last_good_frame)
                }
                Cmd::Report(r) => match recovery.on_report(&r, Instant::now()) {
                    SenderAction::None => {}
                    SenderAction::Invalidate { first_lost } => {
                        stats.recoveries.fetch_add(1, Relaxed);
                        if encoder.invalidate_from(first_lost).is_err() {
                            stats.encoder_errors.fetch_add(1, Relaxed);
                            force_key = true;
                        }
                        recovery_pending = true;
                    }
                    SenderAction::ForceIdr => {
                        stats.idrs.fetch_add(1, Relaxed);
                        force_key = true;
                        recovery_pending = true;
                    }
                },
            }
        }

        if let Some(reason) = pending_rebuild.take() {
            let Some(Ok(new)) = rebuilder.as_mut().map(|r| r.rebuild(reason)) else {
                return;
            };
            capturer = new.capturer;
            encoder = new.encoder;
            interval = frame_interval(new.fps);
            fps.store(new.fps, Relaxed);
            rate.lock().unwrap().set_mode(new.mode);
            // The old frame has the old size, and the new session starts clean.
            last = None;
            recovery_pending = false;
            force_key = true;
            refresh_step = IDLE_REFRESH_AFTER.len();
            continue;
        }

        let frame = match capturer.next(interval * 2) {
            Ok(CaptureEvent::Frame(f)) => {
                last = Some((f.image.clone(), f.captured_at));
                last_content = Instant::now();
                refresh_step = 0;
                f
            }
            Ok(CaptureEvent::Idle) => {
                let refresh_due = refresh_step < IDLE_REFRESH_AFTER.len()
                    && last_content.elapsed() >= IDLE_REFRESH_AFTER[refresh_step];
                let Some((image, captured_at)) =
                    last.clone().filter(|_| recovery_pending || refresh_due)
                else {
                    continue;
                };
                if refresh_due {
                    refresh_step += 1;
                    stats.idle_refreshes.fetch_add(1, Relaxed);
                }
                CapturedFrame {
                    image,
                    captured_at,
                    dirty: Vec::new(),
                }
            }
            Ok(CaptureEvent::ModeChanged(mode)) => {
                pending_rebuild = Some(RebuildReason::ModeChanged(mode));
                if rebuilder.is_none() {
                    stats.capture_lost.fetch_add(1, Relaxed);
                    return;
                }
                continue;
            }
            Ok(CaptureEvent::AccessLost) | Err(_) => {
                stats.capture_lost.fetch_add(1, Relaxed);
                pending_rebuild = Some(RebuildReason::AccessLost);
                if rebuilder.is_none() {
                    return;
                }
                continue;
            }
        };

        if !sink.has_room() {
            stats.skipped_no_room.fetch_add(1, Relaxed);
            continue;
        }
        buf.clear();
        match encoder.encode(&frame, next_id, EncodeOpts { force_key }, &mut buf) {
            Ok(meta) => {
                let mut f = 0;
                if meta.key {
                    f |= flags::IDR;
                }
                if recovery_pending || meta.recovery_point {
                    f |= flags::RECOVERY_POINT;
                }
                sink.push(EncodedFrame {
                    frame_id: next_id,
                    flags: f,
                    ref_epoch: recovery.epoch(),
                    capture_ts_us: frame
                        .captured_at
                        .saturating_duration_since(base)
                        .as_micros() as u32,
                    data: buf.clone(),
                });
                next_id = next_id.wrapping_add(1);
                force_key = false;
                recovery_pending = false;
                stats.encoded.fetch_add(1, Relaxed);
            }
            // Nothing was produced and the reference chain is intact.
            Err(MediaError::FrameDropped) => {}
            Err(_) => {
                stats.encoder_errors.fetch_add(1, Relaxed);
                force_key = true;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_tx(
    conn: Connection,
    tx: Arc<DatagramTx>,
    frames: Receiver<EncodedFrame>,
    depth: Arc<AtomicUsize>,
    rate: Arc<Mutex<RateController>>,
    cmds: Sender<Cmd>,
    p: HostParams,
    fps: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    stats: Arc<HostStats>,
) {
    let mut packetizer = Packetizer::default();
    let mut throttle = EncoderThrottle::new();

    while !stop.load(Relaxed) {
        let f = match frames.recv_timeout(Duration::from_millis(50)) {
            Ok(f) => f,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let now = Instant::now();
        let alloc = rate.lock().unwrap().allocation(now);
        if let Some(bps) = throttle.update(alloc.video_bps, now) {
            let _ = cmds.send(Cmd::SetBitrate(bps.min(u64::from(u32::MAX)) as u32));
        }
        let Some(max_dg) = conn.max_datagram_size() else {
            return;
        };
        let info = FrameInfo {
            stream: p.stream,
            flags: f.flags,
            ref_epoch: f.ref_epoch,
            frame_id: f.frame_id,
            capture_ts_us: f.capture_ts_us,
        };
        let Ok(packets) = packetizer.packetize(&f.data, info, alloc.fec_ratio, max_dg) else {
            depth.fetch_sub(1, Relaxed);
            continue;
        };
        let bytes: u64 = packets.iter().map(|x| x.len() as u64).sum();
        let interval = frame_interval(fps.load(Relaxed));
        let window = rate.lock().unwrap().pacing_window(bytes, interval);
        let start = Instant::now();
        let n = packets.len();
        for (i, pkt) in packets.into_iter().enumerate() {
            let due = start + window.mul_f64(i as f64 / n as f64);
            if due > Instant::now() + Duration::from_micros(500) {
                wait_until(due);
            }
            // send_datagram silently drops the oldest queued datagram when full.
            while conn.datagram_send_buffer_space() < pkt.len() {
                if stop.load(Relaxed) || conn.close_reason().is_some() {
                    return;
                }
                thread::sleep(Duration::from_micros(100));
            }
            if tx.send(pkt, Some(f.frame_id), &rate).is_err() {
                return;
            }
            stats.packets.fetch_add(1, Relaxed);
        }
        depth.fetch_sub(1, Relaxed);
    }
}

/// `base` is the clock `capture_ts_us` counts from, so the viewer's clock offset
/// from `ClockPong` lines up with capture times (§6.1 cross-machine segments).
fn run_back(
    conn: Connection,
    rate: Arc<Mutex<RateController>>,
    cmds: Sender<Cmd>,
    on_pointer: Box<dyn Fn(Pointer) + Send>,
    base: Instant,
    stop: Arc<AtomicBool>,
    stats: Arc<HostStats>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    rt.block_on(async move {
        while !stop.load(Relaxed) {
            let d = match tokio::time::timeout(Duration::from_millis(100), conn.read_datagram())
                .await
            {
                Err(_) => continue,
                Ok(Err(_)) => {
                    stop.store(true, Relaxed);
                    return;
                }
                Ok(Ok(d)) => d,
            };
            let ok = match d.first() {
                Some(&datagram::TYPE_FEEDBACK) => TransportFeedback::parse(&d)
                    .map(|fb| rate.lock().unwrap().on_feedback(&fb, Instant::now()))
                    .is_ok(),
                Some(&datagram::TYPE_VIDEO_REPORT) => VideoReport::parse(&d)
                    .map(|r| cmds.send(Cmd::Report(r)))
                    .is_ok(),
                Some(&datagram::TYPE_POINTER) => Pointer::parse(&d).map(&on_pointer).is_ok(),
                Some(&datagram::TYPE_CLOCK) => match Clock::parse(&d) {
                    Ok(Clock::Ping { t1 }) => {
                        let t = base.elapsed().as_micros() as u64;
                        let _ = conn.send_datagram(bytes::Bytes::copy_from_slice(
                            &Clock::Pong { t1, t2: t, t3: t }.to_bytes(),
                        ));
                        true
                    }
                    Ok(_) => true,
                    Err(_) => false,
                },
                _ => false,
            };
            if !ok {
                stats.malformed.fetch_add(1, Relaxed);
            }
        }
    });
}
