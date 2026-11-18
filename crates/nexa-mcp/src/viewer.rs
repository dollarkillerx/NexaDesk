//! The viewer behind the tools: one `nexa_client::Core` without windows, its sessions,
//! and the hosts it has paired with (docs/MCP.md §3).
//!
//! Every call blocks until it is done (the MCP server handles one request at a time);
//! the core's own threads keep the sessions running in between.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering::Relaxed},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use nexa_client::{
    client::{Core, EventTx, Session},
    deps::Deps,
    memclip::MemoryClipboard,
    proto::{
        ClipboardCode, Command, ConnectAuth, ConnectRequest, Connected, DisconnectReason,
        Disconnected, DisplayInfo, Event, EventBody, KeyPress, PointerButton, PointerMove,
        PointerWheel, SelectDisplay, SendText, SessionCommand, StreamChanged,
    },
    snapshot::{self, Frames},
};
use nexa_wire::control::Mode;
use prost::Message;

use crate::{
    image::{self, MAX_SIDE},
    terminal::{Ran, Shell},
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long things may take.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Until the host answers, which includes the person there deciding.
    pub connect: Duration,
    /// For the first picture of a session.
    pub first_frame: Duration,
    /// After an action, how long the picture must stay unchanged to count as settled.
    pub quiet: Duration,
    /// After an action, the longest wait for it to settle.
    pub settle_max: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(90),
            first_frame: Duration::from_secs(10),
            quiet: Duration::from_millis(500),
            settle_max: Duration::from_secs(3),
        }
    }
}

/// Frame rate asked of hosts: an agent looks now and then, it does not watch.
const FPS: u32 = 10;
/// Text sent in one `nexa_type`.
pub const MAX_TEXT: usize = 4000;
/// Sessions at once.
pub const MAX_SESSIONS: usize = 4;

#[derive(Default)]
struct TagState {
    connected: Option<Connected>,
    ended: Option<Disconnected>,
    /// The stream as the host last configured it: which display, what size.
    stream: Option<StreamChanged>,
    /// Copies the host confirmed it has on its clipboard (`CLIPBOARD_SENT`).
    clips_sent: u64,
}

#[derive(Default)]
struct Events {
    tags: Mutex<HashMap<u64, TagState>>,
    changed: Condvar,
}

impl Events {
    fn on(&self, event: Event) {
        let Some(body) = event.body else { return };
        let mut tags = lock(&self.tags);
        let state = tags.entry(event.tag).or_default();
        match body {
            EventBody::Connected(c) => state.connected = Some(c),
            EventBody::Disconnected(d) => state.ended = Some(d),
            EventBody::Stream(s) => state.stream = Some(s),
            EventBody::Clipboard(n) if n.code == ClipboardCode::Sent as i32 => {
                state.clips_sent += 1
            }
            _ => return,
        }
        drop(tags);
        self.changed.notify_all();
    }

