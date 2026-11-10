//! The Control stream handshake (§3.4, §9.2) for both roles.
//!
//! The host's only source of an access verdict is [`nexa_auth::decide`]. Everything
//! here gathers the evidence it needs and carries out what it returns; the one thing
//! added is an extra prompt when a session is already running, since accepting would
//! take over someone's session (§5.3).

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use nexa_auth::{
    AccessDecision, AccessPolicy, ConsentPrompt, DenyReason, DevicePublicKey, Evidence, HelloAuth,
    HostPairing, HostPassword, HostVerified, OneTimeCode, PairError, PairingContext, PairingStore,
    PasswordGuard, Perms, ViewerEvent, ViewerPairing, ViewerVerified, decide, hello_auth,
    pair_exporter, peer_public_key, store::unix_now,
};
use nexa_wire::{
    control::{Auth, Caps, ClientHello, Display, Os, Permissions, SessionAccept},
    framing::StreamType,
};
use quinn::Connection;
use zeroize::Zeroizing;

use crate::{
    SessionError,
    framed::{FramedRecv, FramedSend, accept_bi, open_bi},
    negotiate,
    proto::{ControlBody, ControlMsg, RejectReason, SessionReject, close},
};

/// Time for a whole exchange, excluding the person at the host making up their mind.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
pub const CONSENT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a refusal waits to reach the peer before the connection is closed.
pub const FLUSH_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_DEVICE_NAME: usize = 64;

/// Pairing attempts allowed per source address per [`PAIR_WINDOW`]. Without this, any
/// reachable peer can burn the code's five attempts and force it to rotate (§9.5).
const PAIR_ATTEMPTS_PER_WINDOW: u32 = 8;
const PAIR_WINDOW: Duration = Duration::from_secs(60);
const LIMITER_MAX_TRACKED: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub handshake: Duration,
    pub consent: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            handshake: HANDSHAKE_TIMEOUT,
            consent: CONSENT_TIMEOUT,
        }
    }
}

/// Which evidence the prompt is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthKind {
    Paired,
    Code,
    Password,
}

/// What a person is being asked (§9.3). `device_name` has been sanitized for display.
#[derive(Clone, Debug)]
pub struct ConsentRequest {
    pub peer: DevicePublicKey,
    pub device_name: String,
    pub remote: SocketAddr,
    pub auth: AuthKind,
    pub prompt: ConsentPrompt,
    /// Accepting ends the session that is running now.
    pub takeover: bool,
    /// Set if the connection goes away; stop asking when it does.
    pub cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConsentAnswer {
    pub allow: bool,
    pub perms: Permissions,
    pub save: bool,
    pub skip_next_time: bool,
}

impl ConsentAnswer {
    pub fn deny() -> Self {
        Self {
            allow: false,
            perms: Permissions {
                view: false,
                input: false,
                clipboard: false,
                file: false,
                audio: false,
            },
            save: false,
            skip_next_time: false,
        }
    }
}

/// How the host asks a person. Implementations must deny when no one can answer.
pub trait Consent: Send + Sync {
    /// Whether someone can see a prompt and a running-session indicator right now.
    /// When false, every session is refused, including those needing no prompt.
    fn can_present(&self) -> bool;

