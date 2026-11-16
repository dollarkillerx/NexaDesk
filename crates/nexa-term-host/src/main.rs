//! `nexa-term-host`: a NexaDesk host with no screen, only the command line. It registers
//! with a nexa-signal server under a device ID; a browser opens the server's page, types
//! that ID and is let in to a shell on this machine (bash/zsh on Linux and macOS,
//! PowerShell on Windows).
//!
//! Sign-in:
//! - the fixed password (`set-password`) lets the owner straight in;
//! - the one-time code printed at start, or only the device ID, asks whoever is at this
//!   console (y/N). Run as a service, with no console, only the password works.

use std::{
    io::{BufRead, IsTerminal, Write},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use nexa_auth::{DeviceIdentity, FileKeyStore, HostPassword, KeyStore, OneTimeCode, PasswordGuard};
use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig};
use nexa_session::{
    input::InputSink,
    rendezvous::{
        seal::SignalKeys,
        signal::{SignalClient, SignalConfig},
    },
};
use nexa_web::{
    BrowserAnswer, BrowserAuth, BrowserRequest, RemoteBrowser, TurnCredentials, WebConfig,
    WebEvent, WebGateway, WebHost, WebScreen,
};
use nexa_wire::signal::{BrowserClose, BrowserText, ClientBody, ServerBody, group_short_id};

const USAGE: &str = "\
usage: nexa-term-host --signal <wss://server[:port]> [--signal-ca <pem>] [--data-dir <dir>]
                      [--max <n>]
       nexa-term-host set-password [--data-dir <dir>]     (the password on stdin)
       nexa-term-host clear-password [--data-dir <dir>]
       nexa-term-host reset-device-id [--data-dir <dir>]

  --signal     the nexa-signal server browsers open; this machine registers there and
               gets a device ID
  --signal-ca  extra CA certificate(s) for the server (default: <data-dir>/signal-ca.pem)
  --data-dir   where this host keeps its key, password and device ID seed (default:
               ~/.config/nexadesk on Linux, %APPDATA%\\NexaDesk on Windows,
               ~/Library/Application Support/NexaDesk-term on macOS)
  --max        how many people at once (default 4)

Browsers sign in with the fixed password (straight in), or with the one-time code
printed here or only the device ID, which are put to whoever is at this console.";

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A connection that lasted this long was not a flap.
const STABLE_AFTER: Duration = Duration::from_secs(10);
const ID_SEED_FILE: &str = "device-id-seed";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("set-password") => data_dir_of(&args[1..]).and_then(|d| set_password(&d)),
        Some("clear-password") => data_dir_of(&args[1..])
            .and_then(|d| password_store(&d).delete().map_err(|e| e.to_string())),
        Some("reset-device-id") => data_dir_of(&args[1..]).and_then(|d| reset_device_id(&d)),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return;
        }
        _ => run(&args),
    };
    if let Err(e) = result {
        eprintln!("nexa-term-host: {e}");
        std::process::exit(1);
    }
}

fn default_data_dir() -> Result<PathBuf, String> {
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    if cfg!(windows) {
        return var("APPDATA")
            .map(|d| d.join("NexaDesk"))
            .ok_or("APPDATA is not set".into());
    }
    let home = var("HOME").ok_or("HOME is not set")?;
    if cfg!(target_os = "macos") {
        return Ok(home.join("Library/Application Support/NexaDesk-term"));
    }
    Ok(var("XDG_CONFIG_HOME")
        .unwrap_or_else(|| home.join(".config"))
        .join("nexadesk"))
}

fn data_dir_of(args: &[String]) -> Result<PathBuf, String> {
    match args {
        [] => default_data_dir(),
        [flag, dir] if flag == "--data-dir" => Ok(PathBuf::from(dir)),
        _ => Err(USAGE.into()),
    }
}

fn password_store(dir: &std::path::Path) -> FileKeyStore {
    FileKeyStore::new(dir.join("host-password"))
}

