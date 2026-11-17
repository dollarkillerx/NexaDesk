//! Keyboard capture on Windows (§7.4, docs/WINDOWS.md): a low-level keyboard hook,
//! the only way to receive Win, Alt+Tab and the like before Windows acts on them.
//!
//! The hook runs on its own thread with a message loop. While capture is active it
//! swallows every key (keys typed by other programs, marked injected, pass) and turns
//! them into [`CaptureEvent`]s; while inactive it passes everything on untouched.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::SeqCst},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
};

use windows::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
        LLKHF_INJECTED, MSG, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
        UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
    },
};

use super::scancode_of;
use crate::{
    Hid, PhysKey,
    scanfilter::{ScanEvent, ScanFilter},
};

/// An output of capture, in physical order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureEvent {
    Key(PhysKey),
    /// The host key chord (Ctrl+Alt+Win+G): leave game mode or show the toolbar.
    HostKey,
}

/// The fake left Ctrl that AltGr sends ahead of right Alt on many layouts.
const ALTGR_FAKE_CTRL: u32 = 0x21D;

/// Scan code (0xE0xx for extended keys) to HID usage: the inverse of [`scancode_of`].
fn hid_of(scan: u16) -> Option<Hid> {
    static TABLE: OnceLock<HashMap<u16, Hid>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            (0u16..=0xFF)
                .map(Hid::key)
                .filter_map(|h| scancode_of(h).map(|s| (s, h)))
                .collect()
        })
        .get(&scan)
        .copied()
}

struct HookState {
    tx: Sender<CaptureEvent>,
    active: Arc<AtomicBool>,
    /// Bumped by `set_active`; the hook resets the filter when it sees a new value.
    generation: Arc<AtomicU64>,
    seen: u64,
    filter: ScanFilter,
    out: Vec<ScanEvent>,
}

thread_local! {
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points at a KBDLLHOOKSTRUCT for this call.
        let k = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let swallow = STATE.with(|cell| {
            let mut cell = cell.borrow_mut();
            let Some(state) = cell.as_mut() else {
                return false;
            };
            let generation = state.generation.load(SeqCst);
            if generation != state.seen {
                state.seen = generation;
                state.filter.reset();
            }
            if !state.active.load(SeqCst) || k.flags.0 & LLKHF_INJECTED.0 != 0 {
                return false;
            }
            if k.scanCode == ALTGR_FAKE_CTRL {
                return true;
            }
            let extended = k.flags.0 & LLKHF_EXTENDED.0 != 0;
            let scan = (k.scanCode as u16 & 0xFF) | if extended { 0xE000 } else { 0 };
            let down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
            if let Some(hid) = hid_of(scan) {
                state.out.clear();
                state.filter.on_key(hid, down, &mut state.out);
                for e in state.out.drain(..) {
                    let _ = state.tx.send(match e {
                        ScanEvent::Key(k) => CaptureEvent::Key(k),
                        ScanEvent::HostKey => CaptureEvent::HostKey,
                    });
                }
            }
            // While active every key belongs to the session, mapped or not.
            true
        });
        if swallow {
            return LRESULT(1);
        }
    }
    // SAFETY: passing the event on, as every hook must when it does not keep it.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// The running hook. Dropping it removes the hook and ends its thread.
pub struct KeyCapture {
    active: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    thread_id: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl KeyCapture {
    /// Installs the hook on a new thread, inactive until [`KeyCapture::set_active`].
    pub fn start() -> Result<(Self, Receiver<CaptureEvent>), String> {
        let (tx, rx) = mpsc::channel();
        let active = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let thread_id = Arc::new(AtomicU32::new(0));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread = {
            let (active, generation, thread_id) =
                (active.clone(), generation.clone(), thread_id.clone());
            thread::Builder::new()
                .name("input-capture".into())
                .spawn(move || {
                    STATE.with(|s| {
                        *s.borrow_mut() = Some(HookState {
                            tx,
                            active,
                            generation,
                            seen: 0,
                            filter: ScanFilter::default(),
                            out: Vec::new(),
                        });
                    });
                    // SAFETY: this thread's id; a hook procedure in this module, which
                    // stays loaded; the hook is removed before the thread ends.
                    unsafe {
                        thread_id.store(GetCurrentThreadId(), SeqCst);
                        let module = GetModuleHandleW(None).ok();
                        let hook = match SetWindowsHookExW(
                            WH_KEYBOARD_LL,
                            Some(hook),
                            module.map(Into::into),
                            0,
                        ) {
                            Ok(h) => h,
                            Err(e) => {
                                let _ = ready_tx.send(Err(e.to_string()));
                                return;
                            }
                        };
                        let _ = ready_tx.send(Ok(()));
                        let mut msg = MSG::default();
                        // Low-level hooks are called through this thread's message loop.
                        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        let _ = UnhookWindowsHookEx(hook);
                    }
                })
                .map_err(|e| e.to_string())?
        };
        ready_rx
            .recv()
            .map_err(|_| "the capture thread ended".to_owned())??;
        Ok((
            Self {
                active,
                generation,
                thread_id,
                thread: Some(thread),
            },
            rx,
        ))
    }

    pub fn set_active(&self, active: bool) {
        if self.active.swap(active, SeqCst) != active {
            self.generation.fetch_add(1, SeqCst);
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(SeqCst)
    }
}

impl Drop for KeyCapture {
    fn drop(&mut self) {
        self.active.store(false, SeqCst);
        // SAFETY: asking the hook thread's message loop to end.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id.load(SeqCst), WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
