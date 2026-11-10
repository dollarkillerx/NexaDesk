//! Local IPC for the desktop UI (§5.3).
//!
//! A Unix socket in the data directory, mode 0600, and every connection's peer must
//! have this process's uid: the check runs before a single byte is read. Frames use
//! the reliable-stream framing (`varint length + body`) carrying the messages in
//! `proto/agent_ipc.proto`. The first frame must be `Hello`; anything malformed,
//! oversized or late closes the connection.
//!
//! Nothing here can grant access by itself. A `Decide` only answers the question
//! [`IpcHub`] is asking on behalf of the handshake, whose verdict still comes from
//! `nexa_auth::decide`.

mod hub;
pub mod proto;

use std::{
    io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub use hub::IpcHub;
use nexa_auth::{DevicePublicKey, Perms};
use nexa_wire::framing::{FrameDecoder, encode_frame};
use prost::Message;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{mpsc, watch},
};

use self::{
    hub::{OUTBOUND_QUEUE, Outgoing},
    proto::{AgentBody, AgentToUi, PairedDevice, Pairings, Refused, Status, UiBody, UiToAgent},
};
use crate::Agent;

pub const PROTOCOL_VERSION: u32 = 1;
pub const SOCKET_NAME: &str = "agent.sock";
/// IPC messages are small; a frame larger than this is an attack or a bug.
pub const MAX_FRAME: usize = 64 * 1024;
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("another nexa-agent is already running ({0})")]
    AlreadyRunning(PathBuf),
    #[error("{0} exists and is not a socket owned by this user")]
    NotOurSocket(PathBuf),
    #[error("{0} is writable by other users")]
    UnsafeDirectory(PathBuf),
    #[error("ipc socket {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

#[derive(Clone, Debug)]
pub struct IpcConfig {
    pub path: PathBuf,
    /// The only uid allowed to connect. This process's uid outside tests.
    pub expected_uid: u32,
    pub hello_timeout: Duration,
}

impl IpcConfig {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(SOCKET_NAME),
            expected_uid: current_uid(),
            hello_timeout: HELLO_TIMEOUT,
        }
    }
}

pub fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> IpcError + '_ {
    move |source| IpcError::Io {
        path: path.to_owned(),
        source,
    }
}

/// A bound socket that is not yet accepting.
pub struct IpcListener {
    config: IpcConfig,
    listener: UnixListener,
    ino: (u64, u64),
}

impl IpcListener {
    /// Binds the socket. A socket left by a dead agent is replaced; a live one is an
    /// error, as is anything at the path that is not our socket. Must be called
    /// inside a tokio runtime.
    pub fn bind(config: IpcConfig) -> Result<Self, IpcError> {
        let path = config.path.clone();
        let dir = path.parent().unwrap_or(Path::new("."));
        if !dir.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(io_err(dir))?;
        }
        let me = current_uid();
        let meta = std::fs::metadata(dir).map_err(io_err(dir))?;
        if meta.uid() != me || meta.mode() & 0o022 != 0 {
            return Err(IpcError::UnsafeDirectory(dir.to_owned()));
        }
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_socket() && m.uid() == me => {
                match std::os::unix::net::UnixStream::connect(&path) {
                    Ok(_) => return Err(IpcError::AlreadyRunning(path)),
                    Err(_) => std::fs::remove_file(&path).map_err(io_err(&path))?,
                }
            }
            Ok(_) => return Err(IpcError::NotOurSocket(path)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path)(e)),
        }
        let listener = UnixListener::bind(&path).map_err(io_err(&path))?;
        // The peer uid check below covers the moment before this applies.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(io_err(&path))?;
        let m = std::fs::symlink_metadata(&path).map_err(io_err(&path))?;
        Ok(Self {
            config,
            listener,
            ino: (m.dev(), m.ino()),
        })
    }

    /// Starts accepting UI connections.
    pub fn serve(
        self,
        hub: Arc<IpcHub>,
        agent: Arc<Agent>,
        on_shutdown: Arc<dyn Fn() + Send + Sync>,
    ) -> IpcServer {
        let (stop, stop_rx) = watch::channel(false);
        let ctx = Arc::new(Ctx {
            hub,
            agent,
            on_shutdown,
            expected_uid: self.config.expected_uid,
            hello_timeout: self.config.hello_timeout,
            stop: stop_rx,
        });
        let listener = self.listener;
        let task = tokio::spawn(accept_loop(listener, ctx));
        IpcServer {
            path: self.config.path,
            ino: self.ino,
            stop,
            task,
        }
    }
}

