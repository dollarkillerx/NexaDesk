//! The viewer core: connection, handshake and the session's lifetime.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering::Relaxed},
        mpsc::{self, Sender},
    },
    thread::JoinHandle,
    time::Duration,
};

use nexa_auth::{
    DeviceIdentity, DevicePublicKey, PairingStore, PeerPolicy, Perms, client_config,
    peer_public_key, store::unix_now, tls::SERVER_NAME,
};
use nexa_clip::{ClipPolicy, GateStats};
use nexa_input::{FocusClass, Keymap, Rewriter};
use nexa_media::PresentMode;
use nexa_net::{
    check::{PathClass, is_tailnet},
    vsock::{VIRTUAL_PEER, bind_media_socket},
};
use nexa_session::{
    SessionError,
    clip::{ClipCallbacks, ClipChannel, ClipHandle, ClipSession, PromiseMode},
    framed::{FramedSend, open_bi, open_uni},
    handshake::{
        ControlChannel, FLUSH_TIMEOUT, Timeouts, ViewerAccepted, ViewerAuth, ViewerConfig,
        viewer_handshake,
    },
    proto::{CloseReason, ControlBody, ControlMsg, Goodbye, InputMsg, RejectReason, StreamRequest},
    rendezvous::{
        self, DialError, Stage,
        link::{Link, describe},
        seal::SignalKeys,
        signal::SignalConfig,
    },
    viewer::{ViewerMedia, ViewerParams},
};
use nexa_wire::signal::normalize_short_id;
use nexa_wire::{
    control::{Caps, Codec, Mode, Os, StreamConfig},
    framing::StreamType,
};
use quinn::{Connection, ConnectionError, Endpoint, EndpointConfig, TokioRuntime};
use tokio::{
    runtime::{Handle, Runtime},
    sync::{OnceCell, mpsc as async_mpsc, watch},
};
use zeroize::Zeroizing;

