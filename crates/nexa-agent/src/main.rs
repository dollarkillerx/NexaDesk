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
    if let Err(message) = run() {
        eprintln!("nexa-agent: {message}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
const USAGE: &str = "usage: nexa-agent --listen <ip:port> [--name <device name>] [--data-dir <dir>] [--consent terminal|ui]\n\
  --listen    UDP address to listen on; use a LAN or Tailscale address, or 127.0.0.1\n\
  --name      name shown to viewers (default: this Mac's host name)\n\
  --data-dir  where pairings.json and agent.sock live (default: ~/Library/Application Support/NexaDesk)\n\
  --consent   who allows connections: the controlling terminal (default), or the NexaDesk\n\
              app over <data-dir>/agent.sock, falling back to the terminal while no app is\n\
              connected. With neither, every connection is refused.";

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
}

#[cfg(target_os = "macos")]
fn parse_args() -> Result<Args, String> {
    let mut listen = None;
    let mut name = None;
    let mut data_dir = None;
    let mut consent = ConsentMode::Terminal;
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
    let data_dir = match data_dir {
        Some(d) => d,
        None => {
            let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
            std::path::Path::new(&home).join("Library/Application Support/NexaDesk")
        }
    };
    Ok(Args {
        listen,
        name,
        data_dir,
        consent,
    })
}

#[cfg(target_os = "macos")]
fn run() -> Result<(), String> {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering::Relaxed},
    };

    use nexa_agent::{AgentConfig, AgentUi, ipc::IpcConfig, start, start_with_ui};
    use nexa_auth::{DeviceIdentity, KeychainStore, PairingStore};
    use nexa_input::platform::macos::{FocusWatcher, accessibility_granted};
    use nexa_media::macos::{has_screen_capture_permission, probe_caps};
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
    let caps = probe_caps();

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
    let config = AgentConfig {
        listen: args.listen,
        device_name: args.name,
        os: Os::Macos,
        caps,
        timeouts: Timeouts::default(),
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
