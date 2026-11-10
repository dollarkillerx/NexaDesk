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
    time::{Duration, Instant},
};

use nexa_auth::{
    DeviceIdentity, DevicePublicKey, PairingStore, PeerPolicy, Perms, client_config,
    peer_public_key, store::unix_now, tls::SERVER_NAME,
};
use nexa_clip::{ClipPolicy, GateStats};
use nexa_input::{FocusClass, Rewriter};
use nexa_media::PresentMode;
use nexa_net::{
    check::{PathClass, is_tailnet},
    path::CandKind,
    tailscale::{TailnetMonitor, TailscaleInfo},
    vsock::{VIRTUAL_PEER, bind_media_socket},
};
use nexa_session::{
    SessionError,
    bulk::{BulkLimiter, UplinkScavenger},
    clip::{ClipCallbacks, ClipChannel, ClipHandle, ClipSession, PromiseMode},
    framed::{FramedSend, accept_bi, open_bi, open_uni},
    handshake::{
        ControlChannel, FLUSH_TIMEOUT, Timeouts, ViewerAccepted, ViewerAuth, ViewerConfig,
        viewer_handshake,
    },
    proto::{
        AudioControl, CandidateUpdate, CloseReason, ControlBody, ControlMsg, Goodbye, InputMsg,
        RejectReason, StreamRequest,
    },
    reconnect::Backoff,
    rendezvous::{
        self, DialError, Stage,
        link::{Link, describe},
        seal::SignalKeys,
        signal::SignalConfig,
    },
    transfer::{self, CONFIRM_TIMEOUT, TransferConfig, TransferLimits, Transfers},
    viewer::{AudioSink, ViewerMedia, ViewerOptions, ViewerParams},
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
    audio::AudioPlayback,
    cursor::CursorState,
    deps::{CursorDisplay, Deps, KeySource},
    input::{self, InputCmd, InputThreadConfig, LockStats, ViewGeometry},
    presenter::PresenterSlot,
    proto::{
        ClipboardCode, ClipboardNotice, Command, ConnectAuth, ConnectRequest, ConnectStage,
        Connected, Connecting, DisconnectReason, Disconnected, DisplayInfo, Event, EventBody,
        KnownHost, LanDevice, LanDevices, PathKind, Permissions, Ready, Reconnecting,
        SessionCommand, Stats, StreamChanged, TailnetDevice, TailnetDevices, TailscaleState,
        Transfer, Warning, WarningCode,
    },
};

/// The port NexaDesk hosts listen on by default (the UI's suggestion).
pub const NEXADESK_PORT: u16 = 47800;
const TAILNET_REFRESH: Duration = Duration::from_secs(15);
/// The LAN list is local state, so checking it often costs nothing.
const LAN_REFRESH: Duration = Duration::from_secs(2);
const TAILNET_PROBE_TIMEOUT: Duration = Duration::from_millis(400);
/// Probing every peer of a large tailnet is not worth it.
const TAILNET_MAX_PROBES: usize = 32;

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
    /// Where files from hosts are saved (§3.1 stream 4).
    downloads: PathBuf,
    device: OnceCell<Arc<Device>>,
    caps: OnceCell<Caps>,
}

impl Core {
    pub fn new(
        deps: Arc<dyn Deps>,
        device_name: String,
        data_dir: PathBuf,
        downloads: PathBuf,
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
            downloads,
            device: OnceCell::new(),
            caps: OnceCell::new(),
        });
        // nexa_init must not block on the Keychain or the disk.
        if let Some(tailnet) = core.deps.tailnet() {
            core.handle
                .spawn(report_tailnet(core.events.clone(), tailnet));
        }
        if let Some(lan) = core.deps.lan() {
            core.handle.spawn(report_lan(core.events.clone(), lan));
        }
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
            stream: Mutex::new(StreamRequest {
                mode: request.mode,
                fps: request.fps,
                display: 0,
            }),
            cursor: Arc::new(Mutex::new(CursorState::default())),
            audio: Mutex::new(None),
            lock_stats: Arc::new(Mutex::new(LockStats::default())),
            transfers: Mutex::new(None),
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
    Control(ControlBody),
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
    /// What the viewer last asked of the host: mode, frame rate and display (§6.7).
    stream: Mutex<StreamRequest>,
    /// The host's cursor over the video (§6.9).
    cursor: Arc<Mutex<CursorState>>,
    /// Only in sessions granted the audio permission.
    audio: Mutex<Option<AudioPlayback>>,
    lock_stats: Arc<Mutex<LockStats>>,
    /// Only while connected, and only if the host granted `file`.
    transfers: Mutex<Option<Transfers>>,
}

impl Session {
    /// What game mode's pointer lock did in this session (engaged, released).
    pub fn lock_stats(&self) -> LockStats {
        *lock(&self.lock_stats)
    }

