//! The accept loop and the single session slot (§5.3).

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::Duration,
};

use nexa_auth::{
    AccessPolicy, DeviceId, DeviceIdentity, DevicePublicKey, OneTimeCode, PairingRecord,
    PairingStore, PeerPolicy, Perms, TlsError, server_config,
};
use nexa_net::vsock::bind_media_socket;
use nexa_session::{
    handshake::{HostAccepted, HostConfig, HostSecrets, PairLimiter, Timeouts, host_handshake},
    proto::CloseReason,
};
use nexa_wire::control::{Caps, Display, FocusContext, Os};
use quinn::{Connection, Endpoint, EndpointConfig, TokioRuntime};
use thiserror::Error;
use tokio::sync::{Semaphore, watch};
use zeroize::Zeroizing;

use crate::{
    AgentEvent, AgentUi, Platform, SessionInfo,
    session::{self, SessionEnv},
};

/// Connections being authenticated at once. Beyond this new ones are refused outright.
const MAX_HANDSHAKES: usize = 8;
const REPLACE_WAIT: Duration = Duration::from_secs(5);
const CLOSE_LINGER: Duration = Duration::from_millis(500);

pub struct AgentConfig {
    pub listen: SocketAddr,
    pub device_name: String,
    pub os: Os,
    pub caps: Caps,
    pub timeouts: Timeouts,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("cannot listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error("could not describe the display: {0}")]
    Screen(String),
}

struct ActiveSession {
    id: u64,
    info: SessionInfo,
    stop: watch::Sender<Option<CloseReason>>,
    done: watch::Receiver<bool>,
}

struct Shared {
    env: Arc<SessionEnv>,
    host: HostConfig,
    ui: Arc<dyn AgentUi>,
    code: OneTimeCode,
    shown_code: Mutex<String>,
    slot: Mutex<Option<ActiveSession>>,
    /// Keeps two accepted connections from replacing the old session at once.
    replacing: tokio::sync::Mutex<()>,
    next_id: AtomicU64,
    shutdown: watch::Sender<bool>,
}

impl Shared {
    fn report_code(&self) {
        let code = self.code.current();
        let mut shown = self.shown_code.lock().unwrap_or_else(|e| e.into_inner());
        if *shown != *code.as_str() {
            shown.clear();
            shown.push_str(code.as_str());
            drop(shown);
            self.ui.event(AgentEvent::CodeChanged(code.to_string()));
        }
    }

    fn session_active(&self) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    fn end_active(&self, reason: CloseReason) {
        self.end_active_if(reason, |_| true);
    }

    fn end_active_if(&self, reason: CloseReason, f: impl Fn(&SessionInfo) -> bool) {
        let slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = slot.as_ref()
            && f(&active.info)
        {
            let _ = active.stop.send(Some(reason));
        }
    }

    fn store(&self) -> std::sync::MutexGuard<'_, PairingStore> {
        self.host.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ends whatever is running, waits for it, and installs the new session.
    async fn take_slot(
        &self,
        info: SessionInfo,
        stop: watch::Sender<Option<CloseReason>>,
    ) -> watch::Sender<bool> {
        let _guard = self.replacing.lock().await;
        let old = {
            let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            slot.take()
        };
        if let Some(old) = old {
            let _ = old.stop.send(Some(CloseReason::Replaced));
            let mut done = old.done;
            let _ = tokio::time::timeout(REPLACE_WAIT, done.wait_for(|d| *d)).await;
        }
        let (done_tx, done_rx) = watch::channel(false);
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(ActiveSession {
            id: info.id,
            info,
            stop,
            done: done_rx,
        });
        done_tx
    }

    fn clear_slot(&self, id: u64) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|s| s.id == id) {
            *slot = None;
        }
    }
}

/// A running agent. Dropping it does not stop it; call [`Agent::shutdown`].
pub struct Agent {
    shared: Arc<Shared>,
    name: String,
    local: SocketAddr,
    done: watch::Receiver<bool>,
    focus: watch::Sender<Option<FocusContext>>,
}

impl Agent {
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub fn device_id(&self) -> DeviceId {
        self.shared.host.me.device_id()
    }

    pub fn public_key(&self) -> DevicePublicKey {
        self.shared.host.me
    }

    pub fn session_active(&self) -> bool {
        self.shared.session_active()
    }

    /// The running (or starting) session, as shown to the person at the host.
    pub fn active_session(&self) -> Option<SessionInfo> {
        let slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map(|s| s.info.clone())
    }