fn set_password(dir: &std::path::Path) -> Result<(), String> {
    let mut line = zeroize::Zeroizing::new(String::new());
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("reading the password: {e}"))?;
    let password = line.trim_end_matches(['\r', '\n']);
    let n = password.chars().count();
    if !(8..=128).contains(&n) {
        return Err("the password must be 8 to 128 characters".into());
    }
    if password.chars().any(char::is_control) {
        return Err("the password cannot contain control characters".into());
    }
    if nexa_auth::normalize_code(password).is_ok() {
        return Err(
            "the password must not look like a one-time code (8 letters and digits)".into(),
        );
    }
    let stored = HostPassword::create(password).map_err(|e| e.to_string())?;
    password_store(dir)
        .store(&stored.to_bytes())
        .map_err(|e| format!("saving the password: {e}"))?;
    println!("fixed password saved");
    Ok(())
}

fn reset_device_id(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("data directory: {e}"))?;
    let seed = DeviceIdentity::generate().public_key().as_bytes()[..16].to_vec();
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(dir.join(ID_SEED_FILE), hex + "\n").map_err(|e| e.to_string())?;
    println!("the device ID changes at the next start");
    Ok(())
}

fn load_id_seed(dir: &std::path::Path) -> Vec<u8> {
    let Ok(text) = std::fs::read_to_string(dir.join(ID_SEED_FILE)) else {
        return Vec::new();
    };
    let text = text.trim();
    if text.len() != 32 {
        return Vec::new();
    }
    (0..16)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default()
}

struct Options {
    signal: String,
    signal_ca: Option<PathBuf>,
    data_dir: PathBuf,
    max: usize,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let (mut signal, mut signal_ca, mut data_dir, mut max) = (None, None, None, 4usize);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        match a.as_str() {
            "--signal" => signal = Some(value()?),
            "--signal-ca" => signal_ca = Some(PathBuf::from(value()?)),
            "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
            "--max" => {
                max = value()?
                    .parse()
                    .map_err(|_| "--max takes a number".to_string())?
            }
            _ => return Err(format!("unknown argument {a}\n\n{USAGE}")),
        }
    }
    Ok(Options {
        signal: signal.ok_or_else(|| USAGE.to_string())?,
        signal_ca,
        data_dir: match data_dir {
            Some(d) => d,
            None => default_data_dir()?,
        },
        max: max.clamp(1, 16),
    })
}

fn run(args: &[String]) -> Result<(), String> {
    let opts = parse(args)?;
    std::fs::create_dir_all(&opts.data_dir).map_err(|e| format!("data directory: {e}"))?;
    let identity =
        DeviceIdentity::load_or_create(&FileKeyStore::new(opts.data_dir.join("identity")))
            .map_err(|e| format!("this host's key: {e}"))?;
    let password = match password_store(&opts.data_dir).load() {
        Ok(Some(bytes)) => Some(Arc::new(
            HostPassword::from_bytes(&bytes).map_err(|e| format!("the saved password: {e}"))?,
        )),
        Ok(None) => None,
        Err(e) => return Err(format!("the saved password: {e}")),
    };

    nexa_session::rendezvous::signal::parse_url(&opts.signal).map_err(|e| e.to_string())?;
    let mut server = SignalConfig::new(opts.signal.clone());
    let ca = opts
        .signal_ca
        .clone()
        .or_else(|| Some(opts.data_dir.join("signal-ca.pem")).filter(|p| p.exists()));
    if let Some(ca) = ca {
        server = server.with_ca_file(&ca)?;
    }
    server.id_seed = load_id_seed(&opts.data_dir);

    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    runtime.block_on(serve(opts, identity, password, server))
}

/// The person at this console, if there is one: lines typed there answer requests.
struct Console {
    lines: Mutex<mpsc::Receiver<String>>,
    /// One question at a time.
    asking: Mutex<()>,
}

impl Console {
    fn attach() -> Option<Arc<Self>> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Some(Arc::new(Self {
            lines: Mutex::new(rx),
            asking: Mutex::new(()),
        }))
    }

    fn ask(&self, question: &str, timeout: Duration) -> bool {
        let _one = self.asking.lock().unwrap_or_else(|e| e.into_inner());
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        while lines.try_recv().is_ok() {}
        print!("{question} [y/N] ");
        let _ = std::io::stdout().flush();
        let answer = lines.recv_timeout(timeout).unwrap_or_default();
        let yes = matches!(answer.trim().to_lowercase().as_str(), "y" | "yes");
        println!("{}", if yes { "allowed" } else { "refused" });
        yes
    }
}

