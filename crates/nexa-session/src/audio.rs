//! System audio (DESIGN §6.1): 48 kHz stereo Opus in 10 ms packets, one per
//! AudioPacket datagram (`nexa_wire::datagram::AudioHeader`).
//!
//! The host side cuts whatever the capture delivers into 10 ms frames ([`Framer`]);
//! the viewer side is a small adaptive jitter buffer ([`Playout`]) that the audio
//! output pulls from. Both are platform-free; capture, codec and output arrive
//! through the traits below. Nothing here plays or records anything.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use nexa_wire::datagram::{AudioHeader, audio_flags};

pub use nexa_media::{AudioCapture, AudioChunk, AudioDecoder, AudioEncoder};

pub const SAMPLE_RATE: u32 = nexa_media::AUDIO_SAMPLE_RATE;
pub const CHANNELS: usize = nexa_media::AUDIO_CHANNELS;
/// Samples per channel in one 10 ms frame.
pub const FRAME_SAMPLES: usize = nexa_media::AUDIO_FRAME_SAMPLES;
/// Interleaved samples in one frame.
pub const FRAME_LEN: usize = nexa_media::AUDIO_FRAME_LEN;
pub const FRAME_DURATION: Duration = Duration::from_millis(10);
/// Opus bitrate for system audio. With headers this stays inside
/// `nexa_net::rate::AUDIO_BPS`, which the rate controller reserves.
pub const OPUS_BITRATE: u32 = 64_000;

/// Cuts captured audio into 10 ms frames.
#[derive(Default)]
pub struct Framer {
    pending: Vec<f32>,
    /// Capture time of `pending[0]`.
    start: Option<Instant>,
}

impl Framer {
    /// Adds a chunk and calls `frame` for every complete frame, with the capture time
    /// of its first sample.
    pub fn push(&mut self, chunk: &AudioChunk, mut frame: impl FnMut(&[f32], Instant)) {
        let samples = chunk.samples.len() - chunk.samples.len() % CHANNELS;
        if samples == 0 {
            return;
        }
        if self.pending.is_empty() {
            let span =
                Duration::from_secs_f64((samples / CHANNELS) as f64 / f64::from(SAMPLE_RATE));
            self.start = Some(
                chunk
                    .captured_at
                    .checked_sub(span)
                    .unwrap_or(chunk.captured_at),
            );
        }
        self.pending.extend_from_slice(&chunk.samples[..samples]);
        let mut used = 0;
        while self.pending.len() - used >= FRAME_LEN {
            let at = self.start.unwrap_or(chunk.captured_at);
            frame(&self.pending[used..used + FRAME_LEN], at);
            used += FRAME_LEN;
            self.start = Some(at + FRAME_DURATION);
        }
        self.pending.drain(..used);
    }
}

/// Numbers outgoing packets. `tx_seq` is filled in when the datagram is sent.
pub struct AudioPacketizer {
    seq: u32,
    restart: bool,
    base: Instant,
}

impl AudioPacketizer {
    /// `base` is the clock the video's `capture_ts_us` counts from.
    pub fn new(base: Instant) -> Self {
        Self {
            seq: 0,
            restart: true,
            base,
        }
    }

