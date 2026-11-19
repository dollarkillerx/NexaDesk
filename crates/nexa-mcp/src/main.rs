//! `nexa-mcp`: hands an AI app (Claude Code, Codex, Claude Desktop…) NexaDesk's MCP
//! server (docs/MCP.md §16). It only relays: MCP messages from standard input to the
//! socket the NexaDesk app serves on, and its answers back to standard output. It
//! holds no key, password or code; the app signs in to the computers the person
//! allowed AI agents to operate.

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("nexa-mcp runs on macOS and Windows");
    std::process::exit(2);
}

#[cfg(any(target_os = "macos", windows))]
fn main() {
    std::process::exit(relay::run());
}

#[cfg(any(target_os = "macos", windows))]
mod relay {
    use std::io::{Read, Write};

    use nexa_client::mcp::listen;

    fn usage() -> String {
        format!(
            "usage: nexa-mcp [--socket <path>]

Gives an AI app NexaDesk's MCP server: add this command to the app as an MCP server
(stdio). It relays to NexaDesk, which must be running with \"Let AI apps on this
computer use NexaDesk\" on (Settings → AI agents). The computers the AI may operate,
and how NexaDesk signs in to them, are set in NexaDesk; nothing secret passes here.
Default socket: {}",
            nexa_client::mcp::default_endpoint()
        )
    }

    pub fn run() -> i32 {
        let mut endpoint = nexa_client::mcp::default_endpoint();
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--socket" => match args.next() {
                    Some(s) => endpoint = s,
                    None => {
                        eprintln!("--socket needs a path\n\n{}", usage());
                        return 2;
                    }
                },
                "--version" | "-V" => {
                    println!("nexa-mcp {}", env!("CARGO_PKG_VERSION"));
                    return 0;
                }
                "--help" | "-h" => {
                    println!("{}", usage());
                    return 0;
                }
                other => {
                    eprintln!("unknown argument {other}\n\n{}", usage());
                    return 2;
                }
            }
        }
        let conn = match listen::connect(&endpoint) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "nexa-mcp: NexaDesk is not serving AI agents ({e}). Open NexaDesk and turn \
                     on \"Let AI apps on this computer use NexaDesk\" in Settings → AI agents."
                );
                return 3;
            }
        };
        let Ok(mut from_app) = conn.try_clone() else {
            eprintln!("nexa-mcp: could not use the connection to NexaDesk");
            return 1;
        };
        // NexaDesk → the AI app, until NexaDesk closes the connection (it quit, or the
        // person turned AI agents off): then this relay ends too.
        std::thread::spawn(move || {
            let mut out = std::io::stdout();
            let mut buf = [0u8; 16 << 10];
            loop {
                match from_app.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out.write_all(&buf[..n]).and_then(|()| out.flush()).is_err() {
                            break;
                        }
                    }
                }
            }
            std::process::exit(0);
        });
        // The AI app → NexaDesk, until the AI app closes standard input: then the
        // connection closes too, and NexaDesk ends the sessions it opened.
        let mut to_app = conn;
        let mut input = std::io::stdin();
        let mut buf = [0u8; 16 << 10];
        loop {
            match input.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_app.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        #[cfg(unix)]
        let _ = to_app.shutdown(std::net::Shutdown::Both);
        0
    }
}