    /// Whether system audio is playing (unmuted, with the permission).
    pub fn audio_playing(&self) -> bool {
        lock(&self.audio).as_ref().is_some_and(|a| a.enabled())
    }

    fn update_view(&self, f: impl FnOnce(&mut ViewGeometry)) {
        let view = {
            let mut view = lock(&self.view);
            f(&mut view);
            *view
        };
        lock(&self.cursor).set_view(view);
    }

    fn send_stream_request(&self, f: impl FnOnce(&mut StreamRequest)) {
        let request = {
            let mut r = lock(&self.stream);
            f(&mut r);
            r.clone()
        };
        if let Some(ctl) = lock(&self.command).as_ref() {
            let _ = ctl.send(SessionCtl::Stream(request));
        }
    }

    /// Mutes or unmutes system audio, locally first and then at the host.
    fn set_audio(&self, enabled: bool) -> Result<(), &'static str> {
        let mut audio = lock(&self.audio);
        let Some(a) = audio.as_mut() else {
            return Err("the host did not share its audio");
        };
        if a.set_enabled(&*self.core.deps, enabled).is_err() {
            drop(audio);
            self.core.events.send(
                self.tag,
                EventBody::Warning(Warning::new(
                    WarningCode::AudioOutput,
                    "this Mac's audio output could not start",
                    "",
                )),
            );
            return Ok(());
        }
        drop(audio);
        if let Some(ctl) = lock(&self.command).as_ref() {
            let _ = ctl.send(SessionCtl::Control(ControlBody::Audio(AudioControl {
                enabled,
            })));
        }
        Ok(())
    }
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
        // SAFETY: forwarded from the caller, like the presenter's.
        let display = unsafe { self.core.deps.cursor_display(handle, kind) };
        lock(&self.cursor).attach(display);
        self.update_view(|view| {
            view.view_w = w;
            view.view_h = h;
            view.scale = scale;
        });
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
        self.update_view(|view| {
            view.view_w = w;
            view.view_h = h;
            view.scale = scale;
        });
        self.repaint();
    }

    pub fn detach_view(&self) {
        self.presenter.detach();
        let display: Option<Box<dyn CursorDisplay>> = lock(&self.cursor).detach();
        drop(display);
        self.update_view(|view| {
            view.view_w = 0;
            view.view_h = 0;
        });
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
                self.send_stream_request(|r| {
                    r.mode = m.mode;
                    r.fps = m.fps;
                });
                return Ok(());
            }
            Command::Display(d) => {
                self.send_stream_request(|r| r.display = d.display);
                return Ok(());
            }
            Command::Audio(a) => return self.set_audio(a.enabled),
            Command::SendFiles(f) => {
                const MAX_FILES: usize = 100;
                let transfers = lock(&self.transfers)
                    .clone()
                    .ok_or("the host did not allow file transfer")?;
                if f.paths.is_empty() || f.paths.len() > MAX_FILES {
                    return Err("send between 1 and 100 files at a time");
                }
                let _runtime = self.core.handle.enter();
                for p in f.paths {
                    transfers.send_file(PathBuf::from(p));
                }
                return Ok(());
            }
            Command::CancelTransfer(c) => {
                if let Some(t) = lock(&self.transfers).as_ref() {
                    t.cancel(c.id);
                }
                return Ok(());
            }
            Command::AnswerTransfer(a) => {
                let answered = lock(&self.transfers)
                    .as_ref()
                    .is_some_and(|t| t.answer(a.id, a.accept));
                return if answered {
                    Ok(())
                } else {
                    Err("no such transfer is waiting for an answer")
                };
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

/// What a reconnect (§4.4) holds on to: the device of the dropped session, and
/// how much of the 30 s window the transport may take.
struct Again {
    host: DevicePublicKey,
    budget: Duration,
}

async fn establish(
    core: &Arc<Core>,
    session: &Arc<Session>,
    request: &ConnectRequest,
    again: Option<&Again>,
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
    let (auth, policy, pinned) = match (again, pinned) {
        // A reconnect only ever goes to the device of the session that dropped.
        // If it is paired by now, it is asked as a known host; otherwise with the
        // same secret as the first time. Either way the host decides afresh:
        // a paired unattended device is let in, anything else is asked again.
        (Some(a), _) => {
            let known = lock(&device.store).pinned(&a.host).is_some();
            let auth = if known { ViewerAuth::Known } else { auth };
            (auth, PeerPolicy::Pinned(a.host), Some(a.host))
        }
        (None, Some(key)) => {
            if lock(&device.store).pinned(&key).is_none() {
                return Err((
                    DisconnectReason::NotPaired,
                    "that host is not paired".to_owned(),
                ));
            }
            (auth, PeerPolicy::Pinned(key), Some(key))
        }
        (None, None) => (auth, PeerPolicy::AcceptUnknown, None),
    };
    let config = client_config(&device.identity, policy)
        .map_err(|e| (DisconnectReason::Internal, e.to_string()))?;
    let transport = async {
        match normalize_short_id(&request.address) {
            Some(id) => {
                let (e, c, l, t) =
                    connect_by_id(core, session, &device, request, &id, pinned, config).await?;
                Ok::<_, (DisconnectReason, String)>((e, c, Some(l), Some(t)))
            }
            None => {
                let (e, c) = connect_quic(&request.address, config).await?;
                Ok((e, c, None, None))
            }
        }
    };
    let (endpoint, conn, link, trickle) = match again {
        Some(a) => tokio::time::timeout(a.budget, transport)
            .await
            .map_err(|_| {
                (
                    DisconnectReason::TimedOut,
                    "the host did not answer".to_owned(),
                )
            })??,
        None => transport.await?,
    };
    if let Some(a) = again
        && peer_public_key(&conn).ok() != Some(a.host)
    {
        conn.close(3u32.into(), b"identity mismatch");
        return Err((
            DisconnectReason::PinMismatch,
            "a different device answered the reconnect".to_owned(),
        ));
    }

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
    audio: Option<AudioSink>,
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
    session.update_view(|view| {
        view.stream_w = config.w;
        view.stream_h = config.h;
        view.display = config.display;
    });
    // The host's StreamConfig is what both ends act on (§6.7), whoever asked.
    {
        let mut r = lock(&session.stream);
        r.mode = config.mode;
        r.display = config.display;
    }
    lock(&session.cursor).set_game(mode == Mode::Game);
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
        let media = ViewerMedia::spawn_full(
            conn.clone(),
            decoder,
            session.presenter.proxy(),
            params,
            ViewerOptions {
                thread_init: core.deps.thread_init(),
                audio: running.audio.clone(),
            },
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
            display: config.display,
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
) -> Option<JoinHandle<mpsc::Receiver<InputCmd>>> {
    let rx = lock(&session.input_rx).take()?;
    let tx = session.input_tx.clone();
    let (keymap, problem) = crate::keymap::for_sessions(&core.data_dir);
    if let Some(reason) = problem {
        core.events.send(
            session.tag,
            EventBody::Warning(Warning::new(
                WarningCode::KeymapInvalid,
                format!(
                    "keymap.toml has a problem, so the built-in keyboard rules are used: {reason}"
                ),
                reason,
            )),
        );
    }
    let profile = keymap.for_session(core.deps.os(), host_os);
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
            pointer_lock: {
                let (deps, tx) = (core.deps.clone(), tx.clone());
                Box::new(move || deps.pointer_lock(tx.clone()))
            },
            cursor: session.cursor.clone(),
            lock_stats: session.lock_stats.clone(),
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
    let mut established = match establish(&core, &session, &request, None).await {
        Ok(e) => e,
        Err((reason, message)) => {
            let _ = session.input_granted.set(false);
            ended(tag, &core, reason, message);
            return;
        }
    };
    let mut granted: Option<nexa_wire::control::Permissions> = None;
    loop {
        let Accepted {
            endpoint,
            conn,
            accepted,
            link,
            _trickle,
        } = established;
        let host = accepted.host;
        let (reason, message, dropped) = match core.device().await {
            Ok(device) => {
                stream_session(
                    &core,
                    &session,
                    device,
                    &conn,
                    accepted,
                    &request,
                    link.as_ref(),
                    &mut granted,
                )
                .await
            }
            Err(m) => (DisconnectReason::Internal, m, false),
        };
        conn.close(3u32.into(), b"session ended");
        let _ = tokio::time::timeout(CLOSE_LINGER, endpoint.wait_idle()).await;
        drop((link, _trickle));
        if !dropped || *session.close.borrow() {
            ended(tag, &core, reason, message);
            return;
        }
        // §4.4 Reconnecting: the window stays; a new connection, same rules.
        match reconnect(&core, &session, &request, host, &message).await {
            Ok(e) => established = e,
            Err((reason, message)) => {
                session.presenter.detach();
                ended(tag, &core, reason, message);
                return;
            }
        }
    }
}

/// Whether a dial failure is worth another try inside the window.
fn worth_retrying(reason: DisconnectReason) -> bool {
    matches!(
        reason,
        DisconnectReason::Network
            | DisconnectReason::TimedOut
            | DisconnectReason::NoPath
            | DisconnectReason::Offline
            | DisconnectReason::SignalServer
            | DisconnectReason::RateLimited
    )
}

/// The transport went away under a running session, rather than anyone ending it.
fn transport_dropped(e: &ConnectionError) -> bool {
    matches!(
        e,
        ConnectionError::TimedOut
            | ConnectionError::Reset
            | ConnectionError::TransportError(_)
            | ConnectionError::ConnectionClosed(_)
    )
}

/// §4.4: new connections to the same device with exponential backoff, for at
/// most 30 s. Each goes through the full handshake.
async fn reconnect(
    core: &Arc<Core>,
    session: &Arc<Session>,
    request: &ConnectRequest,
    host: DevicePublicKey,
    why: &str,
) -> Result<Accepted, (DisconnectReason, String)> {
    let tag = session.tag;
    let mut backoff = Backoff::new(Instant::now());
    let mut close = session.close.subscribe();
    let closed = || {
        (
            DisconnectReason::ClosedLocally,
            "closed by the application".to_owned(),
        )
    };
    let mut last = why.to_owned();
    loop {
        let Some(delay) = backoff.next_delay(Instant::now()) else {
            return Err((
                DisconnectReason::Network,
                format!("could not reconnect within 30 s: {last}"),
            ));
        };
        core.events.send(
            tag,
            EventBody::Reconnecting(Reconnecting {
                attempt: backoff.attempts(),
                reason: why.to_owned(),
                remaining_ms: backoff.remaining(Instant::now()).as_millis() as u32,
            }),
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = close.wait_for(|c| *c) => return Err(closed()),
        }
        let budget = backoff.remaining(Instant::now());
        if budget.is_zero() {
            continue;
        }
        let again = Again { host, budget };
        let r = tokio::select! {
            r = establish(core, session, request, Some(&again)) => r,
            _ = close.wait_for(|c| *c) => return Err(closed()),
        };
        match r {
            Ok(a) => return Ok(a),
            Err((reason, message)) if worth_retrying(reason) => last = message,
            Err(e) => return Err(e),
        }
    }
}

/// Runs one connection of the session. The flag says the transport dropped
/// under it (§4.4: reconnect) rather than anyone ending it.
#[allow(clippy::too_many_arguments)]
async fn stream_session(
    core: &Arc<Core>,
    session: &Arc<Session>,
    device: Arc<Device>,
    conn: &Connection,
    accepted: ViewerAccepted,
    request: &ConnectRequest,
    link: Option<&Arc<Link>>,
    granted: &mut Option<nexa_wire::control::Permissions>,
) -> (DisconnectReason, String, bool) {
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
    // The window was built for what the first connection granted; a reconnect
    // that was given something else (the person at the host decided again)
    // starts over as a new session instead.
    if granted.as_ref().is_some_and(|g| *g != perms) {
        return (
            DisconnectReason::Unspecified,
            "the host granted different permissions after reconnecting; connect again".to_owned(),
            false,
        );
    }
    *granted = Some(perms.clone());
    // Older hosts sent their name as the first display's.
    let host_name = Some(accepted.accept.host_name.clone())
        .filter(|n| !n.is_empty())
        .or_else(|| accepted.accept.displays.first().map(|d| d.name.clone()))
        .unwrap_or_default();
    let displays: Vec<DisplayInfo> = accepted
        .accept
        .displays
        .iter()
        .map(|d| DisplayInfo {
            id: d.id,
            name: d.name.clone(),
            w: d.w,
            h: d.h,
        })
        .collect();
    let paired_now = match store_pairing(&device, &accepted, &host_name) {
        Ok(p) => p,
        Err(e) => {
            core.events.send(
                tag,
                EventBody::Warning(Warning::new(
                    WarningCode::RememberHost,
                    format!("could not remember this host: {e}"),
                    e.to_string(),
                )),
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
    // §4.5: after a network change, our new candidates go on the same stream.
    let update_pump = link
        .and_then(|l| l.take_candidate_updates())
        .map(|mut updates| {
            let ctl = ctl_tx.clone();
            tokio::spawn(async move {
                while let Some(candidates) = updates.recv().await {
                    if ctl
                        .send(ControlBody::Candidates(CandidateUpdate { candidates }))
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
        display: lock(&session.stream).display,
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
            displays,
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
            Err(e) => {
                let dropped = conn.close_reason().as_ref().is_some_and(transport_dropped);
                return (DisconnectReason::Network, e.to_string(), dropped);
            }
        }
    }

    // §4.9: the clipboard and every file transfer share one bulk budget. This side
    // has no rate controller for its uplink, so the budget follows the round trip.
    let bulk = BulkLimiter::new(Arc::new(UplinkScavenger::new(conn.clone())));
    // Files only with the permission, in both directions.
    let transfers = perms.file.then(|| {
        let events = core.events.clone();
        Transfers::new(
            conn.clone(),
            TransferConfig {
                downloads: core.downloads.clone(),
                limits: TransferLimits::default(),
                bulk: bulk.clone(),
                confirm_timeout: CONFIRM_TIMEOUT,
            },
            Arc::new(move |e| events.send(tag, EventBody::Transfer(Transfer::of(&e)))),
        )
    });
    *lock(&session.transfers) = transfers.clone();
    let acceptor = tokio::spawn(accept_host_streams(conn.clone(), transfers.clone()));

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
                            events.send(tag, EventBody::Clipboard(clipboard_notice(notice)));
                        }),
                    },
                    bulk.clone(),
                );
                *lock(&session.clip) = Some(started.handle.clone());
                clipboard = Some(started);
            }
            (Err(e), _) | (_, Err(SessionError::Stream(e))) => {
                core.events.send(
                    tag,
                    EventBody::Clipboard(ClipboardNotice {
                        text: format!("clipboard unavailable: {e}"),
                        code: ClipboardCode::Unavailable as i32,
                        detail: e.to_string(),
                        ..Default::default()
                    }),
                );
            }
            _ => {}
        }
    }

    // Audio only with the permission; it starts unmuted and the UI can mute it.
    let mut audio_sink = None;
    if perms.audio {
        match core.deps.audio_decoder() {
            Ok(decoder) => {
                let playback = AudioPlayback::new(decoder);
                audio_sink = Some(playback.sink());
                *lock(&session.audio) = Some(playback);
                let _ = session.set_audio(true);
            }
            Err(e) => core.events.send(
                tag,
                EventBody::Warning(Warning::new(
                    WarningCode::AudioPlayback,
                    format!("the host's audio cannot be played here: {e}"),
                    e.to_string(),
                )),
            ),
        }
    }

    let mut running = Running {
        media_codec: None,
        last_config: None,
        audio: audio_sink,
    };
    let mut close_rx = session.close.subscribe();
    let mut stats = tokio::time::interval(STATS_INTERVAL);
    let mut warned_path: Option<PathKind> = None;
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
                Ok(Some(ControlMsg { body: Some(ControlBody::CursorShape(shape)) })) => {
                    lock(&session.cursor).on_shape(shape);
                }
                Ok(Some(ControlMsg { body: Some(ControlBody::CursorSet(set)) })) => {
                    lock(&session.cursor).on_set(set);
                }
                Ok(Some(ControlMsg { body: Some(ControlBody::CursorMoved(moved)) })) => {
                    let active = session.want_active.load(Relaxed) && perms.input;
                    if lock(&session.cursor).on_moved(&moved, active).is_some() {
                        session.to_input(InputCmd::Warped(moved.x, moved.y));
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
                Ok(Some(ControlMsg { body: Some(ControlBody::Candidates(c)) })) => {
                    if let Some(l) = link {
                        l.apply_candidate_update(&c.candidates);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break (DisconnectReason::HostEnded, "the host closed the stream".to_owned()),
                Err(e) => break (DisconnectReason::Network, e.to_string()),
            },
            Some(cmd) = cmd_rx.recv() => match cmd {
                SessionCtl::Stream(request) => {
                    let _ = ctl_tx.send(ControlBody::Request(request));
                }
                SessionCtl::Control(body) => {
                    let _ = ctl_tx.send(body);
                }
            },
            _ = stats.tick() => {
                let (path, _, _) = path_stats(link, conn, core.deps.tailnet().as_deref());
                let game = running
                    .last_config
                    .as_ref()
                    .is_some_and(|c| Mode::try_from(c.mode) == Ok(Mode::Game));
                if let Some((code, text)) = path_warning(path, game)
                    && warned_path != Some(path)
                {
                    warned_path = Some(path);
                    core.events.send(tag, EventBody::Warning(Warning::new(code, text, "")));
                }
                if let Some(media) = lock(&session.media).as_ref() {
                    use std::sync::atomic::Ordering::Relaxed as R;
                    let s = &media.stats;
                    let g = *lock(&gate_stats);
                    let (path, path_detail, path_rtt_us) = path_stats(link, conn, core.deps.tailnet().as_deref());
                    let a = lock(&session.audio).as_ref().map(|a| a.stats()).unwrap_or_default();
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
                        audio_played: a.played,
                        audio_concealed: a.concealed,
                        audio_buffer_us: a.mean_delay().as_micros().min(u128::from(u32::MAX)) as u32,
                        latency: latency_of(&media.latency),
                    }));
                }
            }
        }
    };

    // The transport dropped (§4.4): the window stays and shows the last frame
    // while we reconnect. Anything else ends the session.
    let dropped = reason == DisconnectReason::Network
        && conn.close_reason().as_ref().is_some_and(transport_dropped);

    // Teardown. Local capture ends first; the keyboard must never stay captured.
    session.stop_input();
    acceptor.abort();
    *lock(&session.transfers) = None;
    if let Some(t) = &transfers {
        t.shutdown();
    }
    // No sound and no host cursor once the session is over.
    let playback = lock(&session.audio).take();
    drop(playback);
    {
        let mut cursor = lock(&session.cursor);
        cursor.set_locked(false);
        cursor.set_game(true);
    }
    if let Some(p) = switch_pump {
        p.abort();
    }
    if let Some(p) = update_pump {
        p.abort();
    }
    if !dropped {
        session.presenter.detach();
    }
    drop(ctl_tx);
    let _ = tokio::time::timeout(FLUSH_TIMEOUT, writer).await;
    if let Some(writer) = input_writer.take() {
        writer.abort();
    }
    *lock(&session.command) = None;
    *lock(&session.clip) = None;
    let media = lock(&session.media).take();
    let rx = tokio::task::spawn_blocking(move || {
        let rx = input_thread.and_then(|thread| thread.join().ok());
        if let Some(c) = clipboard {
            c.stop();
        }
        if let Some(m) = media {
            m.stop();
        }
        rx
    })
    .await
    .ok()
    .flatten();
    // A reconnected session starts a new input thread on the same channel.
    if let Some(rx) = rx {
        *lock(&session.input_rx) = Some(rx);
    }
    session.input_started.store(false, Relaxed);
    (reason, message, dropped)
}

