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
    proto::{ClientConfig, ConnectRequest, KeymapRequest, SessionCommand},
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
/// A Windows `HWND` (a child window the picture fills).
pub const NEXA_VIEW_HWND: u32 = 2;
/// An iOS `CAMetalLayer*`: the layer of the app's video view (`layerClass`), which
/// the picture is drawn into.
pub const NEXA_VIEW_CAMETALLAYER: u32 = 3;

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

/// The app's data directory: the one the agent uses too (crates/nexa-agent/src/os_*.rs).
pub(crate) fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join("NexaDesk")
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home).join("Library/Application Support/NexaDesk")
    }
}

/// Where the viewer log goes: next to the host's agent.log (`%LOCALAPPDATA%\NexaDesk\Logs`
/// on Windows, `~/Library/Logs/NexaDesk` on macOS).
fn viewer_log() -> PathBuf {
    #[cfg(windows)]
    {
        let local = std::env::var_os("LOCALAPPDATA").unwrap_or_default();
        PathBuf::from(local)
            .join("NexaDesk")
            .join("Logs")
            .join("viewer.log")
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home).join("Library/Logs/NexaDesk/viewer.log")
    }
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
        // Only `nexa_init` defaults to the real Downloads folder; anything else
        // (tests) keeps received files inside its own data directory.
        let downloads = if config.downloads_dir.is_empty() {
            data_dir.join("Downloads")
        } else {
            PathBuf::from(&config.downloads_dir)
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
        let signal_ca = data_dir.join("signal-ca.pem");
        let core = Core::new(
            deps,
            device_name,
            data_dir,
            downloads,
            signal_ca,
            EventTx::new(tx),
        )
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
    #[cfg(any(target_os = "macos", target_os = "ios", windows))]
    {
        nexa_media::diag::set_file(&viewer_log());
        nexa_media::diag!("viewer started");
        let mut defaulted_cfg = Vec::new();
        let defaulted = guarded(|| {
            // SAFETY: per the contract.
            let mut config = parse_config(unsafe { bytes_of(cfg, len)? })?;
            if config.downloads_dir.is_empty() {
                #[cfg(windows)]
                let home = std::env::var_os("USERPROFILE").unwrap_or_default();
                #[cfg(not(windows))]
                let home = std::env::var_os("HOME").unwrap_or_default();
                config.downloads_dir = PathBuf::from(home)
                    .join("Downloads")
                    .join("NexaDesk")
                    .to_string_lossy()
                    .into_owned();
            }
            defaulted_cfg = config.encode_to_vec();
            Ok(())
        });
        if defaulted != NEXA_OK {
            return defaulted;
        }
        let cfg = defaulted_cfg;
        // SAFETY: `cfg` is a live local buffer; the rest is forwarded from the caller.
        unsafe {
            init_with_deps(
                Arc::new(crate::real::RealDeps),
                cfg.as_ptr(),
                cfg.len(),
                cb,
                user,
                out,
            )
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", windows)))]
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

/// Copies `bytes` to `out` and reports their length in `written`;
/// `NEXA_E_BUFFER_TOO_SMALL` (nothing copied) if `cap` is short, so the caller can
/// retry with `written` bytes.
///
/// # Safety
/// `out` must be writable for `cap` bytes (or null with `cap` 0), `written` writable.
unsafe fn give(bytes: &[u8], out: *mut u8, cap: usize, written: *mut usize) -> Result<(), Abi> {
    if written.is_null() {
        return Err(invalid("null written pointer"));
    }
    // SAFETY: checked non-null; writable per the contract.
    unsafe { *written = bytes.len() };
    if bytes.len() > cap {
        return Err(Abi(NEXA_E_BUFFER_TOO_SMALL, "buffer too small".to_owned()));
    }
    if !bytes.is_empty() {
        if out.is_null() {
            return Err(invalid("null buffer"));
        }
        // SAFETY: `out` is writable for `cap` >= `bytes.len()` bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len()) };
    }
    Ok(())
}

/// # Safety
/// `req` must be valid for `len` bytes.
unsafe fn keymap_request(req: *const u8, len: usize) -> Result<KeymapRequest, Abi> {
    // SAFETY: per the contract.
    let body = unsafe { bytes_of(req, len)? };
    KeymapRequest::decode(body).map_err(|e| Abi(NEXA_E_PARSE, format!("bad keymap request: {e}")))
}

/// Lists the rules of one profile for the keyboard settings (§11.2): the built-in
/// ones, switched on or off by keymap.toml, then the user's. `req` is a protobuf
/// `KeymapRequest`; `out` receives a `KeymapView`. Needs no client handle.
///
/// # Safety
/// `req` valid for `len` bytes, `out` writable for `cap` bytes, `written` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_keymap_rules(
    req: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
    written: *mut usize,
) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let req = unsafe { keymap_request(req, len)? };
        let view = crate::keymap::rules(&req, &default_data_dir()).map_err(|e| invalid(&e))?;
        // SAFETY: per the contract.
        unsafe { give(&view.encode_to_vec(), out, cap, written) }
    })
}

