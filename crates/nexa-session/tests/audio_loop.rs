//! Opus through AudioToolbox and the jitter buffer, end to end and offline: synthetic
//! signals in, samples out of `Playout::render`, which here is called on a virtual
//! clock instead of by an audio device. Nothing is captured, played or stored.
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use nexa_media::macos::{OpusDecoder, OpusEncoder};
use nexa_session::audio::{
    AudioChunk, AudioDecoder, AudioEncoder, AudioPacketizer, FRAME_DURATION, FRAME_LEN,
    FRAME_SAMPLES, Framer, OPUS_BITRATE, Playout, PlayoutConfig, SAMPLE_RATE,
};
use nexa_wire::datagram::AudioHeader;

/// A different tone in each channel, so a swap would show.
fn tone(n: usize) -> (f32, f32) {
    let t = n as f32 / SAMPLE_RATE as f32;
    let l = 0.25 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
    let r = 0.25 * (2.0 * std::f32::consts::PI * 660.0 * t).sin();
    (l, r)
}

/// Independent noise per channel: its correlation has one peak, so it shows where
/// the signal came out. (A tone repeats, and so does its correlation.)
fn noise(n: usize) -> (f32, f32) {
    let h = |k: u32| {
        let mut x = k.wrapping_mul(2_654_435_761) ^ 0x9E37_79B9;
        x ^= x >> 15;
        x = x.wrapping_mul(0x2C1B_3C6D);
        x ^= x >> 12;
        x = x.wrapping_mul(0x297A_2D39);
        x ^= x >> 15;
        (x as f32 / u32::MAX as f32 - 0.5) * 0.5
    };
    (h(2 * n as u32), h(2 * n as u32 + 1))
}

fn frames_of(signal: impl Fn(usize) -> (f32, f32), count: usize) -> Vec<Vec<f32>> {
    (0..count)
        .map(|f| {
            let mut v = Vec::with_capacity(FRAME_LEN);
            for i in 0..FRAME_SAMPLES {
                let (l, r) = signal(f * FRAME_SAMPLES + i);
                v.push(l);
                v.push(r);
            }
            v
        })
        .collect()
}

fn encode_all(frames: &[Vec<f32>]) -> Vec<Vec<u8>> {
    let mut enc = OpusEncoder::new(OPUS_BITRATE).unwrap();
    frames
        .iter()
        .map(|f| {
            let mut out = Vec::new();
            enc.encode(f, &mut out).unwrap();
            out
        })
        .collect()
}

/// The lag (samples per channel, up to 100 ms) at which `got`'s left channel best
/// matches the source, and the normalized correlation there.
fn best_lag(source: impl Fn(usize) -> f32, got: &[f32], from: usize, len: usize) -> (usize, f32) {
    let src: Vec<f32> = (from..from + len).map(&source).collect();
    let src_energy: f32 = src.iter().map(|v| v * v).sum();
    let mut best = (0, f32::MIN);
    for lag in 0..4800 {
        let (mut dot, mut energy) = (0.0f32, 0.0f32);
        for (k, s) in src.iter().enumerate() {
            let g = got[(from + k + lag) * 2];
            dot += s * g;
            energy += g * g;
        }
        let rho = dot / (src_energy * energy).sqrt().max(1e-9);
        if rho > best.1 {
            best = (lag, rho);
        }
    }
    best
}

fn snr_db(
    source: impl Fn(usize) -> (f32, f32),
    got: &[f32],
    lag: usize,
    from: usize,
    len: usize,
) -> f64 {
    let (mut signal, mut noise) = (0.0f64, 0.0f64);
    for i in from..from + len {
        let (l, r) = source(i);
        for (s, g) in [(l, got[(i + lag) * 2]), (r, got[(i + lag) * 2 + 1])] {
            signal += f64::from(s) * f64::from(s);
            noise += f64::from(s - g) * f64::from(s - g);
        }
    }
    10.0 * (signal / noise.max(1e-12)).log10()
}

