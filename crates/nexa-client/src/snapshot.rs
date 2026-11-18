//! The latest decoded picture of a session without a view (`nexa-mcp`, docs/MCP.md
//! §3.2): kept for screenshots instead of drawn, with when it last changed, so a
//! caller can wait for the host's screen to settle after an action (§3.3).
//!
//! Only the newest frame is kept; nothing queues. Whether a frame changed the picture
//! is judged from a sparse sample of its luma, with a tolerance: a lossy encoder
//! refining a still screen (§6.8) changes pixels a little without changing what is
//! shown.

use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use nexa_media::{DecodedFrame, NativeImage, PresentMode, PresentStat, Presenter};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every how many pixels, in both directions, luma is sampled to tell a change.
const SAMPLE_STEP: usize = 16;
/// Mean absolute luma difference of the samples below which two frames show the same.
const SAME_PICTURE: f64 = 1.5;

#[derive(Default)]
struct State {
    latest: Option<NativeImage>,
    /// Frames received, changed or not.
    frames: u64,
    /// When the picture last changed (or first arrived).
    changed_at: Option<Instant>,
    samples: Vec<u8>,
}

/// Shared between the session's render thread (through [`Frames::presenter`]) and
/// whoever takes screenshots.
#[derive(Default)]
pub struct Frames {
    state: Mutex<State>,
    arrived: Condvar,
}

/// A picture taken by [`Frames::settle`].
pub struct Settled {
    pub image: NativeImage,
    /// False when the picture was still changing at the deadline.
    pub settled: bool,
}

impl Frames {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The presenter to attach to the session (`Session::attach_presenter`).
    pub fn presenter(self: &Arc<Self>) -> Box<dyn Presenter> {
        Box::new(SnapshotPresenter(self.clone()))
    }

    /// The newest frame, if any arrived.
    pub fn latest(&self) -> Option<NativeImage> {
        lock(&self.state).latest.clone()
    }

    /// Frames received so far.
    pub fn frames(&self) -> u64 {
        lock(&self.state).frames
    }

    /// Waits until the picture has not changed for `quiet`, counting from `since` (the
    /// moment an action was sent), and returns it. Gives up at `since + max` with
    /// whatever is newest. `None` only if no frame has ever arrived by then.
    pub fn settle(&self, since: Instant, quiet: Duration, max: Duration) -> Option<Settled> {
        let deadline = since + max;
        let mut state = lock(&self.state);
        loop {
            let now = Instant::now();
            let calm_from = state.changed_at.map_or(since, |c| c.max(since));
            if state.latest.is_some() && now >= calm_from + quiet {
                return state.latest.clone().map(|image| Settled {
                    image,
                    settled: true,
                });
            }
            if now >= deadline {
                return state.latest.clone().map(|image| Settled {
                    image,
                    settled: false,
                });
            }
            let until = if state.latest.is_some() {
                (calm_from + quiet).min(deadline)
            } else {
                deadline
            };
            state = self
                .arrived
                .wait_timeout(state, until.saturating_duration_since(now))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn on_frame(&self, image: &NativeImage) {
        let samples = luma_samples(image);
        let mut state = lock(&self.state);
        let changed = match (&samples, state.latest.is_some()) {
            (_, false) => true,
            (Some(s), true) => !same_picture(s, &state.samples),
            // Unreadable: count every frame as a change, so waits run to their limit
            // rather than return a picture that may be old.
            (None, true) => true,
        };
        if changed {
            state.changed_at = Some(Instant::now());
        }
        if let Some(s) = samples {
            state.samples = s;
        }
        state.latest = Some(image.clone());
        state.frames += 1;
        drop(state);
        self.arrived.notify_all();
    }
}

fn same_picture(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    let sum: u64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| u64::from(x.abs_diff(*y)))
        .sum();
    (sum as f64 / a.len() as f64) < SAME_PICTURE
}

/// Every [`SAMPLE_STEP`]th luma sample, or `None` if the frame cannot be read here.
fn luma_samples(image: &NativeImage) -> Option<Vec<u8>> {
    match image {
        NativeImage::Cpu(f) => {
            let (w, h) = (f.width as usize, f.height as usize);
            let mut out = Vec::with_capacity((w / SAMPLE_STEP + 1) * (h / SAMPLE_STEP + 1));
            for y in (0..h).step_by(SAMPLE_STEP) {
                for x in (0..w).step_by(SAMPLE_STEP) {
                    out.push(*f.y.get(y * w + x)?);
                }
            }
            Some(out)
        }
        #[cfg(target_vendor = "apple")]
        NativeImage::PixelBuffer(pb) => pb.sample_luma(SAMPLE_STEP),
        #[cfg(windows)]
        NativeImage::Texture(_) => None,
    }
}

struct SnapshotPresenter(Arc<Frames>);

impl Presenter for SnapshotPresenter {
    fn resize(&mut self, _: u32, _: u32, _: f32) -> nexa_media::Result<()> {
        Ok(())
    }

