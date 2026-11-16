//! `nexa-agent`: shares this Mac's main display with one paired viewer at a time.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("nexa-agent currently runs on macOS only");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
mod real;
#[cfg(target_os = "macos")]
mod terminal;

#[cfg(target_os = "macos")]
fn main() {
    // The fixed password is set and cleared by these, which the app runs; they do not
    // start a host.
    match std::env::args().nth(1).as_deref() {
        Some("set-password") => return finish(set_password()),
        Some("clear-password") => return finish(clear_password()),
        Some("reset-device-id") => return finish(reset_device_id()),
        _ => {}
    }
    if let Err(message) = run() {
        eprintln!("nexa-agent: {message}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn finish(r: Result<(), String>) {
    if let Err(message) = r {
        eprintln!("nexa-agent: {message}");
        std::process::exit(1);
    }
}

/// Where the fixed password's Argon2id verifier is kept: never the password itself.
#[cfg(target_os = "macos")]
fn password_store() -> nexa_auth::KeychainStore {
    nexa_auth::KeychainStore::new("app.nexadesk", "host-password")
}

/// The password must be one a person can type, and must not look like a one-time code:
/// what someone types is a code if it can be one.
#[cfg(target_os = "macos")]
fn check_password(p: &str) -> Result<(), String> {
    let n = p.chars().count();
    if n < 8 {
        return Err("the password must be at least 8 characters".into());
    }
    if n > 128 {
        return Err("the password must be at most 128 characters".into());
    }
    if p.chars().any(char::is_control) {
        return Err("the password cannot contain control characters".into());
    }
    if nexa_auth::normalize_code(p).is_ok() {
        return Err("the password looks like a one-time code; choose another".into());
    }
    Ok(())
}

/// `nexa-agent set-password`: reads one line from standard input.
#[cfg(target_os = "macos")]
fn set_password() -> Result<(), String> {
    use nexa_auth::KeyStore;
    let mut line = zeroize::Zeroizing::new(String::new());
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("reading the password: {e}"))?;
    let password = line.trim_end_matches(['\r', '\n']);
    check_password(password)?;
    let stored = nexa_auth::HostPassword::create(password).map_err(|e| e.to_string())?;
    password_store()
        .store(&stored.to_bytes())
        .map_err(|e| format!("keychain: {e}"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn clear_password() -> Result<(), String> {
    use nexa_auth::KeyStore;
    password_store()
        .delete()
        .map_err(|e| format!("keychain: {e}"))
}

/// Mixed into the device ID the signaling server derives from this Mac's key: a new
/// seed is a new ID. Kept beside the pairings; without it the ID is the key's own.
#[cfg(target_os = "macos")]
const ID_SEED_FILE: &str = "device-id-seed";

#[cfg(target_os = "macos")]
fn default_data_dir() -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(std::path::Path::new(&home).join("Library/Application Support/NexaDesk"))
}

/// `reset-device-id [--data-dir <dir>]`: the next start registers under a new device ID,
/// and the old one stops working.
#[cfg(target_os = "macos")]
fn reset_device_id() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let dir = match args.as_slice() {
        [] => default_data_dir()?,
        [flag, dir] if flag == "--data-dir" => std::path::PathBuf::from(dir),
        _ => return Err("usage: nexa-agent reset-device-id [--data-dir <dir>]".into()),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("data directory: {e}"))?;
    let mut seed = [0u8; 16];
    // SAFETY: fills exactly the 16 bytes it is given.
    unsafe { libc::arc4random_buf(seed.as_mut_ptr().cast(), seed.len()) };
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(dir.join(ID_SEED_FILE), hex + "\n")
        .map_err(|e| format!("saving the new device ID: {e}"))
}

/// The seed saved by `reset-device-id`, or none.
#[cfg(target_os = "macos")]
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

/// The fixed password, if one was set.
#[cfg(target_os = "macos")]
fn load_password() -> Result<Option<std::sync::Arc<nexa_auth::HostPassword>>, String> {
    use nexa_auth::KeyStore;
    let Some(bytes) = password_store()
        .load()
        .map_err(|e| format!("keychain: {e}"))?
    else {
        return Ok(None);
    };
    nexa_auth::HostPassword::from_bytes(&bytes)
        .map(|p| Some(std::sync::Arc::new(p)))
        .map_err(|e| format!("stored password: {e}"))
}

/// The page's certificate: the person's own in the data directory, else one this Mac
/// makes for its addresses, its Tailscale name and its `.local` name.
#[cfg(target_os = "macos")]
fn web_certificate(data_dir: &std::path::Path) -> std::io::Result<nexa_web::WebTls> {
    let mut names = nexa_web::certificate_names();
    if let Some(local) = std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    {
        names.push(format!("{local}.local"));
    }
    nexa_web::WebTls::load_or_create(data_dir, &names)
}

#[cfg(target_os = "macos")]
const USAGE: &str = "usage: nexa-agent --listen <ip:port> [--name <device name>] [--data-dir <dir>] [--consent terminal|ui]\n\
                 [--signal <wss://server[:port]>] [--signal-ca <pem>] [--paired-only] [--relay-only]\n\
                 [--downloads <dir>] [--no-lan-discovery] [--web <ip:port>]\n\
  --listen    UDP address to listen on; use a LAN or Tailscale address, or 127.0.0.1\n\
  --name      name shown to viewers (default: this Mac's host name)\n\
  --data-dir  where pairings.json and agent.sock live (default: ~/Library/Application Support/NexaDesk)\n\
  --consent   who allows connections: the controlling terminal (default), or the NexaDesk\n\
              app over <data-dir>/agent.sock, falling back to the terminal while no app is\n\
              connected. With neither, every connection is refused.\n\
  --signal    also be reachable by device ID through this nexa-signal server; the ID is shown\n\
              once registered. Pairing and consent work exactly as for --listen.\n\
  --signal-ca extra CA certificate(s) for the server (default: <data-dir>/signal-ca.pem if present)\n\
  --paired-only  only devices already paired may make this Mac reveal its addresses\n\
  --relay-only   never offer a direct address to connecting devices; always use the relay.\n\
                 Also turns off the LAN announcement.\n\
  --no-lan-discovery  do not announce this Mac to viewers on the local network (mDNS)\n\
  --web       also let web browsers connect: serve the page and WebRTC on this TCP address\n\
              (e.g. [::]:47801); the same one-time code and consent apply. Off by default.\n\
  --downloads where files sent to this Mac are saved (default: ~/Downloads/NexaDesk)\n\
\n\
       nexa-agent set-password      read a fixed password from standard input and keep it\n\
       nexa-agent clear-password    forget it\n\
  With a fixed password set, viewers and browsers may type it instead of the one-time\n\
  code; every connection is still put to the person at this Mac.";

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConsentMode {
    Terminal,
    Ui,
}

#[cfg(target_os = "macos")]
struct Args {
    listen: std::net::SocketAddr,
    name: String,
    data_dir: std::path::PathBuf,
    consent: ConsentMode,
    signal: Option<String>,
    signal_ca: Option<std::path::PathBuf>,
    paired_only: bool,
    relay_only: bool,
    lan_discovery: bool,
    downloads: std::path::PathBuf,
    web: Option<std::net::SocketAddr>,
}

#[cfg(target_os = "macos")]
fn parse_args() -> Result<Args, String> {
    let mut listen = None;
    let mut name = None;
    let mut data_dir = None;
    let mut consent = ConsentMode::Terminal;
    let mut signal = None;
    let mut signal_ca = None;
    let mut paired_only = false;
    let mut relay_only = false;
    let mut lan_discovery = true;
    let mut web = None;
    let mut downloads = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--listen" => {
                let raw = value()?;
                listen = Some(
                    raw.parse()
                        .map_err(|_| format!("bad address: {raw}\n{USAGE}"))?,
                );
            }
            "--name" => name = Some(value()?),
            "--data-dir" => data_dir = Some(std::path::PathBuf::from(value()?)),
            "--consent" => {
                consent = match value()?.as_str() {
                    "terminal" => ConsentMode::Terminal,
                    "ui" => ConsentMode::Ui,
                    other => return Err(format!("unknown consent mode: {other}\n{USAGE}")),
                }
            }
            "--signal" => signal = Some(value()?).filter(|s| !s.trim().is_empty()),
            "--signal-ca" => signal_ca = Some(std::path::PathBuf::from(value()?)),
            "--paired-only" => paired_only = true,
            "--relay-only" => relay_only = true,
            "--no-lan-discovery" => lan_discovery = false,
            "--web" => {
                web = Some(
                    value()?
                        .parse()
                        .map_err(|_| format!("--web needs an ip:port\n{USAGE}"))?,
                )
            }
            "--downloads" => downloads = Some(std::path::PathBuf::from(value()?)),
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument: {other}\n{USAGE}")),
        }
    }
    let listen = listen.ok_or_else(|| format!("--listen is required\n{USAGE}"))?;
    let name = name.unwrap_or_else(|| {
        std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Mac".to_owned())
    });
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let data_dir = match data_dir {
        Some(d) => d,
        None => default_data_dir()?,
    };
    let downloads =
        downloads.unwrap_or_else(|| std::path::Path::new(&home).join("Downloads/NexaDesk"));
    Ok(Args {
        listen,
        name,
        data_dir,
        consent,
        signal,
        signal_ca,
        paired_only,
        relay_only,
        lan_discovery: lan_discovery && !relay_only,
        downloads,
        // A browser connects directly to an address of this Mac: not with --relay-only.
        web: web.filter(|_| !relay_only),
    })
}