#[test]
fn opus_packets_are_10ms_and_small() {
    let packets = encode_all(&frames_of(tone, 100));
    let sent: Vec<&Vec<u8>> = packets.iter().filter(|p| !p.is_empty()).collect();
    assert!(
        sent.len() >= 98,
        "one packet per frame after any look-ahead: {}",
        sent.len()
    );
    let mean = sent.iter().map(|p| p.len()).sum::<usize>() / sent.len();
    // 64 kbit/s is 80 bytes per 10 ms.
    assert!((40..=160).contains(&mean), "mean packet {mean} bytes");
    assert!(sent.iter().all(|p| p.len() <= AudioHeader::MAX_PAYLOAD));
}

fn codec_pass(signal: impl Fn(usize) -> (f32, f32), count: usize) -> Vec<f32> {
    let packets = encode_all(&frames_of(signal, count));
    let mut dec = OpusDecoder::new().unwrap();
    let mut out = Vec::new();
    for p in &packets {
        let mut frame = vec![0.0; FRAME_LEN];
        if !p.is_empty() {
            dec.decode(Some(p), &mut frame).unwrap();
        }
        out.extend_from_slice(&frame);
    }
    out
}

/// Codec delay: where noise comes out; fidelity: a tone at that delay.
#[test]
fn codec_round_trip_delay_and_fidelity() {
    let (lag, rho) = best_lag(|i| noise(i).0, &codec_pass(noise, 100), 4800, 9600);
    let snr = snr_db(tone, &codec_pass(tone, 200), lag, 4800, 48_000);
    let delay_ms = lag as f64 * 1000.0 / f64::from(SAMPLE_RATE);
    println!(
        "codec: encode+decode delay {lag} samples = {delay_ms:.2} ms, noise correlation {rho:.3}, tone SNR {snr:.1} dB"
    );
    assert!(delay_ms < 30.0, "codec delay {delay_ms} ms");
    // Opus codes noise perceptually, so the waveform matches only loosely.
    assert!(rho > 0.3, "noise correlation {rho}");
    assert!(snr > 12.0, "SNR {snr} dB");

    // Concealment is not available through AudioConverter; Playout fades instead.
    assert!(
        OpusDecoder::new()
            .unwrap()
            .decode(None, &mut vec![0.0; FRAME_LEN])
            .is_err()
    );
}

/// The receive path: packets with loss and jitter into `Playout`, rendered by an
/// output pulling 512 frames at a time. Measures what the jitter buffer adds.
fn network_run(
    loss: f64,
    jitter_ms: u64,
    seconds: usize,
) -> (nexa_session::audio::PlayoutStats, f64) {
    let count = seconds * 100;
    let frames = frames_of(noise, count);
    let packets = encode_all(&frames);
    let t0 = Instant::now();
    let mut framer_packets = Vec::new();
    let mut packetizer = AudioPacketizer::new(t0);
    for (i, p) in packets.iter().enumerate() {
        if p.is_empty() {
            continue;
        }
        let mut d = Vec::new();
        packetizer.packet(p, t0 + FRAME_DURATION * i as u32, &mut d);
        framer_packets.push((i, d));
    }

    // Network: 2 ms base delay, uniform jitter, random loss. Deterministic.
    let mut x = 0x1234_5678u64;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut arrivals: Vec<(Instant, Vec<u8>)> = Vec::new();
    for (i, d) in framer_packets {
        if rand() < loss {
            continue;
        }
        let sent = t0 + FRAME_DURATION * i as u32 + FRAME_DURATION; // encoded once the frame is complete
        let delay = Duration::from_micros(2000 + (rand() * jitter_ms as f64 * 1000.0) as u64);
        arrivals.push((sent + delay, d));
    }
    arrivals.sort_by_key(|a| a.0);

    let mut playout = Playout::new(
        Box::new(OpusDecoder::new().unwrap()),
        PlayoutConfig::default(),
    );
    let mut out = Vec::new();
    let mut next = 0;
    let block = 512;
    let mut clock = t0;
    // Stop before the stream ends, which would count as an underrun.
    let total_samples = (count - 6) * FRAME_SAMPLES;
    while out.len() / 2 < total_samples {
        while next < arrivals.len() && arrivals[next].0 <= clock {
            let (h, payload) = AudioHeader::parse(&arrivals[next].1).unwrap();
            playout.push(&h, payload, arrivals[next].0);
            next += 1;
        }
        let mut buf = vec![0.0; block * 2];
        playout.render(&mut buf, clock);
        out.extend_from_slice(&buf);
        clock += Duration::from_secs_f64(block as f64 / f64::from(SAMPLE_RATE));
    }
    let s = playout.stats();
    let (lag, _) = best_lag(|i| noise(i).0, &out, 24_000, 4800);
    let end_to_end_ms = lag as f64 * 1000.0 / f64::from(SAMPLE_RATE);
    (s, end_to_end_ms)
}

