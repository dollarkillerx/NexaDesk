//! Real ScreenCapturer for 5 seconds into VtEncoder and VtDecoder. Prints stats
//! only: no frame is written anywhere, sent anywhere, or logged.
//!
//! Without Screen Recording permission this reports that and exits; pass
//! `--request` to attempt the capture anyway, which lets macOS show its prompt.

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macOS only");
}

#[cfg(target_os = "macos")]
fn main() {
    let request = std::env::args().any(|a| a == "--request");
    run::run(request);
}

#[cfg(target_os = "macos")]
mod run {
    use std::time::{Duration, Instant};

    use nexa_media::macos::{DecoderOutput, ScreenCapturer, VtDecoder, VtEncoder, has_screen_capture_permission};
    use nexa_media::{
        CaptureConfig, CaptureEvent, Capturer, Decoder, EncodeOpts, Encoder, EncoderConfig, FrameMeta, NativeImage,
    };
    use nexa_wire::control::{Chroma, Codec, Mode};

    const RUN: Duration = Duration::from_secs(5);
    const FPS: u32 = 60;
    const BITRATE: u32 = 30_000_000;

    fn pct(v: &[f64], p: f64) -> f64 {
        if v.is_empty() {
            return f64::NAN;
        }
        let mut s = v.to_vec();
        s.sort_by(f64::total_cmp);
        s[((s.len() - 1) as f64 * p) as usize]
    }

    fn line(name: &str, v: &[f64]) -> String {
        format!("{name}: n={} p50={:.2} p99={:.2} max={:.2} ms", v.len(), pct(v, 0.5), pct(v, 0.99), pct(v, 1.0))
    }

    pub fn run(request: bool) {
        let allowed = has_screen_capture_permission();
        println!("screen capture permission preflight: {allowed}");
        if !allowed && !request {
            println!("No Screen Recording permission. Not starting a capture, so macOS shows no prompt.");
            println!("Grant the permission and re-run, or pass --request to let macOS ask.");
            return;
        }

        let config = CaptureConfig {
            display: 0,
            width: 1920,
            height: 1200,
            fps: FPS,
            show_cursor: false,
            chroma: Chroma::Yuv420,
        };
        let mut capturer = match ScreenCapturer::new(config) {
            Ok(c) => c,
            Err(e) => {
                println!("ScreenCapturer::new failed: {e}");
                return;
            }
        };
        let (w, h) = capturer.output_size();
        let encoder_config = EncoderConfig {
            codec: Codec::Hevc,
            chroma: Chroma::Yuv420,
            mode: Mode::Balanced,
            width: w,
            height: h,
            fps: FPS,
            bitrate_bps: BITRATE,
        };
        let mut encoder = VtEncoder::new(encoder_config).expect("encoder");
        let mut decoder = VtDecoder::new(Codec::Hevc, DecoderOutput::Display).expect("decoder");

        let (mut capture_ms, mut encode_ms, mut decode_ms, mut sizes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut frames, mut idle, mut encode_errs, mut decode_errs, mut keys, mut dirty_rects) = (0u32, 0, 0, 0, 0, 0usize);
        let mut formats = std::collections::BTreeSet::new();
        let mut event_note = None;
        let mut out = Vec::new();

        let start = Instant::now();
        while start.elapsed() < RUN {
            match capturer.next(Duration::from_millis(100)) {
                Err(e) => {
                    println!("capture error: {e}");
                    break;
                }
                Ok(CaptureEvent::Idle) => idle += 1,
                Ok(CaptureEvent::AccessLost) => {
                    event_note = Some("access lost".to_string());
                    break;
                }
                Ok(CaptureEvent::ModeChanged(m)) => event_note = Some(format!("mode changed to {m:?}")),
                Ok(CaptureEvent::Frame(frame)) => {
                    let got = Instant::now();
                    capture_ms.push(got.duration_since(frame.captured_at).as_secs_f64() * 1000.0);
                    if let NativeImage::PixelBuffer(pb) = &frame.image {
                        formats.insert(pb.pixel_format());
                    }
                    dirty_rects += frame.dirty.len();
                    let t0 = Instant::now();
                    match encoder.encode(&frame, frames, EncodeOpts::default(), &mut out) {
                        Ok(meta) => {
                            let t1 = Instant::now();
                            encode_ms.push((t1 - t0).as_secs_f64() * 1000.0);
                            keys += usize::from(meta.key);
                            sizes.push(out.len() as f64);
                            match decoder.decode(&out, FrameMeta { frame_id: frames, capture_ts_us: 0 }) {
                                Ok(_) => {
                                    decode_ms.push(t1.elapsed().as_secs_f64() * 1000.0);
                                    encoder.on_frame_acked(frames);
                                }
                                Err(_) => decode_errs += 1,
                            }
                            frames += 1;
                        }
                        Err(_) => encode_errs += 1,
                    }
                }
            }
        }
        drop(capturer);

        let elapsed = start.elapsed().as_secs_f64();
        println!(
            "capture {w}x{h} requested {FPS} fps for {elapsed:.1}s: frames={frames} ({:.1} fps) idle={idle} encode_errs={encode_errs} decode_errs={decode_errs} keys={keys} dirty_rects={dirty_rects}",
            f64::from(frames) / elapsed
        );
        for f in &formats {
            println!("pixel format: 0x{f:08x}");
        }
        println!("{}", line("mailbox (callback to next() return)", &capture_ms));
        println!("{}", line("encode", &encode_ms));
        println!("{}", line("decode", &decode_ms));
        let mbps = sizes.iter().sum::<f64>() * 8.0 / elapsed / 1e6;
        println!("bitrate: {mbps:.1} Mbps (target {} Mbps), frames sized p50={:.0} B", BITRATE / 1_000_000, pct(&sizes, 0.5));
        if let Some(note) = event_note {
            println!("event: {note}");
        }
    }
}
