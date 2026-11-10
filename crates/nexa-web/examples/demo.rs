//! Browser access without a real screen or real input, for trying the page and the
//! WebRTC path anywhere: a synthetic 1280×800 screen through the real H.264 encoder.
//! The pointer, buttons, keys and wheel the browser sends are drawn into the picture, so
//! the round trip is visible. Every consent request is accepted with input.
//!
//!   cargo run -p nexa-web --example demo [-- <addr:port>]     (default 0.0.0.0:47801)
//!
//! Open the printed URL from another device, or from this one by its LAN address
//! (browsers ignore 127.0.0.1 as a WebRTC address), and type the printed code.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the demo uses the macOS H.264 encoder");
}

#[cfg(target_os = "macos")]
fn main() {
    demo::main();
}

#[cfg(target_os = "macos")]
mod demo {
    use std::{
        collections::BTreeSet,
        net::SocketAddr,
        sync::{Arc, Mutex},
        thread,
        time::{Duration, Instant},
    };

    use nexa_auth::OneTimeCode;
    use nexa_media::{
        CaptureConfig, CaptureEvent, CapturedFrame, Capturer, CpuFrame, Encoder, EncoderConfig,
        NativeImage, macos::VtEncoder,
    };
    use nexa_session::input::InputSink;
    use nexa_web::{
        BrowserAnswer, BrowserRequest, WebConfig, WebEvent, WebGateway, WebHost, WebScreen,
    };

    const W: u32 = 1280;
    const H: u32 = 800;

    #[derive(Default)]
    struct Seen {
        pointer: (f64, f64),
        buttons: BTreeSet<u32>,
        keys: BTreeSet<u32>,
        scroll: i64,
        text: usize,
    }

    struct Demo(Arc<Mutex<Seen>>);