struct TermHost {
    console: Option<Arc<Console>>,
    gateway: Mutex<Option<std::sync::Weak<WebGateway>>>,
    max: usize,
}

impl WebHost for TermHost {
    fn has_screen(&self) -> bool {
        false
    }
    fn screen(&self) -> Result<WebScreen, String> {
        Err("this host has only the command line".into())
    }
    fn capturer(&self, _: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        Err(nexa_media::MediaError::Unsupported("no screen"))
    }
    fn encoder(&self, _: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        Err(nexa_media::MediaError::Unsupported("no screen"))
    }
    fn input_available(&self) -> bool {
        false
    }
    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        Err("no screen".into())
    }
    fn terminal(&self, cols: u16, rows: u16) -> Result<nexa_term::Pty, String> {
        nexa_term::Pty::spawn(&nexa_term::Spawn {
            cols,
            rows,
            ..Default::default()
        })
        .map_err(|e| format!("could not start a shell: {e}"))
    }
    fn busy(&self) -> bool {
        let gateway = self.gateway.lock().unwrap_or_else(|e| e.into_inner());
        gateway
            .as_ref()
            .and_then(|g| g.upgrade())
            .is_some_and(|g| g.active_count() >= self.max)
    }
    fn ask(&self, r: &BrowserRequest, timeout: Duration) -> BrowserAnswer {
        let allow = BrowserAnswer {
            allow: true,
            input: false,
            clipboard: false,
            terminal: true,
        };
        if r.auth == BrowserAuth::Password {
            return allow;
        }
        let Some(console) = &self.console else {
            return BrowserAnswer::DENY;
        };
        let how = match r.auth {
            BrowserAuth::Request => "only the device ID, unverified",
            _ => "the one-time code",
        };
        let question = format!(
            "\n{} from {} ({how}) asks for the command line of this machine. Allow?",
            r.browser,
            r.remote.ip()
        );
        if console.ask(&question, timeout)
            && !r.cancelled.load(std::sync::atomic::Ordering::Relaxed)
        {
            allow
        } else {
            BrowserAnswer::DENY
        }
    }
    fn event(&self, event: WebEvent) {
        match event {
            WebEvent::SessionStarted {
                id,
                browser,
                remote,
                ..
            } => {
                println!("[{id}] {browser} from {} connected", remote.ip());
            }
            WebEvent::SessionEnded { id, reason } => println!("[{id}] ended: {reason}"),
            WebEvent::Refused { remote, reason } => println!("refused {}: {reason}", remote.ip()),
        }
    }
}

