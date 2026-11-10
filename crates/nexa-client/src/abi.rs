//! The C ABI (§10.1).
//!
//! Handles are opaque ids looked up in a registry rather than raw pointers to Rust
//! objects, so a stale or double-closed handle returns an error instead of being
//! undefined behaviour. Every entry point catches panics; none can cross this boundary.
//!
//! Status codes: 0 is success, negative is an error, see the `NEXA_E_*` constants.
//! The most recent error message on the calling thread is in [`nexa_last_error`].

#![allow(non_camel_case_types, non_snake_case)]

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        mpsc,
    },
    thread,
};

use prost::Message;

use crate::{
    client::{Core, EventTx, Session},
    deps::Deps,
    proto::{ClientConfig, ConnectRequest, SessionCommand},
};

pub const NEXA_OK: i32 = 0;
pub const NEXA_E_INVALID_ARG: i32 = -1;
pub const NEXA_E_BAD_HANDLE: i32 = -2;
pub const NEXA_E_PARSE: i32 = -3;
pub const NEXA_E_INTERNAL: i32 = -4;
pub const NEXA_E_PANIC: i32 = -5;
pub const NEXA_E_NOT_PERMITTED: i32 = -6;
pub const NEXA_E_BUFFER_TOO_SMALL: i32 = -7;
pub const NEXA_E_VIEW: i32 = -8;

/// `kind` for `nexa_session_attach_view`.
pub const NEXA_VIEW_NSVIEW: u32 = 1;

/// Opaque client handle.
#[repr(C)]
pub struct NexaClient {
    _private: [u8; 0],
}

/// Opaque session handle.
#[repr(C)]
pub struct NexaSession {
    _private: [u8; 0],
}

/// Receives one protobuf `nexa.client.Event`. Called on a Rust thread: copy the bytes,
/// then post to the UI thread. Never call back into `nexa_shutdown` from here on the
/// assumption that it will wait; it will not.
pub type nexa_event_cb = Option<unsafe extern "C" fn(user: *mut c_void, pb: *const u8, len: usize)>;

struct Abi(i32, String);

fn invalid(what: &str) -> Abi {
    Abi(NEXA_E_INVALID_ARG, what.to_owned())
}

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

fn remember(message: String) {
    LAST_ERROR.with(|e| *e.borrow_mut() = message);
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    let text = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned());
    format!("internal error: {text}")
}

/// Runs `f`, turning errors and panics into status codes.
fn guarded(f: impl FnOnce() -> Result<(), Abi>) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => NEXA_OK,
        Ok(Err(Abi(code, message))) => {
            remember(message);
            code
        }
        Err(payload) => {
            remember(panic_text(payload));
            NEXA_E_PANIC
        }
    }
}

fn guarded_void(f: impl FnOnce() -> Result<(), Abi>) {
    let _ = guarded(f);
}

struct Callback {
    f: unsafe extern "C" fn(*mut c_void, *const u8, usize),
    user: *mut c_void,
}

// SAFETY: `user` is an opaque token that this code only hands back to the callback,
// and the C# side promises it may be used from any thread (§10.1).
unsafe impl Send for Callback {}
unsafe impl Sync for Callback {}

struct ClientEntry {
    core: Arc<Core>,
    callback: Arc<RwLock<Option<Callback>>>,
    closing: Arc<AtomicBool>,
    sessions: Mutex<Vec<usize>>,
}

#[derive(Default)]
struct Registry {
    clients: HashMap<usize, Arc<ClientEntry>>,
    sessions: HashMap<usize, Arc<Session>>,
}

fn registry() -> MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Starts at 1, so a handle is never null.
fn next_handle() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    NEXT.fetch_add(1, SeqCst)
}

fn client_of(c: *mut NexaClient) -> Result<Arc<ClientEntry>, Abi> {
    registry()
        .clients
        .get(&(c as usize))
        .cloned()
        .ok_or(Abi(NEXA_E_BAD_HANDLE, "unknown client handle".to_owned()))
}

fn session_of(s: *mut NexaSession) -> Result<Arc<Session>, Abi> {
    registry()
        .sessions
        .get(&(s as usize))
        .cloned()
        .ok_or(Abi(NEXA_E_BAD_HANDLE, "unknown session handle".to_owned()))
}