use crate::{
    deps::{Deps, KeySource},
    input::{self, InputCmd, InputThreadConfig, ViewGeometry},
    presenter::PresenterSlot,
    proto::{
        ClipboardNotice, Command, ConnectAuth, ConnectRequest, ConnectStage, Connected, Connecting,
        DisconnectReason, Disconnected, Event, EventBody, KnownHost, PathKind, Permissions, Ready,
        SessionCommand, Stats, StreamChanged, Warning,
    },
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const STATS_INTERVAL: Duration = Duration::from_secs(1);
const INPUT_STREAM_QUEUE: usize = 1024;
const CLOSE_LINGER: Duration = Duration::from_millis(500);
const FRAME_DEADLINE_MIN: Duration = Duration::from_millis(8);

fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Serializes events onto the callback thread's queue.
#[derive(Clone)]
pub struct EventTx(Sender<Vec<u8>>);

impl EventTx {
    pub fn new(tx: Sender<Vec<u8>>) -> Self {
        Self(tx)
    }

    pub fn send(&self, tag: u64, body: EventBody) {
        use prost::Message;
        let event = Event {
            tag,
            body: Some(body),
        };
        let _ = self.0.send(event.encode_to_vec());
    }
}

struct Device {
    identity: Arc<DeviceIdentity>,
    store: Mutex<PairingStore>,
    /// Signaling keys, made on first use of an ID (§4.3).
    keys: OnceLock<Arc<SignalKeys>>,
}

impl Device {
    fn signal_keys(&self) -> Arc<SignalKeys> {
        self.keys
            .get_or_init(|| Arc::new(SignalKeys::new(self.identity.clone())))
            .clone()
    }
}

pub struct Core {
    deps: Arc<dyn Deps>,
    rt: Mutex<Option<Runtime>>,
    handle: Handle,
    events: EventTx,
    device_name: String,
    data_dir: PathBuf,
    device: OnceCell<Arc<Device>>,
    caps: OnceCell<Caps>,
}

impl Core {
    pub fn new(
        deps: Arc<dyn Deps>,
        device_name: String,
        data_dir: PathBuf,
        events: EventTx,
    ) -> std::io::Result<Arc<Self>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("net")
            .enable_all()
            .build()?;
        let handle = rt.handle().clone();
        let core = Arc::new(Self {
            deps,
            rt: Mutex::new(Some(rt)),
            handle,
            events,
            device_name,
            data_dir,
            device: OnceCell::new(),
            caps: OnceCell::new(),
        });
        // nexa_init must not block on the Keychain or the disk.
        let c = core.clone();
        core.handle.spawn(async move {
            match c.device().await {
                Ok(_) => c.report_ready().await,
                Err(error) => c.events.send(
                    0,
                    EventBody::Ready(Ready {
                        this_device_key: String::new(),
                        hosts: Vec::new(),
                        error,
                    }),
                ),
            }
        });
        Ok(core)
    }

    /// Stops the runtime in the background. Never blocks the caller.
    pub fn shut_down(&self) {
        if let Some(rt) = lock(&self.rt).take() {
            let _ = std::thread::Builder::new()
                .name("nexa-shutdown".into())
                .spawn(move || {
                    rt.shutdown_timeout(Duration::from_secs(3));
                });
        }
    }

    async fn device(&self) -> Result<Arc<Device>, String> {
        self.device
            .get_or_try_init(|| async {
                let deps = self.deps.clone();
                let dir = self.data_dir.clone();
                tokio::task::spawn_blocking(move || -> Result<Arc<Device>, String> {
                    std::fs::create_dir_all(&dir).map_err(|e| format!("data directory: {e}"))?;
                    let identity = DeviceIdentity::load_or_create(&*deps.key_store())
                        .map_err(|e| format!("device key: {e}"))?;
                    let store =
                        PairingStore::open(dir.join("known_hosts.json"), identity.store_mac_key())
                            .map_err(|e| format!("known hosts: {e}"))?;
                    Ok(Arc::new(Device {
                        identity: Arc::new(identity),
                        store: Mutex::new(store),
                        keys: OnceLock::new(),
                    }))
                })
                .await
                .map_err(|e| e.to_string())?
            })
            .await
            .cloned()
    }

    async fn caps(&self) -> Caps {
        self.caps
            .get_or_init(|| async {
                let deps = self.deps.clone();
                tokio::task::spawn_blocking(move || deps.caps())
                    .await
                    .unwrap_or(Caps {
                        encode: Vec::new(),
                        decode: Vec::new(),
                        rfi: false,
                        ltr: false,
                        max_fps: 0,
                    })
            })
            .await
            .clone()
    }

    async fn report_ready(&self) {
        let Ok(device) = self.device().await else {
            return;
        };
        let hosts = lock(&device.store)
            .records()
            .iter()
            .map(|r| KnownHost {
                key: r.pubkey.to_hex(),
                name: r.name.clone(),
                last_seen: r.last_seen,
            })
            .collect();
        self.events.send(
            0,
            EventBody::Ready(Ready {
                this_device_key: device.identity.public_key().to_hex(),
                hosts,
                error: String::new(),
            }),
        );
    }

    /// Starts a session. Returns immediately; progress arrives as events.
    pub fn connect(self: &Arc<Self>, request: ConnectRequest) -> Arc<Session> {
        let (input_tx, input_rx) = mpsc::channel();
        let session = Arc::new(Session {
            tag: request.tag,
            core: self.clone(),
            presenter: Arc::new(PresenterSlot::default()),
            view: Arc::new(Mutex::new(ViewGeometry::default())),
            input_tx,
            input_rx: Mutex::new(Some(input_rx)),
            input_started: AtomicBool::new(false),
            input_granted: OnceLock::new(),
            want_active: AtomicBool::new(false),
            command: Mutex::new(None),
            media: Mutex::new(None),
            fallback: Arc::new(Mutex::new(None)),
            clip: Arc::new(Mutex::new(None)),
            close: watch::channel(false).0,
        });
        let s = session.clone();
        let core = self.clone();
        self.handle.spawn(async move {
            let tag = s.tag;
            // A panic in the session must end it, not leave the keyboard captured.
            let task = core
                .handle
                .spawn(run_session(core.clone(), s.clone(), request));
            if task.await.is_err() {
                s.stop_input();
                core.events.send(
                    tag,
                    EventBody::Disconnected(Disconnected {
                        reason: DisconnectReason::Internal as i32,
                        message: "internal error".into(),
                    }),
                );
            }
        });
        session
    }
}

pub enum SessionCtl {
    Stream(StreamRequest),
}

/// The handle behind `NexaSession*`. All methods return immediately.
pub struct Session {
    tag: u64,
    core: Arc<Core>,
    presenter: Arc<PresenterSlot>,
    view: Arc<Mutex<ViewGeometry>>,
    /// Always the same channel, so a capture created before the input thread starts
    /// (on the main thread, in `attach_view`) reaches it too.
    input_tx: Sender<InputCmd>,
    input_rx: Mutex<Option<mpsc::Receiver<InputCmd>>>,
    input_started: AtomicBool,
    input_granted: OnceLock<bool>,
    want_active: AtomicBool,
    command: Mutex<Option<async_mpsc::UnboundedSender<SessionCtl>>>,
    media: Mutex<Option<ViewerMedia>>,
    fallback: Arc<Mutex<Option<Box<dyn KeySource>>>>,
    clip: Arc<Mutex<Option<ClipHandle>>>,
    close: watch::Sender<bool>,
}

impl Session {
    pub fn tag(&self) -> u64 {
        self.tag
    }

    fn stop_input(&self) {
        let _ = self.input_tx.send(InputCmd::Stop);
    }

    fn to_input(&self, cmd: InputCmd) {
        if self.input_started.load(Relaxed) {
            let _ = self.input_tx.send(cmd);
        }
    }

    /// Ends the session. The view stops being drawn before this returns.
    pub fn close(&self) {
        self.presenter.detach();
        self.stop_input();
        let _ = self.close.send(true);
    }

    fn repaint(&self) {
        if let Some(media) = lock(&self.media).as_ref() {
            media.repaint();
        }
    }

