//! §6.8 static refinement: can a live low-latency VideoToolbox session be pushed to a
//! higher quality on an unchanged picture without emitting an IDR? Synthetic frames
//! only; no screen capture. Run with `--nocapture` to see the measurements.
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use nexa_media::macos::{DecoderOutput, Nv12Pool, PixelBuffer, VtDecoder, VtEncoder};
use nexa_media::{CapturedFrame, Decoder, EncodeOpts, Encoder, EncoderConfig, FrameMeta, MediaError, NativeImage};
use nexa_wire::control::{Chroma, Codec, Mode};

const W: u32 = 1280;
const H: u32 = 720;

/// Fine text-like detail, with the first `typed` glyph cells filled in: what
/// refinement is for, and what a busy encoder blurs.
fn detailed(pool: &Nv12Pool, typed: usize) -> (PixelBuffer, Vec<u8>) {
    let pb = pool.get().unwrap();
    let mut x = 0x2545_F491u32;
    let mut luma = vec![0u8; (W * H) as usize];
    for (i, p) in luma.iter_mut().enumerate() {
        let (r, c) = (i / W as usize, i % W as usize);
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let glyph = ((r / 3 + c / 2) % 5 == 0) as u8 * 120;
        let cell = (r / 24) * (W as usize / 12) + c / 12;
        let ink = if cell < typed && (r + c) % 3 == 0 { 60 } else { 0 };
        *p = 20 + glyph + ink + (x % 40) as u8;
    }
    pb.with_planes_mut(|y, stride, uv, _| {
        for (row, src) in y.chunks_exact_mut(stride).zip(luma.as_chunks::<{ W as usize }>().0) {
            row[..W as usize].copy_from_slice(src);
        }
        uv.fill(128);
    })
    .unwrap();
    (pb, luma)
}

fn psnr(decoded: &nexa_media::DecodedFrame, reference: &[u8]) -> f64 {
    let NativeImage::PixelBuffer(pb) = &decoded.image else { panic!("cpu frame") };
    let got = pb.sample_luma(1).expect("cpu readable");
    let mse: f64 = got
        .iter()
        .zip(reference)
        .map(|(a, b)| {
            let d = f64::from(*a) - f64::from(*b);
            d * d
        })
        .sum::<f64>()
        / reference.len() as f64;
    10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10()
}

struct Step {
    label: String,
    status: i32,
    key: bool,
    bytes: usize,
    psnr: f64,
}

/// What happens at each step of the timeline.
#[derive(Clone, Copy)]
enum Event {
    /// New content (one more glyph typed), at 60 fps.
    Typing,
    /// The §6.8 re-encode of the unchanged last frame, with refinement level 0..4.
    Refine(usize),
}

/// §6.8's timeline: typing at 60 fps for half a second, a pause with the four
/// refinement re-encodes at +100/+300/+600/+1000 ms, then typing again.
fn timeline() -> Vec<(u64, Event)> {
    let mut t = Vec::new();
    for n in 0..30u64 {
        t.push((n * 16_667, Event::Typing));
    }
    let last = 29 * 16_667;
    for (i, after) in [100_000u64, 300_000, 600_000, 1_000_000].into_iter().enumerate() {
        t.push((last + after, Event::Refine(i)));
    }
    for n in 0..30u64 {
        t.push((last + 1_100_000 + n * 16_667, Event::Typing));
    }
    t
}

fn run(
    codec: Codec,
    bitrate_bps: u32,
    mut configure: impl FnMut(&mut VtEncoder, Option<usize>) -> (String, i32),
) -> Vec<Step> {
    let pool = Nv12Pool::new(W, H).unwrap();
    let mut encoder = VtEncoder::new(EncoderConfig {
        codec,
        chroma: Chroma::Yuv420,
        mode: Mode::Office,
        width: W,
        height: H,
        fps: 60,
        bitrate_bps,
    })
    .unwrap();
    let mut decoder = VtDecoder::new(codec, DecoderOutput::CpuReadable420v).unwrap();
    let mut out = Vec::new();
    let mut steps = Vec::new();
    let t0 = Instant::now();
    let (mut typed, mut current) = (0usize, detailed(&pool, 0));
    let mut refining = false;
    for (n, (at_us, event)) in timeline().into_iter().enumerate() {
        let (label, status) = match event {
            Event::Typing => {
                typed += 1;
                current = detailed(&pool, typed);
                if refining {
                    refining = false;
                    let (l, s) = configure(&mut encoder, None);
                    (format!("content, {l}"), s)
                } else {
                    ("content".to_owned(), 0)
                }
            }
            Event::Refine(i) => {
                refining = true;
                configure(&mut encoder, Some(i))
            }
        };
        let frame = CapturedFrame {
            image: NativeImage::PixelBuffer(current.0.clone()),
            captured_at: t0 + Duration::from_micros(at_us),
            dirty: Vec::new(),
        };
        let n = n as u32;
        match encoder.encode(&frame, n, EncodeOpts::default(), &mut out) {
            Ok(meta) => {
                let decoded = decoder.decode(&out, FrameMeta { frame_id: n, capture_ts_us: 0 }).unwrap();
                steps.push(Step { label, status, key: meta.key, bytes: out.len(), psnr: psnr(&decoded, &current.1) });
            }
            Err(MediaError::FrameDropped) => {
                steps.push(Step { label: format!("{label} DROPPED"), status, key: false, bytes: 0, psnr: f64::NAN })
            }
            Err(e) => panic!("{e}"),
        }
    }
    steps
}

