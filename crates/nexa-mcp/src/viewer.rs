//! The viewer behind the tools: one `nexa_client::Core` without windows, its agent
//! sessions, and the hosts it has paired with (docs/MCP.md §3, §13).
//!
//! An agent session has no video. After each action the viewer sends a mark on the
//! input stream; a look asks the host for what the agent wants to see (the frontmost
//! window's elements, a screenshot), once the host has got to that mark and its
//! screen has stopped changing. Coordinates are the host's agent pixels of the
//! display, which its `StreamConfig` gives; the view is made the same size, so they
//! go through unchanged.
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
};
use nexa_session::agent::{ObserveReply, ObserveRequest};
use nexa_wire::control::Mode;
use prost::Message;

use crate::terminal::{Ran, Shell};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long things may take.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Until the host answers, which includes the person there deciding.
    pub connect: Duration,
    /// For a look's answer, the waits on the host included.
    pub look: Duration,
    /// After an action, how long the screen must stay unchanged to count as settled.
    pub quiet: Duration,
    /// After an action, the longest wait for it to settle.
    pub settle_max: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(90),
            look: Duration::from_secs(20),
            quiet: Duration::from_millis(400),
            settle_max: Duration::from_secs(3),
        }
    }
}

/// For the host to say how large its agent pixels are, once connected.
const SPACE_WAIT: Duration = Duration::from_secs(10);
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

/// An agent session.
pub struct Remote {
    pub id: u64,
    pub address: String,
    pub host: Connected,
    session: Arc<Session>,
    /// The last mark sent after an action.
    marks: AtomicU64,
    /// Looks taken, numbered.
    looks: AtomicU64,
    /// The refs of the last element tree read: `@e{i+1}` is at `centers[i]`.
    refs: Mutex<Refs>,
    /// The host's command line, once a command was run.
    shell: Mutex<Option<Shell>>,
}

#[derive(Default)]
struct Refs {
    /// The look they came from; 0: none read yet.
    look: u64,
    centers: Vec<(i32, i32)>,
}

/// Where an action happens: a ref from the last element tree, or a point in the
/// agent's pixels.
pub enum Target {
    Ref(String),
    Point(f64, f64),
}

impl Remote {
    pub fn perms(&self) -> nexa_client::proto::Permissions {
        self.host.perms.clone().unwrap_or_default()
    }