    /// Blocks up to `timeout`. Runs on a blocking thread. Must not act on input
    /// typed before the question was shown.
    fn ask(&self, request: &ConsentRequest, timeout: Duration) -> ConsentAnswer;
}

/// Limits pairing attempts per source address.
#[derive(Default)]
pub struct PairLimiter {
    seen: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl PairLimiter {
    pub fn allow(&self, addr: IpAddr) -> bool {
        self.allow_at(addr, Instant::now())
    }

    fn allow_at(&self, addr: IpAddr, now: Instant) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() >= LIMITER_MAX_TRACKED {
            seen.retain(|_, (_, start)| now.saturating_duration_since(*start) < PAIR_WINDOW);
        }
        if seen.len() >= LIMITER_MAX_TRACKED && !seen.contains_key(&addr) {
            return false;
        }
        let entry = seen.entry(addr).or_insert((0, now));
        if now.saturating_duration_since(entry.1) >= PAIR_WINDOW {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.0 <= PAIR_ATTEMPTS_PER_WINDOW
    }
}

pub struct HostSecrets {
    pub code: OneTimeCode,
    pub password: Option<(HostPassword, PasswordGuard)>,
}

pub struct HostConfig {
    pub me: DevicePublicKey,
    pub store: Arc<Mutex<PairingStore>>,
    pub secrets: HostSecrets,
    pub policy: AccessPolicy,
    pub consent: Arc<dyn Consent>,
    pub limiter: PairLimiter,
    /// Whether injected input would actually work here. If not, the input permission
    /// is withheld so the viewer is not told it has control when it does not.
    pub input_available: Arc<dyn Fn() -> bool + Send + Sync>,
    pub caps: Caps,
    pub os: Os,
    /// Shown to the viewer.
    pub host_name: String,
    /// The displays a viewer may choose from (§6.7), read again for every session
    /// because monitors come and go. `Display.id` is the index.
    pub displays: Arc<dyn Fn() -> Vec<Display> + Send + Sync>,
    pub timeouts: Timeouts,
}

pub struct ControlChannel {
    pub send: FramedSend,
    pub recv: FramedRecv,
}

impl ControlChannel {
    pub async fn send(&mut self, body: ControlBody) -> Result<(), SessionError> {
        self.send.send(&ControlMsg::from(body)).await
    }