    /// # Safety
    /// See [`Deps::attach_view`].
    pub unsafe fn attach_view(
        &self,
        handle: *mut std::ffi::c_void,
        kind: u32,
        w: u32,
        h: u32,
        scale: f32,
    ) -> Result<(), String> {
        // SAFETY: forwarded from the caller.
        let presenter = unsafe {
            self.core
                .deps
                .attach_view(handle, kind, w, h, scale, self.presenter.current_mode())?
        };
        self.presenter.attach(presenter);
        {
            let mut view = lock(&self.view);
            view.view_w = w;
            view.view_h = h;
        }
        if self.input_granted.get().copied().unwrap_or(true) {
            let mut slot = lock(&self.fallback);
            if slot.is_none() {
                *slot = self.core.deps.fallback_key_source(self.input_tx.clone());
            }
        }
        self.repaint();
        Ok(())
    }

    pub fn resize_view(&self, w: u32, h: u32, scale: f32) {
        self.presenter.resize(w, h, scale);
        {
            let mut view = lock(&self.view);
            view.view_w = w;
            view.view_h = h;
        }
        self.repaint();
    }

    pub fn detach_view(&self) {
        self.presenter.detach();
        let mut view = lock(&self.view);
        view.view_w = 0;
        view.view_h = 0;
    }

    /// `Err` if the host refused input for this session.
    pub fn set_input_active(&self, active: bool) -> Result<(), &'static str> {
        if active && self.input_granted.get() == Some(&false) {
            return Err("the host did not grant input");
        }
        self.want_active.store(active, Relaxed);
        self.to_input(InputCmd::SetActive(active));
        Ok(())
    }

    pub fn command(&self, command: SessionCommand) -> Result<(), &'static str> {
        let Some(command) = command.cmd else {
            return Err("empty command");
        };
        let cmd = match command {
            Command::Move(m) => InputCmd::Move(m.x, m.y),
            Command::Button(b) => InputCmd::Button(b.button, b.down),
            Command::Wheel(w) => InputCmd::Wheel(w.dx, w.dy),
            Command::Text(t) => InputCmd::Text(t.utf8),
            Command::Mode(m) => {
                if let Some(ctl) = lock(&self.command).as_ref() {
                    let _ = ctl.send(SessionCtl::Stream(StreamRequest {
                        mode: m.mode,
                        fps: m.fps,
                    }));
                }
                return Ok(());
            }
            Command::Clipboard(p) => {
                if let Some(clip) = lock(&self.clip).as_ref() {
                    clip.set_policy(ClipPolicy {
                        outbound: p.outbound,
                        inbound: p.inbound,
                        sync_sensitive: false,
                    });
                }
                return Ok(());
            }
        };
        self.to_input(cmd);
        Ok(())
    }
}

fn reason_of_rejection(reason: RejectReason) -> DisconnectReason {
    match reason {
        RejectReason::VersionMismatch => DisconnectReason::VersionMismatch,
        RejectReason::AuthFailed => DisconnectReason::AuthFailed,
        RejectReason::RateLimited => DisconnectReason::RateLimited,
        RejectReason::NotPaired => DisconnectReason::NotPaired,
        RejectReason::ConsentDenied => DisconnectReason::ConsentDenied,
        RejectReason::NoCommonCodec => DisconnectReason::NoCommonCodec,
        RejectReason::Timeout => DisconnectReason::TimedOut,
        RejectReason::MethodUnavailable => DisconnectReason::AuthFailed,
        RejectReason::Internal | RejectReason::Unspecified => DisconnectReason::Internal,
    }
}

fn reason_of_close(reason: CloseReason) -> DisconnectReason {
    match reason {
        CloseReason::HostEnded | CloseReason::Shutdown => DisconnectReason::HostEnded,
        CloseReason::Replaced => DisconnectReason::Replaced,
        CloseReason::PipelineFailed => DisconnectReason::PipelineFailed,
        CloseReason::ViewerEnded => DisconnectReason::ClosedLocally,
        CloseReason::Unspecified => DisconnectReason::Unspecified,
    }
}

/// A TLS failure on a connection that pinned the host means it is not who it was.
fn is_tls_failure(e: &ConnectionError) -> bool {
    let ConnectionError::TransportError(t) = e else {
        return false;
    };
    (0x100..0x200).contains(&u64::from(t.code))
}

fn ended(tag: u64, core: &Core, reason: DisconnectReason, message: impl Into<String>) {
    core.events.send(
        tag,
        EventBody::Disconnected(Disconnected {
            reason: reason as i32,
            message: message.into(),
        }),
    );
}

fn perms_message(p: &nexa_wire::control::Permissions) -> Permissions {
    Permissions {
        view: p.view,
        input: p.input,
        clipboard: p.clipboard,
        file: p.file,
        audio: p.audio,
    }
}