/// Saves `req.rules` (the whole list as shown) for one profile to keymap.toml,
/// atomically. Refused with `NEXA_E_INVALID_ARG` and nothing written if a rule is
/// invalid. Sessions started afterwards use it.
///
/// # Safety
/// `req` must be valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_keymap_save(req: *const u8, len: usize) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let req = unsafe { keymap_request(req, len)? };
        crate::keymap::save(&req, &default_data_dir()).map_err(|e| invalid(&e))
    })
}

/// Output of a terminal: `kind` 0 is bytes from the host's shell, 1 its end (a UTF-8
/// reason; nothing follows), 2 only a question (no data). Every call returns the bytes
/// the terminal has received but not shown yet: past a megabyte the host's output is
/// held back until it catches up. Called on the session's threads; `data` is valid
/// only for the call.
#[allow(non_camel_case_types)]
pub type nexa_terminal_cb = Option<
    unsafe extern "C" fn(user: *mut c_void, kind: u32, data: *const u8, len: usize) -> usize,
>;

pub const NEXA_TERMINAL_DATA: u32 = 0;
pub const NEXA_TERMINAL_EXIT: u32 = 1;
pub const NEXA_TERMINAL_BACKLOG: u32 = 2;

/// `user` crosses to the session's threads with the callback, as the caller promises.
struct UserPtr(*mut c_void);
// SAFETY: the caller keeps `user` valid and usable from any thread until
// `nexa_terminal_close` returns (see `nexa_terminal_start`).
unsafe impl Send for UserPtr {}

/// Opens the host's command line in a terminal `cols` × `rows` cells: its output goes to
/// `cb` until [`nexa_terminal_close`]; `id` names it for the other `nexa_terminal_*`
/// calls. Needs `Permissions.terminal`, or the shell ends at once with "denied".
///
/// # Safety
/// `s` a session handle; `id` writable; `cb` and `user` valid, and callable from any
/// thread, until `nexa_terminal_close(id)` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_terminal_start(
    s: *mut NexaSession,
    cols: u32,
    rows: u32,
    cb: nexa_terminal_cb,
    user: *mut c_void,
    id: *mut u64,
) -> i32 {
    guarded(|| {
        let session = session_of(s)?;
        if id.is_null() {
            return Err(invalid("null id"));
        }
        let Some(f) = cb else {
            return Err(invalid("null callback"));
        };
        let user = UserPtr(user);
        let out = Box::new(move |o: crate::terminal::TermOut<'_>| {
            let (kind, bytes): (u32, &[u8]) = match o {
                crate::terminal::TermOut::Data(d) => (NEXA_TERMINAL_DATA, d),
                crate::terminal::TermOut::Exit(why) => (NEXA_TERMINAL_EXIT, why.as_bytes()),
                crate::terminal::TermOut::Backlog => (NEXA_TERMINAL_BACKLOG, &[]),
            };
            let u = &user;
            // SAFETY: per the contract, valid until close, which waits for this call.
            unsafe { f(u.0, kind, bytes.as_ptr(), bytes.len()) }
        });
        let (opened, link) = crate::terminal::start(crate::terminal::size(cols, rows), out);
        session.open_terminal(link);
        // SAFETY: checked non-null; writable per the contract.
        unsafe { *id = opened };
        Ok(())
    })
}

/// Sends keys (or pasted text) to a terminal.
///
/// # Safety
/// `data` valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_terminal_write(id: u64, data: *const u8, len: usize) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let bytes = unsafe { bytes_of(data, len)? };
        if crate::terminal::write(id, bytes) {
            Ok(())
        } else {
            Err(Abi(NEXA_E_BAD_HANDLE, "no such terminal".into()))
        }
    })
}

