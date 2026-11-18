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
    proto::{
        Command, ConnectAuth, ConnectRequest, Connected, DisconnectReason, Disconnected, Event,
        EventBody, KeyPress, PointerButton, PointerMove, PointerWheel, SendText, SessionCommand,
    },
    snapshot::{self, Frames},
};
use nexa_wire::control::Mode;
use prost::Message;

use crate::image::{self, MAX_SIDE};

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
}

impl Remote {
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
}

pub enum Button {
    Left,
    Right,
    Middle,
}

pub struct Viewer {
    core: Arc<Core>,
    events: Arc<Events>,
    data_dir: PathBuf,
    remotes: Mutex<BTreeMap<u64, Arc<Remote>>>,
    next_id: AtomicU64,
    pub timeouts: Timeouts,
}

impl Viewer {
    pub fn new(
        deps: Arc<dyn Deps>,
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
            data_dir,
            remotes: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            timeouts: Timeouts::default(),
        })
    }

    pub fn sessions(&self) -> Vec<Arc<Remote>> {
        self.forget_ended();
        lock(&self.remotes).values().cloned().collect()
    }

    fn forget_ended(&self) {
        lock(&self.remotes).retain(|id, r| {
            let ended = self.events.ended(*id).is_some();
            if ended {
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
                config.signal_secret
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
        });
        lock(&self.remotes).insert(id, remote.clone());
        Ok(remote)
    }

    pub fn disconnect(&self, remote: &Remote) {
        remote.session.close();
        lock(&self.remotes).remove(&remote.id);
    }

    /// The screen as it is now (waiting for the first picture of a new session).
    pub fn screenshot(&self, remote: &Remote) -> Result<Shot, String> {
        let settled = remote
            .frames
            .settle(Instant::now(), Duration::ZERO, self.timeouts.first_frame)
            .ok_or("no picture from the host yet")?;
        self.encode(remote, &settled.image, true)
    }

    /// The screen once it has stopped changing after an action sent at `since`.
    pub fn after(&self, remote: &Remote, since: Instant) -> Result<Shot, String> {
        let s = remote
            .frames
            .settle(since, self.timeouts.quiet, self.timeouts.settle_max)
            .ok_or("no picture from the host yet")?;
        self.encode(remote, &s.image, s.settled)
    }

    fn encode(
        &self,
        remote: &Remote,
        image: &nexa_media::NativeImage,
        settled: bool,
    ) -> Result<Shot, String> {
        let rgb = snapshot::fit(snapshot::to_rgb(image)?, MAX_SIDE);
        let png = image::png(&rgb)?;
        *lock(&remote.shot) = Some((rgb.width, rgb.height));
        Ok(Shot {
            png,
            width: rgb.width,
            height: rgb.height,
            settled,
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
                }),
            )?;
        }
        for &u in usages.iter().rev() {
            self.send(
                remote,
                Command::Key(KeyPress {
                    usage: u32::from(u),
                    down: false,
                }),
            )?;
        }
        Ok(Instant::now())
    }
}

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

/// `config.json` in the data directory: where device IDs are looked up.
#[derive(Default)]
struct Config {
    signal_server: String,
    signal_secret: String,
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
        let config = Self {
            signal_server: field("signal_server"),
            signal_secret: field("signal_secret"),
        };
        #[cfg(unix)]
        if !config.signal_secret.is_empty() {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .map(|m| m.permissions().mode())
                .unwrap_or(0);
            if mode & 0o077 != 0 {
                return Err(format!(
                    "{} holds a key but others can read it: chmod 600 it",
                    path.display()
                ));
            }
        }
        Ok(config)
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

    #[cfg(unix)]
    #[test]
    fn a_readable_config_with_a_key_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"signal_server":"wss://s:8443","signal_secret":"k"}"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        // No Debug on Config: it holds the key.
        match Config::load(dir.path()) {
            Err(e) => assert!(e.contains("chmod 600"), "{e}"),
            Ok(_) => panic!("a readable key was accepted"),
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let Ok(c) = Config::load(dir.path()) else {
            panic!("refused")
        };
        assert_eq!(
            (c.signal_server.as_str(), c.signal_secret.as_str()),
            ("wss://s:8443", "k")
        );
    }
}
