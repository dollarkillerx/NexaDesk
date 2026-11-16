//! Browser access without a real screen or real input, for trying the page and the
//! WebRTC path anywhere: a synthetic 1280×800 screen through the real H.264 encoder.
//! The pointer, buttons, keys and wheel the browser sends are drawn into the picture, so
//! the round trip is visible. Every consent request is accepted with input.
//!
//!   cargo run -p nexa-web --example demo [-- <addr:port> [<signal url>]]
//!
//! The address defaults to 0.0.0.0:47801. With a signaling server URL (e.g.
//! ws://127.0.0.1:8443, or wss://… with the server's certificate trusted), the demo also
//! registers there under a throwaway identity and takes browsers the server puts
//! through, by the device ID it prints: the across-the-internet path, TURN included.
//! `NEXA_SIGNAL_CA=<pem>` trusts a server certificate from a CA of your own.
//! `NEXA_DEMO_PASSWORD=<password>` lets browsers type that instead of the code.
//!
//! Open the printed URL from another device, or from this one by its LAN address
//! (browsers ignore 127.0.0.1 as a WebRTC address), accept the self-signed certificate,
//! and type the printed code.

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

    /// `NEXA_DEMO_SIZE=3072x1920`: the screen's pixels, for trying large pictures.
    fn pixels() -> (u32, u32) {
        std::env::var("NEXA_DEMO_SIZE")
            .ok()
            .and_then(|v| {
                let (w, h) = v.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((W, H))
    }

    #[derive(Default)]
    struct Seen {
        pointer: (f64, f64),
        buttons: BTreeSet<u32>,
        keys: BTreeSet<u32>,
        scroll: i64,
        text: usize,
        /// What the browser put on the demo's clipboard, and text the demo "copied".
        clipboard: String,
        copied: Option<String>,
        /// A key was typed: what a Mac does to its cursor, see DemoCursor.
        typed: bool,
    }

    struct Demo(Arc<Mutex<Seen>>);

    impl WebHost for Demo {
        fn screen(&self) -> Result<WebScreen, String> {
            Ok(WebScreen {
                display: 1,
                origin_points: (0.0, 0.0),
                size_points: (W as f64, H as f64),
                size_pixels: pixels(),
            })
        }
        fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
            println!(
                "capture {}x{} @ {} fps",
                config.width, config.height, config.fps
            );
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
        /// An arrow where the browser last put the pointer, as a real host's cursor.
        fn cursor_source(&self) -> Option<Box<dyn nexa_media::CursorSource>> {
            Some(Box::new(DemoCursor {
                seen: self.0.clone(),
                image: Arc::new(arrow()),
            }))
        }
        fn input_available(&self) -> bool {
            true
        }
        fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
            Ok(Box::new(Sink(self.0.clone())))
        }
        fn clipboard(&self) -> Result<Box<dyn nexa_web::WebClipboard>, String> {
            Ok(Box::new(Clip(self.0.clone())))
        }
        fn ask(&self, r: &BrowserRequest, _timeout: Duration) -> BrowserAnswer {
            println!("consent: {} from {} — accepted", r.browser, r.remote);
            BrowserAnswer {
                allow: true,
                input: true,
                clipboard: true,
                terminal: true,
            }
        }
        fn terminal(&self, cols: u16, rows: u16) -> Result<nexa_term::Pty, String> {
            println!("terminal opened {cols}x{rows}");
            // NEXA_DEMO_SHELL: another shell, e.g. a plain /bin/sh for screenshots.
            nexa_term::Pty::spawn(&nexa_term::Spawn {
                cols,
                rows,
                shell: std::env::var_os("NEXA_DEMO_SHELL").map(Into::into),
                ..Default::default()
            })
            .map_err(|e| e.to_string())
        }
        fn event(&self, e: WebEvent) {
            println!("event: {e:?}");
        }
    }

    struct Sink(Arc<Mutex<Seen>>);

    /// F9 in the browser "copies" a line on the demo host, to try the way back.
    struct Clip(Arc<Mutex<Seen>>);

    impl nexa_web::WebClipboard for Clip {
        fn poll(&mut self) -> Option<String> {
            self.0.lock().unwrap().copied.take()
        }
        fn set_text(&mut self, text: &str) -> Result<(), String> {
            println!("clipboard from the browser: {} chars", text.chars().count());
            self.0.lock().unwrap().clipboard = text.to_string();
            Ok(())
        }
    }

    impl InputSink for Sink {
        fn key(&mut self, hid: u32, down: bool, repeat: bool, _: Instant) {
            println!(
                "key {hid:#x} {} {}",
                if down { "down" } else { "up" },
                if repeat { "repeat" } else { "" }
            );
            let mut s = self.0.lock().unwrap();
            if down && hid == (0x07 << 16) | 0x42 {
                let n = s.clipboard.chars().count();
                s.copied = Some(format!(
                    "Copied on the demo host; it last received {n} characters."
                ));
            }
            if down {
                s.keys.insert(hid);
                s.typed = true;
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

    struct DemoCursor {
        seen: Arc<Mutex<Seen>>,
        image: Arc<nexa_media::CursorImage>,
    }

    impl nexa_media::CursorSource for DemoCursor {
        fn sample(&mut self) -> nexa_media::CursorSample {
            let seen = self.seen.lock().unwrap();
            nexa_media::CursorSample {
                image: Some(self.image.clone()),
                // NEXA_DEMO_HIDE_ON_TYPE: hidden from the first key on, for good, as a
                // Mac reports it after typing even while injected moves come in.
                visible: !(seen.typed && std::env::var_os("NEXA_DEMO_HIDE_ON_TYPE").is_some()),
                position: Some(seen.pointer),
            }
        }
    }

    /// A 2x arrow, 12 by 18 points: white with a black edge, hot spot at its tip.
    fn arrow() -> nexa_media::CursorImage {
        let (w, h) = (24u32, 36u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let inside = |x: i32, y: i32| {
            (0..32).contains(&y) && x >= 0 && x <= y * 2 / 3 && !(y > 24 && x > 32 - y)
        };
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let i = ((y as u32 * w + x as u32) * 4) as usize;
                if inside(x, y) {
                    let edge = !(inside(x - 2, y)
                        && inside(x + 2, y)
                        && inside(x, y - 2)
                        && inside(x, y + 2));
                    let c = if edge { 0 } else { 255 };
                    rgba[i..i + 4].copy_from_slice(&[c, c, c, 255]);
                }
            }
        }
        nexa_media::CursorImage {
            width: w,
            height: h,
            rgba,
            hot_x: 0.0,
            hot_y: 0.0,
            scale: 2.0,
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
            let scroll = seen.scroll / 4;
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

    #[allow(clippy::too_many_arguments)]
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

    /// What the signaling task needs of the gateway, which stays in `main`.
    type Remote = Arc<
        dyn Fn(
                nexa_web::RemoteBrowser,
                tokio::sync::mpsc::Receiver<String>,
                tokio::sync::mpsc::Sender<String>,
            ) + Send
            + Sync,
    >;

    fn gateway_handle(gateway: &Arc<WebGateway>) -> Remote {
        let g = gateway.clone();
        Arc::new(move |b, rx, tx| {
            g.serve_remote(b, rx, tx);
        })
    }

    /// Registers with a signaling server and serves the browsers it puts through, as the
    /// agent does (nexa-agent's signal.rs), with an identity that lives in memory only.
    async fn through_server(url: String, serve: Remote) {
        use std::collections::HashMap;

        use nexa_session::rendezvous::{
            seal::SignalKeys,
            signal::{SignalClient, SignalConfig},
        };
        use nexa_wire::signal::{BrowserClose, BrowserText, ClientBody, ServerBody};

        let keys = SignalKeys::new(Arc::new(nexa_auth::DeviceIdentity::generate()));
        // A server with a certificate of its own CA: NEXA_SIGNAL_CA names the CA file.
        let mut config = SignalConfig::new(url.clone());
        if let Some(ca) = std::env::var_os("NEXA_SIGNAL_CA") {
            config = match config.with_ca_file(std::path::Path::new(&ca)) {
                Ok(c) => c,
                Err(e) => return eprintln!("NEXA_SIGNAL_CA: {e}"),
            };
        }
        let mut client = match SignalClient::connect(&config, &keys).await {
            Ok(c) => c,
            Err(e) => return eprintln!("signaling server {url}: {e}"),
        };
        println!("device ID through {url}: {}", client.registration.short_id);
        let mut browsers: HashMap<u64, tokio::sync::mpsc::Sender<String>> = HashMap::new();
        while let Some(body) = client.recv().await {
            match body {
                ServerBody::BrowserOpen(o) => {
                    println!("browser through the server from {}", o.ip);
                    let (in_tx, in_rx) = tokio::sync::mpsc::channel(32);
                    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(32);
                    let browser = nexa_web::RemoteBrowser {
                        ip: o.ip.parse().unwrap_or([0, 0, 0, 0].into()),
                        turn_server: o.turn_server.parse().expect("TURN address"),
                        turn: nexa_web::TurnCredentials {
                            username: o.turn_username,
                            password: o.turn_password,
                        },
                    };
                    serve(browser, in_rx, out_tx);
                    browsers.insert(o.conn, in_tx);
                    let (sender, conn) = (client.sender(), o.conn);
                    tokio::spawn(async move {
                        while let Some(text) = out_rx.recv().await {
                            let _ =
                                sender.send(ClientBody::BrowserText(BrowserText { conn, text }));
                        }
                        let _ = sender.send(ClientBody::BrowserClose(BrowserClose { conn }));
                    });
                }
                ServerBody::BrowserText(t) => {
                    if let Some(tx) = browsers.get(&t.conn) {
                        let _ = tx.try_send(t.text);
                    }
                }
                ServerBody::BrowserClose(c) => {
                    browsers.remove(&c.conn);
                }
                _ => {}
            }
        }
        eprintln!("signaling server closed the connection");
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
        // HTTPS with a certificate the demo makes for itself, kept next to other temp files.
        let dir = std::env::temp_dir().join("nexa-web-demo");
        let tls = nexa_web::WebTls::load_or_create(&dir, &nexa_web::certificate_names())
            .expect("certificate");
        println!("certificate SHA-256: {}", tls.fingerprint_text());
        // A made-up name, as the agent would send its device name.
        let config = WebConfig {
            name: std::env::var("NEXA_DEMO_NAME").unwrap_or_else(|_| "Studio Mac".into()),
            ..WebConfig::default()
        };
        let gateway = WebGateway::bind(addr, host, code.clone(), config, Some(tls))
            .await
            .expect("bind");
        println!("listening on https://{}", gateway.local_addr());
        let gateway = Arc::new(gateway);
        // A fixed password as well as the code, to try password sign-in.
        if let Ok(pw) = std::env::var("NEXA_DEMO_PASSWORD") {
            let stored = nexa_auth::HostPassword::create(&pw).expect("password");
            gateway.set_password(Some((
                Arc::new(stored),
                nexa_auth::PasswordGuard::default(),
            )));
            println!("fixed password set");
        }
        println!("one-time code: {}-{}", &shown[..4], &shown[4..]);
        if let Some(url) = std::env::args().nth(2) {
            tokio::spawn(through_server(url, gateway_handle(&gateway)));
        }
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