/// Tells the host's shell the terminal's new size in cells.
#[unsafe(no_mangle)]
pub extern "C" fn nexa_terminal_resize(id: u64, cols: u32, rows: u32) -> i32 {
    guarded(|| {
        if crate::terminal::resize(id, cols, rows) {
            Ok(())
        } else {
            Err(Abi(NEXA_E_BAD_HANDLE, "no such terminal".into()))
        }
    })
}

/// Closes a terminal; its shell ends. When this returns its callback is not running and
/// will not be called again.
#[unsafe(no_mangle)]
pub extern "C" fn nexa_terminal_close(id: u64) -> i32 {
    guarded(|| {
        crate::terminal::close(id);
        Ok(())
    })
}

/// The Keychain service under which saved connections keep a host's fixed password.
#[cfg(target_vendor = "apple")]
const SAVED_PASSWORD_SERVICE: &str = "NexaDesk saved connection";

/// A saved connection's password item: `account` names the connection (its host key,
/// or its address before the host is known).
fn saved_password(account: &[u8]) -> Result<impl nexa_auth::keystore::KeyStore, Abi> {
    let account = std::str::from_utf8(account).map_err(|_| invalid("account is not UTF-8"))?;
    if account.is_empty() || account.len() > 256 {
        return Err(invalid("bad account"));
    }
    #[cfg(target_vendor = "apple")]
    let store = nexa_auth::keystore::KeychainStore::new(SAVED_PASSWORD_SERVICE, account);
    #[cfg(not(target_vendor = "apple"))]
    let store = nexa_auth::keystore::FileKeyStore::new(
        default_data_dir()
            .join("saved-passwords")
            .join(saved_password_file(account)),
    );
    Ok(store)
}

/// The longest file name [`saved_password_file`] makes, well under Windows' 255.
#[cfg(any(not(target_vendor = "apple"), test))]
const SAVED_PASSWORD_NAME_MAX: usize = 200;

/// The file a saved password is kept in outside Apple's Keychain. An account that is
/// already a name every system takes is used as it is, as it always was (host keys in
/// hex, plain addresses). Any other starts with `~` and has every byte but letters,
/// digits, `-` and `_` written as `%XX`: a signaling server's account
/// (`signal-access:wss://host:8443`) made a path Windows refused (os error 123, for `:`
/// and `/`), so a public server's access key could not be kept. A long one is cut and
/// ends with a hash of the whole account.
#[cfg(any(not(target_vendor = "apple"), test))]
fn saved_password_file(account: &str) -> String {
    let stem = account
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    let taken_as_is = !account.is_empty()
        && account.len() <= SAVED_PASSWORD_NAME_MAX
        && !account.starts_with('~')
        && !account.ends_with(['.', ' '])
        && !device
        && !account.chars().any(|c| {
            c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        });
    if taken_as_is {
        return account.to_owned();
    }
    let mut name = String::from("~");
    for b in account.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            name.push(char::from(b));
        } else {
            name.push_str(&format!("%{b:02X}"));
        }
    }
    if name.len() > SAVED_PASSWORD_NAME_MAX {
        // FNV-1a: stable from one build to the next, unlike std's hasher.
        let hash = account.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        name.truncate(SAVED_PASSWORD_NAME_MAX - 17);
        name.push_str(&format!("~{hash:016x}"));
    }
    name
}

/// The MCP server for AI agents while the person has it on (docs/MCP.md §16).
#[cfg(any(target_os = "macos", windows))]
static MCP: Mutex<Option<crate::mcp::Service>> = Mutex::new(None);

#[cfg(any(target_os = "macos", windows))]
fn mcp() -> std::sync::MutexGuard<'static, Option<crate::mcp::Service>> {
    MCP.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts serving local AI apps (through `nexa-mcp`) on the socket for `data_dir`
