//! The accept loop and the single session slot (§5.3).

use std::{
    net::{IpAddr, SocketAddr},
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
    handshake::{
        HostAccepted, HostConfig, HostSecrets, PairLimiter, Timeouts, host_handshake_from,
    },
    proto::CloseReason,
    rendezvous::{link::Link, seal::SignalKeys},
};
use nexa_wire::control::{Caps, Display, FocusContext, Os};
use quinn::{Connection, Endpoint, EndpointConfig, TokioRuntime};
use thiserror::Error;
use tokio::sync::{Semaphore, watch};
use zeroize::Zeroizing;

use crate::{
    AgentEvent, AgentUi, Platform, SessionInfo,
    session::{self, SessionEnv},
    signal::{SignalSetup, SignalStatus},
};

/// Connections being authenticated at once. Beyond this new ones are refused outright.
const MAX_HANDSHAKES: usize = 8;
const REPLACE_WAIT: Duration = Duration::from_secs(5);
const CLOSE_LINGER: Duration = Duration::from_millis(500);

/// Where files sent to this Mac go, and how large they may be (§3.1 stream 4).
#[derive(Clone, Debug)]
pub struct FileSettings {
    /// `~/Downloads/NexaDesk` in the app; tests always pass a temporary directory.
    pub downloads: std::path::PathBuf,
    pub limits: nexa_session::transfer::TransferLimits,
}

pub struct AgentConfig {
    pub listen: SocketAddr,
    pub device_name: String,
    pub os: Os,
    pub caps: Caps,
    pub timeouts: Timeouts,
    /// Also be reachable by device ID through this signaling server (§4.3).
    pub signal: Option<SignalSetup>,
    pub files: FileSettings,
    /// Announce this host on the LAN over mDNS (§4.11). Never on a loopback address.
    /// Off with `--relay-only`, which must not reveal a direct address.
    pub lan_discovery: bool,
    /// Also let web browsers connect: serve the page, signaling and WebRTC on this TCP
    /// address (`nexa-web`). Off unless set.
    pub web: Option<SocketAddr>,
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
    /// Present while the session runs and only if it was granted `file`.
    files: session::FileSlot,
}

pub(crate) struct Shared {
    env: Arc<SessionEnv>,
    pub(crate) host: HostConfig,
    pub(crate) server_config: quinn::ServerConfig,
    pub(crate) signal_status: Mutex<SignalStatus>,
    handshakes: Arc<Semaphore>,
    pub(crate) ui: Arc<dyn AgentUi>,
    code: OneTimeCode,
    shown_code: Mutex<String>,
    slot: Mutex<Option<ActiveSession>>,
    /// Keeps two accepted connections from replacing the old session at once.
    replacing: tokio::sync::Mutex<()>,
    next_id: AtomicU64,
    pub(crate) shutdown: watch::Sender<bool>,
    /// The mDNS announcement, withdrawn on shutdown.
    lan: Mutex<Option<nexa_net::discovery::Advertiser>>,
    /// Browser access, when on.
    web: Mutex<Option<nexa_web::WebGateway>>,
}

impl Shared {
    pub(crate) fn report_code(&self) {
        let code = self.code.current();
        let mut shown = self.shown_code.lock().unwrap_or_else(|e| e.into_inner());
        if *shown != *code.as_str() {
            shown.clear();
            shown.push_str(code.as_str());
            drop(shown);
            self.ui.event(AgentEvent::CodeChanged(code.to_string()));
        }
    }

    pub(crate) fn session_active(&self) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Whether accepting `peer` would take over someone else's session. The
    /// same device coming back (§4.4 reconnect, its old connection not yet
    /// timed out here) replaces only its own session: no takeover prompt. The
    /// access decision itself is unchanged and may still ask.
    fn session_active_for_other(&self, peer: &nexa_auth::DevicePublicKey) -> bool {
        let key = peer.to_hex();
        self.slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|a| a.info.device_key != key)
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
        files: session::FileSlot,
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
            files,
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

    /// Reachability by ID: state, and this device's ID while registered.
    pub fn signal_status(&self) -> SignalStatus {
        self.shared
            .signal_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
                    || (s.perms.clipboard && !narrowed.clipboard)
                    || (s.perms.file && !narrowed.file)
                    || (s.perms.audio && !narrowed.audio))
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

    /// The running session's transfers, if it may transfer files.
    fn transfers(&self) -> Option<nexa_session::transfer::Transfers> {
        let slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        let active = slot.as_ref()?;
        let files = active.files.lock().unwrap_or_else(|e| e.into_inner());
        files.clone()
    }