/// The running server. Dropping it stops accepting, closes the UI connection and
/// removes the socket.
pub struct IpcServer {
    path: PathBuf,
    ino: (u64, u64),
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl IpcServer {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.task.abort();
        // Only if it is still the socket this server created.
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && (m.dev(), m.ino()) == self.ino
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct Ctx {
    hub: Arc<IpcHub>,
    agent: Arc<Agent>,
    on_shutdown: Arc<dyn Fn() + Send + Sync>,
    expected_uid: u32,
    hello_timeout: Duration,
    stop: watch::Receiver<bool>,
}

async fn accept_loop(listener: UnixListener, ctx: Arc<Ctx>) {
    let mut stop = ctx.stop.clone();
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = stop.wait_for(|s| *s) => return,
        };
        match accepted {
            Ok((stream, _)) => {
                tokio::spawn(connection(stream, ctx.clone()));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Detaches the UI however the connection ends.
struct Attachment {
    hub: Arc<IpcHub>,
    conn: u64,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.hub.detach(self.conn);
    }
}

enum ReadError {
    Closed,
    Malformed,
}

/// Cancel-safe: the only await is the read, before any byte is consumed.
async fn read_msg(
    rd: &mut OwnedReadHalf,
    decoder: &mut FrameDecoder,
    buf: &mut [u8],
) -> Result<UiToAgent, ReadError> {
    loop {
        match decoder.next_frame() {
            Ok(Some(frame)) => return UiToAgent::decode(frame).map_err(|_| ReadError::Malformed),
            Ok(None) => {}
            Err(_) => return Err(ReadError::Malformed),
        }
        let n = rd.read(buf).await.map_err(|_| ReadError::Closed)?;
        if n == 0 {
            return Err(ReadError::Closed);
        }
        decoder.push(&buf[..n]);
    }
}

async fn write_msg(wr: &mut OwnedWriteHalf, msg: &AgentToUi) -> bool {
    let mut out = bytes::BytesMut::new();
    encode_frame(&msg.encode_to_vec(), &mut out);
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, wr.write_all(&out)).await,
        Ok(Ok(()))
    )
}

fn refused(command: &str, reason: impl Into<String>) -> AgentToUi {
    AgentBody::Refused(Refused {
        command: command.to_owned(),
        reason: reason.into(),
    })
    .into()
}

fn status_of(agent: &Agent) -> AgentToUi {
    AgentBody::Status(Status {
        version: PROTOCOL_VERSION,
        listen: agent.local_addr().to_string(),
        code: agent.current_code().to_string(),
        device_name: agent.device_name().to_owned(),
        device_id: agent.device_id().to_string(),
        device_key: agent.public_key().to_hex(),
        session: agent.active_session().map(|s| (&s).into()),
        input_available: agent.input_available(),
    })
    .into()
}

fn pairings_of(agent: &Agent) -> AgentToUi {
    let devices = agent
        .pairings()
        .into_iter()
        .map(|r| PairedDevice {
            device_key: r.pubkey.to_hex(),
            device_id: r.pubkey.device_id().to_string(),
            name: r.name,
            perms: Some(r.perms.into()),
            added_at: r.added_at,
            last_seen: r.last_seen,
            unattended: r.unattended,
        })
        .collect();
    AgentBody::Pairings(Pairings { devices }).into()
}

fn key_of(hex: &str) -> Option<DevicePublicKey> {
    DevicePublicKey::from_hex(hex).ok()
}

async fn connection(stream: UnixStream, ctx: Arc<Ctx>) {
    // Before anything is read: a process of another user gets nothing at all.
    let Ok(cred) = stream.peer_cred() else { return };
    if cred.uid() != ctx.expected_uid {
        return;
    }
    let (mut rd, mut wr) = stream.into_split();
    let mut decoder = FrameDecoder::new(MAX_FRAME);
    let mut buf = vec![0u8; 4096];

    let hello = tokio::time::timeout(ctx.hello_timeout, read_msg(&mut rd, &mut decoder, &mut buf));
    let hello = match hello.await {
        Ok(Ok(UiToAgent {
            body: Some(UiBody::Hello(h)),
        })) => h,
        _ => return,
    };
    if hello.version != PROTOCOL_VERSION {
        let _ = write_msg(&mut wr, &refused("hello", "unsupported protocol version")).await;
        return;
    }
    let (tx, mut rx) = mpsc::channel(OUTBOUND_QUEUE);
    let Some(conn) = ctx.hub.attach(tx) else {
        let _ = write_msg(
            &mut wr,
            &refused("hello", "another NexaDesk window is already connected"),
        )
        .await;
        return;
    };
    let _attachment = Attachment {
        hub: ctx.hub.clone(),
        conn,
    };
    if !write_msg(&mut wr, &status_of(&ctx.agent)).await {
        return;
    }

    let mut stop = ctx.stop.clone();
    loop {
        let reply = tokio::select! {
            msg = read_msg(&mut rd, &mut decoder, &mut buf) => match msg {
                Ok(msg) => handle(&ctx, conn, msg),
                Err(ReadError::Closed | ReadError::Malformed) => return,
            },
            out = rx.recv() => match out {
                Some(Outgoing::Message(m)) => Some(*m),
                Some(Outgoing::StatusChanged) => Some(status_of(&ctx.agent)),
                // The hub dropped this UI (it fell behind).
                None => return,
            },
            _ = stop.wait_for(|s| *s) => return,
        };
        if let Some(reply) = reply
            && !write_msg(&mut wr, &reply).await
        {
            return;
        }
    }
}

fn perms_of(p: Option<proto::Permissions>) -> Perms {
    let p = p.unwrap_or_default();
    Perms {
        view: p.view,
        input: p.input,
        clipboard: p.clipboard,
        file: p.file,
        audio: p.audio,
    }
}

fn handle(ctx: &Ctx, conn: u64, msg: UiToAgent) -> Option<AgentToUi> {
    let agent = &ctx.agent;
    match msg.body {
        None => Some(refused("unknown", "unknown command")),
        Some(UiBody::Hello(_)) => Some(refused("hello", "already said hello")),
        Some(UiBody::GetStatus(_)) => Some(status_of(agent)),
        Some(UiBody::ListPairings(_)) => Some(pairings_of(agent)),
        Some(UiBody::RevokePairing(r)) => {
            let Some(key) = key_of(&r.device_key) else {
                return Some(refused("revoke", "bad device key"));
            };
            match agent.revoke_pairing(&key) {
                Ok(_) => Some(pairings_of(agent)),
                Err(e) => Some(refused("revoke", e)),
            }
        }
        Some(UiBody::SetAccessPolicy(p)) => {
            let Some(key) = key_of(&p.device_key) else {
                return Some(refused("policy", "bad device key"));
            };
            match agent.restrict_pairing(&key, p.unattended, perms_of(p.perms)) {
                Ok(true) => Some(pairings_of(agent)),
                Ok(false) => Some(refused("policy", "no such paired device")),
                Err(e) => Some(refused("policy", e)),
            }
        }
        Some(UiBody::Disconnect(_)) => {
            agent.disconnect();
            None
        }
        Some(UiBody::RotateCode(_)) => {
            agent.rotate_code();
            None
        }
        Some(UiBody::Decide(d)) => {
            if ctx.hub.decide(conn, &d) {
                None
            } else {
                Some(refused("decide", "no such open request"))
            }
        }
        Some(UiBody::Shutdown(_)) => {
            (ctx.on_shutdown)();
            None
        }
    }
}