/// (empty: the default), signing in to the devices the person allowed AI agents to
/// operate. `name` is what hosts are told the AI's device is called. Already serving:
/// nothing changes.
///
/// # Safety
/// `data_dir` valid for `data_dir_len` bytes, `name` for `name_len`.
#[cfg(any(target_os = "macos", windows))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_mcp_start(
    data_dir: *const u8,
    data_dir_len: usize,
    name: *const u8,
    name_len: usize,
) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let (dir, name) = unsafe { (bytes_of(data_dir, data_dir_len)?, bytes_of(name, name_len)?) };
        let dir = std::str::from_utf8(dir).map_err(|_| invalid("data_dir is not UTF-8"))?;
        let name = std::str::from_utf8(name).map_err(|_| invalid("name is not UTF-8"))?;
        let dir = if dir.is_empty() {
            default_data_dir()
        } else {
            PathBuf::from(dir)
        };
        let mut serving = mcp();
        if serving.is_none() {
            let name = if name.trim().is_empty() {
                "NexaDesk".to_owned()
            } else {
                name.trim().to_owned()
            };
            // In viewer.log either way: a service that did not start leaves AI apps with
            // nothing to reach, and the reason is otherwise only on screen.
            match crate::mcp::Service::start(&dir, name) {
                Ok(service) => {
                    nexa_media::diag!("ai agents: serving");
                    *serving = Some(service);
                }
                Err(e) => {
                    nexa_media::diag!("ai agents: could not serve: {e}");
                    return Err(Abi(NEXA_E_INTERNAL, e));
                }
            }
        }
        Ok(())
    })
}

/// Stops serving AI apps and ends every AI agent's session.
#[cfg(any(target_os = "macos", windows))]
#[unsafe(no_mangle)]
pub extern "C" fn nexa_mcp_stop() -> i32 {
    guarded(|| {
        if let Some(service) = mcp().take() {
            service.stop_sessions();
            nexa_media::diag!("ai agents: stopped");
        }
        Ok(())
    })
}

/// Ends every AI agent's session; the server keeps serving.
#[cfg(any(target_os = "macos", windows))]
#[unsafe(no_mangle)]
pub extern "C" fn nexa_mcp_stop_sessions() -> i32 {
    guarded(|| {
        if let Some(service) = mcp().as_ref() {
            service.stop_sessions();
        }
        Ok(())
    })
}

/// What AI agents are doing, as UTF-8 JSON into `out`:
/// `{"serving": bool, "sessions": [{"client": "claude-code", "device": "MacBook"}]}`.
///
/// # Safety
/// `out` writable for `cap`, `written` writable.
#[cfg(any(target_os = "macos", windows))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_mcp_status(out: *mut u8, cap: usize, written: *mut usize) -> i32 {
    guarded(|| {
        let serving = mcp();
        let sessions: Vec<serde_json::Value> = serving
            .as_ref()
            .map(|s| s.sessions())
            .unwrap_or_default()
            .into_iter()
            .map(|s| serde_json::json!({ "client": s.client, "device": s.device }))
            .collect();
        let status = serde_json::json!({ "serving": serving.is_some(), "sessions": sessions });
        drop(serving);
        // SAFETY: per the contract.
        unsafe { give(status.to_string().as_bytes(), out, cap, written) }
    })
}

/// The password kept for the saved connection `account`, if any: what NexaDesk signs
/// in to it with for an AI agent (docs/MCP.md §16). It stays in this process.
#[cfg(any(target_os = "macos", windows))]
pub(crate) fn load_saved_password(account: &str) -> Option<String> {
    use nexa_auth::keystore::KeyStore;
    let kept = saved_password(account.as_bytes()).ok()?.load().ok()??;
    String::from_utf8(kept.to_vec()).ok()
}

/// Keeps `password` for the saved connection `account` in the login Keychain,
/// replacing one kept before.
///
/// # Safety
/// `account` valid for `account_len` bytes, `password` for `password_len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_saved_password_set(
    account: *const u8,
    account_len: usize,
    password: *const u8,
    password_len: usize,
) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let (account, password) = unsafe {
            (
                bytes_of(account, account_len)?,
                bytes_of(password, password_len)?,
            )
        };
        if password.is_empty() || password.len() > 1024 {
            return Err(invalid("bad password"));
        }
        use nexa_auth::keystore::KeyStore;
        saved_password(account)?
            .store(password)
            .map_err(|e| Abi(NEXA_E_INTERNAL, e.to_string()))
    })
}

/// The kept password, into `out`; `written` 0 and `NEXA_OK` when none is kept.
///
/// # Safety
/// `account` valid for `account_len` bytes; `out` writable for `cap`, `written` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_saved_password_get(
    account: *const u8,
    account_len: usize,
    out: *mut u8,
    cap: usize,
    written: *mut usize,
) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let account = unsafe { bytes_of(account, account_len)? };
        use nexa_auth::keystore::KeyStore;
        let kept = saved_password(account)?
            .load()
            .map_err(|e| Abi(NEXA_E_INTERNAL, e.to_string()))?;
        // SAFETY: per the contract.
        unsafe {
            give(
                kept.as_deref().map_or(&[][..], |k| k.as_slice()),
                out,
                cap,
                written,
            )
        }
    })
}

