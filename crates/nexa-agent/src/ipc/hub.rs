//! The attached UI as a consent surface (§9.3).
//!
//! At most one UI is attached. A question goes to it as `IncomingRequest` and only a
//! `Decide` from that same connection answers it. Everything else denies: no UI, a
//! UI that goes away while the question is open, a full outbound queue, a timeout,
//! the requester leaving, and a `Decide` that does not say `allow` with `view`.

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering::Relaxed},
        mpsc::{self as std_mpsc, RecvTimeoutError, SyncSender},
    },
    time::{Duration, Instant},
};

use nexa_session::handshake::{AuthKind, Consent, ConsentAnswer, ConsentRequest};
use nexa_wire::control::Permissions;
use tokio::sync::mpsc;

use super::proto::{
    self as pb, AgentBody, AgentToUi, IncomingRequest, Notice, NoticeKind, RequestClosed,
};
use crate::{AgentEvent, AgentUi};

/// Messages queued for the attached UI. Beyond this the UI is not keeping up and is
/// dropped, which denies whatever it was being asked.
pub(crate) const OUTBOUND_QUEUE: usize = 256;
const POLL: Duration = Duration::from_millis(50);

/// What the connection task sends to its UI.
pub(crate) enum Outgoing {
    Message(Box<AgentToUi>),
    /// Something in `Status` changed; the connection rebuilds and sends it.
    StatusChanged,
}

struct Attached {
    id: u64,
    tx: mpsc::Sender<Outgoing>,
}

struct Pending {
    req_id: u64,
    conn: u64,
    reply: SyncSender<ConsentAnswer>,
}

#[derive(Default)]
struct State {
    ui: Option<Attached>,
    pending: Option<Pending>,
}

type Hook = Arc<dyn Fn() + Send + Sync>;