/// # Safety
/// `p` must be null or valid for reads of `len` bytes.
unsafe fn bytes_of<'a>(p: *const u8, len: usize) -> Result<&'a [u8], Abi> {
    if len == 0 {
        return Ok(&[]);
    }
    if p.is_null() {
        return Err(invalid("null buffer"));
    }
    // SAFETY: non-null and valid for `len` bytes per the caller's contract.
    Ok(unsafe { std::slice::from_raw_parts(p, len) })
}

fn run_events(
    rx: mpsc::Receiver<Vec<u8>>,
    callback: Arc<RwLock<Option<Callback>>>,
    closing: Arc<AtomicBool>,
) {
    while let Ok(event) = rx.recv() {
        if closing.load(SeqCst) {
            return;
        }
        let guard = callback.read().unwrap_or_else(|e| e.into_inner());
        if let Some(cb) = guard.as_ref() {
            IN_CALLBACK.with(|f| f.set(true));
            // A panic or foreign unwind from the callback stops here, not in Rust's
            // own frames further up.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: `event` lives for the call, per the callback's contract.
                unsafe { (cb.f)(cb.user, event.as_ptr(), event.len()) };
            }));
            IN_CALLBACK.with(|f| f.set(false));
        }
    }
}

fn parse_config(cfg: &[u8]) -> Result<ClientConfig, Abi> {
    if cfg.is_empty() {
        return Ok(ClientConfig::default());
    }
    ClientConfig::decode(cfg).map_err(|e| Abi(NEXA_E_PARSE, format!("bad config: {e}")))
}

fn default_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/NexaDesk")
}

/// `nexa_init` with the devices supplied by the caller. Not exported from the library:
/// nothing outside Rust can reach this, so no client can opt out of the real devices.
///
/// # Safety
/// As for [`nexa_init`].
pub unsafe fn init_with_deps(
    deps: Arc<dyn Deps>,
    cfg: *const u8,
    len: usize,
    cb: nexa_event_cb,
    user: *mut c_void,
    out: *mut *mut NexaClient,
) -> i32 {
    guarded(|| {
        if out.is_null() {
            return Err(invalid("null out pointer"));
        }
        // SAFETY: `out` is non-null and writable per the contract.
        unsafe { *out = std::ptr::null_mut() };
        let Some(f) = cb else {
            return Err(invalid("null callback"));
        };
        // SAFETY: per the contract.
        let config = parse_config(unsafe { bytes_of(cfg, len)? })?;
        let data_dir = if config.data_dir.is_empty() {
            default_data_dir()
        } else {
            PathBuf::from(config.data_dir)
        };
        let device_name = if config.device_name.is_empty() {
            "Viewer".to_owned()
        } else {
            config.device_name
        };

        let (tx, rx) = mpsc::channel();
        let callback = Arc::new(RwLock::new(Some(Callback { f, user })));
        let closing = Arc::new(AtomicBool::new(false));
        {
            let (callback, closing) = (callback.clone(), closing.clone());
            thread::Builder::new()
                .name("nexa-events".into())
                .spawn(move || run_events(rx, callback, closing))
                .map_err(|e| Abi(NEXA_E_INTERNAL, e.to_string()))?;
        }
        let core = Core::new(deps, device_name, data_dir, EventTx::new(tx))
            .map_err(|e| Abi(NEXA_E_INTERNAL, e.to_string()))?;
        let id = next_handle();
        registry().clients.insert(
            id,
            Arc::new(ClientEntry {
                core,
                callback,
                closing,
                sessions: Mutex::new(Vec::new()),
            }),
        );
        // SAFETY: `out` was checked above.
        unsafe { *out = id as *mut NexaClient };
        Ok(())
    })
}

