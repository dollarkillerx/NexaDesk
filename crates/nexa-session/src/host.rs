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
    /// Time spent in `Encoder::encode`, in µs, over all frames.
    pub encode_us: AtomicU64,
    /// Video bytes handed to the connection, repair and probe padding included.
    pub bytes_sent: AtomicU64,
    /// The bitrate last given to the encoder.
    pub encoder_bps: AtomicU64,
    pub skipped_no_room: AtomicU64,
    pub idle_refreshes: AtomicU64,
    pub packets: AtomicU64,
    pub recoveries: AtomicU64,
    pub idrs: AtomicU64,
    pub encoder_errors: AtomicU64,
    pub malformed: AtomicU64,
    pub capture_lost: AtomicU64,
    /// Frames the encoder finished after their own encode call (`Encoder::take_ready`).
    pub late_frames: AtomicU64,
    /// Keys shown (`crate::typing`): how many, and the time from each key going in to the
    /// picture that shows it, in µs: the sum and the longest since the last look.
    pub typed: AtomicU64,
    pub typed_us: AtomicU64,
    pub typed_max_us: AtomicU64,
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
pub const IDLE_REFRESH_AFTER: [Duration; 4] = [
    Duration::from_millis(100),
    Duration::from_millis(300),
    Duration::from_millis(600),
    Duration::from_millis(1000),
];

const MAX_QUEUED_FRAMES: usize = 2;

/// How often a rebuild is tried again while the display cannot be captured for now
/// ([`MediaError::Unavailable`]: a UAC prompt or the lock screen on Windows). The
/// session stays up on its last picture meanwhile.
pub const UNAVAILABLE_RETRY: Duration = Duration::from_millis(500);

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

/// What the rate controller thinks of the link now, for the stream log.
#[derive(Clone, Copy, Debug)]
pub struct RateSnapshot {
    pub target_bps: u64,
    pub link_bps: Option<u64>,
    /// Radio stalls set aside so far.
    pub stalls: u64,
    /// The bottleneck's queue, as of the last frame.
    pub queue_ms: f64,
    /// Smoothed loss, 0 to 1.
    pub loss: f64,
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

    /// The most the rate controller may send, from the viewer's quality choice.
    /// The rate controller's target and its estimate of the link, in bit/s, and the
    /// radio stalls it has set aside so far.
    pub fn rate_snapshot(&self) -> RateSnapshot {
        let rate = self.rate.lock().unwrap_or_else(|e| e.into_inner());
        RateSnapshot {
            target_bps: rate.target_bps(),
            link_bps: rate.link_capacity_bps(),
            stalls: rate.stalls(),
            queue_ms: rate.queue_delay_ms(),
            loss: rate.loss_ewma(),
        }
    }

    /// QUIC's estimate of the round trip to the viewer, for the stream log.
    pub fn round_trip(&self) -> Duration {
        self.tx.conn.rtt()
    }