    impl WebHost for Demo {
        fn screen(&self) -> Result<WebScreen, String> {
            Ok(WebScreen {
                display: 1,
                origin_points: (0.0, 0.0),
                size_points: (W as f64, H as f64),
                size_pixels: (W, H),
            })
        }
        fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
            Ok(Box::new(Synthetic {
                seen: self.0.clone(),
                width: config.width,
                height: config.height,
                interval: Duration::from_secs_f64(1.0 / f64::from(config.fps.max(1))),
                next: Instant::now(),
                t: 0,
            }))
        }
        fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
            Ok(Box::new(VtEncoder::new(config)?))
        }
        fn input_available(&self) -> bool {
            true
        }
        fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
            Ok(Box::new(Sink(self.0.clone())))
        }
        fn ask(&self, r: &BrowserRequest, _timeout: Duration) -> BrowserAnswer {
            println!("consent: {} from {} — accepted", r.browser, r.remote);
            BrowserAnswer {
                allow: true,
                input: true,
            }
        }
        fn event(&self, e: WebEvent) {
            println!("event: {e:?}");
        }
    }

    struct Sink(Arc<Mutex<Seen>>);

    impl InputSink for Sink {
        fn key(&mut self, hid: u32, down: bool, repeat: bool, _: Instant) {
            println!(
                "key {hid:#x} {} {}",
                if down { "down" } else { "up" },
                if repeat { "repeat" } else { "" }
            );
            let mut s = self.0.lock().unwrap();
            if down {
                s.keys.insert(hid);
            } else {
                s.keys.remove(&hid);
            }
        }
        fn move_absolute(&mut self, x: f64, y: f64) {
            self.0.lock().unwrap().pointer = (x, y);
        }
        fn move_relative(&mut self, _: i32, _: i32) {}
        fn button(&mut self, button: u32, down: bool, at: Option<(f64, f64)>, _: Instant) {
            println!(
                "button {button} {} at {at:?}",
                if down { "down" } else { "up" }
            );
            let mut s = self.0.lock().unwrap();
            if let Some(p) = at {
                s.pointer = p;
            }
            if down {
                s.buttons.insert(button);
            } else {
                s.buttons.remove(&button);
            }
        }
        fn wheel(&mut self, dx: i32, dy: i32) {
            println!("wheel {dx} {dy}");
            self.0.lock().unwrap().scroll += i64::from(dy);
        }
        fn text(&mut self, text: &str) {
            println!("text {text:?}");
            self.0.lock().unwrap().text += text.chars().count();
        }
        fn apply_key_state(&mut self, _: &[u32], _: Instant) {}
        fn poll_timeout(&mut self, _: Instant) {}
        fn release_all(&mut self, _: Instant) {
            println!("release all");
            let mut s = self.0.lock().unwrap();
            s.keys.clear();
            s.buttons.clear();
        }
    }

    /// NV12 frames: a scrolling grid (the wheel moves it), a sweeping bar, a block per
    /// held key, and the pointer as a square that turns solid while a button is down.
    struct Synthetic {
        seen: Arc<Mutex<Seen>>,
        width: u32,
        height: u32,
        interval: Duration,
        next: Instant,
        t: u32,
    }

    impl Capturer for Synthetic {
        fn next(&mut self, _timeout: Duration) -> nexa_media::Result<CaptureEvent> {
            let now = Instant::now();
            if self.next > now {
                thread::sleep(self.next - now);
            }
            self.next += self.interval;
            self.t += 1;
            let (w, h) = (self.width as usize, self.height as usize);
            let seen = self.seen.lock().unwrap();
            let scroll = (seen.scroll / 4) as i64;
            let mut y = vec![0u8; w * h];
            for row in 0..h {
                let gy = (row as i64 - scroll).rem_euclid(64);
                for col in 0..w {
                    let grid = gy < 2 || col % 64 < 2;
                    y[row * w + col] = if grid { 90 } else { 40 + (col * 60 / w) as u8 };
                }
            }
            let bar = (self.t as usize * 6) % w;
            for row in 0..h {
                for col in bar..(bar + 8).min(w) {
                    y[row * w + col] = 200;
                }
            }
            for (i, _) in seen.keys.iter().enumerate() {
                fill(&mut y, w, h, 20 + i * 28, 20, 22, 22, 235);
            }
            let (px, py) = (
                (seen.pointer.0 / f64::from(W) * w as f64) as usize,
                (seen.pointer.1 / f64::from(H) * h as f64) as usize,
            );
            if seen.buttons.is_empty() {
                outline(
                    &mut y,
                    w,
                    h,
                    px.saturating_sub(10),
                    py.saturating_sub(10),
                    20,
                    255,
                );
            } else {
                fill(
                    &mut y,
                    w,
                    h,
                    px.saturating_sub(14),
                    py.saturating_sub(14),
                    28,
                    28,
                    255,
                );
            }
            drop(seen);
            // Neutral chroma: a grey picture is enough to see everything above.
            let uv = vec![128u8; w * h / 2];
            let frame = CpuFrame {
                width: self.width,
                height: self.height,
                y,
                uv,
            };
            Ok(CaptureEvent::Frame(CapturedFrame {
                image: NativeImage::Cpu(Arc::new(frame)),
                captured_at: Instant::now(),
                dirty: Vec::new(),
            }))
        }
    }

    fn fill(y: &mut [u8], w: usize, h: usize, x0: usize, y0: usize, fw: usize, fh: usize, v: u8) {
        for row in y0..(y0 + fh).min(h) {
            for col in x0..(x0 + fw).min(w) {
                y[row * w + col] = v;
            }
        }
    }

    fn outline(y: &mut [u8], w: usize, h: usize, x0: usize, y0: usize, s: usize, v: u8) {
        fill(y, w, h, x0, y0, s, 2, v);
        fill(y, w, h, x0, y0 + s - 2, s, 2, v);
        fill(y, w, h, x0, y0, 2, s, v);
        fill(y, w, h, x0 + s - 2, y0, 2, s, v);
    }

    #[tokio::main]
    pub async fn main() {
        let addr: SocketAddr = std::env::args()
            .nth(1)
            .unwrap_or_else(|| format!("0.0.0.0:{}", nexa_web::DEFAULT_PORT))
            .parse()
            .expect("address:port");
        let code = OneTimeCode::generate();
        let shown = code.current();
        let host = Arc::new(Demo(Arc::new(Mutex::new(Seen::default()))));
        let gateway = WebGateway::bind(addr, host, code.clone(), WebConfig::default())
            .await
            .expect("bind");
        println!("listening on http://{}", gateway.local_addr());
        println!("one-time code: {}-{}", &shown[..4], &shown[4..]);
        let mut last = shown.to_string();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let now = code.current();
            if *now != last {
                println!("one-time code: {}-{}", &now[..4], &now[4..]);
                last = now.to_string();
            }
        }
    }
}