async fn control_writer(mut send: FramedSend, mut rx: async_mpsc::UnboundedReceiver<ControlBody>) {
    while let Some(body) = rx.recv().await {
        if send.send(&ControlMsg::from(body)).await.is_err() {
            return;
        }
    }
    send.finish_and_flush(FLUSH_TIMEOUT).await;
}

async fn input_stream_writer(mut send: FramedSend, mut rx: async_mpsc::Receiver<InputMsg>) {
    while let Some(msg) = rx.recv().await {
        if send.send(&msg).await.is_err() {
            return;
        }
    }
}

fn frame_deadline(mode: Mode, fps: u32) -> Duration {
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    let deadline = if mode == Mode::Game {
        interval.mul_f64(1.5)
    } else {
        interval * 3
    };
    deadline.max(FRAME_DEADLINE_MIN)
}

fn parse_auth(auth: ConnectAuth) -> Result<(ViewerAuth, Option<DevicePublicKey>), String> {
    match auth {
        ConnectAuth::KnownHost(hex) => {
            let key = DevicePublicKey::from_hex(&hex).map_err(|_| "bad host key".to_owned())?;
            Ok((ViewerAuth::Known, Some(key)))
        }
        ConnectAuth::Code(code) => Ok((ViewerAuth::Code(Zeroizing::new(code)), None)),
        ConnectAuth::Password(p) => Ok((ViewerAuth::Password(Zeroizing::new(p)), None)),
    }
}

/// A local address for reaching `addr`: never wider than it needs to be.
fn local_address_for(addr: SocketAddr) -> SocketAddr {
    let ip = match (addr.is_ipv4(), addr.ip().is_loopback()) {
        (true, true) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        (true, false) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        (false, true) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        (false, false) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    SocketAddr::new(ip, 0)
}

async fn connect_quic(
    address: &str,
    config: quinn::ClientConfig,
) -> Result<(Endpoint, Connection), (DisconnectReason, String)> {
    let addr = tokio::net::lookup_host(address)
        .await
        .map_err(|e| {
            (
                DisconnectReason::Network,
                format!("cannot resolve {address}: {e}"),
            )
        })?
        .next()
        .ok_or_else(|| {
            (
                DisconnectReason::Network,
                format!("no address for {address}"),
            )
        })?;
    let socket = bind_media_socket(local_address_for(addr))
        .map_err(|e| (DisconnectReason::Network, e.to_string()))?;
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        None,
        socket,
        Arc::new(TokioRuntime),
    )
    .map_err(|e| (DisconnectReason::Network, e.to_string()))?;
    let connecting = endpoint
        .connect_with(config, addr, SERVER_NAME)
        .map_err(|e| (DisconnectReason::Network, e.to_string()))?;
    let conn = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            let reason = if is_tls_failure(&e) {
                DisconnectReason::PinMismatch
            } else {
                DisconnectReason::Network
            };
            return Err((reason, e.to_string()));
        }
        Err(_) => {
            return Err((
                DisconnectReason::TimedOut,
                "connection timed out".to_owned(),
            ));
        }
    };
    Ok((endpoint, conn))
}

struct Accepted {
    endpoint: Endpoint,
    conn: Connection,
    accepted: ViewerAccepted,
    /// Present for connections by ID: the path layer under QUIC.
    link: Option<Arc<Link>>,
    _trickle: Option<TrickleGuard>,
}

/// Stops the rendezvous' trickle pump with the session.
struct TrickleGuard(tokio::task::JoinHandle<()>);

impl Drop for TrickleGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// §4.4 QuicHandshake: 3 s per path, then the next validated one.
const QUIC_PATH_TIMEOUT: Duration = Duration::from_secs(3);
const QUIC_PATH_TRIES: u32 = 3;

fn reason_of_dial(e: &DialError) -> DisconnectReason {
    match e {
        DialError::Offline => DisconnectReason::Offline,
        DialError::RateLimited => DisconnectReason::RateLimited,
        DialError::WrongDevice => DisconnectReason::PinMismatch,
        DialError::NoPath | DialError::NoAnswer => DisconnectReason::NoPath,
        DialError::BadPeer(_) => DisconnectReason::AuthFailed,
        DialError::Signal(_) | DialError::Refused(_) => DisconnectReason::SignalServer,
        DialError::Io(_) => DisconnectReason::Network,
    }
}