async fn serve(
    opts: Options,
    identity: DeviceIdentity,
    password: Option<Arc<HostPassword>>,
    server: SignalConfig,
) -> Result<(), String> {
    let console = Console::attach();
    let code = OneTimeCode::generate();
    let host = Arc::new(TermHost {
        console: console.clone(),
        gateway: Mutex::new(None),
        max: opts.max,
    });
    let gateway = Arc::new(WebGateway::remote_only(
        host.clone(),
        code.clone(),
        WebConfig {
            name: machine_name(),
            ..WebConfig::default()
        },
    ));
    *host.gateway.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&gateway));
    if let Some(p) = password.clone() {
        gateway.set_password(Some((p, PasswordGuard::default())));
    }

    println!("NexaDesk command-line host on {}", std::env::consts::OS);
    println!("server: {}", opts.signal);
    match (&password, &console) {
        (Some(_), _) => println!("fixed password: set (lets the owner straight in)"),
        (None, Some(_)) => println!("fixed password: not set (nexa-term-host set-password)"),
        (None, None) => {
            return Err(
                "no fixed password is set and there is no console to ask: run \
                        `nexa-term-host set-password` first"
                    .into(),
            );
        }
    }
    if console.is_some() {
        println!("one-time code: {}", group_code(&code.current()));
    } else {
        println!("no console: only the fixed password signs in");
    }

    let keys = Arc::new(SignalKeys::new(Arc::new(identity)));
    let mut backoff = BACKOFF_MIN;
    let mut shown_id = String::new();
    loop {
        let mut client = match SignalClient::connect(&server, &keys).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("server: {e}; trying again in {}s", backoff.as_secs());
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        let connected_at = std::time::Instant::now();
        let id = client.registration.short_id.clone();
        if id != shown_id {
            println!("device ID: {}", group_short_id(&id));
            shown_id = id;
        }
        let mut browsers = std::collections::HashMap::new();
        loop {
            let msg = tokio::select! {
                m = client.recv() => m,
                _ = tokio::signal::ctrl_c() => {
                    gateway.disconnect();
                    return Ok(());
                }
            };
            match msg {
                Some(ServerBody::BrowserOpen(open)) => {
                    let conn = open.conn;
                    let sender = client.sender();
                    let parsed = (
                        open.ip.parse::<std::net::IpAddr>(),
                        open.turn_server.parse::<std::net::SocketAddr>(),
                    );
                    let (Ok(ip), Ok(turn_server)) = parsed else {
                        let _ = sender.send(ClientBody::BrowserClose(BrowserClose { conn }));
                        continue;
                    };
                    let (in_tx, in_rx) = tokio::sync::mpsc::channel(32);
                    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(32);
                    gateway.serve_remote(
                        RemoteBrowser {
                            ip,
                            turn_server,
                            turn: TurnCredentials {
                                username: open.turn_username,
                                password: open.turn_password,
                            },
                        },
                        in_rx,
                        out_tx,
                    );
                    tokio::spawn(async move {
                        while let Some(text) = out_rx.recv().await {
                            if sender
                                .send(ClientBody::BrowserText(BrowserText { conn, text }))
                                .is_err()
                            {
                                return;
                            }
                        }
                        let _ = sender.send(ClientBody::BrowserClose(BrowserClose { conn }));
                    });
                    browsers.insert(conn, in_tx);
                }
                Some(ServerBody::BrowserText(t)) => {
                    if browsers
                        .get(&t.conn)
                        .is_some_and(|tx| tx.try_send(t.text).is_err())
                    {
                        browsers.remove(&t.conn);
                    }
                }
                Some(ServerBody::BrowserClose(c)) => {
                    browsers.remove(&c.conn);
                }
                // An app (not a browser) asking: this host has no screen to give it.
                Some(ServerBody::Incoming(_)) => {}
                // Another host with this machine's key registered: the newer one keeps
                // the device ID and this one steps aside, rather than the two taking it
                // from each other forever.
                Some(ServerBody::Error(e))
                    if e.session_id.is_empty()
                        && e.req_id == 0
                        && e.message.contains("registered again elsewhere") =>
                {
                    gateway.disconnect();
                    return Err(
                        "another nexa-term-host with this machine's key took over the \
                                device ID; this one stops"
                            .into(),
                    );
                }
                Some(ServerBody::Error(e)) if e.session_id.is_empty() && e.req_id == 0 => break,
                Some(_) => {}
                None => break,
            }
        }
        // A connection that lasted a while starts over quickly; one that keeps dropping
        // right away waits longer each time.
        if connected_at.elapsed() >= STABLE_AFTER {
            backoff = BACKOFF_MIN;
        }
        eprintln!("lost the server; reconnecting in {}s", backoff.as_secs());
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

fn group_code(code: &str) -> String {
    if code.len() == 8 {
        format!("{}-{}", &code[..4], &code[4..])
    } else {
        code.to_string()
    }
}

/// This machine's name, for browsers to save the connection under; empty if unknown.
fn machine_name() -> String {
    let name = if cfg!(windows) {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    } else {
        std::fs::read_to_string("/etc/hostname")
            .or_else(|_| std::fs::read_to_string("/proc/sys/kernel/hostname"))
            .unwrap_or_default()
    };
    name.trim().chars().take(64).collect()
}