    /// The agent's pixels of the display shown: what coordinates are in.
    pub fn space(&self) -> (u32, u32) {
        let v = self.session.view();
        (v.stream_w, v.stream_h)
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

/// What a look brought back.
pub struct Looked {
    pub reply: ObserveReply,
    /// The screenshot, if one was asked for.
    pub png: Vec<u8>,
    /// How many refs the element tree has (0 without one).
    pub refs: usize,
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
            fps: 0,
            auth: Some(auth),
        });
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
        // The host's agent pixels; the view takes their size, so coordinates pass
        // through unchanged.
        let Some(space) = self.events.wait(id, SPACE_WAIT, |s| s.stream.clone()) else {
            session.close();
            return Err(format!(
                "{address} did not say how large its screen is: is it a NexaDesk too old \
                 for agents?"
            ));
        };
        session.resize_view(space.w, space.h, 1.0);
        if host.perms.as_ref().is_some_and(|p| p.input) {
            let _ = session.set_input_active(true);
        }
        let remote = Arc::new(Remote {
            id,
            address,
            host,
            session,
            marks: AtomicU64::new(0),
            looks: AtomicU64::new(0),
            refs: Mutex::new(Refs::default()),
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

    /// Asks the host for a look: once it has got to mark `after_mark` (0: at once)
    /// and, after a mark, once its screen has stopped changing. The element tree's
    /// refs replace the last ones.
    pub fn look(
        &self,
        remote: &Remote,
        after_mark: u64,
        ui: bool,
        screenshot: bool,
        interactive_only: bool,
        max_nodes: u32,
    ) -> Result<Looked, String> {
        self.remote(Some(remote.id))?;
        let settle = after_mark != 0;
        let request = ObserveRequest {
            after_mark,
            quiet_ms: if settle {
                self.timeouts.quiet.as_millis() as u32
            } else {
                0
            },
            max_ms: self.timeouts.settle_max.as_millis() as u32,
            ui,
            interactive_only,
            max_nodes,
            screenshot,
        };
        let (reply, png) = remote.session.observe(request, self.timeouts.look)?;
        let mut refs = 0;
        if ui && reply.ui_error.is_empty() {
            let centers = ref_centers(&reply.ui);
            refs = centers.len();
            *lock(&remote.refs) = Refs {
                look: remote.looks.fetch_add(1, Relaxed) + 1,
                centers,
            };
        }
        Ok(Looked { reply, png, refs })
    }

    fn can_act(&self, remote: &Remote) -> Result<(), String> {
        self.remote(Some(remote.id))?;
        if !remote.host.perms.as_ref().is_some_and(|p| p.input) {
            return Err("the host allowed viewing only, not keyboard and mouse".into());
        }
        Ok(())
    }

    /// Where `target` is, in the view pixels commands take (the agent's pixels).
    fn point(&self, remote: &Remote, target: &Target) -> Result<(f64, f64), String> {
        let geo = remote.session.view();
        if geo.stream_w == 0 || geo.stream_h == 0 {
            return Err("the host has not said how large its screen is yet".into());
        }
        let (x, y) = match target {
            Target::Point(x, y) => (*x, *y),
            Target::Ref(r) => {
                let refs = lock(&remote.refs);
                if refs.look == 0 {
                    return Err(format!(
                        "no element tree read yet: call nexa_ui first, then use {r}"
                    ));
                }
                let n = parse_ref(r).ok_or_else(|| format!("\"{r}\" is not a ref like @e3"))?;
                let (x, y) = *refs.centers.get(n - 1).ok_or_else(|| {
                    format!(
                        "there is no {r} in the last element tree ({} refs): call nexa_ui again",
                        refs.centers.len()
                    )
                })?;
                (f64::from(x), f64::from(y))
            }
        };
        let (w, h) = (f64::from(geo.stream_w), f64::from(geo.stream_h));
        if !(x.is_finite() && y.is_finite()) || x < 0.0 || y < 0.0 || x >= w || y >= h {
            return Err(format!(
                "({x}, {y}) is outside the screen, which is {}×{} pixels",
                geo.stream_w, geo.stream_h
            ));
        }
        geo.to_view(x.floor() as i32, y.floor() as i32)
            .ok_or_else(|| "the screen has no size yet".to_owned())
    }

    fn send(&self, remote: &Remote, cmd: Command) -> Result<(), String> {
        remote
            .session
            .command(SessionCommand { cmd: Some(cmd) })
            .map_err(str::to_owned)
    }

    /// Ends an action: a mark after it, which the look that follows waits for.
    fn marked(&self, remote: &Remote) -> u64 {
        let id = remote.marks.fetch_add(1, Relaxed) + 1;
        remote.session.mark(id);
        id
    }

    fn button_code(button: Button) -> u32 {
        match button {
            Button::Left => 0,
            Button::Right => 1,
            Button::Middle => 2,
        }
    }

    /// Clicks `count` times at `target`; returns the mark to look after.
    pub fn click(
        &self,
        remote: &Remote,
        target: &Target,
        button: Button,
        count: u32,
    ) -> Result<u64, String> {
        self.can_act(remote)?;
        let (vx, vy) = self.point(remote, target)?;
        let button = Self::button_code(button);
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
        Ok(self.marked(remote))
    }

    /// `dx`/`dy` in wheel notches; positive is right and down.
    pub fn scroll(
        &self,
        remote: &Remote,
        target: &Target,
        dx: i32,
        dy: i32,
    ) -> Result<u64, String> {
        self.can_act(remote)?;
        let (vx, vy) = self.point(remote, target)?;
        self.send(remote, Command::Move(PointerMove { x: vx, y: vy }))?;
        // PointerWheel: 1/120 notch, + is right/up.
        self.send(
            remote,
            Command::Wheel(PointerWheel {
                dx: dx * 120,
                dy: -dy * 120,
            }),
        )?;
        Ok(self.marked(remote))
    }

    pub fn type_text(&self, remote: &Remote, text: &str) -> Result<u64, String> {
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
        Ok(self.marked(remote))
    }

    /// Presses `usages` together: down in order, up in reverse.
    pub fn keys(&self, remote: &Remote, usages: &[u16]) -> Result<u64, String> {
        self.can_act(remote)?;
        for (u, down) in usages
            .iter()
            .map(|u| (u, true))
            .chain(usages.iter().rev().map(|u| (u, false)))
        {
            self.send(
                remote,
                Command::Key(KeyPress {
                    usage: u32::from(*u),
                    down,
                    exact: false,
                }),
            )?;
        }
        Ok(self.marked(remote))
    }

    /// Presses at one point, moves to another in steps a program can follow, and
    /// lets go there.
    pub fn drag(
        &self,
        remote: &Remote,
        from: &Target,
        to: &Target,
        button: Button,
    ) -> Result<u64, String> {
        self.can_act(remote)?;
        let a = self.point(remote, from)?;
        let b = self.point(remote, to)?;
        let button = Self::button_code(button);
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
        Ok(self.marked(remote))
    }

    /// The host's displays, and which one is shown now.
    pub fn displays(&self, remote: &Remote) -> (Vec<DisplayInfo>, Option<u32>) {
        let current = self.events.stream(remote.id).map(|s| s.display);
        (remote.host.displays.clone(), current)
    }

    /// Shows display `id` instead: the host's agent pixels are that display's from
    /// now on, and the last element tree's refs no longer hold.
    pub fn select_display(&self, remote: &Remote, id: u32) -> Result<(u32, u32), String> {
        self.remote(Some(remote.id))?;
        if !remote.host.displays.iter().any(|d| d.id == id) {
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
        }
        self.send(remote, Command::Display(SelectDisplay { display: id }))?;
        let space = self
            .events
            .wait(remote.id, SWITCH_WAIT, |s| {
                s.stream.clone().filter(|st| st.display == id)
            })
            .ok_or_else(|| format!("the host did not switch to display {id}"))?;
        remote.session.resize_view(space.w, space.h, 1.0);
        *lock(&remote.refs) = Refs::default();
        Ok((space.w, space.h))
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

/// `@e12`, `e12` or `12` → 12.
fn parse_ref(r: &str) -> Option<usize> {
    let t = r.trim();
    let t = t.strip_prefix('@').unwrap_or(t);
    let t = t.strip_prefix('e').unwrap_or(t);
    t.parse::<usize>().ok().filter(|n| *n > 0)
}

/// The centre of each ref in an element tree's text, in order: a line holding
/// `@e{n} …` ends with its bounds, `(x,y w×h)`.
fn ref_centers(ui: &str) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    for line in ui.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("@e") else {
            continue;
        };
        let n: usize = match rest.split(' ').next().and_then(|n| n.parse().ok()) {
            Some(n) => n,
            None => continue,
        };
        let Some(open) = line.rfind('(') else {
            continue;
        };
        let inner = line[open + 1..].trim_end().trim_end_matches(')');
        let parsed = (|| {
            let (xy, wh) = inner.split_once(' ')?;
            let (x, y) = xy.split_once(',')?;
            let (w, h) = wh.split_once('×')?;
            Some((
                x.parse::<i32>().ok()?,
                y.parse::<i32>().ok()?,
                w.parse::<i32>().ok()?,
                h.parse::<i32>().ok()?,
            ))
        })();
        if let Some((x, y, w, h)) = parsed
            && n == out.len() + 1
        {
            out.push((x + w / 2, y + h / 2));
        }
    }
    out
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
    fn refs_are_read_from_the_element_tree() {
        let ui = "window \"W\" (0,0 1568×980)\n\
                  \x20\x20@e1 button \"OK (sure)\" (100,100 40×20)\n\
                  \x20\x20text \"Ready\" (0,0 10×10)\n\
                  \x20\x20\x20\x20@e2 textfield value=\"a (b)\" focused (10,50 200×31)\n";
        assert_eq!(ref_centers(ui), vec![(120, 110), (110, 65)]);
        assert_eq!(parse_ref("@e12"), Some(12));
        assert_eq!(parse_ref("e3"), Some(3));
        assert_eq!(parse_ref(" 7 "), Some(7));
        assert_eq!(parse_ref("@e0"), None);
        assert_eq!(parse_ref("button"), None);
    }

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