    /// Sends files from this Mac to the viewer: the person at the host dropped them
    /// on the banner. Refused unless the running session was granted `file`.
    pub fn send_files(&self, paths: Vec<std::path::PathBuf>) -> Result<Vec<u64>, String> {
        const MAX_FILES: usize = 100;
        if paths.is_empty() || paths.len() > MAX_FILES {
            return Err(format!("send between 1 and {MAX_FILES} files at a time"));
        }
        if paths.iter().any(|p| !p.is_absolute()) {
            return Err("file paths must be absolute".into());
        }
        let transfers = self
            .transfers()
            .ok_or("no running session may transfer files")?;
        Ok(paths.into_iter().map(|p| transfers.send_file(p)).collect())
    }

    /// Stops a transfer of the running session.
    pub fn cancel_transfer(&self, id: u64) -> bool {
        self.transfers().is_some_and(|t| t.cancel(id))
    }

    /// Accepts or declines a large incoming file of the running session.
    pub fn answer_transfer(&self, id: u64, accept: bool) -> bool {
        self.transfers().is_some_and(|t| t.answer(id, accept))
    }

    /// The host's way out: ends the running session, native or browser.
    pub fn disconnect(&self) {
        self.shared.end_active(CloseReason::HostEnded);
        if let Some(web) = self.shared.web.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            web.disconnect();
        }
    }

    /// Where browsers connect, when browser access is on.
    pub fn web_addr(&self) -> Option<SocketAddr> {
        self.shared
            .web
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|w| w.local_addr())
    }

    /// Feeds the frontmost-application context to whichever session is running.
    pub fn set_focus(&self, focus: FocusContext) {
        self.focus.send_replace(Some(focus));
    }

    /// Stops listening and ends the session.
    pub fn shutdown(&self) {
        // First, so viewers stop listing a host that is going away.
        drop(
            self.shared
                .lan
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
        let _ = self.shared.shutdown.send(true);
        self.shared.end_active(CloseReason::Shutdown);
        // Dropping the gateway stops accepting and ends a browser session.
        drop(self.shared.web.lock().unwrap_or_else(|e| e.into_inner()).take());
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
    let identity = Arc::new(identity);
    let socket = bind_media_socket(config.listen).map_err(|source| AgentError::Bind {
        addr: config.listen,
        source,
    })?;
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(server.clone()),
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

    // There must be something to share before anyone is invited to look.
    platform.screen().map_err(AgentError::Screen)?;
    let web = config.web;
    let (focus_tx, focus_rx) = watch::channel(None);
    let code = OneTimeCode::generate();
    let (shutdown, _) = watch::channel(false);
    let env = Arc::new(SessionEnv {
        platform: platform.clone(),
        ui: ui.clone(),
        focus: focus_rx,
        caps: config.caps.clone(),
        device: nexa_clip::DeviceId(*identity.device_id().as_bytes()),
        files: config.files.clone(),
    });
    let signal_setup = config.signal.clone();
    let shared = Arc::new(Shared {
        env,
        server_config: server,
        signal_status: Mutex::new(SignalStatus::default()),
        handshakes: Arc::new(Semaphore::new(MAX_HANDSHAKES)),
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
            host_name: config.device_name.clone(),
            displays: {
                let platform = platform.clone();
                Arc::new(move || describe_displays(&*platform))
            },
            timeouts: config.timeouts,
        },
        ui: ui.clone(),
        code,
        shown_code: Mutex::new(String::new()),
        slot: Mutex::new(None),
        replacing: tokio::sync::Mutex::new(()),
        next_id: AtomicU64::new(1),
        shutdown,
        lan: Mutex::new(None),
        web: Mutex::new(None),
    });

    ui.event(AgentEvent::Listening(local));
    if config.lan_discovery && !local.ip().is_loopback() {
        let advert = nexa_net::discovery::Advert {
            id: identity.device_id().to_string(),
            name: config.device_name.clone(),
            os: os_label(config.os).into(),
            port: local.port(),
            ip: Some(local.ip()).filter(|ip| !ip.is_unspecified()),
        };
        match nexa_net::discovery::Advertiser::start(&advert) {
            Ok(a) => *shared.lan.lock().unwrap_or_else(|e| e.into_inner()) = Some(a),
            Err(e) => ui.event(AgentEvent::Warning(format!(
                "not announced on the local network: {e}"
            ))),
        }
    }
    if let Some(addr) = web {
        let host = Arc::new(crate::web::AgentWeb {
            platform: platform.clone(),
            ui: ui.clone(),
            shared: Arc::downgrade(&shared),
        });
        match nexa_web::WebGateway::bind(addr, host, shared.code.clone(), Default::default()).await {
            Ok(g) => *shared.web.lock().unwrap_or_else(|e| e.into_inner()) = Some(g),
            Err(e) => ui.event(AgentEvent::Warning(format!(
                "browser access is off: cannot listen on {addr}: {e}"
            ))),
        }
    }
    shared.report_code();
    let (done_tx, done) = watch::channel(false);
    tokio::spawn(accept_loop(endpoint, shared.clone(), done_tx));
    if let Some(setup) = signal_setup {
        let keys = Arc::new(SignalKeys::new(identity.clone()));
        tokio::spawn(crate::signal::run(shared.clone(), setup, keys));
    }
    Ok(Agent {
        shared,
        name: config.device_name,
        local,
        done,
        focus: focus_tx,
    })
}