    /// The next datagram for `opus`, captured at `at`.
    pub fn packet(&mut self, opus: &[u8], at: Instant, out: &mut Vec<u8>) {
        let flags = if std::mem::take(&mut self.restart) {
            audio_flags::RESTART
        } else {
            0
        };
        AudioHeader {
            stream: 0,
            flags,
            frame_10ms: 1,
            tx_seq: 0,
            seq: self.seq,
            capture_ts_us: at.saturating_duration_since(self.base).as_micros() as u32,
        }
        .encode(opus, out);
        self.seq = self.seq.wrapping_add(1);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PlayoutConfig {
    /// Buffering never goes below this many frames. Two absorb an output that pulls
    /// slightly more than one frame per callback.
    pub min_frames: usize,
    pub max_frames: usize,
}

impl Default for PlayoutConfig {
    fn default() -> Self {
        Self {
            min_frames: 2,
            max_frames: 10,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayoutStats {
    pub received: u64,
    pub played: u64,
    /// Frames made up for a lost or missing packet.
    pub concealed: u64,
    /// Packets that arrived after their slot had been played.
    pub late: u64,
    /// Frames skipped because the buffer had grown well past its target.
    pub skipped: u64,
    pub underruns: u64,
    pub restarts: u64,
    /// Sum and count of arrival-to-playout delay of played packets.
    pub delay_us_sum: u64,
    pub delay_count: u64,
    pub target_frames: usize,
    pub jitter_us: f64,
}

impl PlayoutStats {
    pub fn mean_delay(&self) -> Duration {
        Duration::from_micros(self.delay_us_sum.checked_div(self.delay_count).unwrap_or(0))
    }
}

/// How many concealed frames in a row before playback stops and buffers again.
const MAX_CONCEAL_RUN: u32 = 5;
/// Fade applied per concealed frame when the codec cannot conceal.
const FADE: f32 = 0.5;
const SHRINK_AFTER_PACKETS: u32 = 50;

/// The viewer's jitter buffer and decoder. `push` from the network thread, `render`
/// from the output callback.
pub struct Playout {
    cfg: PlayoutConfig,
    decoder: Box<dyn AudioDecoder>,
    queue: BTreeMap<u64, (Vec<u8>, Instant)>,
    /// Highest unwrapped sequence seen.
    highest: Option<u64>,
    /// Next sequence to play while playing.
    next: Option<u64>,
    playing: bool,
    frame: Vec<f32>,
    pos: usize,
    conceal_run: u32,
    /// For the RFC 3550 jitter estimate.
    last_transit: Option<f64>,
    epoch: Option<Instant>,
    jitter_us: f64,
    target: usize,
    /// Packets in a row that wanted less buffering than `target`.
    calm: u32,
    stats: PlayoutStats,
}

impl Playout {
    pub fn new(decoder: Box<dyn AudioDecoder>, cfg: PlayoutConfig) -> Self {
        Self {
            cfg,
            decoder,
            queue: BTreeMap::new(),
            highest: None,
            next: None,
            playing: false,
            frame: vec![0.0; FRAME_LEN],
            pos: FRAME_LEN,
            conceal_run: 0,
            last_transit: None,
            epoch: None,
            jitter_us: 0.0,
            target: cfg.min_frames,
            calm: 0,
            stats: PlayoutStats::default(),
        }
    }

    pub fn stats(&self) -> PlayoutStats {
        PlayoutStats {
            target_frames: self.target,
            jitter_us: self.jitter_us,
            ..self.stats
        }
    }

    /// Frames waiting, not counting the one being played.
    pub fn buffered(&self) -> usize {
        self.queue.len()
    }

    fn unwrap_seq(&self, seq: u32) -> u64 {
        let Some(h) = self.highest else {
            return u64::from(seq) + (1 << 32);
        };
        let base = h & !0xFFFF_FFFF;
        let c = base | u64::from(seq);
        if c + (1 << 31) < h {
            c + (1 << 32)
        } else if c > h + (1 << 31) && c >= 1 << 32 {
            c - (1 << 32)
        } else {
            c
        }
    }

    /// Drops everything: a muted, restarted or reconfigured stream starts afresh.
    pub fn reset(&mut self) {
        self.queue.clear();
        self.highest = None;
        self.next = None;
        self.playing = false;
        self.pos = FRAME_LEN;
        self.conceal_run = 0;
        self.last_transit = None;
        self.epoch = None;
        self.decoder.reset();
    }

    pub fn push(&mut self, header: &AudioHeader, payload: &[u8], arrival: Instant) {
        if header.flags & audio_flags::RESTART != 0 && self.highest.is_some() {
            self.stats.restarts += 1;
            self.reset();
        }
        let seq = self.unwrap_seq(header.seq);
        self.stats.received += 1;
        if self.next.is_some_and(|n| seq < n) {
            self.stats.late += 1;
            // It came too late to be played: buffer more from now on.
            self.target = (self.target + 1).min(self.cfg.max_frames);
            return;
        }
        if self.queue.contains_key(&seq) {
            return;
        }
        self.highest = Some(self.highest.map_or(seq, |h| h.max(seq)));

        // RFC 3550 §6.4.1, with the sequence number standing in for the media clock.
        let epoch = *self.epoch.get_or_insert(arrival);
        let arrival_us = arrival.saturating_duration_since(epoch).as_secs_f64() * 1e6;
        let transit = arrival_us - (seq as f64) * FRAME_DURATION.as_secs_f64() * 1e6;
        if let Some(last) = self.last_transit {
            self.jitter_us += ((transit - last).abs() - self.jitter_us) / 16.0;
        }
        self.last_transit = Some(transit);
        let wanted = self.cfg.min_frames
            + (2.5 * self.jitter_us / (FRAME_DURATION.as_secs_f64() * 1e6)).ceil() as usize;
        // Grow at once; shrink one frame per half second of calm (the skip in
        // `next_frame` then drains the extra frame).
        if wanted >= self.target {
            self.target = wanted.min(self.cfg.max_frames);
            self.calm = 0;
        } else {
            self.calm += 1;
            if self.calm >= SHRINK_AFTER_PACKETS {
                self.target -= 1;
                self.calm = 0;
            }
        }
        self.queue.insert(seq, (payload.to_vec(), arrival));
    }

    fn conceal(&mut self) {
        self.stats.concealed += 1;
        if self.decoder.decode(None, &mut self.frame).is_err() {
            for s in &mut self.frame {
                *s *= FADE;
            }
        }
    }

    /// Fills `self.frame` with the next 10 ms. `at` is when its first sample plays.
    fn next_frame(&mut self, at: Instant) {
        if !self.playing {
            if self.queue.len() < self.target {
                self.frame.fill(0.0);
                return;
            }
            self.playing = true;
            self.next = self.queue.keys().next().copied();
            self.conceal_run = 0;
        }
        // Past the target: decode one frame without playing it to catch up.
        if self.queue.len() > self.target + 1
            && let Some((&seq, _)) = self.queue.iter().next()
            && self.next == Some(seq)
            && let Some((data, _)) = self.queue.remove(&seq)
        {
            let _ = self.decoder.decode(Some(&data), &mut self.frame);
            self.stats.skipped += 1;
            self.next = Some(seq + 1);
        }
        let next = self.next.unwrap_or(0);
        match self.queue.remove(&next) {
            Some((data, arrival)) => {
                if self.decoder.decode(Some(&data), &mut self.frame).is_err() {
                    self.conceal();
                } else {
                    self.stats.played += 1;
                    self.stats.delay_us_sum +=
                        at.saturating_duration_since(arrival).as_micros() as u64;
                    self.stats.delay_count += 1;
                }
                self.conceal_run = 0;
            }
            None if !self.queue.is_empty() => {
                // A gap with later packets waiting: that packet is lost.
                self.conceal();
                self.conceal_run = 0;
            }
            None => {
                self.conceal();
                self.conceal_run += 1;
                if self.conceal_run == 1 {
                    self.stats.underruns += 1;
                    self.target = (self.target + 1).min(self.cfg.max_frames);
                }
                if self.conceal_run >= MAX_CONCEAL_RUN {
                    // The stream stopped (muted at the host, or the network): rebuffer.
                    self.playing = false;
                    self.next = None;
                    self.frame.fill(0.0);
                    return;
                }
            }
        }
        self.next = Some(next + 1);
    }

    /// Fills interleaved stereo `out`. `now` is when `out[0]` will be heard.
    pub fn render(&mut self, out: &mut [f32], now: Instant) {
        let mut written = 0;
        while written < out.len() {
            if self.pos == FRAME_LEN {
                let at = now
                    + Duration::from_secs_f64((written / CHANNELS) as f64 / f64::from(SAMPLE_RATE));
                self.next_frame(at);
                self.pos = 0;
            }
            let n = (FRAME_LEN - self.pos).min(out.len() - written);
            out[written..written + n].copy_from_slice(&self.frame[self.pos..self.pos + n]);
            self.pos += n;
            written += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lossless stand-in codec: the payload is the first sample of each channel.
    struct Probe;
    impl AudioDecoder for Probe {
        fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> nexa_media::Result<()> {
            let Some(p) = packet else {
                return Err(nexa_media::MediaError::Unsupported("no concealment"));
            };
            let v = f32::from(p[0]);
            out.fill(v);
            Ok(())
        }
        fn reset(&mut self) {}
    }

    fn header(seq: u32, flags: u8) -> AudioHeader {
        AudioHeader {
            stream: 0,
            flags,
            frame_10ms: 1,
            tx_seq: 0,
            seq,
            capture_ts_us: 0,
        }
    }

    #[test]
    fn framer_cuts_any_chunking_into_10ms_frames_with_capture_times() {
        let mut f = Framer::default();
        let t = Instant::now() + Duration::from_secs(1);
        let mut frames = Vec::new();
        // 7 ms, then 13 ms: one frame after the second chunk, 10 ms kept back.
        f.push(
            &AudioChunk {
                samples: vec![1.0; 336 * 2],
                captured_at: t,
            },
            |s, at| frames.push((s.len(), at)),
        );
        assert!(frames.is_empty());
        f.push(
            &AudioChunk {
                samples: vec![2.0; 624 * 2],
                captured_at: t + Duration::from_millis(13),
            },
            |s, at| frames.push((s.len(), at)),
        );
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|(n, _)| *n == FRAME_LEN));
        let first = t - Duration::from_millis(7);
        assert!(frames[0].1.duration_since(first) < Duration::from_micros(50));
        assert_eq!(frames[1].1 - frames[0].1, FRAME_DURATION);
    }

    #[test]
    fn packetizer_flags_the_first_packet_and_counts() {
        let base = Instant::now();
        let mut p = AudioPacketizer::new(base);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        p.packet(&[7], base + Duration::from_millis(5), &mut a);
        p.packet(&[8], base, &mut b);
        let (ha, _) = AudioHeader::parse(&a).unwrap();
        let (hb, pb) = AudioHeader::parse(&b).unwrap();
        assert_eq!(
            (ha.flags, ha.seq, ha.capture_ts_us),
            (audio_flags::RESTART, 0, 5000)
        );
        assert_eq!((hb.flags, hb.seq, pb), (0, 1, &[8u8][..]));
    }

    fn playout() -> Playout {
        Playout::new(Box::new(Probe), PlayoutConfig::default())
    }

    fn render_frames(p: &mut Playout, n: usize, now: Instant) -> Vec<f32> {
        let mut out = vec![0.0; FRAME_LEN * n];
        p.render(&mut out, now);
        out.chunks(FRAME_LEN).map(|c| c[0]).collect()
    }

    #[test]
    fn plays_in_order_after_buffering_and_conceals_a_gap() {
        let mut p = playout();
        let t = Instant::now();
        for (i, seq) in [0u32, 2, 1].into_iter().enumerate() {
            p.push(
                &header(seq, 0),
                &[seq as u8 + 1],
                t + Duration::from_millis(i as u64),
            );
        }
        p.push(&header(4, 0), &[5], t);
        // 1, 2, 3 in order, 4 missing (concealed by fading 3), then 5.
        let got = render_frames(&mut p, 5, t + Duration::from_millis(20));
        assert_eq!(got, vec![1.0, 2.0, 3.0, 1.5, 5.0]);
        let s = p.stats();
        assert_eq!((s.played, s.concealed, s.late), (4, 1, 0));
        // 3 arriving now is too late to play.
        p.push(&header(3, 0), &[4], t);
        assert_eq!(p.stats().late, 1);
    }

    /// Arrival on time: packet `seq` at `t + seq × 10 ms`.
    fn on_time(t: Instant, seq: u32) -> Instant {
        t + FRAME_DURATION * seq
    }

    #[test]
    fn nothing_plays_until_the_target_is_buffered() {
        let mut p = playout();
        let t = Instant::now();
        p.push(&header(0, 0), &[9], on_time(t, 0));
        assert_eq!(render_frames(&mut p, 1, t), vec![0.0]);
        p.push(&header(1, 0), &[9], on_time(t, 1));
        assert_eq!(p.stats().target_frames, 2, "no jitter, so the minimum");
        assert_eq!(render_frames(&mut p, 1, t), vec![9.0]);
    }

    #[test]
    fn a_long_silence_rebuffers_and_a_restart_resets() {
        let mut p = playout();
        let t = Instant::now();
        for s in 0..2 {
            p.push(&header(s, 0), &[1], on_time(t, s));
        }
        let _ = render_frames(&mut p, 2 + MAX_CONCEAL_RUN as usize + 2, t);
        let s = p.stats();
        assert_eq!(s.underruns, 1);
        assert!(!p.playing);
        // The host restarted capture: numbering starts over and plays again.
        let t2 = t + Duration::from_secs(1);
        p.push(&header(0, audio_flags::RESTART), &[3], t2);
        for s in 1..4 {
            p.push(&header(s, 0), &[3], on_time(t2, s));
        }
        assert_eq!(p.stats().restarts, 1);
        assert_eq!(render_frames(&mut p, 1, t2), vec![3.0]);
    }

    #[test]
    fn a_full_buffer_skips_ahead_to_keep_latency_down() {
        let mut p = playout();
        let t = Instant::now();
        for s in 0..12 {
            p.push(&header(s, 0), &[s as u8], on_time(t, s));
        }
        let got = render_frames(&mut p, 4, on_time(t, 12));
        assert_eq!(p.stats().skipped, 4);
        assert!(
            got.windows(2).all(|w| w[1] > w[0]),
            "still in order: {got:?}"
        );
        assert!(p.buffered() <= p.stats().target_frames + 2);
    }

    #[test]
    fn jitter_raises_the_target_and_calm_lowers_it_again() {
        let mut p = playout();
        let t = Instant::now();
        let mut seq = 0u32;
        let mut x = 7u32;
        // ±15 ms of arrival jitter.
        for _ in 0..200 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let wobble = Duration::from_micros(u64::from(x >> 16) % 15_000);
            p.push(&header(seq, 0), &[1], on_time(t, seq) + wobble);
            let _ = render_frames(&mut p, 1, on_time(t, seq));
            seq += 1;
        }
        let jittery = p.stats().target_frames;
        assert!(jittery >= 4, "target {jittery}");
        for _ in 0..600 {
            p.push(&header(seq, 0), &[1], on_time(t, seq));
            let _ = render_frames(&mut p, 1, on_time(t, seq));
            seq += 1;
        }
        let calm = p.stats().target_frames;
        assert!(calm < jittery, "target {calm} after calm, was {jittery}");
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut p = Playout::new(
            Box::new(Probe),
            PlayoutConfig {
                min_frames: 4,
                max_frames: 10,
            },
        );
        let t = Instant::now();
        for (i, s) in [u32::MAX - 1, u32::MAX, 0, 1].into_iter().enumerate() {
            p.push(&header(s, 0), &[i as u8 + 1], on_time(t, i as u32));
        }
        assert_eq!(render_frames(&mut p, 4, t), vec![1.0, 2.0, 3.0, 4.0]);
    }
}
