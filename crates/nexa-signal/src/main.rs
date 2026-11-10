//! `nexa-signal --config <file.toml>`; `--example-config` prints a template.

use std::path::PathBuf;

use nexa_signal::{Config, bind, config::EXAMPLE, start};

const USAGE: &str = "usage: nexa-signal --config <file.toml>\n       nexa-signal --example-config";

fn main() {
    let mut args = std::env::args().skip(1);
    let mut config_path: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config_path = args.next().map(PathBuf::from),
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
    let Some(path) = config_path else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let config = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nexa-signal: {e}");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result: Result<(), String> = runtime.block_on(async {
        let (settings, sockets) = bind(&config).await?;
        if settings.tls.is_none() && !config.signal.listen.ip().is_loopback() {
            eprintln!(
                "nexa-signal: warning: no TLS configured on {}; only run like this behind a TLS proxy",
                config.signal.listen
            );
        }
        if let Some(note) = config.stun_note() {
            eprintln!("nexa-signal: NAT classification degraded: {note}");
        }
        if config.relay.is_none() {
            eprintln!("nexa-signal: no relay configured: peers behind two hard NATs cannot connect");
        }
        if config.signal.server_names.is_empty() {
            eprintln!("nexa-signal: warning: signal.server_names is empty, so registrations made for other servers are accepted");
        }
        let server = start(settings, sockets).await.map_err(|e| e.to_string())?;
        eprintln!(
            "nexa-signal: signaling on {}{}, STUN {:?}, relay {:?}",
            server.ws_addr,
            if config.signal.tls_cert.is_some() { " (TLS)" } else { "" },
            config.stun_advertised(),
            config.relay_advertised()
        );
        tokio::signal::ctrl_c().await.map_err(|e| e.to_string())?;
        server.shutdown();
        Ok(())
    });
    if let Err(e) = result {
        eprintln!("nexa-signal: {e}");
        std::process::exit(1);
    }
}