    /// Waits until `f` gives something for `tag`, or `timeout`.
    fn wait<T>(
        &self,
        tag: u64,
        timeout: Duration,
        f: impl Fn(&TagState) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut tags = lock(&self.tags);
        loop {
            if let Some(v) = tags.get(&tag).and_then(&f) {
                return Some(v);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            tags = self
                .changed
                .wait_timeout(tags, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn ended(&self, tag: u64) -> Option<Disconnected> {
        lock(&self.tags).get(&tag).and_then(|s| s.ended.clone())
    }

    fn stream(&self, tag: u64) -> Option<StreamChanged> {
        lock(&self.tags).get(&tag).and_then(|s| s.stream.clone())
    }

    fn clips_sent(&self, tag: u64) -> u64 {
        lock(&self.tags).get(&tag).map_or(0, |s| s.clips_sent)
    }
}

/// What a session was asked to show, and the screenshot coordinates refer to.
pub struct Remote {
    pub id: u64,
    pub address: String,
    pub host: Connected,
    session: Arc<Session>,
    frames: Arc<Frames>,
    /// The size of the last screenshot, whose pixels actions are given in.
    shot: Mutex<Option<(u32, u32)>>,
    /// The host's command line, once a command was run.
    shell: Mutex<Option<Shell>>,
}

impl Remote {
    pub fn perms(&self) -> nexa_client::proto::Permissions {
        self.host.perms.clone().unwrap_or_default()
    }

    pub fn os_name(&self) -> &'static str {
        match self.host.host_os {
            1 => "Windows",
            2 => "macOS",
            3 => "Linux",
            _ => "an unknown system",
        }
    }
}

/// A screenshot, encoded.
pub struct Shot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// False when the screen was still changing when it was taken.
    pub settled: bool,
    /// How long it waited for the screen to settle (zero for a plain screenshot).
    pub waited: Duration,
    /// The previous screenshot's size, when this one's differs: older coordinates
    /// no longer fit.
    pub resized_from: Option<(u32, u32)>,
}

pub enum Button {
    Left,
    Right,
    Middle,
}

pub struct Viewer {
    core: Arc<Core>,
    events: Arc<Events>,
    /// The clipboard every session shares, in memory (`HeadlessDeps` hands it out).
    clipboard: Arc<MemoryClipboard>,
    data_dir: PathBuf,
    remotes: Mutex<BTreeMap<u64, Arc<Remote>>>,
    next_id: AtomicU64,
    pub timeouts: Timeouts,
    /// The signaling server's access key, read when a device ID is connected to.
    signal_secret: Box<dyn Fn() -> String + Send + Sync>,
}

impl Viewer {
    /// `clipboard` must be the one `deps` hands to sessions.
    pub fn new(
        deps: Arc<dyn Deps>,
        clipboard: Arc<MemoryClipboard>,
        data_dir: PathBuf,
        device_name: String,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(&data_dir).map_err(|e| format!("data directory: {e}"))?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let events = Arc::new(Events::default());
        let sink = events.clone();
        thread::Builder::new()
            .name("mcp-events".into())
            .spawn(move || {
                while let Ok(bytes) = rx.recv() {
                    if let Ok(event) = Event::decode(bytes.as_slice()) {
                        sink.on(event);
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let core = Core::new(
            deps,
            device_name,
            data_dir.clone(),
            data_dir.join("downloads"),
            EventTx::new(tx),
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            core,
            events,
            clipboard,
            data_dir,
            remotes: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            timeouts: Timeouts::default(),
            signal_secret: Box::new(String::new),
        })
    }

    /// Where the signaling server's access key comes from (the system's secret store).
    pub fn with_signal_secret(mut self, f: impl Fn() -> String + Send + Sync + 'static) -> Self {
        self.signal_secret = Box::new(f);
        self
    }

    pub fn sessions(&self) -> Vec<Arc<Remote>> {
        self.forget_ended();
        lock(&self.remotes).values().cloned().collect()
    }

    fn forget_ended(&self) {
        lock(&self.remotes).retain(|id, r| {
            let ended = self.events.ended(*id).is_some();
            if ended {
                drop(lock(&r.shell).take());
                r.session.close();
            }
            !ended
        });
    }

    /// The session `id`, or the only one when `id` is not given.
    pub fn remote(&self, id: Option<u64>) -> Result<Arc<Remote>, String> {
        if let Some(id) = id
            && let Some(ended) = self.events.ended(id)
        {
            self.forget_ended();
            return Err(format!("session {id} has ended: {}", reason_text(&ended)));
        }
        self.forget_ended();
        let remotes = lock(&self.remotes);
        match id {
            Some(id) => remotes
                .get(&id)
                .cloned()
                .ok_or_else(|| format!("there is no session {id}")),
            None => match remotes.len() {
                1 => Ok(remotes.values().next().cloned().unwrap()),
                0 => Err("not connected: use nexa_connect first".into()),
                _ => Err("several sessions are open: say which with `session`".into()),
            },
        }
    }

    /// Connects to `address` (`host:port` or a 9-digit device ID). Without `code` it
    /// signs in as a host paired before, or (by device ID) asks the person there.
    pub fn connect(&self, address: &str, code: Option<&str>) -> Result<Arc<Remote>, String> {
        let address = normalize(address)?;
        self.forget_ended();
        if let Some(r) = lock(&self.remotes).values().find(|r| r.address == address) {
            return Ok(r.clone());
        }
        if lock(&self.remotes).len() >= MAX_SESSIONS {
            return Err(format!(
                "at most {MAX_SESSIONS} sessions at once: disconnect one first"
            ));
        }
        let by_id = is_device_id(&address);
        let mut known = KnownHosts::load(&self.data_dir);
        let (auth, signed_in) = match code.map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => (ConnectAuth::Code(c.to_owned()), false),
            None => match known.key(&address) {
                Some(key) => (ConnectAuth::KnownHost(key), true),
                None if by_id => (ConnectAuth::Request(true), false),
                None => {
                    return Err(format!(
                        "{address} has not been paired with this agent: give the one-time code \
                         the host shows (`code`)"
                    ));
                }
            },
        };
        let config = Config::load(&self.data_dir)?;
        if by_id && config.signal_server.is_empty() {
            return Err(format!(
                "{address} is a device ID, which needs a signaling server: set \"signal_server\" \
                 in {}",
                self.data_dir.join("config.json").display()
            ));
        }
        let id = self.next_id.fetch_add(1, Relaxed);
        let session = self.core.connect(ConnectRequest {
            address: address.clone(),
            signal_server: if by_id {
                config.signal_server
            } else {
                String::new()
            },
            signal_secret: if by_id {
                (self.signal_secret)()
            } else {
                String::new()
            },
            tag: id,
            mode: Mode::Office as i32,
            fps: FPS,
            auth: Some(auth),
        });
        let frames = Frames::new();
        // A first size; once the host's display is known the view takes its shape.
        session.attach_presenter(frames.presenter(), MAX_SIDE, MAX_SIDE * 10 / 16);
        let outcome = self.events.wait(id, self.timeouts.connect, |s| {
            s.ended
                .clone()
                .map(Err)
                .or_else(|| s.connected.clone().map(Ok))
        });
        let host = match outcome {
            Some(Ok(c)) => c,
            Some(Err(d)) => {
                session.close();
                if signed_in && d.reason == DisconnectReason::NotPaired as i32 {
                    known.forget(&address);
                    known.save(&self.data_dir);
                    return Err(format!(
                        "{address} no longer knows this agent: give the one-time code it shows \
                         (`code`)"
                    ));
                }
                return Err(format!(
                    "could not connect to {address}: {}",
                    reason_text(&d)
                ));
            }
            None => {
                session.close();
                return Err(format!(
                    "{address} did not answer within {} s (nobody allowed the connection there?)",
                    self.timeouts.connect.as_secs()
                ));
            }
        };
        if !host.host_key.is_empty() && (host.paired_now || signed_in) {
            known.remember(&address, &host.host_key, &host.host_name);
            known.save(&self.data_dir);
        }
        if let Some(d) = host.displays.first() {
            let (w, h) = image::fitted(d.w.max(1), d.h.max(1), MAX_SIDE);
            session.resize_view(w, h, 1.0);
        }
        if host.perms.as_ref().is_some_and(|p| p.input) {
            let _ = session.set_input_active(true);
        }
        let remote = Arc::new(Remote {
            id,
            address,
            host,
            session,
            frames,
            shot: Mutex::new(None),
            shell: Mutex::new(None),
        });
        lock(&self.remotes).insert(id, remote.clone());
        Ok(remote)
    }

    pub fn disconnect(&self, remote: &Remote) {
        drop(lock(&remote.shell).take());
        remote.session.close();
        lock(&self.remotes).remove(&remote.id);
    }

    /// The screen as it is now (waiting for the first picture of a new session).
    pub fn screenshot(&self, remote: &Remote) -> Result<Shot, String> {
        let settled = remote
            .frames
            .settle(Instant::now(), Duration::ZERO, self.timeouts.first_frame)
            .ok_or("no picture from the host yet")?;
        self.encode(remote, &settled.image, true, Duration::ZERO)
    }

    /// The screen once it has stopped changing after an action sent at `since`.
    pub fn after(&self, remote: &Remote, since: Instant) -> Result<Shot, String> {
        let s = remote
            .frames
            .settle(since, self.timeouts.quiet, self.timeouts.settle_max)
            .ok_or("no picture from the host yet")?;
        self.encode(remote, &s.image, s.settled, since.elapsed())
    }

    fn encode(
        &self,
        remote: &Remote,
        image: &nexa_media::NativeImage,
        settled: bool,
        waited: Duration,
    ) -> Result<Shot, String> {
        let rgb = snapshot::fit(snapshot::to_rgb(image)?, MAX_SIDE);
        let png = image::png(&rgb)?;
        let size = (rgb.width, rgb.height);
        let before = lock(&remote.shot).replace(size);
        Ok(Shot {
            png,
            width: rgb.width,
            height: rgb.height,
            settled,
            waited,
            resized_from: before.filter(|b| *b != size),
        })
    }

    fn can_act(&self, remote: &Remote) -> Result<(), String> {
        self.remote(Some(remote.id))?;
        if !remote.host.perms.as_ref().is_some_and(|p| p.input) {
            return Err("the host allowed viewing only, not keyboard and mouse".into());
        }
        Ok(())
    }

    /// A point in the last screenshot to the view pixels commands take.
    fn point(&self, remote: &Remote, x: f64, y: f64) -> Result<(f64, f64), String> {
        let geo = remote.session.view();
        if geo.stream_w == 0 || geo.stream_h == 0 {
            return Err("no picture from the host yet: take a screenshot first".into());
        }
        let shot = lock(&remote.shot)
            .unwrap_or_else(|| image::fitted(geo.stream_w, geo.stream_h, MAX_SIDE));
        let (sx, sy) = image::to_stream(x, y, shot, (geo.stream_w, geo.stream_h))?;
        geo.to_view(sx, sy)
            .ok_or_else(|| "the picture has no size yet".to_owned())
    }

    fn send(&self, remote: &Remote, cmd: Command) -> Result<(), String> {
        remote
            .session
            .command(SessionCommand { cmd: Some(cmd) })
            .map_err(str::to_owned)
    }

    pub fn click(
        &self,
        remote: &Remote,
        x: f64,
        y: f64,
        button: Button,
        count: u32,
    ) -> Result<Instant, String> {
        self.can_act(remote)?;
        let (vx, vy) = self.point(remote, x, y)?;
        let button = match button {
            Button::Left => 0,
            Button::Right => 1,
            Button::Middle => 2,
        };
        self.send(remote, Command::Move(PointerMove { x: vx, y: vy }))?;
        for _ in 0..count {
            self.send(
                remote,
                Command::Button(PointerButton { button, down: true }),
            )?;
            self.send(
                remote,
                Command::Button(PointerButton {
                    button,
                    down: false,
                }),
            )?;
        }
        Ok(Instant::now())
    }

    /// `dx`/`dy` in wheel notches; positive is right and down.
    pub fn scroll(
        &self,
        remote: &Remote,
        x: f64,
        y: f64,
        dx: i32,
        dy: i32,
    ) -> Result<Instant, String> {
        self.can_act(remote)?;
        let (vx, vy) = self.point(remote, x, y)?;
        self.send(remote, Command::Move(PointerMove { x: vx, y: vy }))?;
        // PointerWheel: 1/120 notch, + is right/up.
        self.send(
            remote,
            Command::Wheel(PointerWheel {
                dx: dx * 120,
                dy: -dy * 120,
            }),
        )?;
        Ok(Instant::now())
    }

    pub fn type_text(&self, remote: &Remote, text: &str) -> Result<Instant, String> {
        self.can_act(remote)?;
        if text.is_empty() {
            return Err("no text given".into());
        }
        if text.chars().count() > MAX_TEXT {
            return Err(format!("at most {MAX_TEXT} characters at a time"));
        }
        self.send(
            remote,
            Command::Text(SendText {
                utf8: text.to_owned(),
            }),
        )?;
        Ok(Instant::now())
    }

    /// Presses `usages` together: down in order, up in reverse.
    pub fn keys(&self, remote: &Remote, usages: &[u16]) -> Result<Instant, String> {
        self.can_act(remote)?;
        for &u in usages {
            self.send(
                remote,
                Command::Key(KeyPress {
                    usage: u32::from(u),
                    down: true,
                    exact: false,
                }),
            )?;
        }
        for &u in usages.iter().rev() {
            self.send(
                remote,
                Command::Key(KeyPress {
                    usage: u32::from(u),
                    down: false,
                    exact: false,
                }),
            )?;
        }
        Ok(Instant::now())
    }

    /// Presses at one point, moves to another in steps a program can follow, and
    /// lets go there.
    pub fn drag(
        &self,
        remote: &Remote,
        from: (f64, f64),
        to: (f64, f64),
        button: Button,
    ) -> Result<Instant, String> {
        self.can_act(remote)?;
        let a = self.point(remote, from.0, from.1)?;
        let b = self.point(remote, to.0, to.1)?;
        let button = match button {
            Button::Left => 0,
            Button::Right => 1,
            Button::Middle => 2,
        };
        self.send(remote, Command::Move(PointerMove { x: a.0, y: a.1 }))?;
        thread::sleep(DRAG_STEP);
        self.send(
            remote,
            Command::Button(PointerButton { button, down: true }),
        )?;
        for i in 1..=DRAG_STEPS {
            let t = f64::from(i) / f64::from(DRAG_STEPS);
            thread::sleep(DRAG_STEP);
            self.send(
                remote,
                Command::Move(PointerMove {
                    x: a.0 + (b.0 - a.0) * t,
                    y: a.1 + (b.1 - a.1) * t,
                }),
            )?;
        }
        thread::sleep(DRAG_STEP);
        self.send(
            remote,
            Command::Button(PointerButton {
                button,
                down: false,
            }),
        )?;
        Ok(Instant::now())
    }

    /// The host's displays, and which one is shown now.
    pub fn displays(&self, remote: &Remote) -> (Vec<DisplayInfo>, Option<u32>) {
        let current = self.events.stream(remote.id).map(|s| s.display);
        (remote.host.displays.clone(), current)
    }

    /// Shows display `id` instead, and returns once its picture has arrived.
    pub fn select_display(&self, remote: &Remote, id: u32) -> Result<Instant, String> {
        self.remote(Some(remote.id))?;
        let Some(d) = remote.host.displays.iter().find(|d| d.id == id) else {
            let ids: Vec<String> = remote
                .host
                .displays
                .iter()
                .map(|d| d.id.to_string())
                .collect();
            return Err(format!(
                "there is no display {id}; the host has {}",
                ids.join(", ")
            ));
        };
        let before = remote.frames.frames();
        let (w, h) = image::fitted(d.w.max(1), d.h.max(1), MAX_SIDE);
        remote.session.resize_view(w, h, 1.0);
        self.send(remote, Command::Display(SelectDisplay { display: id }))?;
        let switched = self.events.wait(remote.id, SWITCH_WAIT, |s| {
            s.stream.as_ref().filter(|st| st.display == id).map(|_| ())
        });
        if switched.is_none() {
            return Err(format!("the host did not switch to display {id}"));
        }
        // The last screenshot showed the other display; its size is kept so the next
        // one can say it changed.
        let since = Instant::now();
        let deadline = since + self.timeouts.first_frame;
        while remote.frames.frames() == before && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        Ok(since)
    }

    /// Runs `command` on the host's command line and waits up to `timeout` for it.
    pub fn run(&self, remote: &Remote, command: &str, timeout: Duration) -> Result<Ran, String> {
        // The viewer is not told whether the command line was granted: opening it
        // says (a refused one ends at once with "denied").
        self.remote(Some(remote.id))?;
        if command.trim().is_empty() {
            return Err("no command given".into());
        }
        let mut shell = lock(&remote.shell);
        if shell.as_ref().is_some_and(Shell::exited) {
            *shell = None;
        }
        if shell.is_none() {
            *shell = Some(Shell::open(
                &remote.session,
                remote.host.host_os == 1,
                SHELL_READY,
            )?);
        }
        let result = shell.as_ref().unwrap().run(command, timeout);
        if result.is_err() {
            *shell = None;
        }
        result
    }

    /// The text on the shared clipboard: what a host copied last, or what was set.
    pub fn clipboard_text(&self, remote: &Remote) -> Result<Option<String>, String> {
        self.remote(Some(remote.id))?;
        if !remote.perms().clipboard {
            return Err("the host did not allow the clipboard".into());
        }
        Ok(self.clipboard.text())
    }

    /// Puts `text` on the clipboard and waits for the host to have it, so a paste
    /// there right after gets it. False if the host did not confirm in time.
    pub fn set_clipboard(&self, remote: &Remote, text: &str) -> Result<bool, String> {
        self.remote(Some(remote.id))?;
        if !remote.perms().clipboard {
            return Err("the host did not allow the clipboard".into());
        }
        let before = self.events.clips_sent(remote.id);
        self.clipboard.set_text(text);
        Ok(self
            .events
            .wait(remote.id, CLIP_WAIT, |s| {
                (s.clips_sent > before).then_some(())
            })
            .is_some())
    }
}

/// A drag moves in this many steps, this far apart.
const DRAG_STEPS: u32 = 12;
const DRAG_STEP: Duration = Duration::from_millis(16);
/// For the host to switch displays.
const SWITCH_WAIT: Duration = Duration::from_secs(10);
/// For a new shell to answer.
const SHELL_READY: Duration = Duration::from_secs(15);
/// For the host to confirm a copy.
const CLIP_WAIT: Duration = Duration::from_secs(3);

impl Drop for Viewer {
    fn drop(&mut self) {
        for r in lock(&self.remotes).values() {
            r.session.close();
        }
        self.core.shut_down();
    }
}

fn reason_text(d: &Disconnected) -> String {
    let reason = DisconnectReason::try_from(d.reason)
        .map(|r| format!("{r:?}"))
        .unwrap_or_else(|_| "unknown".into());
    if d.message.is_empty() {
        reason
    } else {
        format!("{} ({reason})", d.message)
    }
}

fn is_device_id(address: &str) -> bool {
    address.len() == 9 && address.bytes().all(|b| b.is_ascii_digit())
}

/// Trims, and takes the spaces out of a device ID written in groups ("123 456 789").
fn normalize(address: &str) -> Result<String, String> {
    let a = address.trim();
    if a.is_empty() {
        return Err("no address given".into());
    }
    let digits: String = a.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    if digits.len() == 9 && digits.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(digits);
    }
    Ok(a.to_owned())
}

/// `config.json` in the data directory: where device IDs are looked up. The server's
/// access key is not kept here but in the system's secret store
/// (`nexa-mcp set-signal-secret`, [`crate::secret`]).
#[derive(Debug, Default)]
struct Config {
    signal_server: String,
}

impl Config {
    fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join("config.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
        let field = |k: &str| {
            v.get(k)
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim()
                .to_owned()
        };
        if !field("signal_secret").is_empty() {
            return Err(format!(
                "{} holds the server's access key, which belongs in the system's secret store: \
                 run `nexa-mcp set-signal-secret` and take it out of the file",
                path.display()
            ));
        }
        Ok(Self {
            signal_server: field("signal_server"),
        })
    }
}

/// `hosts.json`: the address each paired host was reached at, and its key, so it can
/// be signed in to again without a code. The pairing itself is the core's
/// (known_hosts.json); this only finds it by address.
#[derive(Default)]
struct KnownHosts(serde_json::Map<String, serde_json::Value>);

impl KnownHosts {
    fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join("hosts.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.as_object().cloned())
            .map(Self)
            .unwrap_or_default()
    }

    fn key(&self, address: &str) -> Option<String> {
        self.0
            .get(address)
            .and_then(|h| h.get("key"))
            .and_then(|k| k.as_str())
            .map(str::to_owned)
    }

    fn remember(&mut self, address: &str, key: &str, name: &str) {
        self.0.insert(
            address.to_owned(),
            serde_json::json!({ "key": key, "name": name }),
        );
    }

    fn forget(&mut self, address: &str) {
        self.0.remove(address);
    }

    fn save(&self, dir: &Path) {
        let path = dir.join("hosts.json");
        let tmp = dir.join("hosts.json.tmp");
        let text = serde_json::to_string_pretty(&self.0).unwrap_or_default();
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_lose_their_grouping() {
        assert_eq!(normalize(" 123 456 789 ").unwrap(), "123456789");
        assert_eq!(normalize("123-456-789").unwrap(), "123456789");
        assert_eq!(normalize("192.168.0.7:47800").unwrap(), "192.168.0.7:47800");
        assert!(normalize("  ").is_err());
        assert!(is_device_id("123456789"));
        assert!(!is_device_id("12345678"));
    }

    #[test]
    fn known_hosts_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut k = KnownHosts::load(dir.path());
        assert!(k.key("a:1").is_none());
        k.remember("a:1", "abcd", "Mac");
        k.save(dir.path());
        let k2 = KnownHosts::load(dir.path());
        assert_eq!(k2.key("a:1").as_deref(), Some("abcd"));
        let mut k2 = k2;
        k2.forget("a:1");
        k2.save(dir.path());
        assert!(KnownHosts::load(dir.path()).key("a:1").is_none());
    }

    #[test]
    fn the_config_names_the_server_and_never_holds_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"signal_server":" wss://s:8443 "}"#).unwrap();
        assert_eq!(
            Config::load(dir.path()).unwrap().signal_server,
            "wss://s:8443"
        );
        std::fs::write(
            &path,
            r#"{"signal_server":"wss://s:8443","signal_secret":"k"}"#,
        )
        .unwrap();
        let e = Config::load(dir.path()).unwrap_err();
        assert!(e.contains("set-signal-secret"), "{e}");
    }
}