/// Creates a client. `cfg` is a protobuf `ClientConfig` and may be empty.
///
/// # Safety
/// `cfg` must be valid for `len` bytes, `out` must be writable, and `cb`/`user` must
/// stay valid until `nexa_shutdown` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_init(
    cfg: *const u8,
    len: usize,
    cb: nexa_event_cb,
    user: *mut c_void,
    out: *mut *mut NexaClient,
) -> i32 {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: forwarded from the caller.
        unsafe { init_with_deps(Arc::new(crate::real::RealDeps), cfg, len, cb, user, out) }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cfg, len, cb, user, out);
        remember("this platform is not supported yet".to_owned());
        NEXA_E_INTERNAL
    }
}

fn close_session(id: usize) {
    if let Some(session) = registry().sessions.remove(&id) {
        session.close();
    }
}

/// Ends all sessions and frees the client. No callback runs after this returns, unless
/// it is called from inside the callback itself, where it cannot wait.
///
/// # Safety
/// `c` must be a handle from `nexa_init`, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_shutdown(c: *mut NexaClient) {
    guarded_void(|| {
        let entry = registry().clients.remove(&(c as usize));
        let Some(entry) = entry else {
            return Err(Abi(NEXA_E_BAD_HANDLE, "unknown client handle".to_owned()));
        };
        let sessions =
            std::mem::take(&mut *entry.sessions.lock().unwrap_or_else(|e| e.into_inner()));
        for id in sessions {
            close_session(id);
        }
        entry.closing.store(true, SeqCst);
        if !IN_CALLBACK.with(|f| f.get()) {
            // Waits for an in-flight callback, so `user` can be freed once we return.
            *entry.callback.write().unwrap_or_else(|e| e.into_inner()) = None;
        }
        entry.core.shut_down();
        Ok(())
    });
}

/// Starts connecting. `req` is a protobuf `ConnectRequest`. Returns immediately; the
/// outcome arrives as `Connected` or `Disconnected` events carrying `req.tag`.
///
/// # Safety
/// `c` must be a handle from `nexa_init`, `req` valid for `len` bytes, `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_connect(
    c: *mut NexaClient,
    req: *const u8,
    len: usize,
    out: *mut *mut NexaSession,
) -> i32 {
    guarded(|| {
        if out.is_null() {
            return Err(invalid("null out pointer"));
        }
        // SAFETY: `out` is non-null and writable per the contract.
        unsafe { *out = std::ptr::null_mut() };
        let entry = client_of(c)?;
        // SAFETY: per the contract.
        let body = unsafe { bytes_of(req, len)? };
        let request = ConnectRequest::decode(body)
            .map_err(|e| Abi(NEXA_E_PARSE, format!("bad request: {e}")))?;
        let session = entry.core.connect(request);
        let id = next_handle();
        registry().sessions.insert(id, session);
        entry
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id);
        // SAFETY: `out` was checked above.
        unsafe { *out = id as *mut NexaSession };
        Ok(())
    })
}

/// Ends the session and invalidates the handle. Local input capture stops first.
///
/// # Safety
/// `s` must be a handle from `nexa_connect`, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_session_close(s: *mut NexaSession) {
    guarded_void(|| {
        close_session(s as usize);
        Ok(())
    });
}

/// Attaches the video to a native view. **macOS: call on the main thread.**
/// `kind` is `NEXA_VIEW_NSVIEW` and `handle` an `NSView*`, which must outlive the attachment.
///
/// # Safety
/// `s` must be a session handle and `handle` a live view of the given kind.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_session_attach_view(
    s: *mut NexaSession,
    handle: *mut c_void,
    kind: u32,
    w_px: u32,
    h_px: u32,
    scale: f32,
) -> i32 {
    guarded(|| {
        let session = session_of(s)?;
        if handle.is_null() {
            return Err(invalid("null view"));
        }
        // SAFETY: per the contract.
        unsafe { session.attach_view(handle, kind, w_px, h_px, scale) }
            .map_err(|e| Abi(NEXA_E_VIEW, e))
    })
}

/// Tells the presenter the view's new size in pixels. Thread-safe and non-blocking.
///
/// # Safety
/// `s` must be a session handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_session_resize_view(
    s: *mut NexaSession,
    w_px: u32,
    h_px: u32,
    scale: f32,
) -> i32 {
    guarded(|| {
        session_of(s)?.resize_view(w_px, h_px, scale);
        Ok(())
    })
}

