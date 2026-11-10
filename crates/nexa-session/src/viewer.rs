//! Viewer media path (§1.2): receive and reassemble, decode, present, each on its own thread.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use bytes::Bytes;
use nexa_media::{DecodedFrame, Decoder, FrameMeta, Presenter};
use nexa_net::{
    recovery::{Decision, ReceiverRecovery},
    shard::{Assembler, AssemblerEvent, CompleteFrame},
};
use nexa_wire::datagram::{self, AudioHeader, Clock, ReportKind, VideoReport, VideoShardHeader};
use quinn::Connection;

use crate::{feedback::FeedbackBuilder, latency::LatencyStats, mailbox::Mailbox};

/// §1.2: at most four whole frames wait for the decoder.
const DECODE_QUEUE: usize = 4;
/// §3.3: also keeps inbound traffic flowing for failover detection on a static screen (§4.5).
const CLOCK_PING_INTERVAL: Duration = Duration::from_millis(250);
const TICK: Duration = Duration::from_millis(2);

#[derive(Debug, Clone)]
pub struct ViewerParams {
    pub stream: u8,
    pub frame_deadline: Duration,
    pub reorder_grace: Duration,
}

/// Receives AudioPacket datagrams on the `media-rx` thread. Must not block.
pub type AudioSink = Arc<dyn Fn(&AudioHeader, &[u8], Instant) + Send + Sync>;

/// Optional behaviour for [`ViewerMedia::spawn_full`].
#[derive(Default)]
pub struct ViewerOptions {
    /// Runs first on every media thread (§1.2).
    pub thread_init: Option<fn()>,
    /// Where audio goes. Without one, audio packets still count towards transport
    /// feedback (they share `tx_seq` with video) and are then dropped.
    pub audio: Option<AudioSink>,
}

#[derive(Default, Debug)]
pub struct ViewerStats {
    pub complete: AtomicU64,
    pub lost: AtomicU64,
    pub dropped_waiting: AtomicU64,
    pub fec_recovered: AtomicU64,
    pub decoded: AtomicU64,
    pub decode_errors: AtomicU64,
    pub presented: AtomicU64,
    pub replaced_before_present: AtomicU64,
    pub decoder_overruns: AtomicU64,
    pub malformed: AtomicU64,
    pub rtt_us: AtomicU64,
    pub audio_packets: AtomicU64,
}

/// The most recently presented frame, kept so a newly attached or resized view is
/// not blank on a static screen.
type LastFrame = Arc<Mutex<Option<DecodedFrame>>>;

pub struct ViewerMedia {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    mailbox: Arc<Mailbox<DecodedFrame>>,
    last: LastFrame,
    pub stats: Arc<ViewerStats>,
    /// §6.1 segments for the HUD.
    pub latency: Arc<LatencyStats>,
}

impl ViewerMedia {
    pub fn spawn(
        conn: Connection,
        decoder: Box<dyn Decoder>,
        presenter: Box<dyn Presenter>,
        params: ViewerParams,
    ) -> Self {
        Self::spawn_with(conn, decoder, presenter, params, None)
    }

    /// [`Self::spawn`], running `thread_init` first on every media thread (§1.2).
    pub fn spawn_with(
        conn: Connection,
        decoder: Box<dyn Decoder>,
        presenter: Box<dyn Presenter>,
        params: ViewerParams,
        thread_init: Option<fn()>,
    ) -> Self {
        Self::spawn_full(
            conn,
            decoder,
            presenter,
            params,
            ViewerOptions {
                thread_init,
                audio: None,
            },
        )
    }

    /// [`Self::spawn`] with [`ViewerOptions`].
    pub fn spawn_full(
        conn: Connection,
        decoder: Box<dyn Decoder>,
        presenter: Box<dyn Presenter>,
        params: ViewerParams,
        options: ViewerOptions,
    ) -> Self {
        let ViewerOptions { thread_init, audio } = options;
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(ViewerStats::default());
        let latency = Arc::new(LatencyStats::new(Instant::now()));
        let mailbox = Arc::new(Mailbox::<DecodedFrame>::default());
        let last = LastFrame::default();
        let (decode_tx, decode_rx) = mpsc::sync_channel(DECODE_QUEUE);
        let (failed_tx, failed_rx) = mpsc::channel();

        let mut threads = Vec::new();
        {
            let (s, st, m, l) = (
                stop.clone(),
                stats.clone(),
                mailbox.clone(),
                latency.clone(),
            );
            threads.push(spawn("decode", thread_init, move || {
                run_decode(decoder, decode_rx, failed_tx, m, s, st, l)
            }));
        }
        {
            let (s, st, m, l) = (stop.clone(), stats.clone(), mailbox.clone(), last.clone());
            let lat = latency.clone();
            threads.push(spawn("render", thread_init, move || {
                run_render(presenter, m, l, s, st, lat)
            }));
        }
        {
            let (s, st, l) = (stop.clone(), stats.clone(), latency.clone());
            threads.push(spawn("media-rx", thread_init, move || {
                run_rx(conn, params, decode_tx, failed_rx, audio, s, st, l)
            }));
        }
        Self {
            stop,
            threads,
            mailbox,
            last,
            stats,
            latency,
        }
    }