pub struct IpcHub {
    state: Mutex<State>,
    /// One question at a time, as on the terminal.
    prompt_lock: Mutex<()>,
    next_conn: AtomicU64,
    next_req: AtomicU64,
    input_available: Mutex<Option<Arc<dyn Fn() -> bool + Send + Sync>>>,
    on_detached: Mutex<Option<Hook>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Request ids start at a random point so one from an earlier run never matches.
fn first_req_id() -> u64 {
    let mut b = [0u8; 8];
    if getrandom(&mut b) {
        u64::from_le_bytes(b) >> 16 | 1
    } else {
        1
    }
}

fn getrandom(buf: &mut [u8]) -> bool {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .is_ok()
}

fn auth_kind(a: AuthKind) -> pb::AuthKind {
    match a {
        AuthKind::Paired => pb::AuthKind::Paired,
        AuthKind::Code => pb::AuthKind::Code,
        AuthKind::Password => pb::AuthKind::Password,
    }
}

/// Turns a `Decide` into what the handshake carries out. Only an explicit allow with
/// view is an allow. Files and audio are granted only when the dialog says so (both
/// boxes start unticked).
pub(crate) fn answer_of(d: &pb::Decide) -> ConsentAnswer {
    let p = d.perms.clone().unwrap_or_default();
    if !d.allow || !p.view {
        return ConsentAnswer::deny();
    }
    ConsentAnswer {
        allow: true,
        perms: Permissions {
            view: true,
            input: p.input,
            clipboard: p.clipboard,
            file: p.file,
            audio: p.audio,
        },
        save: d.save,
        skip_next_time: d.skip_next_time,
    }
}

impl IpcHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            prompt_lock: Mutex::new(()),
            next_conn: AtomicU64::new(1),
            next_req: AtomicU64::new(first_req_id()),
            input_available: Mutex::new(None),
            on_detached: Mutex::new(None),
        })
    }

    /// Whether a UI is attached.
    pub fn ui_attached(&self) -> bool {
        lock(&self.state).ui.is_some()
    }

    /// Whether a question is waiting for the UI's answer.
    pub fn question_open(&self) -> bool {
        lock(&self.state).pending.is_some()
    }

    /// Called, outside any lock, after a UI detaches for whatever reason.
    pub fn set_on_detached(&self, hook: impl Fn() + Send + Sync + 'static) {
        *lock(&self.on_detached) = Some(Arc::new(hook));
    }

    pub(crate) fn set_input_available(&self, f: Arc<dyn Fn() -> bool + Send + Sync>) {
        *lock(&self.input_available) = Some(f);
    }

    fn input_available(&self) -> bool {
        lock(&self.input_available).as_ref().is_some_and(|f| f())
    }

    /// Attaches a UI connection. `None` if one is already attached.
    pub(crate) fn attach(&self, tx: mpsc::Sender<Outgoing>) -> Option<u64> {
        let mut state = lock(&self.state);
        if state.ui.is_some() {
            return None;
        }
        let id = self.next_conn.fetch_add(1, Relaxed);
        state.ui = Some(Attached { id, tx });
        Some(id)
    }

    /// Detaches `conn` if it is the attached UI. Its open question, if any, is denied
    /// at once: dropping the reply channel wakes the asking thread.
    pub(crate) fn detach(&self, conn: u64) {
        let detached = {
            let mut state = lock(&self.state);
            Self::drop_ui_locked(&mut state, conn)
        };
        if detached {
            self.run_detached_hook();
        }
    }

    fn drop_ui_locked(state: &mut State, conn: u64) -> bool {
        if !state.ui.as_ref().is_some_and(|u| u.id == conn) {
            return false;
        }
        state.ui = None;
        if state.pending.as_ref().is_some_and(|p| p.conn == conn) {
            state.pending = None;
        }
        true
    }

    fn run_detached_hook(&self) {
        let hook = lock(&self.on_detached).clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Queues `out` for the UI. A UI whose queue is full is dropped.
    fn send(&self, out: Outgoing) {
        let dropped = {
            let mut state = lock(&self.state);
            let Some(ui) = state.ui.as_ref() else {
                return;
            };
            match ui.tx.try_send(out) {
                Ok(()) => false,
                Err(_) => {
                    let id = ui.id;
                    Self::drop_ui_locked(&mut state, id)
                }
            }
        };
        if dropped {
            self.run_detached_hook();
        }
    }

    /// Delivers the attached UI's answer. Returns false if `req_id` is not the open
    /// question of this connection, in which case nothing happens.
    pub(crate) fn decide(&self, conn: u64, decision: &pb::Decide) -> bool {
        let mut state = lock(&self.state);
        let matches = state
            .pending
            .as_ref()
            .is_some_and(|p| p.conn == conn && p.req_id == decision.req_id);
        if !matches {
            return false;
        }
        if let Some(p) = state.pending.take() {
            let _ = p.reply.try_send(answer_of(decision));
        }
        true
    }

    fn close_request(&self, req_id: u64, reason: &str) {
        {
            let mut state = lock(&self.state);
            if state.pending.as_ref().is_some_and(|p| p.req_id == req_id) {
                state.pending = None;
            }
        }
        self.send(message(AgentBody::RequestClosed(RequestClosed {
            req_id,
            reason: reason.to_owned(),
        })));
    }
}

impl Consent for IpcHub {
    fn can_present(&self) -> bool {
        self.ui_attached()
    }

    fn ask(&self, request: &ConsentRequest, timeout: Duration) -> ConsentAnswer {
        let _one_at_a_time = lock(&self.prompt_lock);
        if request.cancelled.load(Relaxed) {
            return ConsentAnswer::deny();
        }
        let deadline = Instant::now() + timeout;
        let req_id = self.next_req.fetch_add(1, Relaxed);
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        let question = IncomingRequest {
            req_id,
            device_name: request.device_name.clone(),
            device_id: request.peer.id_text(),
            device_key: request.peer.key_hex(),
            remote: request.remote.to_string(),
            auth: auth_kind(request.auth) as i32,
            perms: Some((&request.prompt.perms).into()),
            offer_save: request.prompt.offer_save,
            save_default: request.prompt.save_default,
            offer_skip_next_time: request.prompt.offer_skip_next_time,
            takeover: request.takeover,
            timeout_ms: timeout.as_millis().min(u128::from(u32::MAX)) as u32,
            input_available: self.input_available(),
            browser: request.peer.is_browser(),
        };
        {
            let mut state = lock(&self.state);
            let Some(ui) = state.ui.as_ref() else {
                return ConsentAnswer::deny();
            };
            let conn = ui.id;
            let sent = ui.tx.try_send(message(AgentBody::Incoming(question)));
            if sent.is_err() {
                let dropped = Self::drop_ui_locked(&mut state, conn);
                drop(state);
                if dropped {
                    self.run_detached_hook();
                }
                return ConsentAnswer::deny();
            }
            state.pending = Some(Pending {
                req_id,
                conn,
                reply: reply_tx,
            });
        }

        let (answer, why) = loop {
            if request.cancelled.load(Relaxed) {
                break (ConsentAnswer::deny(), "cancelled");
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break (ConsentAnswer::deny(), "timed out");
            }
            match reply_rx.recv_timeout(left.min(POLL)) {
                Ok(answer) => break (answer, "answered"),
                Err(RecvTimeoutError::Timeout) => {}
                // The UI went away and took the question with it.
                Err(RecvTimeoutError::Disconnected) => break (ConsentAnswer::deny(), "ui gone"),
            }
        };
        self.close_request(req_id, why);
        answer
    }
}