fn os_label(os: Os) -> &'static str {
    match os {
        Os::Macos => "macos",
        Os::Windows => "windows",
        Os::Linux => "linux",
        Os::Unknown => "",
    }
}

/// `SessionAccept.displays` (§3.4, §3.6): `id` is the index a viewer asks for,
/// `x`/`y` the origin in points (so the layout can be drawn), `w`/`h` pixels.
pub(crate) fn describe_displays(platform: &dyn Platform) -> Vec<Display> {
    platform
        .displays()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .take(usize::from(u8::MAX))
        .map(|(i, s)| Display {
            id: i as u32,
            x: s.origin_points.0.round() as i32,
            y: s.origin_points.1.round() as i32,
            w: s.size_pixels.0,
            h: s.size_pixels.1,
            name: s.name,
            refresh_hz: s.refresh_hz,
        })
        .collect()
}

async fn accept_loop(endpoint: Endpoint, shared: Arc<Shared>, done: watch::Sender<bool>) {
    let limit = shared.handshakes.clone();
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
            // A dual-stack listener reports IPv4 peers as ::ffff:a.b.c.d.
            let remote = nexa_net::vsock::canonical(incoming.remote_address());
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
            let remote = nexa_net::vsock::canonical(conn.remote_address());
            handle_connection(conn, shared, permit, remote, remote.ip(), None).await;
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

/// A connection that came through the rendezvous (§4.3). From here on it is
/// treated exactly like one accepted on the listening address.
pub(crate) async fn serve_connection(
    conn: Connection,
    shared: Arc<Shared>,
    remote: SocketAddr,
    origin: IpAddr,
    link: Option<Arc<Link>>,
) {
    let Ok(permit) = shared.handshakes.clone().try_acquire_owned() else {
        conn.close(1u32.into(), b"busy");
        return;
    };
    handle_connection(conn, shared, permit, remote, origin, link).await;
}

async fn handle_connection(
    conn: Connection,
    shared: Arc<Shared>,
    permit: tokio::sync::OwnedSemaphorePermit,
    remote: SocketAddr,
    origin: IpAddr,
    link: Option<Arc<Link>>,
) {
    // One session at a time: a browser has the screen (see crate::web).
    let browser_busy = shared
        .web
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|w| w.busy());
    if browser_busy {
        shared.ui.event(AgentEvent::ConnectionFailed {
            remote,
            reason: "a browser session is running".into(),
        });
        conn.close(1u32.into(), b"busy");
        drop(permit);
        return;
    }
    let result = {
        let probe = shared.clone();
        let peer = nexa_auth::peer_public_key(&conn).ok();
        let active = move || match &peer {
            Some(p) => probe.session_active_for_other(p),
            None => probe.session_active(),
        };
        host_handshake_from(&conn, &shared.host, &active, remote, origin).await
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
    run_session(conn, accepted, shared, remote, link).await;
}

async fn run_session(
    conn: Connection,
    accepted: HostAccepted,
    shared: Arc<Shared>,
    remote: SocketAddr,
    link: Option<Arc<Link>>,
) {
    let id = shared.next_id.fetch_add(1, Relaxed);
    let (stop_tx, stop_rx) = watch::channel(None);
    let info = SessionInfo {
        id,
        device_name: accepted.device_name.clone(),
        device_id: accepted.peer.device_id().to_string(),
        device_key: accepted.peer.to_hex(),
        remote,
        perms: accepted.perms.clone(),
    };
    let files = session::FileSlot::default();
    let done = shared.take_slot(info.clone(), stop_tx, files.clone()).await;
    if let Some(warning) = accepted.warning.clone() {
        shared.ui.event(AgentEvent::Warning(format!(
            "could not save the device: {warning}"
        )));
    }
    let device_name = info.device_name.clone();
    let reason = session::run(
        conn,
        accepted,
        shared.env.clone(),
        stop_rx,
        info,
        link,
        files,
    )
    .await;
    shared.clear_slot(id);
    let _ = done.send(true);
    shared.ui.event(AgentEvent::SessionEnded {
        id,
        device_name,
        reason,
    });
}