    fn set_mode(&mut self, _: PresentMode) -> nexa_media::Result<()> {
        Ok(())
    }

    fn present(&mut self, frame: &DecodedFrame) -> nexa_media::Result<PresentStat> {
        self.0.on_frame(&frame.image);
        Ok(PresentStat::default())
    }
}

/// A picture as packed 8-bit RGB, top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Reads a decoded frame back as RGB. Needs a CPU-readable frame: on macOS the
/// decoder must hand out `420v` (`DecoderOutput::CpuReadable420v`).
pub fn to_rgb(image: &NativeImage) -> Result<Rgb, String> {
    match image {
        NativeImage::Cpu(f) => {
            let (w, h) = (f.width as usize, f.height as usize);
            let uv_stride = w.div_ceil(2) * 2;
            if f.y.len() < w * h || f.uv.len() < uv_stride * h.div_ceil(2) {
                return Err("frame planes are shorter than its size".into());
            }
            Ok(Rgb {
                width: f.width,
                height: f.height,
                data: nv12_to_rgb(&f.y, w, &f.uv, uv_stride, w, h),
            })
        }
        #[cfg(target_vendor = "apple")]
        NativeImage::PixelBuffer(pb) => {
            let (w, h) = (pb.width(), pb.height());
            pb.with_planes(|y, ys, uv, uvs| Rgb {
                width: w,
                height: h,
                data: nv12_to_rgb(y, ys, uv, uvs, w as usize, h as usize),
            })
            .map_err(|e| e.to_string())
        }
        #[cfg(windows)]
        NativeImage::Texture(_) => Err("reading a GPU frame back is not supported yet".into()),
    }
}

/// NV12, BT.709 video range, to RGB: the same matrix as the Metal presenter
/// (nexa-media/src/macos/present.rs), so a screenshot shows what the window would.
fn nv12_to_rgb(y: &[u8], ys: usize, uv: &[u8], uvs: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3];
    let clamp = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    for row in 0..h {
        let yl = &y[row * ys..];
        let cl = &uv[(row / 2) * uvs..];
        let o = &mut out[row * w * 3..(row + 1) * w * 3];
        for col in 0..w {
            let l = 1.164_384 * (f32::from(yl[col]) - 16.0);
            let c = (col / 2) * 2;
            let u = f32::from(cl[c]) - 128.0;
            let v = f32::from(cl[c + 1]) - 128.0;
            o[col * 3] = clamp(l + 1.792_741 * v);
            o[col * 3 + 1] = clamp(l - 0.213_249 * u - 0.532_909 * v);
            o[col * 3 + 2] = clamp(l + 2.112_402 * u);
        }
    }
    out
}