    pub fn set_max_bps(&self, max_bps: u64) {
        self.rate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_max_bps(max_bps);
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

/// Holds the place of the capture and encode stages while they are rebuilt: the old
/// ones are let go first, since some platforms allow only one at a time (DXGI desktop
/// duplication: one per display per process). Never used to capture or encode.
struct Released;

impl Capturer for Released {
    fn next(&mut self, _: Duration) -> nexa_media::Result<CaptureEvent> {
        Ok(CaptureEvent::Idle)
    }
}

impl Encoder for Released {
    fn caps(&self) -> nexa_media::EncoderCaps {
        nexa_media::EncoderCaps::default()
    }
    fn encode(
        &mut self,
        _: &CapturedFrame,
        _: nexa_media::FrameId,
        _: EncodeOpts,
        _: &mut Vec<u8>,
    ) -> nexa_media::Result<nexa_media::EncodedMeta> {
        Err(MediaError::Other("no encoder while rebuilding".into()))
    }
    fn set_bitrate(&mut self, _: u32) -> nexa_media::Result<()> {
        Ok(())
    }
    fn invalidate_from(
        &mut self,
        _: nexa_media::FrameId,
    ) -> nexa_media::Result<nexa_media::Recovery> {
        Err(MediaError::Other("no encoder while rebuilding".into()))
    }
    fn on_frame_acked(&mut self, _: nexa_media::FrameId) {}
    fn force_idr(&mut self) {}
}

/// Sends the frames the encoder finished late (`Encoder::take_ready`), each as a frame
/// of its own and in order, stamped with `captured_at` (the newest capture they could
/// show). False when the encoder failed, and the next frame must be a key frame.
#[allow(clippy::too_many_arguments)]
fn send_ready(
    encoder: &mut dyn Encoder,
    sink: &mut dyn FrameSink,
    buf: &mut Vec<u8>,
    next_id: &mut u32,
    ref_epoch: u8,
    captured_at: Instant,
    base: Instant,
    stats: &HostStats,
) -> bool {
    loop {
        buf.clear();
        match encoder.take_ready(*next_id, buf) {
            Ok(Some(meta)) => {
                sink.push(EncodedFrame {
                    frame_id: *next_id,
                    flags: if meta.key { flags::IDR } else { 0 },
                    ref_epoch,
                    capture_ts_us: captured_at.saturating_duration_since(base).as_micros() as u32,
                    data: buf.clone(),
                });
                *next_id = next_id.wrapping_add(1);
                stats.encoded.fetch_add(1, Relaxed);
                stats.late_frames.fetch_add(1, Relaxed);
            }
            Ok(None) => return true,
            Err(_) => {
                stats.encoder_errors.fetch_add(1, Relaxed);
                return false;
            }
        }
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
    // Scheduled as media work (MMCSS on Windows): frames go on time.
    nexa_media::media_thread();
    let Pipeline {
        mut capturer,
        mut encoder,
        mut rebuilder,
        rate,
        fps,
    } = pipeline;
    let mut interval = frame_interval(fps.load(Relaxed));
    let mut pending_rebuild: Option<RebuildReason> = None;
    // Set while the display cannot be captured for now: the next try.
    let mut retry_at: Option<Instant> = None;
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

        if let Some(at) = retry_at {
            let now = Instant::now();
            if now < at {
                // Short naps, so commands and a stop are still seen.
                thread::sleep((at - now).min(Duration::from_millis(50)));
                continue;
            }
        }
        if let Some(reason) = pending_rebuild.take() {
            let Some(rebuilder) = rebuilder.as_mut() else {
                return;
            };
            // The old stages go first: the new capture may need the display to itself.
            drop(std::mem::replace(&mut capturer, Box::new(Released)));
            drop(std::mem::replace(&mut encoder, Box::new(Released)));
            let new = match rebuilder.rebuild(reason) {
                Ok(new) => new,
                // The display comes back by itself; a newer request replaces this one.
                Err(MediaError::Unavailable(_)) => {
                    pending_rebuild = Some(reason);
                    retry_at = Some(Instant::now() + UNAVAILABLE_RETRY);
                    continue;
                }
                Err(_) => return,
            };
            retry_at = None;
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

        // An idle refresh, not a new picture (`flags::REPEAT`).
        let mut repeat = false;
        let frame = match capturer.next(interval * 2) {
            Ok(CaptureEvent::Frame(f)) => {
                if let Some(d) = crate::typing::picture(f.captured_at) {
                    let us = d.as_micros() as u64;
                    stats.typed.fetch_add(1, Relaxed);
                    stats.typed_us.fetch_add(us, Relaxed);
                    stats.typed_max_us.fetch_max(us, Relaxed);
                }
                last = Some((f.image.clone(), f.captured_at));
                last_content = Instant::now();
                refresh_step = 0;
                f
            }
            Ok(CaptureEvent::Idle) => {
                // A frame the encoder held goes now, not with the next change, which on a
                // still screen can be seconds away.
                if let Some((_, captured_at)) = &last
                    && !send_ready(
                        &mut *encoder,
                        &mut *sink,
                        &mut buf,
                        &mut next_id,
                        recovery.epoch(),
                        *captured_at,
                        base,
                        &stats,
                    )
                {
                    force_key = true;
                }
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
                repeat = true;
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
        let started = Instant::now();
        let encoded = encoder.encode(&frame, next_id, EncodeOpts { force_key }, &mut buf);
        stats
            .encode_us
            .fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        match encoded {
            Ok(meta) => {
                let mut f = 0;
                if meta.key {
                    f |= flags::IDR;
                }
                if recovery_pending || meta.recovery_point {
                    f |= flags::RECOVERY_POINT;
                }
                if repeat {
                    f |= flags::REPEAT;
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
                // And whatever else is ready: a late frame's output came first.
                if !send_ready(
                    &mut *encoder,
                    &mut *sink,
                    &mut buf,
                    &mut next_id,
                    recovery.epoch(),
                    frame.captured_at,
                    base,
                    &stats,
                ) {
                    force_key = true;
                }
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
    // Scheduled as media work (MMCSS on Windows): frames go on time.
    nexa_media::media_thread();
    let mut packetizer = Packetizer::default();
    let mut throttle = EncoderThrottle::new();
    let mut last_capture_us: Option<u32> = None;

    while !stop.load(Relaxed) {
        let f = match frames.recv_timeout(Duration::from_millis(50)) {
            Ok(f) => f,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                rate.lock().unwrap().set_send_backlog(false);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let now = Instant::now();
        // `depth` counts this frame too: more than one means frames are waiting.
        let alloc = {
            let mut rate = rate.lock().unwrap();
            rate.set_send_backlog(depth.load(Relaxed) > 1);
            rate.allocation(now)
        };
        if let Some(bps) = throttle.update(alloc.video_bps, now) {
            stats.encoder_bps.store(bps, Relaxed);
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
        let interval = frame_interval(fps.load(Relaxed));
        // The time this frame covers: since the one before it, by capture time. The
        // session's first frame follows nothing, like one after a still screen.
        let covers = last_capture_us.map_or(Duration::MAX, |last| {
            // A repeated frame (an idle refresh) has its original's time: no time.
            let since = f.capture_ts_us.wrapping_sub(last) as i32;
            Duration::from_micros(since.max(0) as u64)
        });
        last_capture_us = Some(f.capture_ts_us);
        // §4.9 recovery probes: what fills this frame up to the probe's rate.
        let wire = (f.data.len() as f64 * (1.0 + alloc.fec_ratio)) as u64;
        let padding = rate
            .lock()
            .unwrap()
            .recovery_fill_bytes(wire, covers, interval, now) as usize;
        let Ok(packets) =
            packetizer.packetize_padded(&f.data, info, alloc.fec_ratio, padding, max_dg)
        else {
            depth.fetch_sub(1, Relaxed);
            continue;
        };
        let bytes: u64 = packets.iter().map(|x| x.len() as u64).sum();
        let window = rate
            .lock()
            .unwrap()
            .pacing_window(bytes, interval, covers, now);
        let start = Instant::now();
        let n = packets.len();
        for (i, pkt) in packets.into_iter().enumerate() {
            let due = start + window.mul_f64(i as f64 / n as f64);
            if due > Instant::now() + Duration::from_micros(500) {
                wait_until(due);
            }
            // send_datagram silently drops the oldest queued datagram when full.
            let mut blocked = false;
            while conn.datagram_send_buffer_space() < pkt.len() {
                if stop.load(Relaxed) || conn.close_reason().is_some() {
                    return;
                }
                if !std::mem::replace(&mut blocked, true) {
                    // QUIC is not taking what the pacer hands it: behind, not idle.
                    rate.lock().unwrap().set_send_backlog(true);
                }
                thread::sleep(Duration::from_micros(100));
            }
            let len = pkt.len() as u64;
            if tx.send(pkt, Some(f.frame_id), &rate).is_err() {
                return;
            }
            stats.packets.fetch_add(1, Relaxed);
            stats.bytes_sent.fetch_add(len, Relaxed);
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

#[cfg(test)]
mod tests {
    use nexa_media::{CpuFrame, EncodedMeta, EncoderCaps, FrameId, Recovery};

    use super::*;

    /// Delivers small frames; the first look reports the display lost if asked.
    struct Screen {
        lost: bool,
    }

    impl Capturer for Screen {
        fn next(&mut self, _: Duration) -> nexa_media::Result<CaptureEvent> {
            if std::mem::take(&mut self.lost) {
                return Ok(CaptureEvent::AccessLost);
            }
            thread::sleep(Duration::from_millis(5));
            Ok(CaptureEvent::Frame(CapturedFrame {
                image: NativeImage::Cpu(Arc::new(CpuFrame {
                    width: 2,
                    height: 2,
                    y: vec![0; 4],
                    uv: vec![0; 2],
                })),
                captured_at: Instant::now(),
                dirty: Vec::new(),
            }))
        }
    }

    struct Plain;

    impl Encoder for Plain {
        fn caps(&self) -> EncoderCaps {
            EncoderCaps::default()
        }
        fn encode(
            &mut self,
            _: &CapturedFrame,
            frame_id: FrameId,
            opts: EncodeOpts,
            out: &mut Vec<u8>,
        ) -> nexa_media::Result<EncodedMeta> {
            out.push(0);
            Ok(EncodedMeta {
                frame_id,
                key: opts.force_key,
                recovery_point: false,
            })
        }
        fn set_bitrate(&mut self, _: u32) -> nexa_media::Result<()> {
            Ok(())
        }
        fn invalidate_from(&mut self, _: FrameId) -> nexa_media::Result<Recovery> {
            Ok(Recovery::Idr)
        }
        fn on_frame_acked(&mut self, _: FrameId) {}
        fn force_idr(&mut self) {}
    }

    struct Collect(Arc<Mutex<Vec<EncodedFrame>>>);

    impl FrameSink for Collect {
        fn has_room(&self) -> bool {
            true
        }
        fn push(&mut self, frame: EncodedFrame) {
            self.0.lock().unwrap().push(frame);
        }
    }

    /// Fails the first `failing` tries with `error`, then gives a working screen.
    struct Rebuilds {
        failing: u32,
        error: MediaError,
        tries: Arc<AtomicU32>,
    }

    impl StreamRebuilder for Rebuilds {
        fn rebuild(&mut self, _: RebuildReason) -> nexa_media::Result<Rebuilt> {
            if self.tries.fetch_add(1, Relaxed) < self.failing {
                return Err(self.error.clone());
            }
            Ok(Rebuilt {
                capturer: Box::new(Screen { lost: false }),
                encoder: Box::new(Plain),
                mode: Mode::Game,
                fps: 60,
            })
        }
    }

    struct Running {
        frames: Arc<Mutex<Vec<EncodedFrame>>>,
        tries: Arc<AtomicU32>,
        stop: Arc<AtomicBool>,
        thread: JoinHandle<()>,
    }

    /// The capture thread on a display that is lost at once, with `rebuilds` failing.
    fn lose_display(failing: u32, error: MediaError) -> Running {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let tries = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let pipeline = Pipeline {
            capturer: Box::new(Screen { lost: true }),
            encoder: Box::new(Plain),
            rebuilder: Some(Box::new(Rebuilds {
                failing,
                error,
                tries: tries.clone(),
            })),
            rate: Arc::new(Mutex::new(RateController::new(
                RateConfig {
                    initial_bps: 10_000_000,
                    min_bps: 300_000,
                    max_bps: 100_000_000,
                },
                Mode::Game,
                Instant::now(),
            ))),
            fps: Arc::new(AtomicU32::new(60)),
        };
        // Kept open so the thread never sees its commands end.
        let (cmd_tx, cmd_rx) = mpsc::channel();
        std::mem::forget(cmd_tx);
        let (sink, s) = (Box::new(Collect(frames.clone())), stop.clone());
        let thread = thread::spawn(move || {
            run_encoder(
                pipeline,
                sink,
                cmd_rx,
                Instant::now(),
                s,
                Arc::new(HostStats::default()),
            )
        });
        Running {
            frames,
            tries,
            stop,
            thread,
        }
    }

    fn wait_for(what: impl Fn() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            if what() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Five frames, then a still screen.
    struct FiveThenStill(u32);

    impl Capturer for FiveThenStill {
        fn next(&mut self, timeout: Duration) -> nexa_media::Result<CaptureEvent> {
            if self.0 >= 5 {
                thread::sleep(timeout.min(Duration::from_millis(10)));
                return Ok(CaptureEvent::Idle);
            }
            self.0 += 1;
            thread::sleep(Duration::from_millis(5));
            Ok(CaptureEvent::Frame(CapturedFrame {
                image: NativeImage::Cpu(Arc::new(CpuFrame {
                    width: 2,
                    height: 2,
                    y: vec![0; 4],
                    uv: vec![0; 2],
                })),
                captured_at: Instant::now(),
                dirty: Vec::new(),
            }))
        }
    }

    /// As an asynchronous hardware encoder: each frame's output is ready some time after
    /// it goes in, the second one late; an encode returns the oldest ready output, or
    /// says the frame was held back.
    struct Late {
        n: u8,
        queue: std::collections::VecDeque<(u8, Instant)>,
    }

    impl Late {
        fn ready(&mut self, out: &mut Vec<u8>) -> bool {
            match self.queue.front() {
                Some(&(n, at)) if Instant::now() >= at => {
                    self.queue.pop_front();
                    out.push(n);
                    true
                }
                _ => false,
            }
        }
    }

    impl Encoder for Late {
        fn caps(&self) -> EncoderCaps {
            EncoderCaps::default()
        }
        fn encode(
            &mut self,
            _: &CapturedFrame,
            frame_id: FrameId,
            _: EncodeOpts,
            out: &mut Vec<u8>,
        ) -> nexa_media::Result<EncodedMeta> {
            self.n += 1;
            let late = if self.n == 2 { 80 } else { 0 };
            self.queue
                .push_back((self.n, Instant::now() + Duration::from_millis(late)));
            if !self.ready(out) {
                return Err(MediaError::FrameDropped);
            }
            Ok(EncodedMeta {
                frame_id,
                key: false,
                recovery_point: false,
            })
        }
        fn set_bitrate(&mut self, _: u32) -> nexa_media::Result<()> {
            Ok(())
        }
        fn invalidate_from(&mut self, _: FrameId) -> nexa_media::Result<Recovery> {
            Ok(Recovery::Idr)
        }
        fn on_frame_acked(&mut self, _: FrameId) {}
        fn force_idr(&mut self) {}
        fn take_ready(
            &mut self,
            frame_id: FrameId,
            out: &mut Vec<u8>,
        ) -> nexa_media::Result<Option<EncodedMeta>> {
            Ok(self.ready(out).then_some(EncodedMeta {
                frame_id,
                key: false,
                recovery_point: false,
            }))
        }
    }

    #[test]
    fn frames_an_encoder_finishes_late_all_go_and_none_waits_for_the_next_change() {
        // One late frame used to leave every later one a frame behind for the rest of the
        // session, and on a still screen the last change never went at all.
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pipeline = Pipeline {
            capturer: Box::new(FiveThenStill(0)),
            encoder: Box::new(Late {
                n: 0,
                queue: Default::default(),
            }),
            rebuilder: None,
            rate: Arc::new(Mutex::new(RateController::new(
                RateConfig {
                    initial_bps: 10_000_000,
                    min_bps: 300_000,
                    max_bps: 100_000_000,
                },
                Mode::Game,
                Instant::now(),
            ))),
            fps: Arc::new(AtomicU32::new(60)),
        };
        let (cmd_tx, cmd_rx) = mpsc::channel();
        std::mem::forget(cmd_tx);
        let stats = Arc::new(HostStats::default());
        let (sink, s, st) = (
            Box::new(Collect(frames.clone())),
            stop.clone(),
            stats.clone(),
        );
        let thread =
            thread::spawn(move || run_encoder(pipeline, sink, cmd_rx, Instant::now(), s, st));
        let sent = || -> Vec<u8> { frames.lock().unwrap().iter().map(|f| f.data[0]).collect() };
        assert!(wait_for(|| sent().len() >= 5), "sent only {:?}", sent());
        // Every frame, in order, each once, with ids that follow on.
        assert_eq!(sent(), [1, 2, 3, 4, 5]);
        let ids: Vec<u32> = frames.lock().unwrap().iter().map(|f| f.frame_id).collect();
        assert_eq!(ids, [0, 1, 2, 3, 4]);
        assert!(stats.late_frames.load(Relaxed) >= 1);
        stop.store(true, Relaxed);
        thread.join().unwrap();
    }

    #[test]
    fn refreshes_of_a_still_screen_are_marked_as_repeats() {
        // A viewer's latency figures leave them out: they carry their original's
        // capture time, and counted as 100-300 ms of delay that was not there.
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pipeline = Pipeline {
            capturer: Box::new(FiveThenStill(0)),
            encoder: Box::new(Plain),
            rebuilder: None,
            rate: Arc::new(Mutex::new(RateController::new(
                RateConfig {
                    initial_bps: 10_000_000,
                    min_bps: 300_000,
                    max_bps: 100_000_000,
                },
                Mode::Game,
                Instant::now(),
            ))),
            fps: Arc::new(AtomicU32::new(60)),
        };
        let (cmd_tx, cmd_rx) = mpsc::channel();
        std::mem::forget(cmd_tx);
        let (sink, s) = (Box::new(Collect(frames.clone())), stop.clone());
        let thread = thread::spawn(move || {
            run_encoder(
                pipeline,
                sink,
                cmd_rx,
                Instant::now(),
                s,
                Arc::new(HostStats::default()),
            )
        });
        // Five pictures, then the refresh 100 ms into the still screen.
        assert!(wait_for(|| frames.lock().unwrap().len() >= 6));
        stop.store(true, Relaxed);
        thread.join().unwrap();
        let repeats: Vec<bool> = frames
            .lock()
            .unwrap()
            .iter()
            .map(|f| f.flags & flags::REPEAT != 0)
            .collect();
        assert_eq!(repeats[..5], [false; 5]);
        assert!(repeats[5..].iter().all(|r| *r), "{repeats:?}");
    }

    #[test]
    fn a_display_unavailable_for_now_is_tried_again_until_it_is_back() {
        // A UAC prompt up for two tries: the stream waits, then carries on.
        let started = Instant::now();
        let run = lose_display(2, MediaError::Unavailable("a secure desktop".into()));
        assert!(wait_for(|| !run.frames.lock().unwrap().is_empty()));
        assert_eq!(run.tries.load(Relaxed), 3);
        assert!(started.elapsed() >= UNAVAILABLE_RETRY * 2);
        assert!(!run.thread.is_finished());
        // The first frame after the wait starts a new picture.
        assert_ne!(run.frames.lock().unwrap()[0].flags & flags::IDR, 0);
        run.stop.store(true, Relaxed);
        run.thread.join().unwrap();
    }

    #[test]
    fn any_other_rebuild_failure_still_ends_the_stream() {
        let run = lose_display(1, MediaError::Other("the encoder went away".into()));
        assert!(wait_for(|| run.thread.is_finished()));
        assert_eq!(run.tries.load(Relaxed), 1);
        assert!(run.frames.lock().unwrap().is_empty());
    }
}