/// §6.1 segments for the HUD, in the ABI's numbering.
fn latency_of(l: &nexa_session::latency::LatencyStats) -> Vec<crate::proto::LatencySegment> {
    use crate::proto::Segment as P;
    use nexa_session::latency::Segment as S;
    l.summary()
        .into_iter()
        .map(|p| crate::proto::LatencySegment {
            segment: match p.segment {
                S::HostAndNetwork => P::HostAndNetwork,
                S::Receive => P::Receive,
                S::DecodeQueue => P::DecodeQueue,
                S::Decode => P::Decode,
                S::PresentQueue => P::PresentQueue,
                S::Present => P::Present,
                S::Total => P::Total,
            } as i32,
            p50_us: p.p50_us,
            p99_us: p.p99_us,
        })
        .collect()
}

/// Streams the host opens: only File (§3.1), and only into `transfers`. Without the
/// permission a File stream is refused unread; any other kind is stopped.
async fn accept_host_streams(conn: Connection, transfers: Option<Transfers>) {
    loop {
        match accept_bi(&conn).await {
            Ok((StreamType::File, send, recv)) => match &transfers {
                Some(t) => t.accept(send, recv),
                None => transfer::refuse(send, recv),
            },
            Ok((_, send, recv)) => {
                recv.stop();
                drop(send);
            }
            Err(SessionError::Connection(_)) => return,
            Err(_) => {}
        }
    }
}

