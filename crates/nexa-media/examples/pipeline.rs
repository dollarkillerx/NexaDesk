//! Synthetic IOSurface frames through VtEncoder and VtDecoder (M0 S3 in Rust):
//! per-stage latency percentiles, plus the LTR loss-recovery test. No screen
//! capture happens here.

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macOS only");
}

#[cfg(target_os = "macos")]
fn main() {
    let ok = run::run();
    std::process::exit(if ok { 0 } else { 1 });
}

#[cfg(target_os = "macos")]
mod run {
    use std::thread;
    use std::time::{Duration, Instant};

    use nexa_media::macos::bitstream::{NalCodec, NalKind, annexb_nals, nal_kind};
    use nexa_media::macos::{DecoderOutput, MetalPresenter, Nv12Pool, PixelBuffer, VtDecoder, VtEncoder, probe_caps};
    use nexa_media::{
        CapturedFrame, DecodedFrame, Decoder, EncodeOpts, Encoder, EncoderConfig, FrameMeta, NativeImage, Recovery,
    };
    use nexa_wire::control::{Chroma, Codec, Mode};

    const W: usize = 1920;
    const H: usize = 1080;

    fn pct(v: &[f64], p: f64) -> f64 {
        if v.is_empty() {
            return f64::NAN;
        }
        let mut s = v.to_vec();
        s.sort_by(f64::total_cmp);
        s[((s.len() - 1) as f64 * p) as usize]
    }

    /// Scrolling textured background plus a moving block: compressible but never static.
    struct Texture(Vec<u8>);

    impl Texture {
        fn new() -> Self {
            let mut seed = 12345u32;
            let mut tiles = vec![0u8; 128 * 128];
            for t in &mut tiles {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *t = 40 + ((seed >> 24) % 150) as u8;
            }
            let mut pixels = vec![0u8; W * 2 * H];
            for y in 0..H {
                for x in 0..W * 2 {
                    let tile = tiles[((y >> 4) & 127) * 128 + ((x >> 4) & 127)];
                    pixels[y * W * 2 + x] = tile.wrapping_add((((x ^ y) & 7) * 3) as u8);
                }
            }
            Self(pixels)
        }

        fn fill(&self, pb: &PixelBuffer, n: usize) {
            let shift = (n * 3) % W;
            pb.with_planes_mut(|y, y_stride, uv, _| {
                for row in 0..H {
                    let start = row * W * 2 + shift;
                    y[row * y_stride..][..W].copy_from_slice(&self.0[start..start + W]);
                }
                let (bx, by) = ((n * 11) % (W - 320), (n * 5) % (H - 320));
                for row in by..by + 320 {
                    y[row * y_stride + bx..][..320].fill((200 + n % 50) as u8);
                }
                uv.fill(128);
            })
            .expect("synthetic buffer is writable");
        }
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let n = a.len().min(b.len());
        if n == 0 {
            return f64::NAN;
        }
        let sse: f64 = a[..n].iter().zip(&b[..n]).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum();
        let mse = sse / n as f64;
        if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
    }

    /// Recovers BT.709 limited-range luma from a BGRA render, sampled like `sample_luma(8)`.
    fn luma_from_bgra(bgra: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(W * H / 64);
        for y in (0..H).step_by(8) {
            for x in (0..W).step_by(8) {
                let p = &bgra[(y * W + x) * 4..];
                let (b, g, r) = (f64::from(p[0]), f64::from(p[1]), f64::from(p[2]));
                let luma = 16.0 + 0.182_586 * r + 0.614_231 * g + 0.062_007 * b;
                out.push(luma.round().clamp(0.0, 255.0) as u8);
            }
        }
        out
    }

    fn config(codec: Codec, fps: u32, bitrate_bps: u32) -> EncoderConfig {
        EncoderConfig {
            codec,
            chroma: Chroma::Yuv420,
            mode: Mode::Balanced,
            width: W as u32,
            height: H as u32,
            fps,
            bitrate_bps,
        }
    }

    fn captured(pb: &PixelBuffer) -> CapturedFrame {
        CapturedFrame { image: NativeImage::PixelBuffer(pb.clone()), captured_at: Instant::now(), dirty: Vec::new() }
    }