fn report(name: &str, steps: &[Step]) {
    println!("--- {name}");
    for (n, s) in steps.iter().enumerate() {
        println!(
            "frame {n:2} {:>18} status {:>6} key {:5} {:7} bytes  PSNR {:5.2} dB",
            s.label, s.status, s.key, s.bytes, s.psnr
        );
    }
}

fn summary(name: &str, steps: &[Step]) {
    let typing_before = &steps[20..30];
    let refine = &steps[30..34];
    let after = &steps[34..44];
    let mean = |s: &[Step]| {
        let v: Vec<f64> = s.iter().filter(|x| x.bytes > 0).map(|x| x.psnr).collect();
        v.iter().sum::<f64>() / v.len().max(1) as f64
    };
    let drops = |s: &[Step]| s.iter().filter(|x| x.bytes == 0).count();
    println!(
        "{name}: typing PSNR {:.2} dB | refine PSNR {:?} bytes {:?} status {:?} | next 10 frames: {} dropped, mean {:.0} bytes, PSNR {:.2} dB",
        mean(typing_before),
        refine.iter().map(|s| (s.psnr * 100.0).round() / 100.0).collect::<Vec<_>>(),
        refine.iter().map(|s| s.bytes).collect::<Vec<_>>(),
        refine.iter().map(|s| s.status).collect::<Vec<_>>(),
        drops(after),
        after.iter().map(|s| s.bytes).sum::<usize>() as f64 / 10.0,
        mean(after),
    );
}

/// The measurement behind the decision recorded in `nexa_session::host`: this
/// prints what each knob does and asserts only what must hold for any outcome.
/// `NEXA_REFINE_VERBOSE=1` prints every frame.
#[test]
fn refinement_knobs_on_a_live_low_latency_session() {
    let verbose = std::env::var_os("NEXA_REFINE_VERBOSE").is_some();
    let qp = [28u32, 24, 20, 16];
    let quality = [0.6f32, 0.75, 0.9, 1.0];
    let bitrate = 8_000_000u32;
    for codec in [Codec::Hevc, Codec::H264] {
        type Knob = Box<dyn FnMut(&mut VtEncoder, Option<usize>) -> (String, i32)>;
        let knobs: [(&str, Knob); 4] = [
            (
                "MaxAllowedFrameQP, back to 51",
                Box::new(move |e, i| match i {
                    Some(i) => (format!("QP cap {}", qp[i]), e.set_max_frame_qp(Some(qp[i]))),
                    None => ("QP cap 51".into(), e.set_max_frame_qp(Some(51))),
                }),
            ),
            (
                "MaxAllowedFrameQP, back to default",
                Box::new(move |e, i| match i {
                    Some(i) => (format!("QP cap {}", qp[i]), e.set_max_frame_qp(Some(qp[i]))),
                    None => ("QP cap NULL".into(), e.set_max_frame_qp(None)),
                }),
            ),
            (
                "Quality",
                Box::new(move |e, i| {
                    let q = i.map_or(0.5, |i| quality[i]);
                    (format!("Quality {q}"), e.set_quality(q))
                }),
            ),
            (
                "AverageBitRate boost",
                Box::new(move |e, i| {
                    let r = i.map_or(bitrate, |i| bitrate * (2 << i));
                    (format!("bitrate {}M", r / 1_000_000), if e.set_bitrate(r).is_ok() { 0 } else { -1 })
                }),
            ),
        ];
        for (what, knob) in knobs {
            let steps = run(codec, bitrate, knob);
            let name = format!("{codec:?} 1280x720 8M {what}");
            if verbose {
                report(&name, &steps);
            }
            summary(&name, &steps);
            assert!(steps[0].key);
            assert!(steps[1..].iter().all(|s| !s.key), "{name}: no IDR after the first frame");
        }
    }
}