/// A clipboard status for the toolbar: a code the UI words, and an English fallback.
fn clipboard_notice(n: nexa_clip::ClipNotice) -> ClipboardNotice {
    use nexa_clip::{ClipKind, ClipNotice as N};
    let kind_name = |k: ClipKind| match k {
        ClipKind::Text => "text",
        ClipKind::Html => "html",
        ClipKind::Rtf => "rtf",
        ClipKind::Png => "png",
    };
    let (code, kind, size) = match n {
        N::SensitiveKeptLocal => (ClipboardCode::SensitiveKeptLocal, "", 0),
        N::TooLarge { kind, size } => (ClipboardCode::TooLarge, kind_name(kind), size),
        N::PeerRejected { .. } => (ClipboardCode::PeerRejected, "", 0),
        N::WriteFailed { .. } => (ClipboardCode::WriteFailed, "", 0),
        N::FetchFailed { kind, .. } => (ClipboardCode::FetchFailed, kind_name(kind), 0),
    };
    ClipboardNotice {
        text: format!("{n:?}"),
        code: code as i32,
        kind: kind.into(),
        size,
        detail: String::new(),
    }
}

/// What to tell someone in game mode about the path (§4.6, §5.2). The UI words
/// it from the code; the text is the English fallback.
fn path_warning(path: PathKind, game: bool) -> Option<(WarningCode, &'static str)> {
    if !game {
        return None;
    }
    match path {
        PathKind::RelayWss => Some((
            WarningCode::GameWssRelay,
            "game mode: UDP is blocked, so the session goes through the WSS (TCP) relay; expect more latency and stutter",
        )),
        PathKind::TailscaleDerp => Some((
            WarningCode::GameDerp,
            "game mode: Tailscale is relaying through DERP, which limits latency and bandwidth",
        )),
        _ => None,
    }
}