impl AgentUi for IpcHub {
    fn event(&self, event: AgentEvent) {
        let out = match event {
            AgentEvent::Listening(_) | AgentEvent::CodeChanged(_) | AgentEvent::Signal(_) => {
                Outgoing::StatusChanged
            }
            AgentEvent::SessionStarted(info) => {
                self.send(message(AgentBody::SessionStarted(pb::SessionStarted {
                    session: Some((&info).into()),
                })));
                Outgoing::StatusChanged
            }
            AgentEvent::SessionEnded {
                id,
                device_name,
                reason,
            } => {
                self.send(message(AgentBody::SessionEnded(pb::SessionEnded {
                    id,
                    device_name,
                    reason,
                })));
                Outgoing::StatusChanged
            }
            AgentEvent::ConnectionFailed { remote, reason } => message(AgentBody::Notice(Notice {
                kind: NoticeKind::ConnectionFailed as i32,
                text: format!("a connection from {remote} failed: {reason}"),
                remote: remote.to_string(),
                reason,
            })),
            AgentEvent::ClipboardNotice(n) => notice(NoticeKind::Clipboard, format!("{n:?}")),
            AgentEvent::Warning(w) => notice(NoticeKind::Warning, w),
            AgentEvent::Transfer { session, event } => {
                message(AgentBody::Transfer(pb::Transfer::of(session, &event)))
            }
        };
        self.send(out);
    }
}

fn notice(kind: NoticeKind, text: String) -> Outgoing {
    message(AgentBody::Notice(Notice {
        kind: kind as i32,
        text,
        ..Default::default()
    }))
}

fn message(body: AgentBody) -> Outgoing {
    Outgoing::Message(Box::new(body.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(allow: bool, view: bool) -> pb::Decide {
        pb::Decide {
            req_id: 1,
            allow,
            perms: Some(pb::Permissions {
                view,
                input: true,
                clipboard: true,
                file: true,
                audio: true,
            }),
            save: true,
            skip_next_time: true,
        }
    }

    #[test]
    fn every_default_denies() {
        assert!(!answer_of(&pb::Decide::default()).allow);
        assert!(!answer_of(&decide(false, true)).allow);
        assert!(
            !answer_of(&decide(true, false)).allow,
            "no view, no session"
        );
        let mut no_perms = decide(true, true);
        no_perms.perms = None;
        assert!(!answer_of(&no_perms).allow);
    }

    #[test]
    fn files_follow_the_dialog_and_default_to_off() {
        let a = answer_of(&decide(true, true));
        assert!(a.allow && a.perms.view && a.perms.input && a.perms.clipboard);
        assert!(a.perms.file, "ticked");
        let mut unticked = decide(true, true);
        unticked.perms.as_mut().unwrap().file = false;
        assert!(!answer_of(&unticked).perms.file);
        assert!(!answer_of(&pb::Decide::default()).perms.file);
    }

    #[test]
    fn audio_follows_the_dialog_and_defaults_to_off() {
        assert!(answer_of(&decide(true, true)).perms.audio, "ticked");
        let mut unticked = decide(true, true);
        unticked.perms.as_mut().unwrap().audio = false;
        assert!(!answer_of(&unticked).perms.audio);
        assert!(!answer_of(&pb::Decide::default()).perms.audio);
    }

    #[test]
    fn a_second_ui_cannot_attach() {
        let hub = IpcHub::new();
        let (tx, _rx) = mpsc::channel(4);
        let (tx2, _rx2) = mpsc::channel(4);
        let first = hub.attach(tx).unwrap();
        assert!(hub.attach(tx2).is_none());
        hub.detach(first + 100);
        assert!(hub.ui_attached(), "only the attached connection can detach");
        hub.detach(first);
        assert!(!hub.ui_attached());
    }
}