/// Rendezvous through the signaling server, then QUIC over the validated path.
async fn connect_by_id(
    core: &Arc<Core>,
    session: &Arc<Session>,
    device: &Device,
    request: &ConnectRequest,
    short_id: &str,
    pinned: Option<DevicePublicKey>,
    config: quinn::ClientConfig,
) -> Result<(Endpoint, Connection, Arc<Link>, TrickleGuard), (DisconnectReason, String)> {
    if request.signal_server.trim().is_empty() {
        return Err((
            DisconnectReason::InvalidRequest,
            "no signaling server is set, so a device ID cannot be used".to_owned(),
        ));
    }
    let mut server = SignalConfig::new(request.signal_server.trim().to_owned());
    let ca = core.data_dir.join("signal-ca.pem");
    if ca.exists() {
        server = server
            .with_ca_file(&ca)
            .map_err(|e| (DisconnectReason::InvalidRequest, e))?;
    }
    let keys = device.signal_keys();
    let tag = session.tag;
    let events = core.events.clone();
    let progress = move |stage: Stage| {
        let stage = match stage {
            Stage::Resolving => ConnectStage::Resolving,
            Stage::Gathering => ConnectStage::Gathering,
            Stage::Checking => ConnectStage::Checking,
        };
        events.send(
            tag,
            EventBody::Connecting(Connecting {
                stage: stage as i32,
            }),
        );
    };
    let mut close = session.close.subscribe();
    let dialed = tokio::select! {
        d = rendezvous::dial(&server, &keys, short_id, pinned, core.deps.link_options(), &progress) => d,
        _ = close.wait_for(|c| *c) => {
            return Err((DisconnectReason::ClosedLocally, "closed by the application".to_owned()));
        }
    };
    let dialed = dialed.map_err(|e| (reason_of_dial(&e), e.to_string()))?;
    core.events.send(
        tag,
        EventBody::Connecting(Connecting {
            stage: ConnectStage::Quic as i32,
        }),
    );
    let endpoint = dialed.link.endpoint.clone();
    let connecting = endpoint
        .connect_with(config, VIRTUAL_PEER, SERVER_NAME)
        .map_err(|e| (DisconnectReason::Network, e.to_string()))?;
    tokio::pin!(connecting);
    let mut tries = 0;
    let conn = loop {
        tokio::select! {
            r = &mut connecting => match r {
                Ok(c) => break c,
                Err(e) => {
                    let reason = if is_tls_failure(&e) {
                        DisconnectReason::PinMismatch
                    } else {
                        DisconnectReason::Network
                    };
                    return Err((reason, e.to_string()));
                }
            },
            _ = tokio::time::sleep(QUIC_PATH_TIMEOUT) => {
                tries += 1;
                if tries >= QUIC_PATH_TRIES {
                    return Err((DisconnectReason::TimedOut, "connection timed out".to_owned()));
                }
                dialed.link.rotate();
            }
            _ = close.wait_for(|c| *c) => {
                return Err((DisconnectReason::ClosedLocally, "closed by the application".to_owned()));
            }
        }
    };
    // The QUIC peer must be the device the rendezvous was sealed with.
    if peer_public_key(&conn).ok() != Some(dialed.host_key) {
        conn.close(3u32.into(), b"identity mismatch");
        return Err((
            DisconnectReason::PinMismatch,
            "the device that answered is not the one at this ID".to_owned(),
        ));
    }
    Ok((endpoint, conn, dialed.link, TrickleGuard(dialed.trickle)))
}

async fn establish(
    core: &Arc<Core>,
    session: &Arc<Session>,
    request: &ConnectRequest,
) -> Result<Accepted, (DisconnectReason, String)> {
    let auth = request.auth.clone().ok_or_else(|| {
        (
            DisconnectReason::InvalidRequest,
            "no authentication method".to_owned(),
        )
    })?;
    let (auth, pinned) = parse_auth(auth).map_err(|m| (DisconnectReason::InvalidRequest, m))?;
    let device = core
        .device()
        .await
        .map_err(|m| (DisconnectReason::Internal, m))?;
    let caps = core.caps().await;

    // Known-host connections are pinned; anything else must prove itself with a
    // secret. There is no third way to reach a host.
    let policy = match pinned {
        Some(key) => {
            if lock(&device.store).pinned(&key).is_none() {
                return Err((
                    DisconnectReason::NotPaired,
                    "that host is not paired".to_owned(),
                ));
            }
            PeerPolicy::Pinned(key)
        }
        None => PeerPolicy::AcceptUnknown,
    };
    let config = client_config(&device.identity, policy)
        .map_err(|e| (DisconnectReason::Internal, e.to_string()))?;
    let (endpoint, conn, link, trickle) = match normalize_short_id(&request.address) {
        Some(id) => {
            let (e, c, l, t) =
                connect_by_id(core, session, &device, request, &id, pinned, config).await?;
            (e, c, Some(l), Some(t))
        }
        None => {
            let (e, c) = connect_quic(&request.address, config).await?;
            (e, c, None, None)
        }
    };

    let handshake = viewer_handshake(
        &conn,
        ViewerConfig {
            me: device.identity.public_key(),
            device_name: core.device_name.clone(),
            os: core.deps.os(),
            caps,
            auth,
            timeouts: Timeouts::default(),
        },
    );
    let mut close = session.close.subscribe();
    let accepted = tokio::select! {
        r = handshake => r,
        _ = close.wait_for(|c| *c) => {
            conn.close(3u32.into(), b"closed");
            return Err((DisconnectReason::ClosedLocally, "closed by the application".to_owned()));
        }
    };
    let accepted = match accepted {
        Ok(a) => a,
        Err(SessionError::Rejected { reason, message }) => {
            return Err((reason_of_rejection(reason), message));
        }
        Err(SessionError::Auth(e)) => return Err((DisconnectReason::AuthFailed, e.to_string())),
        Err(SessionError::Timeout) => {
            return Err((
                DisconnectReason::TimedOut,
                "the host did not answer".to_owned(),
            ));
        }
        Err(e) => return Err((DisconnectReason::Network, e.to_string())),
    };
    Ok(Accepted {
        endpoint,
        conn,
        accepted,
        link,
        _trickle: trickle,
    })
}