/// What the diagnostics show about the path (§11).
fn path_stats(
    link: Option<&Arc<Link>>,
    conn: &Connection,
    tailnet: Option<&TailnetMonitor>,
) -> (PathKind, String, u64) {
    let kind_of = |c: PathClass, k: CandKind| match (c, k) {
        (_, CandKind::WssRelay) => PathKind::RelayWss,
        (_, CandKind::TailnetDerp) => PathKind::TailscaleDerp,
        (PathClass::Direct, _) => PathKind::Direct,
        (PathClass::Punched, _) => PathKind::Punched,
        (PathClass::Relay, _) => PathKind::Relay,
        (PathClass::Tailscale, _) => PathKind::Tailscale,
    };
    match link {
        Some(l) => match l.state().active {
            Some(a) => (
                kind_of(a.class, a.kind),
                describe(&a),
                a.rtt.map_or(0, |r| r.as_micros() as u64),
            ),
            None => (PathKind::Unknown, String::new(), 0),
        },
        None => {
            let remote = nexa_net::vsock::canonical(conn.remote_address());
            let kind = if is_tailnet(remote.ip()) {
                match tailnet.and_then(|t| t.direct(remote.ip())) {
                    Some(false) => PathKind::TailscaleDerp,
                    _ => PathKind::Tailscale,
                }
            } else {
                PathKind::Direct
            };
            (kind, remote.to_string(), 0)
        }
    }
}