    pub fn device_name(&self) -> &str {
        &self.name
    }

    /// Whether input could be granted right now (macOS: Accessibility).
    pub fn input_available(&self) -> bool {
        self.shared.env.platform.input_available()
    }

    pub fn pairings(&self) -> Vec<PairingRecord> {
        self.shared.store().records().to_vec()
    }

    /// Forgets a paired device and ends its session if it has one.
    pub fn revoke_pairing(&self, key: &DevicePublicKey) -> Result<bool, String> {
        let removed = self.shared.store().revoke(key).map_err(|e| e.to_string())?;
        let hex = key.to_hex();
        self.shared
            .end_active_if(CloseReason::HostEnded, |s| s.device_key == hex);
        Ok(removed)
    }

    /// Narrows what a paired device may do: `unattended` can only be switched off and
    /// permissions only removed. Widening needs the consent prompt. Returns false if
    /// the device is not paired. A running session that now has more than allowed
    /// is ended.
    pub fn restrict_pairing(
        &self,
        key: &DevicePublicKey,
        unattended: bool,
        perms: Perms,
    ) -> Result<bool, String> {
        let narrowed = {
            let mut store = self.shared.store();
            let Some(current) = store.pinned(key).map(|p| p.record().clone()) else {
                return Ok(false);
            };
            if unattended && !current.unattended {
                return Err(
                    "connecting without asking can only be turned on when allowing a connection"
                        .into(),
                );
            }
            let c = current.perms;
            let narrowed = Perms {
                view: c.view && perms.view,
                input: c.input && perms.input,
                clipboard: c.clipboard && perms.clipboard,
                file: c.file && perms.file,
                audio: c.audio && perms.audio,
            };
            store.set_perms(key, narrowed).map_err(|e| e.to_string())?;
            store
                .set_unattended(key, unattended)
                .map_err(|e| e.to_string())?;
            narrowed
        };
        let hex = key.to_hex();
        self.shared.end_active_if(CloseReason::HostEnded, |s| {
            s.device_key == hex
                && ((s.perms.view && !narrowed.view)
                    || (s.perms.input && !narrowed.input)
                    || (s.perms.clipboard && !narrowed.clipboard))
        });
        Ok(true)
    }

    /// The code to display to the person at the host.
    pub fn current_code(&self) -> Zeroizing<String> {
        self.shared.code.current()
    }

    /// Replaces the code, ending any exchange that used the old one.
    pub fn rotate_code(&self) {
        self.shared.code.rotate();
        self.shared.report_code();
    }

    /// The host's way out: ends the running session.
    pub fn disconnect(&self) {
        self.shared.end_active(CloseReason::HostEnded);
    }

    /// Feeds the frontmost-application context to whichever session is running.
    pub fn set_focus(&self, focus: FocusContext) {
        self.focus.send_replace(Some(focus));
    }

    /// Stops listening and ends the session.
    pub fn shutdown(&self) {
        let _ = self.shared.shutdown.send(true);
        self.shared.end_active(CloseReason::Shutdown);
    }

    /// Completes once the agent has shut down and closed its session.
    pub async fn wait(&self) {
        let mut done = self.done.clone();
        let _ = done.wait_for(|d| *d).await;
    }
}

