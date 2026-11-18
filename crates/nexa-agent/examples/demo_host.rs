//! A host to try a viewer against (the iOS app in the simulator) without touching this
//! Mac: the real agent and session code, a synthetic picture encoded by the real
//! hardware encoder, and input that is printed and drawn instead of injected. No
//! screen capture, Keychain, pasteboard or input system is used, and every session is
//! allowed with view and input.
//!
//! Usage: `cargo run -p nexa-agent --example demo_host --release -- [addr] [--lan]
//! [--signal wss://server[:port] [--signal-ca <pem>]]` (default `127.0.0.1:47900`).
//! `--displays 2` adds a second, smaller display tinted blue, to try switching.
//! `--lan` announces it over mDNS (listen on `0.0.0.0:…` for an address to announce);
//! `--signal` registers it with a signaling server and prints its device ID. Connect
//! with the fixed password [`PASSWORD`].
//! A white square follows the injected pointer and turns green while a button is
//! held; every key, click and piece of text is printed.

#[cfg(target_os = "macos")]
fn main() {
    demo::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("demo_host needs macOS (VideoToolbox)");
}

#[cfg(target_os = "macos")]
mod demo {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
        thread,
        time::{Duration, Instant},
    };

    use nexa_agent::{AgentConfig, AgentEvent, AgentUi, Platform, ScreenInfo, start};
    use nexa_auth::{DeviceIdentity, HostPassword, PairingStore};
    use nexa_media::{
        CaptureConfig, CaptureEvent, CapturedFrame, Capturer, Encoder, EncoderConfig, NativeImage,
        macos::{Nv12Pool, VtEncoder, probe_caps},
    };
    use nexa_session::{
        clip::ClipPlatform,
        handshake::{Consent, ConsentAnswer, ConsentRequest, Timeouts},
        input::InputSink,
    };
    use nexa_wire::control::{Os, Permissions};

    /// A test value for this demo only. Letters only: the iOS Simulator's typing tool
    /// stops at a hyphen.
    pub const PASSWORD: &str = "nexademohost";
    const SIZE: (u32, u32) = (1280, 800);
    const FPS: u64 = 30;

    #[derive(Default)]
    struct Pointer {
        x: f64,
        y: f64,
        down: bool,
    }

    /// One of the demo's displays: where it sits in global points, its size, and a tint
    /// (NV12 chroma) so a switch is plain to see.
    #[derive(Clone, Copy)]
    struct Display {
        id: u32,
        origin: (f64, f64),
        size: (u32, u32),
        tint: (u8, u8),
        name: &'static str,
    }

    /// The main display, and with `--displays 2` a smaller blue one to its right.
    const DISPLAYS: [Display; 2] = [
        Display {
            id: 1,
            origin: (0.0, 0.0),
            size: SIZE,
            tint: (128, 128),
            name: "Demo",
        },
        Display {
            id: 2,
            origin: (1280.0, 0.0),
            size: (1024, 768),
            tint: (170, 100),
            name: "Demo (blue)",
        },
    ];

    #[derive(Clone, Default)]
    struct Demo {
        pointer: Arc<Mutex<Pointer>>,
        displays: usize,
    }

    fn info(d: &Display) -> ScreenInfo {
        ScreenInfo {
            display_id: d.id,
            origin_points: d.origin,
            size_points: (f64::from(d.size.0), f64::from(d.size.1)),
            size_pixels: d.size,
            refresh_hz: 60,
            name: d.name.into(),
        }
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    impl Platform for Demo {
        fn screen(&self) -> Result<ScreenInfo, String> {
            Ok(info(&DISPLAYS[0]))
        }
        fn displays(&self) -> Result<Vec<ScreenInfo>, String> {
            Ok(DISPLAYS[..self.displays.clamp(1, 2)]
                .iter()
                .map(info)
                .collect())
        }
        fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
            let display = *DISPLAYS
                .iter()
                .find(|d| d.id == config.display)
                .unwrap_or(&DISPLAYS[0]);
            println!(
                "CAPTURE display {} ({}x{})",
                display.id, display.size.0, display.size.1
            );
            Ok(Box::new(DemoCapture {
                pool: Nv12Pool::new(display.size.0, display.size.1)?,
                display,
                started: Instant::now(),
                next: Instant::now(),
                pointer: self.pointer.clone(),
            }))
        }
        fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
            VtEncoder::new(config).map(|e| Box::new(e) as Box<dyn Encoder>)
        }
        fn input_available(&self) -> bool {
            true
        }
        fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
            Ok(Box::new(DemoInput(self.pointer.clone())))
        }
        fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
            Err("no clipboard in the demo".into())
        }
    }

    struct DemoCapture {
        pool: Nv12Pool,
        display: Display,
        started: Instant,
        next: Instant,
        pointer: Arc<Mutex<Pointer>>,
    }

    impl Capturer for DemoCapture {
        fn next(&mut self, _: Duration) -> nexa_media::Result<CaptureEvent> {
            let now = Instant::now();
            if self.next > now {
                thread::sleep(self.next - now);
            }
            self.next += Duration::from_millis(1000 / FPS);
            let t = self.started.elapsed().as_secs_f64();
            // The pointer in this display's pixels (points are pixels here).
            let (px, py, down) = {
                let p = lock(&self.pointer);
                (
                    (p.x - self.display.origin.0) as i64,
                    (p.y - self.display.origin.1) as i64,
                    p.down,
                )
            };
            let (tint_cb, tint_cr) = self.display.tint;
            let size = self.display.size;
            let pb = self.pool.get()?;
            pb.with_planes_mut(|y, ys, uv, uvs| {
                let (w, h) = (size.0 as usize, size.1 as usize);
                // A slow diagonal gradient with a moving bar: something to see change.
                let bar = ((t * 200.0) as usize) % w;
                for row in 0..h {
                    for col in 0..w {
                        let mut v = (40 + (col + row) * 120 / (w + h)) as u8;
                        if col.abs_diff(bar) < 6 {
                            v = 200;
                        }
                        let near = (col as i64 - px).abs() < 14 && (row as i64 - py).abs() < 14;
                        if near {
                            v = 235;
                        }
                        y[row * ys + col] = v;
                    }
                }
                for row in 0..h / 2 {
                    for col in 0..w / 2 {
                        let near = ((col * 2) as i64 - px).abs() < 14
                            && ((row * 2) as i64 - py).abs() < 14;
                        // Green while a button is held, grey otherwise.
                        let (cb, cr) = if near && down {
                            (60u8, 60u8)
                        } else {
                            (tint_cb, tint_cr)
                        };
                        uv[row * uvs + col * 2] = cb;
                        uv[row * uvs + col * 2 + 1] = cr;
                    }
                }
            })?;
            Ok(CaptureEvent::Frame(CapturedFrame {
                image: NativeImage::PixelBuffer(pb),
                captured_at: Instant::now(),
                dirty: vec![],
            }))
        }
    }

    struct DemoInput(Arc<Mutex<Pointer>>);

    impl InputSink for DemoInput {
        fn key(&mut self, hid: u32, down: bool, _: bool, _: Instant) {
            println!(
                "KEY 0x{:02X} {}",
                hid & 0xFFFF,
                if down { "down" } else { "up" }
            );
        }
        fn move_absolute(&mut self, x: f64, y: f64) {
            let mut p = lock(&self.0);
            p.x = x;
            p.y = y;
        }
        fn move_relative(&mut self, dx: i32, dy: i32) {
            let mut p = lock(&self.0);
            p.x += f64::from(dx);
            p.y += f64::from(dy);
        }
        fn button(&mut self, button: u32, down: bool, at: Option<(f64, f64)>, _: Instant) {
            let mut p = lock(&self.0);
            if let Some((x, y)) = at {
                p.x = x;
                p.y = y;
            }
            p.down = down;
            println!(
                "BUTTON {button} {} at ({:.0}, {:.0})",
                if down { "down" } else { "up" },
                p.x,
                p.y
            );
        }
        fn wheel(&mut self, dx: i32, dy: i32) {
            println!("WHEEL {dx} {dy}");
        }
        fn text(&mut self, text: &str) {
            println!("TEXT {text:?}");
        }
        fn apply_key_state(&mut self, _: &[u32], _: Instant) {}
        fn poll_timeout(&mut self, _: Instant) {}
        fn release_all(&mut self, _: Instant) {
            lock(&self.0).down = false;
        }
    }

    struct DemoUi;

    impl Consent for DemoUi {
        fn can_present(&self) -> bool {
            true
        }
        fn ask(&self, request: &ConsentRequest, _: Duration) -> ConsentAnswer {
            println!("CONSENT allowing {:?}", request.device_name);
            ConsentAnswer {
                allow: true,
                perms: Permissions {
                    view: true,
                    input: true,
                    ..Default::default()
                },
                save: false,
                skip_next_time: false,
            }
        }
    }

    impl AgentUi for DemoUi {
        fn event(&self, event: AgentEvent) {
            match event {
                AgentEvent::Listening(addr) => println!("LISTENING {addr} password {PASSWORD}"),
                AgentEvent::CodeChanged(code) => println!("CODE {code}"),
                AgentEvent::SessionStarted(s) => println!("SESSION started: {}", s.device_name),
                AgentEvent::SessionEnded { reason, .. } => println!("SESSION ended: {reason}"),
                AgentEvent::ConnectionFailed { remote, reason } => {
                    println!("CONNECTION from {remote} failed: {reason}")
                }
                AgentEvent::Signal(s) => match (&s.state, s.short_id) {
                    (nexa_agent::SignalState::Online, Some(id)) => {
                        println!("SIGNAL online, device ID {id}")
                    }
                    (state, _) => println!("SIGNAL {state:?}"),
                },
                _ => {}
            }
        }
    }

    pub fn main() {
        let mut listen: SocketAddr = "127.0.0.1:47900".parse().expect("address");
        let (mut lan, mut signal_url, mut signal_ca) = (false, None::<String>, None::<String>);
        let mut displays = 1usize;
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--lan" => lan = true,
                "--displays" => displays = args.next().and_then(|n| n.parse().ok()).unwrap_or(1),
                "--signal" => signal_url = args.next(),
                "--signal-ca" => signal_ca = args.next(),
                addr => listen = addr.parse().expect("an address such as 127.0.0.1:47900"),
            }
        }
        let signal = signal_url.map(|url| {
            let mut server = nexa_session::rendezvous::signal::SignalConfig::new(url);
            if let Some(ca) = signal_ca {
                server = server
                    .with_ca_file(std::path::Path::new(&ca))
                    .expect("signal CA");
            }
            nexa_agent::SignalSetup {
                server,
                bind_ip: if listen.ip().is_unspecified() {
                    std::net::Ipv6Addr::UNSPECIFIED.into()
                } else {
                    listen.ip()
                },
                interfaces: None,
                paired_only: false,
                relay_only: false,
                // No port mapping on the real router for a demo.
                system_services: false,
            }
        });
        let dir = std::env::temp_dir().join(format!("nexa-demo-host-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temporary directory");
        let identity = DeviceIdentity::generate();
        let store = PairingStore::open(dir.join("pairings.json"), identity.store_mac_key())
            .expect("pairing store");
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let agent = start(
                AgentConfig {
                    listen,
                    device_name: "NexaDesk demo host".into(),
                    os: Os::Macos,
                    caps: probe_caps(),
                    timeouts: Timeouts::default(),
                    signal,
                    files: nexa_agent::FileSettings {
                        downloads: dir.join("Downloads"),
                        limits: Default::default(),
                    },
                    lan_discovery: lan,
                    web: None,
                    web_tls: None,
                    password: Some(Arc::new(HostPassword::create(PASSWORD).expect("password"))),
                },
                identity,
                store,
                Arc::new(Demo {
                    displays,
                    ..Demo::default()
                }),
                Arc::new(DemoUi),
            )
            .await
            .expect("agent");
            println!("READY {}", agent.local_addr());
            tokio::signal::ctrl_c().await.ok();
        });
    }
}