    /// Presents the last frame again, for a view that just appeared or changed size.
    pub fn repaint(&self) {
        let again = self.last.lock().unwrap().as_ref().map(copy_frame);
        if let Some(frame) = again
            && let Some(newer) = self.mailbox.put(frame)
        {
            self.mailbox.put(newer); // a real frame beat us to it
        }
    }

    pub fn stop(self) {
        self.stop.store(true, Relaxed);
        for t in self.threads {
            let _ = t.join();
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

fn copy_frame(f: &DecodedFrame) -> DecodedFrame {
    DecodedFrame {
        image: f.image.clone(),
        meta: f.meta,
    }
}

fn run_decode(
    mut decoder: Box<dyn Decoder>,
    frames: Receiver<CompleteFrame>,
    failed: mpsc::Sender<u32>,
    mailbox: Arc<Mailbox<DecodedFrame>>,
    stop: Arc<AtomicBool>,
    stats: Arc<ViewerStats>,
    latency: Arc<LatencyStats>,
) {
    while !stop.load(Relaxed) {
        let f = match frames.recv_timeout(Duration::from_millis(50)) {
            Ok(f) => f,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let started = Instant::now();
        match decoder.decode(
            &f.data,
            FrameMeta {
                frame_id: f.frame_id,
                capture_ts_us: f.capture_ts_us,
            },
        ) {
            Ok(d) => {
                latency.on_decoded(f.frame_id, started, Instant::now());
                stats.decoded.fetch_add(1, Relaxed);
                if mailbox.put(d).is_some() {
                    stats.replaced_before_present.fetch_add(1, Relaxed);
                }
            }
            Err(_) => {
                stats.decode_errors.fetch_add(1, Relaxed);
                let _ = failed.send(f.frame_id);
            }
        }
    }
}

fn run_render(
    mut presenter: Box<dyn Presenter>,
    mailbox: Arc<Mailbox<DecodedFrame>>,
    last: LastFrame,
    stop: Arc<AtomicBool>,
    stats: Arc<ViewerStats>,
    latency: Arc<LatencyStats>,
) {
    while !stop.load(Relaxed) {
        if let Some(f) = mailbox.take(Duration::from_millis(50)) {
            let call = Instant::now();
            if presenter.present(&f).is_ok() {
                latency.on_presented(f.meta.frame_id, call, Instant::now());
                stats.presented.fetch_add(1, Relaxed);
            }
            *last.lock().unwrap() = Some(f);
        }
    }
}

fn send(conn: &Connection, b: &[u8]) {
    let _ = conn.send_datagram(Bytes::copy_from_slice(b));
}

#[allow(clippy::too_many_arguments)]
fn run_rx(
    conn: Connection,
    p: ViewerParams,
    decode: SyncSender<CompleteFrame>,
    failed: Receiver<u32>,
    audio: Option<AudioSink>,
    stop: Arc<AtomicBool>,
    stats: Arc<ViewerStats>,
    latency: Arc<LatencyStats>,
) {
    let base = latency.base();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    rt.block_on(async move {
        let mut asm = Assembler::new(p.frame_deadline, p.reorder_grace);
        let mut recovery = ReceiverRecovery::new(p.stream);
        let mut fb = FeedbackBuilder::default();
        let mut events: Vec<AssemblerEvent> = Vec::new();
        let mut next_ping = Instant::now();

        while !stop.load(Relaxed) {
            match tokio::time::timeout(TICK, conn.read_datagram()).await {
                Err(_) => {}
                Ok(Err(_)) => {
                    stop.store(true, Relaxed);
                    return;
                }
                Ok(Ok(d)) => {
                    let now = Instant::now();
                    match d.first() {
                        Some(&datagram::TYPE_VIDEO) => match VideoShardHeader::parse(&d) {
                            Ok((h, body)) if h.stream == p.stream => {
                                fb.on_packet(h.tx_seq, base.elapsed().as_micros() as u64);
                                asm.push(h, body, now, &mut events);
                            }
                            Ok(_) => {}
                            Err(_) => {
                                stats.malformed.fetch_add(1, Relaxed);
                            }
                        },
                        Some(&datagram::TYPE_AUDIO) => match AudioHeader::parse(&d) {
                            Ok((h, body)) => {
                                fb.on_packet(h.tx_seq, base.elapsed().as_micros() as u64);
                                stats.audio_packets.fetch_add(1, Relaxed);
                                if let Some(sink) = &audio {
                                    sink(&h, body, now);
                                }
                            }
                            Err(_) => {
                                stats.malformed.fetch_add(1, Relaxed);
                            }
                        },
                        Some(&datagram::TYPE_CLOCK) => {
                            if let Ok(Clock::Pong { t1, t2, t3 }) = Clock::parse(&d) {
                                let t4 = base.elapsed().as_micros() as u64;
                                latency.on_clock(t1, t2, t3, t4);
                                stats.rtt_us.store(
                                    t4.saturating_sub(t1).saturating_sub(t3.saturating_sub(t2)),
                                    Relaxed,
                                );
                            }
                        }
                        _ => {
                            stats.malformed.fetch_add(1, Relaxed);
                        }
                    }
                }
            }

            let now = Instant::now();
            asm.poll(now, &mut events);
            for frame_id in failed.try_iter() {
                // A frame the decoder rejected broke the reference chain just like a lost one.
                recovery.on_event(AssemblerEvent::Lost { frame_id }, now);
            }
            for ev in events.drain(..) {
                if matches!(ev, AssemblerEvent::Lost { .. }) {
                    stats.lost.fetch_add(1, Relaxed);
                }
                if let AssemblerEvent::Complete(f) = &ev {
                    stats.complete.fetch_add(1, Relaxed);
                    stats
                        .fec_recovered
                        .fetch_add(u64::from(f.recovered_shards), Relaxed);
                }
                match recovery.on_event(ev, now) {
                    Decision::None => {}
                    Decision::Drop { .. } => {
                        stats.dropped_waiting.fetch_add(1, Relaxed);
                    }
                    Decision::Decode(f) => {
                        let id = f.frame_id;
                        if let Some((first, complete)) = f.arrival {
                            latency.on_complete(id, f.capture_ts_us, first, complete);
                        }
                        match decode.try_send(f) {
                            Ok(()) => send(
                                &conn,
                                &VideoReport {
                                    stream: p.stream,
                                    client_epoch: recovery.epoch(),
                                    kind: ReportKind::FrameAck,
                                    last_good_frame: id,
                                    first_lost_frame: 0,
                                    latest_seen_frame: id,
                                }
                                .to_bytes(),
                            ),
                            Err(TrySendError::Full(_)) => {
                                // The chain is broken whichever frame we skip, so treat it as lost.
                                stats.decoder_overruns.fetch_add(1, Relaxed);
                                recovery.on_event(AssemblerEvent::Lost { frame_id: id }, now);
                                send(
                                    &conn,
                                    &VideoReport {
                                        stream: p.stream,
                                        client_epoch: recovery.epoch(),
                                        kind: ReportKind::DecoderSlow,
                                        last_good_frame: id,
                                        first_lost_frame: id,
                                        latest_seen_frame: id,
                                    }
                                    .to_bytes(),
                                );
                            }
                            Err(TrySendError::Disconnected(_)) => return,
                        }
                    }
                }
            }
            if let Some(r) = recovery.poll(now, conn.rtt()) {
                send(&conn, &r.to_bytes());
            }
            if let Some(f) = fb.poll(base.elapsed().as_micros() as u64) {
                let mut w = Vec::with_capacity(512);
                f.encode(&mut w);
                let _ = conn.send_datagram(w.into());
            }
            if now >= next_ping {
                next_ping = now + CLOCK_PING_INTERVAL;
                send(
                    &conn,
                    &Clock::Ping {
                        t1: base.elapsed().as_micros() as u64,
                    }
                    .to_bytes(),
                );
            }
        }
    });
}