    /// The next message with a body. `Ok(None)` is end of stream.
    pub async fn recv(&mut self) -> Result<Option<ControlBody>, SessionError> {
        loop {
            match self.recv.next::<ControlMsg>().await? {
                Some(ControlMsg { body: Some(body) }) => return Ok(Some(body)),
                Some(ControlMsg { body: None }) => continue, // unknown message from a newer peer
                None => return Ok(None),
            }
        }
    }
}

pub struct HostAccepted {
    pub control: ControlChannel,
    pub peer: DevicePublicKey,
    pub device_name: String,
    pub perms: Permissions,
    pub viewer_caps: Caps,
    /// Remembering this device failed; the session itself is fine.
    pub warning: Option<String>,
}

/// Device names come from the peer and reach a terminal or a dialog: this drops
/// control characters (terminal escapes) and bidi or zero-width format characters
/// that could make one name look like another.
pub fn sanitize_name(name: &str) -> String {
    let is_invisible = |c: char| {
        matches!(c,
            '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
    };
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && !is_invisible(*c))
        .take(MAX_DEVICE_NAME)
        .collect();
    if cleaned.trim().is_empty() {
        "(unnamed device)".to_owned()
    } else {
        cleaned
    }
}

fn perms_of(p: &Permissions) -> Perms {
    Perms {
        view: p.view,
        input: p.input,
        clipboard: p.clipboard,
        file: p.file,
        audio: p.audio,
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

type Refusal = (RejectReason, &'static str);

fn refusal_of_pair_error(e: &PairError) -> Refusal {
    match e {
        PairError::UnsupportedProtocol(_) => (
            RejectReason::VersionMismatch,
            "unsupported protocol version",
        ),
        PairError::TooManyAttempts | PairError::LockedOut => (
            RejectReason::RateLimited,
            "too many attempts, try again later",
        ),
        _ => (RejectReason::AuthFailed, "authentication failed"),
    }
}

fn refusal_of_deny(reason: DenyReason) -> Refusal {
    match reason {
        DenyReason::NoEvidence => (RejectReason::NotPaired, "this device is not paired"),
        DenyReason::IdentityMismatch => (RejectReason::AuthFailed, "identity mismatch"),
        DenyReason::PasswordDisabled => (
            RejectReason::MethodUnavailable,
            "password login is disabled",
        ),
    }
}

fn map_stage<T>(r: Result<T, SessionError>) -> Result<T, Refusal> {
    r.map_err(|_| {
        (
            RejectReason::Internal,
            "connection problem during handshake",
        )
    })
}

async fn refuse(conn: &Connection, control: ControlChannel, refusal: Refusal) -> SessionError {
    let ControlChannel { mut send, recv } = control;
    let (reason, message) = refusal;
    let body = ControlBody::Reject(SessionReject {
        reason: reason as i32,
        message: message.to_owned(),
    });
    let _ = send.send(&ControlMsg::from(body)).await;
    send.finish_and_flush(FLUSH_TIMEOUT).await;
    drop(recv);
    conn.close(close::REJECTED.into(), message.as_bytes());
    SessionError::Refused(reason)
}

struct Authenticated {
    hello: ClientHello,
    auth: AuthKind,
    proof: Option<HostVerified>,
    decision: AccessDecision,
}

/// Runs the exchange the hello asks for and returns `decide()`'s verdict.
async fn authenticate(
    conn: &Connection,
    cfg: &HostConfig,
    peer: DevicePublicKey,
    control: &mut ControlChannel,
    remote: SocketAddr,
) -> Result<Authenticated, Refusal> {
    let hello = match map_stage(control.recv().await)? {
        Some(ControlBody::Hello(h)) => h,
        _ => return Err((RejectReason::AuthFailed, "expected ClientHello")),
    };
    let method = hello_auth(&hello).map_err(|e| refusal_of_pair_error(&e))?;
    if !hello
        .caps
        .as_ref()
        .is_some_and(|v| negotiate::any_common(&cfg.caps, v))
    {
        return Err((RejectReason::NoCommonCodec, "no codec in common"));
    }

    match method {
        HelloAuth::Known => {
            // The key comes from the TLS handshake, not from the hello.
            let store = lock(&cfg.store);
            let evidence = store.pinned(&peer).map_or(Evidence::None, Evidence::Pinned);
            let decision = decide(&peer, evidence, &cfg.policy);
            drop(store);
            Ok(Authenticated {
                hello,
                auth: AuthKind::Paired,
                proof: None,
                decision,
            })
        }
        HelloAuth::Code | HelloAuth::Password => {
            if !cfg.limiter.allow(remote.ip().to_canonical()) {
                return Err((
                    RejectReason::RateLimited,
                    "too many attempts, try again later",
                ));
            }
            let exporter =
                pair_exporter(conn).map_err(|_| (RejectReason::Internal, "tls exporter"))?;
            let ctx = PairingContext::new(peer, cfg.me, exporter)
                .map_err(|e| refusal_of_pair_error(&e))?;
            let (mut machine, offer, auth) = match method {
                HelloAuth::Code => {
                    let (m, offer) = HostPairing::with_code(ctx, &cfg.secrets.code)
                        .map_err(|e| refusal_of_pair_error(&e))?;
                    (m, offer, AuthKind::Code)
                }
                HelloAuth::Password => {
                    let Some((stored, guard)) = cfg.secrets.password.as_ref() else {
                        return Err((
                            RejectReason::MethodUnavailable,
                            "password login is not set up",
                        ));
                    };
                    let (m, offer) = HostPairing::with_password(ctx, stored, guard)
                        .map_err(|e| refusal_of_pair_error(&e))?;
                    (m, offer, AuthKind::Password)
                }
                HelloAuth::Known => unreachable!("handled above"),
            };
            map_stage(control.send(ControlBody::Auth(offer)).await)?;
            let step = match map_stage(control.recv().await)? {
                Some(ControlBody::Auth(s)) => s,
                _ => return Err((RejectReason::AuthFailed, "expected AuthStep")),
            };
            let outcome = machine
                .on_step(&step)
                .map_err(|e| refusal_of_pair_error(&e))?;
            map_stage(control.send(ControlBody::Auth(outcome.reply)).await)?;
            let decision = decide(&peer, Evidence::Exchange(&outcome.verified), &cfg.policy);
            Ok(Authenticated {
                hello,
                auth,
                proof: Some(outcome.verified),
                decision,
            })
        }
    }
}

async fn ask_person(
    conn: &Connection,
    cfg: &HostConfig,
    mut request: ConsentRequest,
) -> ConsentAnswer {
    let cancelled = Arc::new(AtomicBool::new(false));
    request.cancelled = cancelled.clone();
    let consent = cfg.consent.clone();
    let wait = cfg.timeouts.consent;
    let asked = tokio::task::spawn_blocking(move || consent.ask(&request, wait));
    let limit = wait + Duration::from_secs(2);
    tokio::select! {
        r = tokio::time::timeout(limit, asked) => {
            cancelled.store(true, Relaxed);
            match r {
                Ok(Ok(answer)) => answer,
                _ => ConsentAnswer::deny(),
            }
        }
        _ = conn.closed() => {
            cancelled.store(true, Relaxed);
            ConsentAnswer::deny()
        }
    }
}

/// Runs the host side up to and including `SessionAccept`.
///
/// `session_active` reports whether accepting would take over a running session.
pub async fn host_handshake(
    conn: &Connection,
    cfg: &HostConfig,
    session_active: &(dyn Fn() -> bool + Send + Sync),
) -> Result<HostAccepted, SessionError> {
    host_handshake_from(conn, cfg, session_active, conn.remote_address()).await
}

/// [`host_handshake`] for a connection whose QUIC peer address is the virtual
/// one (§4.1): `remote` is the real path, shown to the person and used for the
/// pairing rate limit. Nothing else differs.
pub async fn host_handshake_from(
    conn: &Connection,
    cfg: &HostConfig,
    session_active: &(dyn Fn() -> bool + Send + Sync),
    remote: SocketAddr,
) -> Result<HostAccepted, SessionError> {
    let accept = async {
        let (kind, send, recv) = accept_bi(conn).await?;
        if kind != StreamType::Control {
            return Err(SessionError::Protocol("expected the Control stream first"));
        }
        Ok(ControlChannel { send, recv })
    };
    let mut control = match tokio::time::timeout(cfg.timeouts.handshake, accept).await {
        Ok(r) => r?,
        Err(_) => {
            conn.close(close::REJECTED.into(), b"handshake timed out");
            return Err(SessionError::Timeout);
        }
    };
    let peer = peer_public_key(conn).map_err(SessionError::Tls)?;

    let authenticated = match tokio::time::timeout(
        cfg.timeouts.handshake,
        authenticate(conn, cfg, peer, &mut control, remote),
    )
    .await
    {
        Ok(Ok(a)) => a,
        Ok(Err(refusal)) => return Err(refuse(conn, control, refusal).await),
        Err(_) => {
            return Err(refuse(
                conn,
                control,
                (RejectReason::Timeout, "handshake timed out"),
            )
            .await);
        }
    };
    let Authenticated {
        hello,
        auth,
        proof,
        decision,
    } = authenticated;
    let device_name = sanitize_name(&hello.device_name);

    // Nothing may run unseen: with no one to tell, even a paired device is refused.
    if !cfg.consent.can_present() {
        return Err(refuse(
            conn,
            control,
            (
                RejectReason::ConsentDenied,
                "nobody is present at the host to allow this",
            ),
        )
        .await);
    }

    let takeover = session_active();
    let prompt = match (&decision, takeover) {
        (AccessDecision::Deny(reason), _) => {
            return Err(refuse(conn, control, refusal_of_deny(*reason)).await);
        }
        (AccessDecision::NeedsConsent(prompt), _) => Some(prompt.clone()),
        (AccessDecision::Allow { perms }, true) => Some(ConsentPrompt {
            perms: perms.clone(),
            offer_save: false,
            save_default: false,
            offer_skip_next_time: false,
        }),
        (AccessDecision::Allow { .. }, false) => None,
    };

    let mut answer = None;
    if let Some(prompt) = prompt {
        let got = ask_person(
            conn,
            cfg,
            ConsentRequest {
                peer,
                device_name: device_name.clone(),
                remote,
                auth,
                prompt,
                takeover,
                cancelled: Arc::new(AtomicBool::new(false)),
            },
        )
        .await;
        if !got.allow {
            return Err(refuse(
                conn,
                control,
                (RejectReason::ConsentDenied, "the host declined"),
            )
            .await);
        }
        answer = Some(got);
    }

    let mut perms = match (&decision, &answer) {
        (_, Some(a)) => a.perms.clone(),
        (AccessDecision::Allow { perms }, None) => perms.clone(),
        _ => unreachable!("every other decision returned above or produced an answer"),
    };
    perms.input &= (cfg.input_available)();
    if !perms.view {
        return Err(refuse(
            conn,
            control,
            (RejectReason::ConsentDenied, "no permission to view"),
        )
        .await);
    }

    let warning = remember(
        cfg,
        &peer,
        &device_name,
        proof.as_ref(),
        answer.as_ref(),
        &decision,
    )
    .err();

    let accepted = SessionAccept {
        perms: Some(perms.clone()),
        displays: (cfg.displays)(),
        caps: Some(cfg.caps.clone()),
        os: cfg.os as i32,
        host_name: cfg.host_name.clone(),
    };
    control.send(ControlBody::Accept(accepted)).await?;
    Ok(HostAccepted {
        control,
        peer,
        device_name,
        perms,
        viewer_caps: hello.caps.unwrap_or(Caps {
            encode: Vec::new(),
            decode: Vec::new(),
            rfi: false,
            ltr: false,
            max_fps: 0,
        }),
        warning,
    })
}

/// Applies the person's "save" and "don't ask again" answers.
fn remember(
    cfg: &HostConfig,
    peer: &DevicePublicKey,
    name: &str,
    proof: Option<&HostVerified>,
    answer: Option<&ConsentAnswer>,
    decision: &AccessDecision,
) -> Result<(), String> {
    let mut store = lock(&cfg.store);
    let offered = match decision {
        AccessDecision::NeedsConsent(p) => Some(p),
        _ => None,
    };
    let mut result = Ok(());
    if let (Some(answer), Some(offered)) = (answer, offered) {
        if answer.save
            && offered.offer_save
            && let Some(proof) = proof
            && let Err(e) = store.add(proof, name, perms_of(&answer.perms), unix_now())
        {
            result = Err(e.to_string());
        }
        if answer.skip_next_time
            && offered.offer_skip_next_time
            && let Err(e) = store.set_unattended(peer, true)
        {
            result = Err(e.to_string());
        }
    }
    if store.pinned(peer).is_some()
        && let Err(e) = store.touch(peer, unix_now())
    {
        result = result.and(Err(e.to_string()));
    }
    result
}

// ---------------------------------------------------------------------------------
// Viewer side
// ---------------------------------------------------------------------------------

pub enum ViewerAuth {
    /// The host must be pinned by the TLS configuration this connection used.
    Known,
    Code(Zeroizing<String>),
    Password(Zeroizing<String>),
}

pub struct ViewerConfig {
    pub me: DevicePublicKey,
    pub device_name: String,
    pub os: Os,
    pub caps: Caps,
    pub auth: ViewerAuth,
    pub timeouts: Timeouts,
}

pub struct ViewerAccepted {
    pub control: ControlChannel,
    pub accept: SessionAccept,
    pub host: DevicePublicKey,
    /// Present after a code or password exchange, to pin the host with.
    pub proof: Option<ViewerVerified>,
}

/// Runs the viewer side up to and including `SessionAccept`.
pub async fn viewer_handshake(
    conn: &Connection,
    cfg: ViewerConfig,
) -> Result<ViewerAccepted, SessionError> {
    let total = cfg.timeouts.handshake + cfg.timeouts.consent + Duration::from_secs(5);
    let result = tokio::time::timeout(total, viewer_inner(conn, cfg)).await;
    match result {
        Ok(r) => r,
        Err(_) => {
            conn.close(close::VIEWER_ENDED.into(), b"handshake timed out");
            Err(SessionError::Timeout)
        }
    }
}

async fn step_pairing(
    machine: ViewerPairing,
    step: nexa_wire::control::AuthStep,
) -> Result<(ViewerPairing, ViewerEvent), SessionError> {
    // The first step of password mode runs Argon2id.
    let joined = tokio::task::spawn_blocking(move || {
        let mut machine = machine;
        let event = machine.on_step(&step);
        event.map(|e| (machine, e))
    })
    .await
    .map_err(|_| SessionError::Protocol("pairing task failed"))?;
    joined.map_err(SessionError::Auth)
}

async fn viewer_inner(
    conn: &Connection,
    cfg: ViewerConfig,
) -> Result<ViewerAccepted, SessionError> {
    let host = peer_public_key(conn).map_err(SessionError::Tls)?;
    let build_ctx = || -> Result<PairingContext, SessionError> {
        let exporter = pair_exporter(conn).map_err(SessionError::Tls)?;
        PairingContext::new(cfg.me, host, exporter).map_err(SessionError::Auth)
    };
    let (mut machine, auth) = match &cfg.auth {
        ViewerAuth::Known => (None, Auth::Known(true)),
        ViewerAuth::Code(code) => (
            Some(ViewerPairing::with_code(build_ctx()?, code).map_err(SessionError::Auth)?),
            Auth::Code(true),
        ),
        ViewerAuth::Password(password) => (
            Some(ViewerPairing::with_password(build_ctx()?, password).map_err(SessionError::Auth)?),
            Auth::Password(true),
        ),
    };
    let (send, recv) = open_bi(conn, StreamType::Control).await?;
    let mut control = ControlChannel { send, recv };
    control
        .send(ControlBody::Hello(ClientHello {
            proto: nexa_auth::PROTOCOL_VERSION,
            device_name: cfg.device_name.clone(),
            os: cfg.os as i32,
            caps: Some(cfg.caps.clone()),
            auth: Some(auth),
        }))
        .await?;

    let mut proof = None;
    loop {
        match control.recv().await? {
            Some(ControlBody::Auth(step)) => {
                let Some(m) = machine.take() else {
                    return Err(SessionError::Protocol("unexpected AuthStep"));
                };
                let (m, event) = step_pairing(m, step).await.inspect_err(|_| {
                    conn.close(close::VIEWER_ENDED.into(), b"authentication failed");
                })?;
                match event {
                    ViewerEvent::Send(reply) => {
                        control.send(ControlBody::Auth(reply)).await?;
                        machine = Some(m);
                    }
                    ViewerEvent::Complete(v) => proof = Some(v),
                }
            }
            Some(ControlBody::Accept(accept)) => {
                // A host must not be able to skip proving it knows the secret.
                if machine.is_some() || (proof.is_none() && !matches!(cfg.auth, ViewerAuth::Known))
                {
                    conn.close(close::VIEWER_ENDED.into(), b"protocol error");
                    return Err(SessionError::Protocol(
                        "SessionAccept before pairing finished",
                    ));
                }
                return Ok(ViewerAccepted {
                    control,
                    accept,
                    host,
                    proof,
                });
            }
            Some(ControlBody::Reject(r)) => {
                conn.close(close::VIEWER_ENDED.into(), b"rejected");
                return Err(SessionError::Rejected {
                    reason: RejectReason::try_from(r.reason).unwrap_or(RejectReason::Unspecified),
                    message: r.message,
                });
            }
            Some(_) => return Err(SessionError::Protocol("unexpected message in handshake")),
            None => return Err(SessionError::Protocol("control stream ended")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_lose_escapes_and_invisibles() {
        assert_eq!(sanitize_name("mac\x1b[2Jbook"), "mac[2Jbook");
        assert_eq!(sanitize_name("evil\u{202E}txt"), "eviltxt");
        assert_eq!(sanitize_name("\n\t"), "(unnamed device)");
        assert_eq!(
            sanitize_name(&"a".repeat(500)).chars().count(),
            MAX_DEVICE_NAME
        );
    }

    #[test]
    fn limiter_blocks_then_recovers_per_address() {
        let limiter = PairLimiter::default();
        let (a, b): (IpAddr, IpAddr) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        let t = Instant::now();
        for _ in 0..PAIR_ATTEMPTS_PER_WINDOW {
            assert!(limiter.allow_at(a, t));
        }
        assert!(!limiter.allow_at(a, t));
        assert!(limiter.allow_at(b, t));
        assert!(limiter.allow_at(a, t + PAIR_WINDOW));
    }
}