/// Forgets the kept password; succeeds when there was none.
///
/// # Safety
/// `account` valid for `account_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_saved_password_delete(account: *const u8, account_len: usize) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let account = unsafe { bytes_of(account, account_len)? };
        use nexa_auth::keystore::KeyStore;
        saved_password(account)?
            .delete()
            .map_err(|e| Abi(NEXA_E_INTERNAL, e.to_string()))
    })
}

/// 1 when symbol keys type what this Mac's keyboard prints on them (the default), 0
/// when they go by key position. Needs no client handle.
#[unsafe(no_mangle)]
pub extern "C" fn nexa_symbols_as_printed() -> i32 {
    i32::from(crate::keymap::symbols_as_printed(&default_data_dir()))
}

/// Sets that choice: kept for later sessions, and applied to one running now.
#[unsafe(no_mangle)]
pub extern "C" fn nexa_set_symbols_as_printed(on: i32) -> i32 {
    guarded(|| {
        crate::keymap::set_symbols_as_printed(&default_data_dir(), on != 0)
            .map_err(|e| invalid(&e.to_string()))
    })
}

/// "按下 → 发送": runs the real rewriter on `req.chord` and writes a `KeyPreview`.
/// Nothing is sent anywhere and no session is involved.
///
/// # Safety
/// `req` valid for `len` bytes, `out` writable for `cap` bytes, `written` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_keymap_preview(
    req: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
    written: *mut usize,
) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let req = unsafe { keymap_request(req, len)? };
        let preview =
            crate::keymap::preview_chord(&req, &default_data_dir()).map_err(|e| invalid(&e))?;
        // SAFETY: per the contract.
        unsafe { give(&preview.encode_to_vec(), out, cap, written) }
    })
}

/// `kind` for [`nexa_normalize`]: a one-time code, as the host shows it.
pub const NEXA_NORMALIZE_CODE: u32 = 1;
/// `kind` for [`nexa_normalize`]: a signaling server's address.
pub const NEXA_NORMALIZE_SERVER: u32 = 2;

/// What a person typed, in the one form the core uses, so every app shares one set of
/// rules: a one-time code (8 characters of A–Z and 2–7, any case, spaces and hyphens
/// ignored) or a signaling server (`wss://host:port`; `https://` and a bare host work).
/// `NEXA_E_PARSE` when it is not one: for a code, the secret is then a password.
///
/// # Safety
/// `input` valid for `len` bytes, `out` writable for `cap` bytes, `written` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_normalize(
    kind: u32,
    input: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
    written: *mut usize,
) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let input = std::str::from_utf8(unsafe { bytes_of(input, len)? })
            .map_err(|_| invalid("not UTF-8"))?;
        let normal = match kind {
            NEXA_NORMALIZE_CODE => nexa_auth::normalize_code(input)
                .map(|c| c.to_string())
                .map_err(|_| Abi(NEXA_E_PARSE, "not a one-time code".into()))?,
            NEXA_NORMALIZE_SERVER => nexa_session::rendezvous::signal::canonical_url(input)
                .map_err(|e| Abi(NEXA_E_PARSE, e.to_string()))?,
            _ => return Err(invalid("unknown kind")),
        };
        // SAFETY: per the contract.
        unsafe { give(normal.as_bytes(), out, cap, written) }
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

/// The trusted roots for a self-hosted signaling server: signal-ca.pem in the data
/// directory, read by the viewer at each connection and by the agent when it starts.
fn signal_ca_file() -> PathBuf {
    default_data_dir().join("signal-ca.pem")
}

fn signal_config(url: &[u8]) -> Result<nexa_session::rendezvous::signal::SignalConfig, Abi> {
    let url = std::str::from_utf8(url)
        .map_err(|_| invalid("server is not UTF-8"))?
        .trim();
    if url.is_empty() || url.len() > 512 {
        return Err(invalid("bad server address"));
    }
    let mut config = nexa_session::rendezvous::signal::SignalConfig::new(url.to_owned());
    let ca = signal_ca_file();
    if ca.exists() {
        config = config.with_ca_file(&ca).map_err(|e| invalid(&e))?;
    }
    Ok(config)
}