/// The agent runs from inside the app's bundle, so its first AppKit call (the pasteboard,
/// screen capture) would give it a Dock icon of its own beside the app's. As an accessory
/// it stays out of the Dock and the app switcher.
#[cfg(target_os = "macos")]
fn stay_out_of_the_dock() {
    use objc2::{
        msg_send,
        runtime::{AnyClass, AnyObject},
    };
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}
    /// NSApplicationActivationPolicyAccessory.
    const ACCESSORY: isize = 1;
    let Some(class) = AnyClass::get(c"NSApplication") else {
        return;
    };
    // SAFETY: called on the main thread before anything else touches AppKit;
    // `sharedApplication` and `setActivationPolicy:` take and return what is declared.
    unsafe {
        let app: *mut AnyObject = msg_send![class, sharedApplication];
        if !app.is_null() {
            let _: bool = msg_send![app, setActivationPolicy: ACCESSORY];
        }
    }
}

#[cfg(target_os = "macos")]
fn run() -> Result<(), String> {
    stay_out_of_the_dock();
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering::Relaxed},
    };

    use nexa_agent::{AgentConfig, AgentUi, ipc::IpcConfig, start, start_with_ui};
    use nexa_auth::{DeviceIdentity, KeychainStore, PairingStore};
    use nexa_input::platform::macos::{FocusWatcher, accessibility_granted};
    use nexa_media::macos::{cached_caps, has_screen_capture_permission, probe_caps};
    use nexa_session::handshake::Timeouts;
    use nexa_wire::control::{FocusContext, Os};
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};
    use terminal::{Command, TerminalUi};

    let args = parse_args()?;

    if !has_screen_capture_permission() {
        return Err(
            "Screen Recording permission is missing. Grant it to this program (or the terminal \
             running it) in System Settings > Privacy & Security > Screen Recording, then start again."
                .to_owned(),
        );
    }
    if !accessibility_granted() {
        eprintln!(
            "nexa-agent: Accessibility permission is missing, so viewers cannot be given keyboard or mouse control."
        );
    }

    std::fs::create_dir_all(&args.data_dir).map_err(|e| format!("data directory: {e}"))?;
    let identity =
        DeviceIdentity::load_or_create(&KeychainStore::new("app.nexadesk", "agent-device-key"))
            .map_err(|e| format!("device key: {e}"))?;
    let store = PairingStore::open(
        args.data_dir.join("pairings.json"),
        identity.store_mac_key(),
    )
    .map_err(|e| format!("pairing store (fix or remove it to continue): {e}"))?;
    // §3.5: probed with real sessions once per OS build and GPU, then cached.
    let caps = match nexa_media::caps_cache::default_path() {
        Some(path) => cached_caps(&path),
        None => probe_caps(),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("net")
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    let quit = Arc::new(AtomicBool::new(false));
    let commands: Arc<Mutex<Option<Arc<nexa_agent::Agent>>>> = Arc::default();
    let ui: Arc<TerminalUi> = Arc::new({
        let (commands, quit) = (commands.clone(), quit.clone());
        TerminalUi::open(move |command| {
            let agent = commands.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let Some(agent) = agent else { return };
            match command {
                Command::Disconnect => agent.disconnect(),
                Command::RotateCode => agent.rotate_code(),
                Command::Quit => {
                    agent.shutdown();
                    quit.store(true, Relaxed);
                }
            }
        })
    });
    let signal = match &args.signal {
        None => None,
        Some(url) => {
            nexa_session::rendezvous::signal::parse_url(url).map_err(|e| e.to_string())?;
            let mut server = nexa_session::rendezvous::signal::SignalConfig::new(url.clone());
            let ca = args
                .signal_ca
                .clone()
                .or_else(|| Some(args.data_dir.join("signal-ca.pem")).filter(|p| p.exists()));
            if let Some(ca) = ca {
                server = server.with_ca_file(&ca)?;
            }
            server.id_seed = load_id_seed(&args.data_dir);
            Some(nexa_agent::SignalSetup {
                server,
                // Session sockets stay on the interface --listen names; all
                // interfaces means dual-stack (IPv4 only where there is no IPv6).
                bind_ip: if args.listen.ip().is_unspecified() {
                    std::net::Ipv6Addr::UNSPECIFIED.into()
                } else {
                    args.listen.ip()
                },
                interfaces: None,
                paired_only: args.paired_only,
                relay_only: args.relay_only,
                system_services: true,
            })
        }
    };
    // Browser access is served over HTTPS only. Without a certificate it stays off,
    // rather than falling back to plain HTTP.
    let (web, web_tls) = match args.web {
        Some(addr) => match web_certificate(&args.data_dir) {
            Ok(tls) => {
                eprintln!(
                    "nexa-agent: browsers: https://<this Mac>:{} — certificate SHA-256 {}{}",
                    addr.port(),
                    tls.fingerprint_text(),
                    if tls.custom {
                        " (yours)"
                    } else {
                        " (self-signed)"
                    }
                );
                (Some(addr), Some(tls))
            }
            Err(e) => {
                eprintln!("nexa-agent: browser access is off: certificate: {e}");
                (None, None)
            }
        },
        None => (None, None),
    };
    let config = AgentConfig {
        listen: args.listen,
        device_name: args.name,
        os: Os::Macos,
        caps,
        timeouts: Timeouts::default(),
        signal,
        files: nexa_agent::FileSettings {
            downloads: args.downloads.clone(),
            limits: Default::default(),
        },
        lan_discovery: args.lan_discovery,
        web,
        web_tls,
        password: load_password()?,
    };
    // Held until exit: dropping it removes the socket.
    let mut _ipc = None;
    let agent = match args.consent {
        ConsentMode::Terminal => {
            if !ui.has_terminal() {
                eprintln!("nexa-agent: no controlling terminal; every connection will be refused");
            }
            Arc::new(
                runtime
                    .block_on(start(
                        config,
                        identity,
                        store,
                        Arc::new(real::RealPlatform),
                        ui.clone() as Arc<dyn AgentUi>,
                    ))
                    .map_err(|e| e.to_string())?,
            )
        }
        ConsentMode::Ui => {
            let fallback = ui.has_terminal().then(|| ui.clone() as Arc<dyn AgentUi>);
            let on_shutdown: Arc<dyn Fn() + Send + Sync> = {
                let (commands, quit) = (commands.clone(), quit.clone());
                Arc::new(move || {
                    let agent = commands.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if let Some(agent) = agent {
                        agent.shutdown();
                    }
                    quit.store(true, Relaxed);
                })
            };
            let host = runtime
                .block_on(start_with_ui(
                    config,
                    identity,
                    store,
                    Arc::new(real::RealPlatform),
                    fallback,
                    IpcConfig::in_dir(&args.data_dir),
                    on_shutdown,
                ))
                .map_err(|e| e.to_string())?;
            let agent = host.agent.clone();
            _ipc = Some(host);
            agent
        }
    };
    *commands.lock().unwrap_or_else(|e| e.into_inner()) = Some(agent.clone());

    {
        let (agent, quit) = (agent.clone(), quit.clone());
        runtime.spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut term), Ok(mut int)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
            ) else {
                return;
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            agent.shutdown();
            quit.store(true, Relaxed);
        });
    }

    // The main thread belongs to AppKit: FocusWatcher notifications and pasteboard
    // promise callbacks both arrive through its run loop.
    let extra_terminals: Vec<String> = Vec::new();
    if let Some((class, id)) = FocusWatcher::current(&extra_terminals) {
        agent.set_focus(FocusContext {
            cls: class as i32,
            app_id: id,
        });
    }
    let watcher = {
        let agent = agent.clone();
        FocusWatcher::start(extra_terminals, move |class, id| {
            agent.set_focus(FocusContext {
                cls: class as i32,
                app_id: id.to_owned(),
            });
        })
    };
    let run_loop = NSRunLoop::currentRunLoop();
    while !quit.load(Relaxed) {
        let until = NSDate::dateWithTimeIntervalSinceNow(0.2);
        // SAFETY: the mode is a Foundation constant and the date is valid.
        unsafe {
            run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &until);
        }
    }
    drop(watcher);
    runtime.block_on(agent.wait());
    drop(_ipc);
    Ok(())
}
