//! A host to try a viewer against (the iOS app in the simulator) without touching this
//! Mac: the real agent and session code, a synthetic picture encoded by the real
//! hardware encoder, and input that is printed and drawn instead of injected. No
//! screen capture, Keychain, pasteboard or input system is used, and every session is
//! allowed with view and input.
//!
//! Usage: `cargo run -p nexa-agent --example demo_host --release -- [addr] [--lan]
//! [--signal wss://server[:port] [--signal-ca <pem>]]` (default `127.0.0.1:47900`).
//! `--displays 2` adds a second, smaller display tinted blue, to try switching.
//! `--remember` keeps each viewer's pairing, so it connects again with no code.
//! `--agents` lets AI agents (`nexa-mcp`) connect; without it they are refused. An
//! agent is shown a made-up window (docs/MCP.md §13): a "Click me" button that counts
//! its clicks, a "Name" field that takes what is typed once clicked (Backspace, and
//! Command or Control + A to select it all), and the count as text; its screenshots
//! draw them, with the count as a bar and each character as a block.
//! `--grant-all` allows files and the command line too (a real shell on this Mac,
//! run as you), to try file transfer and `nexa_run`; files sent to it land in the
//! folder its `DOWNLOADS` line names.
//! `--lan` announces it over mDNS (listen on `0.0.0.0:…` for an address to announce);
//! `--signal` registers it with a signaling server and prints its device ID; the
//! server's access password comes from `NEXA_SIGNAL_SECRET`, as for the real agent.
//! Connect with the fixed password [`PASSWORD`].
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

    use nexa_agent::{
        AgentConfig, AgentEvent, AgentUi, Platform, ScreenInfo,
        agent_view::{
            Act, AgentView, Element, Frontmost, PointRect, Rgba, UiNode, UiOptions, UiTree,
            fingerprint,
        },
        start,
    };
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
        form: Arc<Mutex<Form>>,
        displays: usize,
    }

    /// The made-up window an agent is shown, on the main display, in points.
    #[derive(Default)]
    struct Form {
        clicks: u32,
        name: String,
        field_focused: bool,
        /// Its text is selected (Command or Control + A): typing replaces it, Backspace
        /// clears it.
        selected: bool,
        /// Command or Control is held.
        modifier: bool,
    }

    /// The demo window's elements an agent can act on without the pointer.
    #[derive(Debug)]
    enum DemoElement {
        Button,
        Field,
    }

    const BUTTON: (f64, f64, f64, f64) = (100.0, 100.0, 160.0, 40.0);
    const FIELD: (f64, f64, f64, f64) = (100.0, 180.0, 320.0, 32.0);

    fn inside(r: (f64, f64, f64, f64), x: f64, y: f64) -> bool {
        x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
    }

    fn rect(r: (f64, f64, f64, f64)) -> PointRect {
        PointRect {
            x: r.0,
            y: r.1,
            w: r.2,
            h: r.3,
        }
    }

    impl AgentView for Demo {
        fn screenshot(&self, screen: &ScreenInfo, size: (u32, u32)) -> Result<Rgba, String> {
            let (px, py) = {
                let p = lock(&self.pointer);
                (p.x - screen.origin_points.0, p.y - screen.origin_points.1)
            };
            let main = screen.display_id == DISPLAYS[0].id;
            // The form's state is drawn too, so an agent's looks see it change: the
            // field turns pale yellow while it has focus, each click adds to a green bar
            // under the button, and each typed character is a dark block in the field.
            let (clicks, typed, focused, selected) = {
                let form = lock(&self.form);
                (
                    form.clicks,
                    form.name.chars().count(),
                    form.field_focused,
                    form.selected,
                )
            };
            let bar = (100.0, 240.0, (f64::from(clicks) * 80.0).min(1000.0), 40.0);
            let (sw, sh) = screen.size_points;
            let (w, h) = (size.0.max(1) as usize, size.1.max(1) as usize);
            let mut data = vec![0u8; w * h * 4];
            for row in 0..h {
                for col in 0..w {
                    // Back to points, to draw what is there.
                    let x = (col as f64 + 0.5) * sw / w as f64;
                    let y = (row as f64 + 0.5) * sh / h as f64;
                    let mut c = (40.0 + (x + y) * 120.0 / (sw + sh)) as u8;
                    let mut rgb = [c, c, c];
                    if main && inside(BUTTON, x, y) {
                        rgb = [30, 90, 200];
                    } else if main && inside(FIELD, x, y) {
                        // Character blocks 9×20 points, 12 apart, from (6, 6) in the field.
                        let (cx, cy) = (x - FIELD.0 - 6.0, y - FIELD.1 - 6.0);
                        rgb = if cx >= 0.0
                            && ((cx / 12.0) as usize) < typed
                            && cx % 12.0 < 9.0
                            && (0.0..20.0).contains(&cy)
                        {
                            if selected {
                                [30, 90, 200]
                            } else {
                                [20, 20, 20]
                            }
                        } else if focused {
                            [255, 245, 190]
                        } else {
                            [250, 250, 250]
                        };
                    } else if main && inside(bar, x, y) {
                        rgb = [40, 170, 80];
                    }
                    if (x - px).abs() < 14.0 && (y - py).abs() < 14.0 {
                        c = 235;
                        rgb = [c, c, c];
                    }
                    let i = (row * w + col) * 4;
                    data[i..i + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                }
            }
            Ok(Rgba {
                width: w as u32,
                height: h as u32,
                data,
            })
        }

        fn frontmost(&self) -> Frontmost {
            let form = lock(&self.form);
            Frontmost {
                app: "NexaDesk demo".into(),
                window: "Demo".into(),
                focused: if form.field_focused {
                    "textfield \"Name\"".into()
                } else {
                    String::new()
                },
                focused_state: if form.field_focused {
                    fingerprint(&form.name)
                } else {
                    0
                },
            }
        }

        fn act(&self, element: &Element, act: &Act) -> Result<(), String> {
            let which = element
                .0
                .downcast_ref::<DemoElement>()
                .ok_or("not the demo's")?;
            let mut form = lock(&self.form);
            match (which, act) {
                (DemoElement::Button, Act::Press) => form.clicks += 1,
                (DemoElement::Field, Act::Focus | Act::Press) => form.field_focused = true,
                (DemoElement::Field, Act::SetValue(text)) => {
                    form.name = text.clone();
                    form.selected = false;
                }
                _ => return Err("this element cannot do that: click it instead".into()),
            }
            println!("ACT {which:?} {act:?}");
            Ok(())
        }

        fn ui(&self, _: &UiOptions) -> Result<UiTree, String> {
            let form = lock(&self.form);
            let d = DISPLAYS[0];
            Ok(vec![
                UiNode {
                    depth: 0,
                    role: "window".into(),
                    name: "Demo".into(),
                    bounds: Some(rect((0.0, 0.0, f64::from(d.size.0), f64::from(d.size.1)))),
                    ..UiNode::default()
                },
                UiNode {
                    depth: 1,
                    role: "button".into(),
                    name: "Click me".into(),
                    bounds: Some(rect(BUTTON)),
                    interactive: true,
                    element: Some(Element(Arc::new(DemoElement::Button))),
                    ..UiNode::default()
                },
                UiNode {
                    depth: 1,
                    role: "textfield".into(),
                    name: "Name".into(),
                    value: Some(form.name.clone()),
                    states: if form.field_focused {
                        vec!["focused"]
                    } else {
                        vec![]
                    },
                    bounds: Some(rect(FIELD)),
                    interactive: true,
                    element: Some(Element(Arc::new(DemoElement::Field))),
                },
                UiNode {
                    depth: 1,
                    role: "text".into(),
                    name: format!("Clicked {} times", form.clicks),
                    bounds: Some(rect((100.0, 240.0, 200.0, 20.0))),
                    ..UiNode::default()
                },
            ]
            .into())
        }

        fn pointer(&self) -> Option<(f64, f64)> {
            let p = lock(&self.pointer);
            Some((p.x, p.y))
        }
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
            Ok(Box::new(DemoInput(self.pointer.clone(), self.form.clone())))
        }
        fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
            Err("no clipboard in the demo".into())
        }
        fn agent_view(&self) -> Option<Arc<dyn AgentView>> {
            Some(Arc::new(self.clone()))
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

    struct DemoInput(Arc<Mutex<Pointer>>, Arc<Mutex<Form>>);

    impl InputSink for DemoInput {
        fn key(&mut self, hid: u32, down: bool, _: bool, _: Instant) {
            let mut form = lock(&self.1);
            match hid & 0xFFFF {
                // Control and Command (GUI), left and right.
                0xE0 | 0xE3 | 0xE4 | 0xE7 => form.modifier = down,
                // A.
                0x04 if down && form.modifier && form.field_focused => form.selected = true,
                // Backspace, Delete.
                0x2A | 0x4C if down && form.field_focused => {
                    if form.selected {
                        form.name.clear();
                    } else if hid & 0xFFFF == 0x2A {
                        form.name.pop();
                    }
                    form.selected = false;
                }
                _ => {}
            }
            drop(form);
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
            if !down && button == 0 {
                let mut form = lock(&self.1);
                if inside(BUTTON, p.x, p.y) {
                    form.clicks += 1;
                }
                form.field_focused = inside(FIELD, p.x, p.y);
                form.selected = false;
            }
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
            let mut form = lock(&self.1);
            if form.field_focused {
                if form.selected {
                    form.name.clear();
                    form.selected = false;
                }
                form.name.push_str(text);
            }
            println!("TEXT {text:?}");
        }
        fn apply_key_state(&mut self, _: &[u32], _: Instant) {}
        fn poll_timeout(&mut self, _: Instant) {}
        fn release_all(&mut self, _: Instant) {
            lock(&self.0).down = false;
        }
    }

    /// `remember`: keep each viewer's pairing, as when the person at the computer ticks
    /// "remember this device", so the viewer connects again with no code.
    struct DemoUi {
        remember: bool,
        /// Files and the command line as well as view and input.
        grant_all: bool,
    }

    impl Consent for DemoUi {
        fn can_present(&self) -> bool {
            true
        }
        fn ask(&self, request: &ConsentRequest, _: Duration) -> ConsentAnswer {
            println!(
                "CONSENT allowing {:?}{}",
                request.device_name,
                if request.agent { " (an AI agent)" } else { "" }
            );
            ConsentAnswer {
                allow: true,
                perms: Permissions {
                    view: true,
                    input: true,
                    file: self.grant_all,
                    terminal: self.grant_all,
                    ..Default::default()
                },
                save: self.remember,
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
        let mut remember = false;
        let mut agents = false;
        let mut grant_all = false;
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--lan" => lan = true,
                "--remember" => remember = true,
                "--agents" => agents = true,
                "--grant-all" => grant_all = true,
                "--displays" => displays = args.next().and_then(|n| n.parse().ok()).unwrap_or(1),
                "--signal" => signal_url = args.next(),
                "--signal-ca" => signal_ca = args.next(),
                addr => listen = addr.parse().expect("an address such as 127.0.0.1:47900"),
            }
        }
        let signal = signal_url.map(|url| {
            let secret = std::env::var("NEXA_SIGNAL_SECRET").unwrap_or_default();
            let mut server = nexa_session::rendezvous::signal::SignalConfig::new(url)
                .with_access_secret(&secret);
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
        println!("DOWNLOADS {}", dir.join("Downloads").display());
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
                    code_asks: false,
                },
                identity,
                store,
                Arc::new(Demo {
                    displays,
                    ..Demo::default()
                }),
                Arc::new(DemoUi {
                    remember,
                    grant_all,
                }),
            )
            .await
            .expect("agent");
            agent.set_allow_agents(agents);
            println!("READY {}", agent.local_addr());
            tokio::signal::ctrl_c().await.ok();
        });
    }
}