/// Scales `rgb` down so neither side exceeds `max_side`, averaging the source pixels
/// each output pixel covers. Pictures already small enough come back unchanged.
pub fn fit(rgb: Rgb, max_side: u32) -> Rgb {
    let (w, h) = (rgb.width, rgb.height);
    if w.max(h) <= max_side || w == 0 || h == 0 {
        return rgb;
    }
    let k = f64::from(max_side) / f64::from(w.max(h));
    let ow = ((f64::from(w) * k).round() as u32).max(1);
    let oh = ((f64::from(h) * k).round() as u32).max(1);
    let (w, h, ow_, oh_) = (w as usize, h as usize, ow as usize, oh as usize);
    let mut out = vec![0u8; ow_ * oh_ * 3];
    for oy in 0..oh_ {
        let y0 = oy * h / oh_;
        let y1 = ((oy + 1) * h / oh_).max(y0 + 1);
        for ox in 0..ow_ {
            let x0 = ox * w / ow_;
            let x1 = ((ox + 1) * w / ow_).max(x0 + 1);
            let mut sum = [0u32; 3];
            for sy in y0..y1 {
                let line = &rgb.data[(sy * w + x0) * 3..(sy * w + x1) * 3];
                for px in line.as_chunks::<3>().0 {
                    sum[0] += u32::from(px[0]);
                    sum[1] += u32::from(px[1]);
                    sum[2] += u32::from(px[2]);
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (oy * ow_ + ox) * 3;
            for i in 0..3 {
                out[o + i] = ((sum[i] + n / 2) / n) as u8;
            }
        }
    }
    Rgb {
        width: ow,
        height: oh,
        data: out,
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use nexa_media::CpuFrame;

    use super::*;

    fn gray(w: u32, h: u32, luma: u8) -> NativeImage {
        NativeImage::Cpu(Arc::new(CpuFrame {
            width: w,
            height: h,
            y: vec![luma; (w * h) as usize],
            uv: vec![128; (w.div_ceil(2) * 2 * h.div_ceil(2)) as usize],
        }))
    }

    fn frame(image: NativeImage) -> DecodedFrame {
        DecodedFrame {
            image,
            meta: nexa_media::FrameMeta {
                frame_id: Default::default(),
                capture_ts_us: 0,
            },
        }
    }

    #[test]
    fn video_range_black_and_white_map_to_the_ends() {
        let rgb = to_rgb(&gray(4, 2, 16)).unwrap();
        assert_eq!((rgb.width, rgb.height), (4, 2));
        assert!(rgb.data.iter().all(|&v| v == 0));
        let rgb = to_rgb(&gray(4, 2, 235)).unwrap();
        assert!(rgb.data.iter().all(|&v| v == 255));
    }

    #[test]
    fn chroma_gives_colour() {
        // Pure red in BT.709 video range: Y 63, Cb 102, Cr 240.
        let image = NativeImage::Cpu(Arc::new(CpuFrame {
            width: 2,
            height: 2,
            y: vec![63; 4],
            uv: vec![102, 240],
        }));
        let rgb = to_rgb(&image).unwrap();
        let px = &rgb.data[0..3];
        assert!(px[0] > 240 && px[1] < 20 && px[2] < 20, "{px:?}");
    }

    #[test]
    fn short_planes_are_an_error_not_a_panic() {
        let image = NativeImage::Cpu(Arc::new(CpuFrame {
            width: 8,
            height: 8,
            y: vec![0; 10],
            uv: vec![],
        }));
        assert!(to_rgb(&image).is_err());
    }

    #[test]
    fn fit_keeps_the_aspect_and_averages() {
        let rgb = Rgb {
            width: 4,
            height: 2,
            data: [[0u8, 0, 0], [255, 255, 255]]
                .iter()
                .cycle()
                .take(8)
                .flatten()
                .copied()
                .collect(),
        };
        let small = fit(rgb.clone(), 2);
        assert_eq!((small.width, small.height), (2, 1));
        assert!(small.data.iter().all(|&v| v == 128), "{:?}", small.data);
        assert_eq!(fit(rgb.clone(), 4), rgb);
    }

    #[test]
    fn settle_returns_once_the_picture_stops_changing() {
        let frames = Frames::new();
        let mut p = frames.presenter();
        let since = Instant::now();
        assert!(
            frames
                .settle(since, Duration::from_millis(10), Duration::from_millis(30))
                .is_none(),
            "no frame yet"
        );
        p.present(&frame(gray(64, 64, 50))).unwrap();
        let since = Instant::now();
        let s = frames
            .settle(since, Duration::from_millis(50), Duration::from_secs(2))
            .unwrap();
        assert!(s.settled);
        assert!(since.elapsed() >= Duration::from_millis(50));
        assert!(since.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn a_changing_picture_is_waited_for_and_a_steady_stream_is_not() {
        let frames = Frames::new();
        let mut p = frames.presenter();
        p.present(&frame(gray(64, 64, 50))).unwrap();
        let f = frames.clone();
        let since = Instant::now();
        let changer = thread::spawn(move || {
            let mut p = f.presenter();
            // Changes for 150 ms, then the same picture keeps arriving.
            for i in 0..30u8 {
                let luma = if i < 15 { 60 + i * 5 } else { 200 };
                p.present(&frame(gray(64, 64, luma))).unwrap();
                thread::sleep(Duration::from_millis(10));
            }
        });
        let s = frames
            .settle(since, Duration::from_millis(80), Duration::from_secs(2))
            .unwrap();
        changer.join().unwrap();
        assert!(s.settled);
        let waited = since.elapsed();
        assert!(waited >= Duration::from_millis(200), "{waited:?}");
        assert!(waited < Duration::from_millis(1000), "{waited:?}");
        assert_eq!(
            to_rgb(&s.image).unwrap().data[0],
            to_rgb(&gray(2, 2, 200)).unwrap().data[0]
        );
    }

    #[test]
    fn a_picture_that_never_settles_is_returned_at_the_deadline() {
        let frames = Frames::new();
        let f = frames.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let changer = thread::spawn(move || {
            let mut p = f.presenter();
            let mut i = 0u8;
            while !stop2.load(std::sync::atomic::Ordering::SeqCst) {
                p.present(&frame(gray(64, 64, i))).unwrap();
                i = i.wrapping_add(40);
                thread::sleep(Duration::from_millis(5));
            }
        });
        let since = Instant::now();
        let s = frames
            .settle(since, Duration::from_millis(50), Duration::from_millis(200))
            .unwrap();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        changer.join().unwrap();
        assert!(!s.settled);
        assert!(since.elapsed() >= Duration::from_millis(200));
    }
}
