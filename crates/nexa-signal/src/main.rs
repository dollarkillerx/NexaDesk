//! `nexa-signal --config <file.toml>`; `--check-config <file.toml>` validates a
//! configuration and its certificate without binding anything; `--example-config`
//! prints a template. See docs/DEPLOY.md.

use std::path::{Path, PathBuf};

use nexa_signal::{Config, bind, config::EXAMPLE, start, tls_from_pem};

const USAGE: &str = "usage: nexa-signal --config <file.toml>\n       nexa-signal --check-config <file.toml>\n       nexa-signal --example-config";

enum Mode {
    Run(PathBuf),
    Check(PathBuf),
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut mode = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => mode = args.next().map(|p| Mode::Run(PathBuf::from(p))),
            "--check-config" => mode = args.next().map(|p| Mode::Check(PathBuf::from(p))),
            "--example-config" => {
                print!("{EXAMPLE}");
                return;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => {
                eprintln!("unknown argument: {other}\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let Some(mode) = mode else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let (Mode::Run(path) | Mode::Check(path)) = &mode;
    let config = match Config::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nexa-signal: {e}");
            std::process::exit(2);
        }
    };
    if let Mode::Check(path) = &mode {
        std::process::exit(check(path, &config));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result: Result<(), String> = runtime.block_on(async {
        let (settings, sockets) = bind(&config).await?;
        warn(&config, settings.tls.is_some());
        let server = start(settings, sockets).await.map_err(|e| e.to_string())?;
        eprintln!(
            "nexa-signal: signaling on {}{}, {}, STUN {:?}, relay {:?}",
            server.ws_addr,
            if config.signal.tls_cert.is_some() {
                " (TLS)"
            } else {
                ""
            },
            access(&config),
            config.stun_advertised(),
            config.relay_advertised()
        );
        stopped().await?;
        eprintln!("nexa-signal: stopping");
        server.shutdown();
        Ok(())
    });
    if let Err(e) = result {
        eprintln!("nexa-signal: {e}");
        std::process::exit(1);
    }
}

/// Everything `--config` would check before binding, plus the certificate.
fn check(path: &Path, config: &Config) -> i32 {
    let tls = match (&config.signal.tls_cert, &config.signal.tls_key) {
        (Some(c), Some(k)) => match tls_from_pem(c, k) {
            Ok(_) => true,
            Err(e) => {
                eprintln!("nexa-signal: TLS: {e}");
                return 1;
            }
        },
        _ => false,
    };
    warn(config, tls);
    println!(
        "{}: OK. signaling {}{}, {}, STUN {:?}, relay {:?}",
        path.display(),
        config.signal.listen,
        if tls { " (TLS)" } else { "" },
        access(config),
        config.stun_advertised(),
        config.relay_advertised()
    );
    0
}

fn access(config: &Config) -> String {
    let keys = match config.signal.access_secrets.len() {
        1 => "signaling key required".to_owned(),
        n => format!("signaling key required ({n} accepted)"),
    };
    match &config.web {
        None => keys,
        Some(w) if w.password.is_some() => format!("{keys}, browsers need their password"),
        Some(_) => format!("{keys}, browsers need no password"),
    }
}

fn warn(config: &Config, tls: bool) {
    if !tls && !config.signal.listen.ip().is_loopback() {
        eprintln!(
            "nexa-signal: warning: no TLS configured on {}; clients need wss://, and a TLS proxy in front hides their addresses from the rate limits (docs/DEPLOY.md)",
            config.signal.listen
        );
    }
    if let Some(note) = config.stun_note() {
        eprintln!("nexa-signal: NAT classification degraded: {note}");
    }
    if config.relay.is_none() {
        eprintln!("nexa-signal: no relay configured: peers behind two hard NATs cannot connect");
    }
    if config.web.as_ref().is_some_and(|w| w.password.is_none()) {
        eprintln!(
            "nexa-signal: note: [web] has no password, so anyone can open the page and use TURN to reach a device; what lets a browser into a device is still that device's own password, code or consent"
        );
    }
    if config.signal.server_names.is_empty() {
        eprintln!(
            "nexa-signal: warning: signal.server_names is empty, so registrations made for other servers are accepted"
        );
    }
}

/// Ctrl-C, or SIGTERM from systemd or `docker stop`.
async fn stopped() -> Result<(), String> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r.map_err(|e| e.to_string()),
            _ = term.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map_err(|e| e.to_string())
    }
}
