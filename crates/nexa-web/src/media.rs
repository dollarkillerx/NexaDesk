//! Capture and encode for one browser: a thread that hands Annex-B frames to the
//! WebRTC loop. Frames are dropped before encoding, never after, when the loop is behind
//! (§1.3), and a key frame is forced whenever the browser asks for one.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering::Relaxed},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use nexa_media::{CaptureConfig, CaptureEvent, EncodeOpts, EncoderConfig};
use nexa_wire::control::{Chroma, Codec, Mode};
use tokio::sync::mpsc;

use crate::{WebConfig, WebHost, WebScreen};

pub(crate) struct Frame {
    pub data: Vec<u8>,
    /// Since the stream started; becomes the RTP timestamp.
    pub at: Duration,
}

/// Shared with the WebRTC loop.
#[derive(Default)]
pub(crate) struct Control {
    pub force_key: AtomicBool,
    /// 0 = unchanged.
    pub bitrate: AtomicU32,
    pub stop: AtomicBool,
    /// Why the thread stopped by itself, if it did.
    pub failed: std::sync::Mutex<Option<String>>,
}

/// The size sent to the browser: the display's pixels scaled down to fit `max`, even.
pub(crate) fn stream_size(pixels: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let (w, h) = (pixels.0.max(2) as f64, pixels.1.max(2) as f64);
    let scale = (max.0 as f64 / w).min(max.1 as f64 / h).min(1.0);
    let even = |v: f64| ((v * scale) as u32 / 2 * 2).max(2);
    (even(w), even(h))
}

pub(crate) struct MediaThread {
    pub control: Arc<Control>,
    handle: Option<JoinHandle<()>>,
}

impl MediaThread {
    pub(crate) fn spawn(
        host: Arc<dyn WebHost>,
        screen: WebScreen,
        config: &WebConfig,
        out: mpsc::Sender<Frame>,
    ) -> Self {
        let control = Arc::new(Control::default());
        let (w, h) = stream_size(screen.size_pixels, config.max_size);
        let capture = CaptureConfig {
            display: screen.display,
            width: w,
            height: h,
            fps: config.fps,
            show_cursor: true,
            chroma: Chroma::Yuv420,
        };
        let encode = EncoderConfig {
            codec: Codec::H264,
            chroma: Chroma::Yuv420,
            mode: Mode::Balanced,
            width: w,
            height: h,
            fps: config.fps,
            bitrate_bps: config.start_bitrate_bps,
        };
        let ctl = control.clone();
        let handle = thread::Builder::new()
            .name("web-capture-encode".into())
            .spawn(move || {
                if let Err(e) = run(host, capture, encode, &ctl, out) {
                    *ctl.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                }
            })
            .expect("spawn media thread");
        Self {
            control,
            handle: Some(handle),
        }
    }
}

impl Drop for MediaThread {
    fn drop(&mut self) {
        self.control.stop.store(true, Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(
    host: Arc<dyn WebHost>,
    capture: CaptureConfig,
    encode: EncoderConfig,
    ctl: &Control,
    out: mpsc::Sender<Frame>,
) -> Result<(), String> {
    let mut capturer = host.capturer(capture).map_err(|e| e.to_string())?;
    let mut encoder = host.encoder(encode).map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut frame_id: u32 = 0;
    let mut first = true;
    let mut buf = Vec::new();
    while !ctl.stop.load(Relaxed) {
        let frame = match capturer.next(Duration::from_millis(100)) {
            Ok(CaptureEvent::Frame(f)) => f,
            Ok(CaptureEvent::Idle) => continue,
            Ok(CaptureEvent::ModeChanged(_) | CaptureEvent::AccessLost) => {
                // Start over at the same size: the browser scales whatever arrives.
                capturer = host.capturer(capture).map_err(|e| e.to_string())?;
                encoder = host.encoder(encode).map_err(|e| e.to_string())?;
                first = true;
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        if out.capacity() == 0 {
            // The network loop is behind: skip this frame, and make the next one
            // independent of anything the browser may not have.
            ctl.force_key.store(true, Relaxed);
            continue;
        }
        let bps = ctl.bitrate.swap(0, Relaxed);
        if bps != 0 {
            let _ = encoder.set_bitrate(bps);
        }
        let opts = EncodeOpts {
            force_key: first || ctl.force_key.swap(false, Relaxed),
        };
        buf.clear();
        match encoder.encode(&frame, frame_id, opts, &mut buf) {
            Ok(_) => {
                first = false;
                frame_id = frame_id.wrapping_add(1);
                let at = frame.captured_at.saturating_duration_since(start);
                if out
                    .try_send(Frame {
                        data: buf.clone(),
                        at,
                    })
                    .is_err()
                {
                    if out.is_closed() {
                        return Ok(());
                    }
                    ctl.force_key.store(true, Relaxed);
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_fit_the_box_and_stay_even() {
        assert_eq!(stream_size((3024, 1964), (1920, 1200)), (1846, 1200));
        assert_eq!(stream_size((1280, 720), (1920, 1200)), (1280, 720));
        let (w, h) = stream_size((1921, 1081), (1920, 1200));
        assert!(w <= 1920 && h <= 1080 && w % 2 == 0 && h % 2 == 0 && w >= 1916);
        assert_eq!(stream_size((0, 0), (1920, 1200)), (2, 2));
    }
}