fn probe(url: &[u8]) -> Result<nexa_session::rendezvous::signal::ServerCertificate, Abi> {
    let config = signal_config(url)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Abi(NEXA_E_INTERNAL, e.to_string()))?;
    runtime
        .block_on(nexa_session::rendezvous::signal::server_certificate(
            &config,
        ))
        .map_err(|e| Abi(NEXA_E_INTERNAL, e.to_string()))
}

/// Looks at a signaling server's TLS certificate, so a self-hosted server can be
/// trusted on first use. `out` receives UTF-8 text: `trusted` or `untrusted`, a
/// newline, and the SHA-256 fingerprint (AA:BB:…) of the certificate that would be
/// trusted. Fails when the server cannot be reached, or when trusting its certificate
/// would not help (it names another server). Blocks for up to about 30 s: never call
/// it on the main thread.
///
/// # Safety
/// `url` valid for `url_len` bytes, `out` writable for `cap` bytes, `written` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_signal_certificate(
    url: *const u8,
    url_len: usize,
    out: *mut u8,
    cap: usize,
    written: *mut usize,
) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let found = probe(unsafe { bytes_of(url, url_len)? })?;
        let text = format!(
            "{}\n{}",
            if found.trusted {
                "trusted"
            } else {
                "untrusted"
            },
            found.fingerprint
        );
        // SAFETY: per the contract.
        unsafe { give(text.as_bytes(), out, cap, written) }
    })
}

