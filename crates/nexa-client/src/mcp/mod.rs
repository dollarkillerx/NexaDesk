//! NexaDesk's MCP server for AI agents (docs/MCP.md §16), inside the app: a local AI
//! app (Claude Code, Codex…) runs `nexa-mcp`, which relays its stdio to the socket
//! [`Service`] listens on, and operates the computers the person allowed it to.
//!
//! The AI never handles a password, code or key: it asks for a device by name, and
//! the [`Directory`](viewer::Directory) signs in with what the person saved in
//! NexaDesk. AI sessions run on a core of their own, which tells hosts it is an agent
//! and has a device key of its own, so the person's pairings (some of which let them
//! in without asking) are never the AI's.

pub mod app_directory;
pub mod keys;
pub mod listen;
pub mod server;
pub mod terminal;
pub mod viewer;

use std::{
    io::BufReader,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

use crate::{headless::HeadlessDeps, memclip::MemoryClipboard};

use self::{
    listen::Listener,
    server::Server,
    viewer::{AgentSession, Directory, Viewer},
};

/// Where the app serves AI apps, for its default data directory: what `nexa-mcp`
/// connects to.
pub fn default_endpoint() -> String {
    listen::endpoint(&crate::abi::default_data_dir())
}

/// The MCP server while it is on: the AI's core and the socket AI apps reach it on.
pub struct Service {
    viewer: Arc<Viewer>,
    listener: Option<Listener>,
    /// The AI apps connected now, to cut off when the person turns AI agents off.
    clients: Arc<Mutex<Vec<listen::Conn>>>,
}

impl Drop for Service {
    fn drop(&mut self) {
        drop(self.listener.take());
        for conn in lock(&self.clients).drain(..) {
            listen::hang_up(&conn);
        }
        self.viewer.disconnect_all();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Service {
    /// Serves on the socket for `data_dir` (the app's data directory), with the AI's
    /// own core in `data_dir/ai`, signing in through the app's saved connections.
    pub fn start(data_dir: &Path, device_name: String) -> Result<Self, String> {
        let directory = Arc::new(app_directory::AppDirectory::new(data_dir.to_path_buf()));
        Self::start_with(
            &listen::endpoint(data_dir),
            data_dir.join("ai"),
            device_name,
            directory,
        )
    }

    /// As [`start`](Self::start), with the socket, the AI's data and the directory
    /// given (tests use made-up ones).
    pub fn start_with(
        endpoint: &str,
        ai_dir: PathBuf,
        device_name: String,
        directory: Arc<dyn Directory>,
    ) -> Result<Self, String> {
        let clipboard = MemoryClipboard::new();
        let deps = Arc::new(HeadlessDeps::new(
            ai_dir.join("device.key"),
            clipboard.clone(),
        ));
        let viewer = Arc::new(Viewer::new(
            deps,
            clipboard,
            ai_dir,
            device_name,
            directory,
        )?);
        let next_client = Arc::new(AtomicU64::new(1));
        let clients: Arc<Mutex<Vec<listen::Conn>>> = Arc::default();
        let v = viewer.clone();
        let connected = clients.clone();
        let listener = listen::listen(endpoint, move |conn| {
            let client = next_client.fetch_add(1, Relaxed);
            let viewer = v.clone();
            if let Ok(c) = conn.try_clone() {
                lock(&connected).push(c);
            }
            let _ = std::thread::Builder::new()
                .name(format!("mcp-client-{client}"))
                .spawn(move || {
                    let Ok(reader) = conn.try_clone() else {
                        return;
                    };
                    let mut server = Server::new(viewer, client);
                    let _ = server.serve(BufReader::new(reader), conn);
                    // Dropping the server ends the client's sessions.
                });
        })?;
        Ok(Self {
            viewer,
            listener: Some(listener),
            clients,
        })
    }

    /// Every AI agent's sessions, for the app to show.
    pub fn sessions(&self) -> Vec<AgentSession> {
        self.viewer.agent_sessions()
    }

    /// Ends every AI agent's sessions.
    pub fn stop_sessions(&self) {
        self.viewer.disconnect_all();
    }
}