#[test]
fn jitter_buffer_with_loss_and_jitter() {
    for (loss, jitter) in [(0.0, 0u64), (0.0, 10), (0.05, 10), (0.10, 25)] {
        let (s, end_to_end) = network_run(loss, jitter, 6);
        println!(
            "loss {:>4.0}% jitter 0-{jitter:>2} ms: buffer adds {:.1} ms (target {} frames, est. jitter {:.1} ms), \
             capture-to-playout {end_to_end:.1} ms, played {}, concealed {}, late {}, skipped {}, underruns {}",
            loss * 100.0,
            s.mean_delay().as_secs_f64() * 1000.0,
            s.target_frames,
            s.jitter_us / 1000.0,
            s.played,
            s.concealed,
            s.late,
            s.skipped,
            s.underruns,
        );
        let expected = 600.0 * (1.0 - loss);
        assert!(
            s.played as f64 >= expected * 0.9,
            "played {} of ~{expected}",
            s.played
        );
        assert!(
            s.mean_delay() < Duration::from_millis(80),
            "buffer delay {:?}",
            s.mean_delay()
        );
        if jitter == 0 && loss == 0.0 {
            assert_eq!(s.concealed, 0);
            assert!(
                s.mean_delay() < Duration::from_millis(35),
                "{:?}",
                s.mean_delay()
            );
        }
    }
}

#[test]
fn host_framing_then_viewer_playout_keeps_the_signal() {
    // Capture delivers odd-sized chunks; the framer still makes 10 ms frames.
    let t0 = Instant::now();
    let mut framer = Framer::default();
    let mut enc = OpusEncoder::new(OPUS_BITRATE).unwrap();
    let mut packetizer = AudioPacketizer::new(t0);
    let mut playout = Playout::new(
        Box::new(OpusDecoder::new().unwrap()),
        PlayoutConfig::default(),
    );
    let mut n = 0usize;
    let mut out = Vec::new();
    for chunk_frames in [333usize, 517, 1024, 47, 960].iter().cycle().take(60) {
        let mut samples = Vec::with_capacity(chunk_frames * 2);
        for _ in 0..*chunk_frames {
            let (l, r) = noise(n);
            samples.push(l);
            samples.push(r);
            n += 1;
        }
        let at = t0 + Duration::from_secs_f64(n as f64 / f64::from(SAMPLE_RATE));
        framer.push(
            &AudioChunk {
                samples,
                captured_at: at,
            },
            |pcm, captured| {
                let mut opus = Vec::new();
                enc.encode(pcm, &mut opus).unwrap();
                if opus.is_empty() {
                    return;
                }
                let mut d = Vec::new();
                packetizer.packet(&opus, captured, &mut d);
                let (h, p) = AudioHeader::parse(&d).unwrap();
                playout.push(&h, p, captured + FRAME_DURATION);
            },
        );
        let mut buf = vec![0.0; chunk_frames * 2];
        playout.render(&mut buf, at);
        out.extend_from_slice(&buf);
    }
    let (lag, rho) = best_lag(|i| noise(i).0, &out, 9600, 4800);
    println!("chunked capture: lag {lag} samples, correlation {rho:.3}");
    assert!(rho > 0.3, "correlation {rho}");
    assert_eq!(playout.stats().concealed, 0);
}