/// Binds the UDP port and starts accepting. Must run inside a multi-threaded tokio runtime.
pub async fn start(
    config: AgentConfig,
    identity: DeviceIdentity,
    store: PairingStore,
    platform: Arc<dyn Platform>,
    ui: Arc<dyn AgentUi>,
) -> Result<Agent, AgentError> {
    // §9.2: anyone may complete the TLS handshake; nothing is granted by doing so.
    let server = server_config(&identity, PeerPolicy::AcceptUnknown)?;
    let socket = bind_media_socket(config.listen).map_err(|source| AgentError::Bind {
        addr: config.listen,
        source,
    })?;
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(server),
        socket,
        Arc::new(TokioRuntime),
    )
    .map_err(|source| AgentError::Bind {
        addr: config.listen,
        source,
    })?;
    let local = endpoint.local_addr().map_err(|source| AgentError::Bind {
        addr: config.listen,
        source,
    })?;

    let screen = platform.screen().map_err(AgentError::Screen)?;
    let (focus_tx, focus_rx) = watch::channel(None);
    let code = OneTimeCode::generate();
    let (shutdown, _) = watch::channel(false);
    let env = Arc::new(SessionEnv {
        platform: platform.clone(),
        ui: ui.clone(),
        focus: focus_rx,
        caps: config.caps.clone(),
        device: nexa_clip::DeviceId(*identity.device_id().as_bytes()),
    });
    let shared = Arc::new(Shared {
        env,
        host: HostConfig {
            me: identity.public_key(),
            store: Arc::new(Mutex::new(store)),
            secrets: HostSecrets {
                code: code.clone(),
                password: None,
            },
            policy: AccessPolicy::default(),
            consent: ui.clone(),
            limiter: PairLimiter::default(),
            input_available: {
                let platform = platform.clone();
                Arc::new(move || platform.input_available())
            },
            caps: config.caps,
            os: config.os,
            displays: vec![Display {
                // Pointer.display is one byte, so this is an index, not a CGDirectDisplayID.
                id: 0,
                x: 0,
                y: 0,
                w: screen.size_pixels.0,
                h: screen.size_pixels.1,
                // Shown by the viewer; SessionAccept has no field for the host's name.
                name: config.device_name.clone(),
                refresh_hz: screen.refresh_hz,
            }],
            timeouts: config.timeouts,
        },
        ui: ui.clone(),
        code,
        shown_code: Mutex::new(String::new()),
        slot: Mutex::new(None),
        replacing: tokio::sync::Mutex::new(()),
        next_id: AtomicU64::new(1),
        shutdown,
    });

    ui.event(AgentEvent::Listening(local));
    shared.report_code();
    let (done_tx, done) = watch::channel(false);
    tokio::spawn(accept_loop(endpoint, shared.clone(), done_tx));
    Ok(Agent {
        shared,
        name: config.device_name,
        local,
        done,
        focus: focus_tx,
    })
}

async fn accept_loop(endpoint: Endpoint, shared: Arc<Shared>, done: watch::Sender<bool>) {
    let limit = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    let mut shutdown = shared.shutdown.subscribe();
    loop {
        let incoming = tokio::select! {
            incoming = endpoint.accept() => incoming,
            _ = shutdown.wait_for(|s| *s) => None,
        };
        let Some(incoming) = incoming else { break };
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            incoming.refuse();
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    shared.ui.event(AgentEvent::ConnectionFailed {
                        remote,
                        reason: e.to_string(),
                    });
                    return;
                }
            };
            handle_connection(conn, shared, permit).await;
        });
    }
    endpoint.close(0u32.into(), b"agent stopped");
    // Let the session finish and the close frame leave.
    while shared.session_active() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = tokio::time::timeout(CLOSE_LINGER, endpoint.wait_idle()).await;
    let _ = done.send(true);
}

async fn handle_connection(
    conn: Connection,
    shared: Arc<Shared>,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let remote = conn.remote_address();
    let result = {
        let probe = shared.clone();
        let active = move || probe.session_active();
        host_handshake(&conn, &shared.host, &active).await
    };
    shared.report_code();
    let accepted = match result {
        Ok(a) => a,
        Err(e) => {
            shared.ui.event(AgentEvent::ConnectionFailed {
                remote,
                reason: e.to_string(),
            });
            conn.close(1u32.into(), b"refused");
            return;
        }
    };
    drop(permit);
    if *shared.shutdown.borrow() {
        conn.close(1u32.into(), b"shutting down");
        return;
    }
    run_session(conn, accepted, shared).await;
}

async fn run_session(conn: Connection, accepted: HostAccepted, shared: Arc<Shared>) {
    let id = shared.next_id.fetch_add(1, Relaxed);
    let (stop_tx, stop_rx) = watch::channel(None);
    let info = SessionInfo {
        id,
        device_name: accepted.device_name.clone(),
        device_id: accepted.peer.device_id().to_string(),
        device_key: accepted.peer.to_hex(),
        remote: conn.remote_address(),
        perms: accepted.perms.clone(),
    };
    let done = shared.take_slot(info.clone(), stop_tx).await;
    if let Some(warning) = accepted.warning.clone() {
        shared.ui.event(AgentEvent::Warning(format!(
            "could not save the device: {warning}"
        )));
    }
    let device_name = info.device_name.clone();
    let reason = session::run(conn, accepted, shared.env.clone(), stop_rx, info).await;
    shared.clear_slot(id);
    let _ = done.send(true);
    shared.ui.event(AgentEvent::SessionEnded {
        id,
        device_name,
        reason,
    });
}