/// The Tailscale peers worth listing in the connect card: online, with a
/// NexaDesk host answering on its port (§4.7, LocalAPI level).
pub fn tailnet_devices(
    info: &TailscaleInfo,
    port: u16,
    probe: &dyn Fn(SocketAddr) -> bool,
) -> Vec<TailnetDevice> {
    info.peers
        .iter()
        .filter(|p| p.online)
        .filter_map(|p| {
            let ip = p.ips.iter().find(|ip| ip.is_ipv4()).or(p.ips.first())?;
            Some((p, SocketAddr::new(*ip, port)))
        })
        .take(TAILNET_MAX_PROBES)
        .filter(|(_, addr)| probe(*addr))
        .map(|(p, addr)| TailnetDevice {
            name: if p.name.is_empty() {
                p.dns_name.clone()
            } else {
                p.name.clone()
            },
            address: addr.to_string(),
            direct: p.direct != Some(false),
            os: p.os.clone(),
            dns_name: p.dns_name.clone(),
        })
        .collect()
}

/// Connection IDs for the QUIC probe: unpredictable enough to match answers,
/// nothing secret rides on them.
fn probe_ids(buf: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    for chunk in buf.chunks_mut(8) {
        h.write_u64(Instant::now().elapsed().as_nanos() as u64);
        let v = h.finish().to_ne_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
}

/// This machine's Tailscale without the device list: what the status says,
/// or, when it cannot be read, whatever tailnet addresses the interfaces have.
pub fn tailnet_status(info: Option<&TailscaleInfo>, local: &[std::net::IpAddr]) -> TailnetDevices {
    match info {
        Some(i) => TailnetDevices {
            devices: Vec::new(),
            state: TailscaleState::Running as i32,
            self_name: i.self_name.clone(),
            self_dns_name: i.self_dns_name.clone(),
            self_ips: i.self_ips.iter().map(|ip| ip.to_string()).collect(),
        },
        None => TailnetDevices {
            devices: Vec::new(),
            state: if local.is_empty() {
                TailscaleState::Absent
            } else {
                TailscaleState::AddressOnly
            } as i32,
            self_ips: local.iter().map(|ip| ip.to_string()).collect(),
            ..Default::default()
        },
    }
}

/// Sends this machine's Tailscale and the tailnet device list (tag 0)
/// whenever either changes, and once at start even when there is no Tailscale.
async fn report_tailnet(events: EventTx, tailnet: Arc<TailnetMonitor>) {
    let mut last: Option<TailnetDevices> = None;
    loop {
        let info = tailnet.snapshot();
        let local = if info.is_none() {
            nexa_net::tailscale::discover_local()
        } else {
            Vec::new()
        };
        let mut status = tailnet_status(info.as_ref(), &local);
        if let Some(info) = info {
            status.devices = tokio::task::spawn_blocking(move || {
                tailnet_devices(&info, NEXADESK_PORT, &|addr| {
                    nexa_net::tailscale::probe_quic(addr, TAILNET_PROBE_TIMEOUT, &mut probe_ids)
                })
            })
            .await
            .unwrap_or_default();
        }
        if last.as_ref() != Some(&status) {
            events.send(0, EventBody::Tailnet(status.clone()));
            last = Some(status);
        }
        tokio::time::sleep(TAILNET_REFRESH).await;
    }
}

/// The LAN list for the connect card: what was heard, less this machine's own host.
pub fn lan_devices(
    peers: &[nexa_net::discovery::LanPeer],
    local: &[std::net::IpAddr],
) -> Vec<LanDevice> {
    peers
        .iter()
        .filter(|p| !p.is_this_machine(local))
        .map(|p| LanDevice {
            name: p.name.clone(),
            address: p.addrs[0].to_string(),
            os: p.os.clone(),
            device_id: p.id.clone(),
        })
        .collect()
}

/// Sends the LAN device list (tag 0) whenever it changes.
async fn report_lan(events: EventTx, browser: Arc<nexa_net::discovery::Browser>) {
    let mut last: Option<Vec<LanDevice>> = None;
    loop {
        let local = nexa_net::discovery::local_ips();
        let devices = lan_devices(&browser.peers(), &local);
        if last.as_ref() != Some(&devices) {
            events.send(
                0,
                EventBody::Lan(LanDevices {
                    devices: devices.clone(),
                }),
            );
            last = Some(devices);
        }
        tokio::time::sleep(LAN_REFRESH).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use nexa_net::tailscale::TsPeer;

    fn peer(name: &str, ip: &str, online: bool, direct: Option<bool>) -> TsPeer {
        TsPeer {
            name: name.into(),
            dns_name: format!("{name}.tail1234.ts.net"),
            os: "macOS".into(),
            ips: vec![ip.parse().unwrap(), "fd7a:115c:a1e0::9".parse().unwrap()],
            online,
            direct,
            relay: "fra".into(),
        }
    }

    #[test]
    fn lan_list_leaves_out_this_machine_and_dials_the_best_address() {
        use nexa_net::discovery::LanPeer;
        let peer = |id: &str, addrs: &[&str]| LanPeer {
            id: id.into(),
            name: format!("host {id}"),
            os: "macos".into(),
            addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
        };
        let peers = [
            peer("a", &["192.168.1.5:47800", "[2001:db8::5]:47800"]),
            peer("me", &["192.168.1.9:47800"]),
        ];
        let local: Vec<std::net::IpAddr> =
            vec!["127.0.0.1".parse().unwrap(), "192.168.1.9".parse().unwrap()];
        let list = lan_devices(&peers, &local);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].device_id, "a");
        assert_eq!(list[0].address, "192.168.1.5:47800");
        assert_eq!(list[0].name, "host a");
    }

    #[test]
    fn tailnet_status_says_running_address_only_or_absent() {
        let info = TailscaleInfo {
            self_ips: vec!["100.64.0.1".parse().unwrap()],
            self_name: "me".into(),
            self_dns_name: "me.tail1234.ts.net".into(),
            ..Default::default()
        };
        let running = tailnet_status(Some(&info), &[]);
        assert_eq!(running.state, TailscaleState::Running as i32);
        assert_eq!(running.self_dns_name, "me.tail1234.ts.net");
        assert_eq!(running.self_ips, vec!["100.64.0.1"]);
        let local = ["100.101.22.7".parse().unwrap()];
        let address_only = tailnet_status(None, &local);
        assert_eq!(address_only.state, TailscaleState::AddressOnly as i32);
        assert_eq!(address_only.self_ips, vec!["100.101.22.7"]);
        assert!(address_only.self_dns_name.is_empty());
        assert_eq!(
            tailnet_status(None, &[]).state,
            TailscaleState::Absent as i32
        );
    }

    #[test]
    fn tailnet_list_has_online_peers_that_answer_on_the_port() {
        let info = TailscaleInfo {
            self_ips: vec![],
            peers: vec![
                peer("studio", "100.64.0.2", true, Some(true)),
                peer("laptop", "100.64.0.3", true, Some(false)),
                peer("asleep", "100.64.0.4", false, None),
                peer("no-nexadesk", "100.64.0.5", true, Some(true)),
            ],
            ..Default::default()
        };
        let asked = Mutex::new(Vec::new());
        let list = tailnet_devices(&info, NEXADESK_PORT, &|a| {
            lock(&asked).push(a);
            a.ip() != "100.64.0.5".parse::<std::net::IpAddr>().unwrap()
        });
        let names: Vec<&str> = list.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["studio", "laptop"]);
        assert_eq!(list[0].address, "100.64.0.2:47800", "IPv4 first");
        assert!(list[0].direct);
        assert!(!list[1].direct, "DERP");
        assert_eq!(lock(&asked).len(), 3, "offline peers are not probed");
    }

    #[test]
    fn game_mode_warns_about_wss_and_derp_only() {
        assert!(path_warning(PathKind::RelayWss, true).is_some());
        assert!(path_warning(PathKind::TailscaleDerp, true).is_some());
        assert!(path_warning(PathKind::Relay, true).is_none());
        assert!(path_warning(PathKind::RelayWss, false).is_none());
    }

    #[test]
    fn only_transport_failures_reconnect() {
        assert!(transport_dropped(&ConnectionError::TimedOut));
        assert!(transport_dropped(&ConnectionError::Reset));
        assert!(!transport_dropped(&ConnectionError::LocallyClosed));
        assert!(worth_retrying(DisconnectReason::NoPath));
        assert!(!worth_retrying(DisconnectReason::AuthFailed));
        assert!(!worth_retrying(DisconnectReason::PinMismatch));
        assert!(!worth_retrying(DisconnectReason::ConsentDenied));
    }
}