fn store_pairing(device: &Device, accepted: &ViewerAccepted, name: &str) -> Result<bool, String> {
    let Some(proof) = accepted.proof.as_ref() else {
        lock(&device.store)
            .touch(&accepted.host, unix_now())
            .map_err(|e| e.to_string())?;
        return Ok(false);
    };
    let p = accepted.accept.perms.as_ref();
    let perms = Perms {
        view: p.is_some_and(|p| p.view),
        input: p.is_some_and(|p| p.input),
        clipboard: p.is_some_and(|p| p.clipboard),
        file: p.is_some_and(|p| p.file),
        audio: p.is_some_and(|p| p.audio),
    };
    lock(&device.store)
        .add(proof, name, perms, unix_now())
        .map_err(|e| e.to_string())?;
    Ok(true)
}

struct Running {
    media_codec: Option<Codec>,
    last_config: Option<StreamConfig>,
}

fn apply_config(
    core: &Arc<Core>,
    session: &Arc<Session>,
    conn: &Connection,
    running: &mut Running,
    config: StreamConfig,
) -> Result<(), String> {
    let Ok(codec) = Codec::try_from(config.codec) else {
        return Err("unknown codec".to_owned());
    };
    let mode = Mode::try_from(config.mode).unwrap_or(Mode::Balanced);
    {
        let mut view = lock(&session.view);
        view.stream_w = config.w;
        view.stream_h = config.h;
    }
    session.presenter.set_mode(if mode == Mode::Game {
        PresentMode::Immediate
    } else {
        PresentMode::Vsync
    });
    session.to_input(InputCmd::GameMode(mode == Mode::Game));
    if running.media_codec != Some(codec) {
        let decoder = core.deps.decoder(codec)?;
        let old = lock(&session.media).take();
        if let Some(old) = old {
            std::thread::spawn(move || old.stop());
        }
        let params = ViewerParams {
            stream: 0,
            frame_deadline: frame_deadline(mode, config.fps),
            reorder_grace: Duration::from_millis(2),
        };
        let media = ViewerMedia::spawn_with(
            conn.clone(),
            decoder,
            session.presenter.proxy(),
            params,
            core.deps.thread_init(),
        );
        *lock(&session.media) = Some(media);
        running.media_codec = Some(codec);
    }
    core.events.send(
        session.tag,
        EventBody::Stream(StreamChanged {
            codec: config.codec as u32,
            mode: config.mode,
            w: config.w,
            h: config.h,
            fps: config.fps,
        }),
    );
    running.last_config = Some(config);
    Ok(())
}

fn start_input(
    core: &Arc<Core>,
    session: &Arc<Session>,
    conn: &Connection,
    host_os: Os,
    stream: async_mpsc::Sender<InputMsg>,
    gate_stats: Arc<Mutex<GateStats>>,
) -> Option<JoinHandle<()>> {
    let rx = lock(&session.input_rx).take()?;
    let tx = session.input_tx.clone();
    let profile = Keymap::builtin().for_session(core.deps.os(), host_os);
    let handle = input::spawn(
        InputThreadConfig {
            conn: conn.clone(),
            stream,
            rewriter: Rewriter::new(profile),
            key_source: core.deps.key_source(tx.clone()),
            fallback: session.fallback.clone(),
            clip: session.clip.clone(),
            view: session.view.clone(),
            events: core.events.clone(),
            tag: session.tag,
            start_active: session.want_active.load(Relaxed),
            gate_stats,
        },
        rx,
    );
    session.input_started.store(true, Relaxed);
    Some(handle)
}

async fn run_session(core: Arc<Core>, session: Arc<Session>, request: ConnectRequest) {
    let tag = session.tag;
    let stage = if normalize_short_id(&request.address).is_some() {
        ConnectStage::Resolving
    } else {
        ConnectStage::Quic
    };
    core.events.send(
        tag,
        EventBody::Connecting(Connecting {
            stage: stage as i32,
        }),
    );
    let established = match establish(&core, &session, &request).await {
        Ok(e) => e,
        Err((reason, message)) => {
            let _ = session.input_granted.set(false);
            ended(tag, &core, reason, message);
            return;
        }
    };
    let Accepted {
        endpoint,
        conn,
        accepted,
        link,
        _trickle,
    } = established;

    let (reason, message) = match core.device().await {
        Ok(device) => {
            stream_session(
                &core,
                &session,
                device,
                &conn,
                accepted,
                &request,
                link.as_ref(),
            )
            .await
        }
        Err(m) => (DisconnectReason::Internal, m),
    };
    ended(tag, &core, reason, message);

    conn.close(3u32.into(), b"session ended");
    let _ = tokio::time::timeout(CLOSE_LINGER, endpoint.wait_idle()).await;
}

