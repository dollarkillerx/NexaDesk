//! `nexa-mcp`: NexaDesk's MCP server over stdio (docs/MCP.md).
//!
//! ```json
//! { "mcpServers": { "nexadesk": { "command": "/path/to/nexa-mcp" } } }
//! ```
//!
//! Standard output carries only the protocol; diagnostics go to `mcp.log` in the data
//! directory.

#[cfg(target_os = "macos")]
fn main() {
    std::process::exit(run());
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("nexa-mcp runs on macOS for now");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
const USAGE: &str = "usage: nexa-mcp [--data-dir <dir>] [--name <device name>]

An MCP server (stdio) that lets an AI agent operate another computer through NexaDesk.
Its device key, pairings and settings are its own, apart from the NexaDesk app's:
  <dir>/device.key   this agent's identity (created on first run)
  <dir>/hosts.json   computers paired with, by address
  <dir>/config.json  {\"signal_server\": \"wss://…\", \"signal_secret\": \"…\"} for device IDs
  <dir>/mcp.log      diagnostics
Default <dir>: ~/Library/Application Support/NexaDesk-mcp";

#[cfg(target_os = "macos")]
fn run() -> i32 {
    use std::{path::PathBuf, sync::Arc};

    let mut data_dir: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data-dir" => data_dir = args.next().map(PathBuf::from),
            "--name" => name = args.next(),
            "--version" | "-V" => {
                println!("nexa-mcp {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return 0;
            }
            other => {
                eprintln!("unknown argument {other}\n\n{USAGE}");
                return 2;
            }
        }
    }
    let Some(data_dir) = data_dir.or_else(|| {
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library")
                .join("Application Support")
                .join("NexaDesk-mcp")
        })
    }) else {
        eprintln!("no HOME: give --data-dir");
        return 2;
    };
    nexa_media::diag::set_file(&data_dir.join("mcp.log"));
    nexa_media::diag!("nexa-mcp {} started", env!("CARGO_PKG_VERSION"));
    let name = name.unwrap_or_else(|| format!("{} (AI agent)", host_name()));
    let deps = Arc::new(nexa_client::headless::HeadlessDeps::new(
        data_dir.join("device.key"),
    ));
    let viewer = match nexa_mcp::viewer::Viewer::new(deps, data_dir, name) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("nexa-mcp could not start: {e}");
            return 1;
        }
    };
    let mut server = nexa_mcp::server::Server::new(viewer);
    let stdin = std::io::stdin();
    match server.serve(stdin.lock(), std::io::stdout()) {
        Ok(()) => 0,
        Err(e) => {
            nexa_media::diag!("nexa-mcp stopped: {e}");
            1
        }
    }
}

/// This Mac's name as the network knows it, without `.local`.
#[cfg(target_os = "macos")]
fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates within it
    // or truncates, and the last byte is never written.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
    let name = if ok {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    let name = name.trim_end_matches(".local").trim();
    if name.is_empty() {
        "This computer".into()
    } else {
        name.to_owned()
    }
}