/// Trusts a signaling server's certificate: looks again, and only if it is still the
/// one with `fingerprint` (what the person accepted) adds it to signal-ca.pem. The
/// agent reads that file when remote control next starts. Blocks like
/// [`nexa_signal_certificate`].
///
/// # Safety
/// `url` valid for `url_len` bytes, `fingerprint` for `fingerprint_len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nexa_signal_trust(
    url: *const u8,
    url_len: usize,
    fingerprint: *const u8,
    fingerprint_len: usize,
) -> i32 {
    guarded(|| {
        // SAFETY: per the contract.
        let (url, expected) = unsafe {
            (
                bytes_of(url, url_len)?,
                bytes_of(fingerprint, fingerprint_len)?,
            )
        };
        let expected = std::str::from_utf8(expected).map_err(|_| invalid("bad fingerprint"))?;
        let found = probe(url)?;
        if found.trusted {
            return Ok(());
        }
        if !found.fingerprint.eq_ignore_ascii_case(expected.trim()) {
            return Err(Abi(
                NEXA_E_NOT_PERMITTED,
                "the server's certificate changed since it was shown".to_owned(),
            ));
        }
        let path = signal_ca_file();
        let mut pem = std::fs::read_to_string(&path).unwrap_or_default();
        if !pem.is_empty() && !pem.ends_with('\n') {
            pem.push('\n');
        }
        pem.push_str(&found.pem());
        let dir = path.parent().ok_or_else(|| invalid("no data directory"))?;
        std::fs::create_dir_all(dir).map_err(|e| Abi(NEXA_E_INTERNAL, e.to_string()))?;
        let tmp = path.with_extension("pem.tmp");
        std::fs::write(&tmp, pem).map_err(|e| Abi(NEXA_E_INTERNAL, e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| Abi(NEXA_E_INTERNAL, e.to_string()))
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn saved_password_files_have_names_every_system_takes() {
        use super::{SAVED_PASSWORD_NAME_MAX, saved_password_file};
        // As before: host keys in hex, plain addresses and names.
        for kept in [
            "4d27201d549f6456b7ad9c58f3fd605c",
            "192.168.1.20",
            "studio.tail1234.ts.net",
        ] {
            assert_eq!(saved_password_file(kept), kept);
        }
        assert_eq!(
            saved_password_file("signal-access:wss://209.33.171.123:8443"),
            "~signal-access%3Awss%3A%2F%2F209%2E33%2E171%2E123%3A8443"
        );
        for odd in [
            "con",
            "COM1.txt",
            "a:b",
            "x.",
            "~x",
            "a/b",
            "a\\b",
            "a*b",
            "tab\there",
        ] {
            let name = saved_password_file(odd);
            assert!(
                name.starts_with('~')
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_%~".contains(&b)),
                "{odd:?} -> {name}"
            );
        }
        // Long accounts fit, and stay apart.
        let long = |port: u32| format!("signal-access:wss://{}.example:{port}", "a".repeat(200));
        let (a, b) = (
            saved_password_file(&long(8443)),
            saved_password_file(&long(8444)),
        );
        assert!(a.len() <= SAVED_PASSWORD_NAME_MAX && b.len() <= SAVED_PASSWORD_NAME_MAX);
        assert_ne!(a, b);
    }

    /// The access key of a public signaling server is kept and read back (os error 123
    /// before).
    #[cfg(windows)]
    #[test]
    fn a_servers_access_key_is_kept_on_windows() {
        use nexa_auth::keystore::{FileKeyStore, KeyStore};
        let dir = tempfile::tempdir().unwrap();
        let account = "signal-access:wss://209.33.171.123:8443";
        let store = FileKeyStore::new(dir.path().join(super::saved_password_file(account)));
        store.store(b"secret").unwrap();
        assert_eq!(
            store.load().unwrap().as_deref().map(Vec::as_slice),
            Some(&b"secret"[..])
        );
        store.delete().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    fn normalized(kind: u32, input: &str) -> Result<String, i32> {
        let mut out = [0u8; 256];
        let mut written = 0usize;
        // SAFETY: buffers valid for the lengths passed.
        let rc = unsafe {
            nexa_normalize(
                kind,
                input.as_ptr(),
                input.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut written,
            )
        };
        if rc == NEXA_OK {
            Ok(String::from_utf8(out[..written].to_vec()).unwrap())
        } else {
            Err(rc)
        }
    }

    /// One set of rules for every app: the iOS app used to take six digits for a code.
    #[test]
    fn codes_and_servers_normalize_through_the_abi() {
        assert_eq!(
            normalized(NEXA_NORMALIZE_CODE, "abcd-2345").as_deref(),
            Ok("ABCD2345")
        );
        assert_eq!(
            normalized(NEXA_NORMALIZE_CODE, "ab cd 23 45").as_deref(),
            Ok("ABCD2345")
        );
        assert_eq!(normalized(NEXA_NORMALIZE_CODE, "123456"), Err(NEXA_E_PARSE));
        assert_eq!(
            normalized(NEXA_NORMALIZE_CODE, "my password"),
            Err(NEXA_E_PARSE)
        );
        assert_eq!(
            normalized(NEXA_NORMALIZE_SERVER, "https://signal.example.com:8443/").as_deref(),
            Ok("wss://signal.example.com:8443")
        );
        assert_eq!(
            normalized(NEXA_NORMALIZE_SERVER, "ftp://x"),
            Err(NEXA_E_PARSE)
        );
        assert_eq!(normalized(9, "x"), Err(NEXA_E_INVALID_ARG));
    }
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

    fn call(
        f: unsafe extern "C" fn(*const u8, usize, *mut u8, usize, *mut usize) -> i32,
        req: &KeymapRequest,
    ) -> (i32, Vec<u8>) {
        let bytes = req.encode_to_vec();
        let mut written = 0usize;
        // SAFETY: valid buffers; a first call with no room asks for the size.
        let first = unsafe {
            f(
                bytes.as_ptr(),
                bytes.len(),
                std::ptr::null_mut(),
                0,
                &mut written,
            )
        };
        if first != NEXA_E_BUFFER_TOO_SMALL {
            return (first, Vec::new());
        }
        let mut out = vec![0u8; written];
        // SAFETY: as above, now with room.
        let status = unsafe {
            f(
                bytes.as_ptr(),
                bytes.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut written,
            )
        };
        out.truncate(written);
        (status, out)
    }

    #[test]
    fn keymap_settings_load_save_and_preview_through_the_abi() {
        use crate::proto::{KeyPreview, KeymapView};
        let dir = tempfile::tempdir().unwrap();
        let mut req = KeymapRequest {
            data_dir: dir.path().to_string_lossy().into_owned(),
            local_os: 2,
            remote_os: 1,
            ..Default::default()
        };
        let (status, bytes) = call(nexa_keymap_rules, &req);
        assert_eq!(status, NEXA_OK);
        let view = KeymapView::decode(bytes.as_slice()).unwrap();
        assert!(view.error.is_empty());
        assert!(
            view.rules
                .iter()
                .any(|r| r.builtin && r.when == "Cmd+Shift+Z" && r.enabled)
        );

        req.chord = "Cmd+Shift+Z".into();
        let (status, bytes) = call(nexa_keymap_preview, &req);
        assert_eq!(status, NEXA_OK);
        let p = KeyPreview::decode(bytes.as_slice()).unwrap();
        assert_eq!(p.sent, vec!["Ctrl+Y"]);
        assert!(p.events.iter().any(|e| e.key == "Y" && e.down));

        // Switch it off and preview the edit before saving: the general rule applies.
        let mut rules = view.rules.clone();
        rules
            .iter_mut()
            .find(|r| r.when == "Cmd+Shift+Z")
            .unwrap()
            .enabled = false;
        rules.push(crate::proto::KeymapRule {
            when: "Cmd+J".into(),
            then: vec!["Ctrl+Alt+J".into()],
            enabled: true,
            ..Default::default()
        });
        req.rules = rules;
        req.edited = true;
        let (_, bytes) = call(nexa_keymap_preview, &req);
        assert_eq!(
            KeyPreview::decode(bytes.as_slice()).unwrap().sent,
            vec!["Ctrl+Shift+Z"]
        );
        assert!(
            !dir.path().join("keymap.toml").exists(),
            "a preview writes nothing"
        );

        let bytes = req.encode_to_vec();
        // SAFETY: valid buffer.
        assert_eq!(
            unsafe { nexa_keymap_save(bytes.as_ptr(), bytes.len()) },
            NEXA_OK
        );
        assert!(dir.path().join("keymap.toml").exists());
        let saved = KeymapRequest {
            rules: Vec::new(),
            edited: false,
            chord: "Cmd+J".into(),
            ..req.clone()
        };
        let (_, bytes) = call(nexa_keymap_preview, &saved);
        assert_eq!(
            KeyPreview::decode(bytes.as_slice()).unwrap().sent,
            vec!["Ctrl+Alt+J"]
        );
        let (_, bytes) = call(nexa_keymap_rules, &saved);
        let listed = KeymapView::decode(bytes.as_slice()).unwrap();
        assert!(
            !listed
                .rules
                .iter()
                .find(|r| r.when == "Cmd+Shift+Z")
                .unwrap()
                .enabled
        );
        assert!(
            listed
                .rules
                .last()
                .is_some_and(|r| !r.builtin && r.when == "Cmd+J")
        );
        // What sessions load.
        let (keymap, problem) = crate::keymap::for_sessions(dir.path());
        assert!(problem.is_none());
        let p = nexa_input::user::preview(
            &keymap,
            nexa_input::Os::Macos,
            nexa_input::Os::Windows,
            "Cmd+J",
            nexa_input::FocusClass::Default,
        )
        .unwrap();
        assert_eq!(p.sent, vec!["Ctrl+Alt+J"]);

        // An invalid rule is refused and the file is unchanged.
        let before = std::fs::read(dir.path().join("keymap.toml")).unwrap();
        let mut bad = req.clone();
        bad.rules.push(crate::proto::KeymapRule {
            when: "Cmd+Nope".into(),
            then: vec!["Ctrl+K".into()],
            enabled: true,
            ..Default::default()
        });
        let bytes = bad.encode_to_vec();
        // SAFETY: valid buffer.
        assert_eq!(
            unsafe { nexa_keymap_save(bytes.as_ptr(), bytes.len()) },
            NEXA_E_INVALID_ARG
        );
        assert_eq!(
            std::fs::read(dir.path().join("keymap.toml")).unwrap(),
            before
        );
        // A bad chord in a preview is an answer, not an error.
        let (status, bytes) = call(
            nexa_keymap_preview,
            &KeymapRequest {
                chord: "Cmd+Nope".into(),
                ..saved.clone()
            },
        );
        assert_eq!(status, NEXA_OK);
        assert!(
            !KeyPreview::decode(bytes.as_slice())
                .unwrap()
                .error
                .is_empty()
        );
        // A damaged file: built-in rules and a reason; sessions fall back too.
        std::fs::write(dir.path().join("keymap.toml"), "garbage = [").unwrap();
        let (_, bytes) = call(nexa_keymap_rules, &saved);
        assert!(
            !KeymapView::decode(bytes.as_slice())
                .unwrap()
                .error
                .is_empty()
        );
        assert!(crate::keymap::for_sessions(dir.path()).1.is_some());
        // Garbage requests are errors, not crashes.
        let junk = [0xFFu8; 8];
        let mut written = 0usize;
        // SAFETY: valid buffers.
        let status = unsafe {
            nexa_keymap_rules(
                junk.as_ptr(),
                junk.len(),
                std::ptr::null_mut(),
                0,
                &mut written,
            )
        };
        assert_eq!(status, NEXA_E_PARSE);
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
