//! `nexa-mcp`: NexaDesk's MCP server over stdio (docs/MCP.md).
//!
//! ```json
//! { "mcpServers": { "nexadesk": { "command": "/path/to/nexa-mcp" } } }
//! ```
//!
//! Standard output carries only the protocol; diagnostics go to `mcp.log` in the data
//! directory.

#[cfg(any(target_os = "macos", windows))]
fn main() {
    std::process::exit(app::run());
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("nexa-mcp runs on macOS and Windows");
    std::process::exit(2);
}

#[cfg(any(target_os = "macos", windows))]
mod app {
    use std::{
        io::Read,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use nexa_client::{headless::HeadlessDeps, memclip::MemoryClipboard};
    use nexa_mcp::{secret, server::Server, viewer::Viewer};

    #[cfg(target_os = "macos")]
    const DEFAULT_DIR: &str = "~/Library/Application Support/NexaDesk-mcp";
    #[cfg(windows)]
    const DEFAULT_DIR: &str = "%APPDATA%\\NexaDesk-mcp";

    fn usage() -> String {
        format!(
            "usage: nexa-mcp [--data-dir <dir>] [--name <device name>]
       nexa-mcp set-signal-secret [--data-dir <dir>]   (reads the key from standard input)
       nexa-mcp clear-signal-secret [--data-dir <dir>]

An MCP server (stdio) that lets an AI agent operate another computer through NexaDesk.
Its device key, pairings and settings are its own, apart from the NexaDesk app's:
  <dir>/device.key   this agent's identity (created on first run)
  <dir>/hosts.json   computers paired with, by address
  <dir>/config.json  {{\"signal_server\": \"wss://…\"}} to reach computers by device ID
  <dir>/mcp.log      diagnostics
The signaling server's access key, if it asks for one, is kept in {}: set it with
set-signal-secret.
Default <dir>: {DEFAULT_DIR}",
            if cfg!(windows) {
                "a file only this Windows user can decrypt (DPAPI)"
            } else {
                "the Keychain"
            }
        )
    }

    fn default_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            std::env::var_os("HOME").map(|h| {
                PathBuf::from(h)
                    .join("Library")
                    .join("Application Support")
                    .join("NexaDesk-mcp")
            })
        }
        #[cfg(windows)]
        {
            std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("NexaDesk-mcp"))
        }
    }

    enum Mode {
        Serve,
        SetSecret,
        ClearSecret,
    }

    pub fn run() -> i32 {
        let mut data_dir: Option<PathBuf> = None;
        let mut name: Option<String> = None;
        let mut mode = Mode::Serve;
        let mut args = std::env::args().skip(1).peekable();
        match args.peek().map(String::as_str) {
            Some("set-signal-secret") => mode = Mode::SetSecret,
            Some("clear-signal-secret") => mode = Mode::ClearSecret,
            _ => {}
        }
        if !matches!(mode, Mode::Serve) {
            args.next();
        }
        while let Some(a) = args.next() {
            match a.as_str() {
                "--data-dir" => data_dir = args.next().map(PathBuf::from),
                "--name" => name = args.next(),
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
        let Some(data_dir) = data_dir.or_else(default_dir) else {
            eprintln!("no home folder: give --data-dir");
            return 2;
        };
        match mode {
            Mode::Serve => serve(data_dir, name),
            Mode::SetSecret => set_secret(&data_dir),
            Mode::ClearSecret => match secret::clear(&*secret::store(&data_dir)) {
                Ok(()) => {
                    eprintln!("the signaling server's access key is forgotten");
                    0
                }
                Err(e) => {
                    eprintln!("could not forget the key: {e}");
                    1
                }
            },
        }
    }

    fn set_secret(data_dir: &Path) -> i32 {
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            eprintln!("data directory: {e}");
            return 1;
        }
        let mut key = String::new();
        if std::io::stdin()
            .take(4096)
            .read_to_string(&mut key)
            .is_err()
        {
            eprintln!("could not read the key from standard input");
            return 1;
        }
        match secret::save(&*secret::store(data_dir), &key) {
            Ok(()) => {
                eprintln!("the signaling server's access key is kept");
                0
            }
            Err(e) => {
                eprintln!("could not keep the key: {e}");
                1
            }
        }
    }

    fn serve(data_dir: PathBuf, name: Option<String>) -> i32 {
        nexa_media::diag::set_file(&data_dir.join("mcp.log"));
        nexa_media::diag!("nexa-mcp {} started", env!("CARGO_PKG_VERSION"));
        let name = name.unwrap_or_else(|| format!("{} (AI agent)", host_name()));
        let clipboard = MemoryClipboard::new();
        let deps = Arc::new(HeadlessDeps::new(
            data_dir.join("device.key"),
            clipboard.clone(),
        ));
        let store = secret::store(&data_dir);
        let viewer = match Viewer::new(deps, clipboard, data_dir, name) {
            Ok(v) => v.with_signal_secret(move || secret::load(&*store)),
            Err(e) => {
                eprintln!("nexa-mcp could not start: {e}");
                return 1;
            }
        };
        let mut server = Server::new(viewer);
        let stdin = std::io::stdin();
        match server.serve(stdin.lock(), std::io::stdout()) {
            Ok(()) => 0,
            Err(e) => {
                nexa_media::diag!("nexa-mcp stopped: {e}");
                1
            }
        }
    }

    /// This computer's name as the network knows it.
    #[cfg(target_os = "macos")]
    fn host_name() -> String {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname NUL-terminates within
        // it or truncates, and the last byte is never written.
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
        let name = if ok {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            String::new()
        };
        tidy(name.trim_end_matches(".local"))
    }

    #[cfg(windows)]
    fn host_name() -> String {
        tidy(&std::env::var("COMPUTERNAME").unwrap_or_default())
    }

    fn tidy(name: &str) -> String {
        let name = name.trim();
        if name.is_empty() {
            "This computer".into()
        } else {
            name.to_owned()
        }
    }
}