/// Detaches the video from the view. Thread-safe and non-blocking.
///
/// # Safety
/// `s` must be a session handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_session_detach_view(s: *mut NexaSession) -> i32 {
    guarded(|| {
        session_of(s)?.detach_view();
        Ok(())
    })
}

/// Starts or stops capturing local input for the session (§7.4). While active, every
/// key goes to the remote side. Returns `NEXA_E_NOT_PERMITTED` if the host did not
/// grant input.
///
/// # Safety
/// `s` must be a session handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_session_set_input_active(s: *mut NexaSession, active: bool) -> i32 {
    guarded(|| {
        session_of(s)?
            .set_input_active(active)
            .map_err(|m| Abi(NEXA_E_NOT_PERMITTED, m.to_owned()))
    })
}

/// Sends a protobuf `SessionCommand`: pointer events, typed text, mode changes.
///
/// # Safety
/// `s` must be a session handle and `cmd` valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_session_command(
    s: *mut NexaSession,
    cmd: *const u8,
    len: usize,
) -> i32 {
    guarded(|| {
        let session = session_of(s)?;
        // SAFETY: per the contract.
        let body = unsafe { bytes_of(cmd, len)? };
        let command = SessionCommand::decode(body)
            .map_err(|e| Abi(NEXA_E_PARSE, format!("bad command: {e}")))?;
        session
            .command(command)
            .map_err(|m| Abi(NEXA_E_INVALID_ARG, m.to_owned()))
    })
}

/// Copies this thread's last error message as UTF-8 and reports its full length in
/// `written`. Returns `NEXA_E_BUFFER_TOO_SMALL` if it had to truncate.
///
/// # Safety
/// `buf` must be writable for `cap` bytes (or null with `cap` 0), `written` writable or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_last_error(buf: *mut u8, cap: usize, written: *mut usize) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        LAST_ERROR.with(|e| {
            let e = e.borrow();
            if !written.is_null() {
                // SAFETY: writable per the contract.
                unsafe { *written = e.len() };
            }
            let n = e.len().min(cap);
            if n > 0 && !buf.is_null() {
                // SAFETY: `buf` is writable for `cap` >= `n` bytes.
                unsafe { std::ptr::copy_nonoverlapping(e.as_ptr(), buf, n) };
            }
            if e.len() > cap {
                NEXA_E_BUFFER_TOO_SMALL
            } else {
                NEXA_OK
            }
        })
    }))
    .unwrap_or(NEXA_E_PANIC)
}

#[cfg(test)]
pub(crate) fn test_panic() -> i32 {
    guarded(|| panic!("boom"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_becomes_a_status_code_and_a_message() {
        assert_eq!(test_panic(), NEXA_E_PANIC);
        let mut buf = [0u8; 64];
        let mut written = 0usize;
        // SAFETY: valid buffers.
        let status = unsafe { nexa_last_error(buf.as_mut_ptr(), buf.len(), &mut written) };
        assert_eq!(status, NEXA_OK);
        assert!(
            std::str::from_utf8(&buf[..written])
                .unwrap()
                .contains("boom")
        );
    }

    #[test]
    fn bad_handles_are_errors_not_crashes() {
        let bogus = 123456usize as *mut NexaSession;
        // SAFETY: handles are never dereferenced; the registry refuses this one.
        unsafe {
            assert_eq!(nexa_session_detach_view(bogus), NEXA_E_BAD_HANDLE);
            assert_eq!(
                nexa_session_set_input_active(bogus, true),
                NEXA_E_BAD_HANDLE
            );
            nexa_session_close(bogus);
            nexa_shutdown(987654usize as *mut NexaClient);
        }
    }

    #[test]
    fn short_error_buffer_reports_truncation() {
        remember("a fairly long error message".to_owned());
        let mut buf = [0u8; 4];
        let mut written = 0usize;
        // SAFETY: valid buffers.
        let status = unsafe { nexa_last_error(buf.as_mut_ptr(), buf.len(), &mut written) };
        assert_eq!(status, NEXA_E_BUFFER_TOO_SMALL);
        assert_eq!(written, "a fairly long error message".len());
        assert_eq!(&buf, b"a fa");
    }
}