async fn stream_session(
    core: &Arc<Core>,
    session: &Arc<Session>,
    device: Arc<Device>,
    conn: &Connection,
    accepted: ViewerAccepted,
    request: &ConnectRequest,
    link: Option<&Arc<Link>>,
) -> (DisconnectReason, String) {
    let tag = session.tag;
    let perms = accepted
        .accept
        .perms
        .clone()
        .unwrap_or(nexa_wire::control::Permissions {
            view: false,
            input: false,
            clipboard: false,
            file: false,
            audio: false,
        });
    let host_name = accepted
        .accept
        .displays
        .first()
        .map(|d| d.name.clone())
        .unwrap_or_default();
    let paired_now = match store_pairing(&device, &accepted, &host_name) {
        Ok(p) => p,
        Err(e) => {
            core.events.send(
                tag,
                EventBody::Warning(Warning {
                    text: format!("could not remember this host: {e}"),
                }),
            );
            false
        }
    };
    if paired_now {
        core.report_ready().await;
    }
    let host_os = Os::try_from(accepted.accept.os).unwrap_or(Os::Unknown);
    let host_key = accepted.host.to_hex();
    let ViewerAccepted {
        control: ControlChannel { send, mut recv },
        ..
    } = accepted;

    let (ctl_tx, ctl_rx) = async_mpsc::unbounded_channel();
    let writer = tokio::spawn(control_writer(send, ctl_rx));
    // §4.5: the path manager's decisions go to the host on this authenticated stream.
    let switch_pump = link.and_then(|l| l.take_switches()).map(|mut switches| {
        let ctl = ctl_tx.clone();
        tokio::spawn(async move {
            while let Some(path_id) = switches.recv().await {
                if ctl
                    .send(ControlBody::PathSwitch(nexa_wire::control::PathSwitch {
                        path_id,
                    }))
                    .is_err()
                {
                    break;
                }
            }
        })
    });
    let _ = ctl_tx.send(ControlBody::Request(StreamRequest {
        mode: request.mode,
        fps: request.fps,
    }));

    let (cmd_tx, mut cmd_rx) = async_mpsc::unbounded_channel();
    *lock(&session.command) = Some(cmd_tx);

    // Set before the Connected event, so the UI can never activate capture it lacks.
    let _ = session.input_granted.set(perms.input);
    core.events.send(
        tag,
        EventBody::Connected(Connected {
            host_key,
            host_name,
            perms: Some(perms_message(&perms)),
            host_os: host_os as u32,
            paired_now,
        }),
    );

    let gate_stats = Arc::new(Mutex::new(GateStats::default()));
    let mut input_thread = None;
    let mut input_writer = None;
    if perms.input {
        match open_uni(conn, StreamType::Input).await {
            Ok(stream) => {
                let (stream_tx, stream_rx) = async_mpsc::channel(INPUT_STREAM_QUEUE);
                input_writer = Some(tokio::spawn(input_stream_writer(stream, stream_rx)));
                input_thread =
                    start_input(core, session, conn, host_os, stream_tx, gate_stats.clone());
            }
            Err(e) => return (DisconnectReason::Network, e.to_string()),
        }
    }

    let mut clipboard: Option<ClipSession> = None;
    if perms.clipboard {
        match (
            core.deps.clipboard(),
            open_bi(conn, StreamType::Clipboard).await,
        ) {
            (Ok(backend), Ok(streams)) => {
                let channel = ClipChannel::default();
                let device_id = nexa_clip::DeviceId(*device.identity.device_id().as_bytes());
                let release_to = session.input_tx.clone();
                let events = core.events.clone();
                let started = channel.spawn(
                    backend,
                    device_id,
                    // §8.6: a promise here would block the UI thread on a slow fetch.
                    PromiseMode::Eager,
                    streams,
                    ClipCallbacks {
                        on_gate_release: Box::new(move |generation| {
                            let _ = release_to.send(InputCmd::GateRelease(generation));
                        }),
                        on_notice: Box::new(move |notice| {
                            events.send(
                                tag,
                                EventBody::Clipboard(ClipboardNotice {
                                    text: format!("{notice:?}"),
                                }),
                            );
                        }),
                    },
                );
                *lock(&session.clip) = Some(started.handle.clone());
                clipboard = Some(started);
            }
            (Err(e), _) | (_, Err(SessionError::Stream(e))) => {
                core.events.send(
                    tag,
                    EventBody::Clipboard(ClipboardNotice {
                        text: format!("clipboard unavailable: {e}"),
                    }),
                );
            }
            _ => {}
        }
    }

    let mut running = Running {
        media_codec: None,
        last_config: None,
    };
    let mut close_rx = session.close.subscribe();
    let mut stats = tokio::time::interval(STATS_INTERVAL);
    let (reason, message): (DisconnectReason, String) = loop {
        tokio::select! {
            changed = close_rx.wait_for(|c| *c) => {
                let _ = changed;
                let _ = ctl_tx.send(ControlBody::Bye(Goodbye {
                    reason: CloseReason::ViewerEnded as i32,
                    message: "closed by the application".to_owned(),
                }));
                break (DisconnectReason::ClosedLocally, "closed by the application".to_owned());
            }
            e = conn.closed() => {
                let reason = match &e {
                    ConnectionError::ApplicationClosed(c) => reason_of_close_code(c.error_code.into_inner()),
                    ConnectionError::TimedOut => DisconnectReason::Network,
                    _ => DisconnectReason::Network,
                };
                break (reason, e.to_string());
            }
            msg = recv.next::<ControlMsg>() => match msg {
                Ok(Some(ControlMsg { body: Some(ControlBody::Config(config)) })) => {
                    if let Err(e) = apply_config(core, session, conn, &mut running, config) {
                        break (DisconnectReason::PipelineFailed, e);
                    }
                }
                Ok(Some(ControlMsg { body: Some(ControlBody::Focus(focus)) })) => {
                    let class = FocusClass::try_from(focus.cls).unwrap_or(FocusClass::Default);
                    session.to_input(InputCmd::Focus(class));
                }
                Ok(Some(ControlMsg { body: Some(ControlBody::Bye(bye)) })) => {
                    let reason = CloseReason::try_from(bye.reason).unwrap_or(CloseReason::Unspecified);
                    break (reason_of_close(reason), bye.message);
                }
                Ok(Some(_)) => {}
                Ok(None) => break (DisconnectReason::HostEnded, "the host closed the stream".to_owned()),
                Err(e) => break (DisconnectReason::Network, e.to_string()),
            },
            Some(cmd) = cmd_rx.recv() => {
                let SessionCtl::Stream(request) = cmd;
                let _ = ctl_tx.send(ControlBody::Request(request));
            }
            _ = stats.tick() => {
                if let Some(media) = lock(&session.media).as_ref() {
                    use std::sync::atomic::Ordering::Relaxed as R;
                    let s = &media.stats;
                    let g = *lock(&gate_stats);
                    let (path, path_detail, path_rtt_us) = path_stats(link, conn);
                    core.events.send(tag, EventBody::Stats(Stats {
                        complete: s.complete.load(R),
                        lost: s.lost.load(R),
                        fec_recovered: s.fec_recovered.load(R),
                        decoded: s.decoded.load(R),
                        decode_errors: s.decode_errors.load(R),
                        presented: s.presented.load(R),
                        rtt_us: s.rtt_us.load(R),
                        gate_pastes: g.pastes as u32,
                        gate_held: g.held as u32,
                        gate_timeouts: g.released_by_deadline as u32,
                        rx_bytes: conn.stats().udp_rx.bytes,
                        path: path as i32,
                        path_detail,
                        path_rtt_us,
                    }));
                }
            }
        }
    };

    // Teardown. Local capture ends first; the keyboard must never stay captured.
    session.stop_input();
    if let Some(p) = switch_pump {
        p.abort();
    }
    session.presenter.detach();
    drop(ctl_tx);
    let _ = tokio::time::timeout(FLUSH_TIMEOUT, writer).await;
    if let Some(writer) = input_writer.take() {
        writer.abort();
    }
    *lock(&session.command) = None;
    *lock(&session.clip) = None;
    let media = lock(&session.media).take();
    let _ = tokio::task::spawn_blocking(move || {
        if let Some(thread) = input_thread {
            let _ = thread.join();
        }
        if let Some(c) = clipboard {
            c.stop();
        }
        if let Some(m) = media {
            m.stop();
        }
    })
    .await;
    (reason, message)
}