    fn name(codec: Codec) -> &'static str {
        if codec == Codec::Hevc { "hevc" } else { "h264" }
    }

    fn latency(codec: Codec, fps: u32, bitrate_bps: u32, texture: &Texture, presenter: &mut MetalPresenter) -> bool {
        let label = format!("{}-lowlatency-{fps}fps", name(codec));
        let mut encoder = match VtEncoder::new(config(codec, fps, bitrate_bps)) {
            Ok(e) => e,
            Err(e) => {
                println!("{label}: encoder create FAILED: {e}");
                return false;
            }
        };
        let mut decoder = VtDecoder::new(codec, DecoderOutput::Display).expect("codec is supported");
        let pool = Nv12Pool::new(W as u32, H as u32).expect("pool");
        let (mut en, mut de, mut gpu, mut total, mut sizes, mut psnrs) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut keys, mut errs, mut bind_fail) = (0, 0, 0);
        let frames = 6 * fps as usize;
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        let start = Instant::now();
        let mut out = Vec::new();
        for n in 0..frames {
            let due = start + interval * n as u32;
            while Instant::now() < due {
                thread::sleep(Duration::from_micros(100));
            }
            let pb = pool.get().expect("pool buffer");
            texture.fill(&pb, n);
            let frame = captured(&pb);
            let t0 = Instant::now();
            let meta = match encoder.encode(&frame, n as u32, EncodeOpts::default(), &mut out) {
                Ok(m) => m,
                Err(e) => {
                    errs += 1;
                    if n % 60 == 0 {
                        println!("{label}: encode error at {n}: {e}");
                    }
                    continue;
                }
            };
            let t1 = Instant::now();
            keys += usize::from(meta.key);
            sizes.push(out.len() as f64);
            let decoded = match decoder.decode(&out, FrameMeta { frame_id: n as u32, capture_ts_us: 0 }) {
                Ok(d) => d,
                Err(e) => {
                    errs += 1;
                    if n % 60 == 0 {
                        println!("{label}: decode error at {n}: {e}");
                    }
                    continue;
                }
            };
            let t2 = Instant::now();
            if let Err(e) = presenter.render_offscreen(&decoded, W as u32, H as u32) {
                bind_fail += 1;
                if bind_fail == 1 {
                    println!("{label}: GPU bind failed: {e}");
                }
            }
            let t3 = Instant::now();
            if n >= 10 {
                en.push((t1 - t0).as_secs_f64() * 1000.0);
                de.push((t2 - t1).as_secs_f64() * 1000.0);
                gpu.push((t3 - t2).as_secs_f64() * 1000.0);
                total.push((t3 - t0).as_secs_f64() * 1000.0);
            }
            if n % 30 == 0
                && let Ok(bgra) = presenter.render_to_bgra(&decoded, W as u32, H as u32)
            {
                psnrs.push(psnr(&pb.sample_luma(8).expect("source is 420v"), &luma_from_bgra(&bgra)));
            }
        }
        let mbps = sizes.iter().sum::<f64>() * 8.0 * f64::from(fps) / sizes.len().max(1) as f64 / 1e6;
        let mean_psnr = psnrs.iter().sum::<f64>() / psnrs.len().max(1) as f64;
        println!(
            "{label}: hw={} enc p50={:.2} p99={:.2} | dec p50={:.2} p99={:.2} | gpu-bind p50={:.2} p99={:.2} | total p50={:.2} p99={:.2} ms | {mbps:.1} Mbps keys={keys} errs={errs} bindFail={bind_fail} psnr(GPU path)={mean_psnr:.1} rejected={:?}",
            encoder.caps().hardware,
            pct(&en, 0.5),
            pct(&en, 0.99),
            pct(&de, 0.5),
            pct(&de, 0.99),
            pct(&gpu, 0.5),
            pct(&gpu, 0.99),
            pct(&total, 0.5),
            pct(&total, 0.99),
            encoder.rejected_properties(),
        );
        errs == 0 && bind_fail == 0
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Rec {
        Ltr,
        ForceKey,
    }

    fn nal_codec(codec: Codec) -> NalCodec {
        if codec == Codec::Hevc { NalCodec::Hevc } else { NalCodec::H264 }
    }

    fn is_idr(codec: Codec, au: &[u8]) -> bool {
        annexb_nals(au).any(|n| nal_kind(nal_codec(codec), n) == NalKind::Slice { idr: true })
    }

    /// Drops frames 100..103 from the decoder, then recovers as the design would.
    fn loss_test(codec: Codec, rec: Rec, texture: &Texture) -> bool {
        const LOST: std::ops::Range<usize> = 100..103;
        const RESUME: usize = 103;
        let label = format!("{}-lowlatency {}", name(codec), if rec == Rec::Ltr { "LTR" } else { "force-IDR" });
        let mut encoder = VtEncoder::new(config(codec, 60, 20_000_000)).expect("encoder");
        assert!(encoder.caps().ltr, "M0 says low-latency mode supports LTR");
        let mut decoder = VtDecoder::new(codec, DecoderOutput::CpuReadable420v).expect("codec is supported");
        let pool = Nv12Pool::new(W as u32, H as u32).expect("pool");
        let (mut sizes, mut keys_after, mut decode_errs) = (Vec::with_capacity(200), 0, 0);
        let mut psnr_by_frame = std::collections::BTreeMap::new();
        let mut recovery_result = None;
        let mut recovery_flag = false;
        let mut out = Vec::new();
        for n in 0..200usize {
            let pb = pool.get().expect("pool buffer");
            texture.fill(&pb, n);
            if n == RESUME {
                match rec {
                    Rec::Ltr => recovery_result = Some(encoder.invalidate_from(LOST.start as u32).expect("invalidate")),
                    Rec::ForceKey => encoder.force_idr(),
                }
            }
            let meta = encoder
                .encode(&captured(&pb), n as u32, EncodeOpts::default(), &mut out)
                .unwrap_or_else(|e| panic!("{label}: encode at {n}: {e}"));
            sizes.push(out.len());
            if n == 0 {
                let ok = out.starts_with(&[0, 0, 0, 1]);
                assert!(ok, "output must be Annex-B");
                let kinds: Vec<_> = annexb_nals(&out).map(|x| nal_kind(nal_codec(codec), x)).collect();
                println!("{label}: first key frame NAL sequence {kinds:?}");
            }
            if n >= RESUME && (meta.key || is_idr(codec, &out)) {
                keys_after += 1;
            }
            if n == RESUME {
                recovery_flag = meta.recovery_point;
            }
            if LOST.contains(&n) {
                continue;
            }
            match decoder.decode(&out, FrameMeta { frame_id: n as u32, capture_ts_us: 0 }) {
                Ok(frame) => {
                    if (RESUME - 6..=RESUME + 20).contains(&n) || n % 20 == 0 {
                        psnr_by_frame.insert(n, psnr(&pb.sample_luma(8).expect("420v"), &decoded_luma(&frame)));
                    }
                    encoder.on_frame_acked(n as u32);
                }
                Err(e) => {
                    decode_errs += 1;
                    if n <= RESUME + 3 {
                        println!("{label}: decode error at {n}: {e}");
                    }
                }
            }
        }
        let avg = sizes[10..100].iter().sum::<usize>() as f64 / 90.0;
        let ratio = sizes[RESUME] as f64 / avg.max(1.0);
        let at = |n: usize| psnr_by_frame.get(&n).copied().unwrap_or(f64::NAN);
        let (before, resume, later) = (at(99), at(RESUME), at(RESUME + 10));
        println!(
            "{label}: recovery={recovery_result:?} recovery_point={recovery_flag} decode_errs={decode_errs} keyframes_after_loss={keys_after} resume_frame_size={ratio:.1}x avg psnr before={before:.1} resume={resume:.1} resume+10={later:.1}"
        );
        match rec {
            Rec::Ltr => {
                let ok = decode_errs == 0
                    && keys_after == 0
                    && recovery_result == Some(Recovery::RefInvalidated)
                    && recovery_flag
                    && resume > 40.0
                    && later > 40.0;
                println!("{label}: {}", if ok { "PASS" } else { "FAIL" });
                ok
            }
            Rec::ForceKey => true,
        }
    }

    fn decoded_luma(frame: &DecodedFrame) -> Vec<u8> {
        match &frame.image {
            NativeImage::PixelBuffer(pb) => pb.sample_luma(8).expect("CpuReadable420v output is readable"),
            NativeImage::Cpu(_) => unreachable!("VtDecoder always returns pixel buffers"),
        }
    }

    pub fn run() -> bool {
        let caps = probe_caps();
        println!(
            "probe_caps: encode={:?} decode={:?} rfi={} ltr={} max_fps={}",
            caps.encode.iter().map(|c| Codec::try_from(c.codec).ok()).collect::<Vec<_>>(),
            caps.decode.iter().map(|c| Codec::try_from(c.codec).ok()).collect::<Vec<_>>(),
            caps.rfi,
            caps.ltr,
            caps.max_fps
        );
        let texture = Texture::new();
        let mut presenter = MetalPresenter::offscreen(W as u32, H as u32).expect("Metal presenter");
        let mut ok = true;
        ok &= latency(Codec::H264, 60, 20_000_000, &texture, &mut presenter);
        ok &= latency(Codec::Hevc, 60, 20_000_000, &texture, &mut presenter);
        ok &= latency(Codec::H264, 120, 40_000_000, &texture, &mut presenter);
        ok &= latency(Codec::Hevc, 120, 40_000_000, &texture, &mut presenter);
        ok &= loss_test(Codec::H264, Rec::Ltr, &texture);
        ok &= loss_test(Codec::Hevc, Rec::Ltr, &texture);
        ok &= loss_test(Codec::H264, Rec::ForceKey, &texture);
        ok &= loss_test(Codec::Hevc, Rec::ForceKey, &texture);
        println!("pipeline: {}", if ok { "PASS" } else { "FAIL" });
        ok
    }
}