/// What the diagnostics show about the path (§11).
fn path_stats(link: Option<&Arc<Link>>, conn: &Connection) -> (PathKind, String, u64) {
    let kind_of = |c: PathClass| match c {
        PathClass::Direct => PathKind::Direct,
        PathClass::Punched => PathKind::Punched,
        PathClass::Relay => PathKind::Relay,
        PathClass::Tailscale => PathKind::Tailscale,
    };
    match link {
        Some(l) => match l.state().active {
            Some(a) => (
                kind_of(a.class),
                describe(&a),
                a.rtt.map_or(0, |r| r.as_micros() as u64),
            ),
            None => (PathKind::Unknown, String::new(), 0),
        },
        None => {
            let remote = conn.remote_address();
            let kind = if is_tailnet(remote.ip()) {
                PathKind::Tailscale
            } else {
                PathKind::Direct
            };
            (kind, remote.to_string(), 0)
        }
    }
}

fn reason_of_close_code(code: u64) -> DisconnectReason {
    use nexa_session::proto::close;
    match code as u32 {
        close::HOST_ENDED | close::SHUTDOWN => DisconnectReason::HostEnded,
        close::REPLACED => DisconnectReason::Replaced,
        close::PIPELINE_FAILED => DisconnectReason::PipelineFailed,
        close::REJECTED => DisconnectReason::ConsentDenied,
        _ => DisconnectReason::Unspecified,
    }
}
