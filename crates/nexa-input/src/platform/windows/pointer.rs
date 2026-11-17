//! Game mode's pointer lock on Windows (§7.4, docs/WINDOWS.md): Raw Input for the
//! mouse's own motion, and the cursor pinned where it is with `ClipCursor`.
//!
//! A message-only window on its own thread receives `WM_INPUT` (registered with
//! `RIDEV_INPUTSINK`, so it keeps coming while another window of this app has focus)
//! and sends the running totals of the motion, as `Pointer` carries them. Dropping the
//! lock frees the cursor first, then stops the thread.

use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering::SeqCst},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{
            Input::{
                GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE,
                RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEMOUSE,
                RegisterRawInputDevices,
            },
            WindowsAndMessaging::{
                ClipCursor, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
                GetCursorPos, GetMessageW, HWND_MESSAGE, MSG, PostThreadMessageW, RegisterClassW,
                TranslateMessage, WINDOW_EX_STYLE, WM_INPUT, WM_QUIT, WNDCLASSW, WS_OVERLAPPED,
            },
        },
    },
    core::w,
};

/// One motion: running totals since the lock began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Motion {
    pub x: i32,
    pub y: i32,
}

thread_local! {
    static SINK: Cell<Option<*const Sender<Motion>>> = const { Cell::new(None) };
    static TOTAL: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
}

unsafe extern "system" fn window(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        let mut raw = RAWINPUT::default();
        let mut size = size_of::<RAWINPUT>() as u32;
        // SAFETY: reading this message's raw input into a buffer of the stated size.
        let read = unsafe {
            GetRawInputData(
                HRAWINPUT(lparam.0 as _),
                RID_INPUT,
                Some((&mut raw as *mut RAWINPUT).cast()),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if read != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE.0 {
            // SAFETY: the header says this is mouse input.
            let mouse = unsafe { raw.data.mouse };
            // Tablets report absolute positions; only relative motion moves the lock.
            if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 == 0
                && (mouse.lLastX != 0 || mouse.lLastY != 0)
            {
                let (x, y) = TOTAL.get();
                let total = (x.wrapping_add(mouse.lLastX), y.wrapping_add(mouse.lLastY));
                TOTAL.set(total);
                if let Some(tx) = SINK.get() {
                    // SAFETY: the sender lives on this thread's stack for the whole loop.
                    let _ = unsafe { &*tx }.send(Motion {
                        x: total.0,
                        y: total.1,
                    });
                }
            }
        }
    }
    // SAFETY: default handling for everything (WM_INPUT included, as documented).
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// The running lock. Dropping it gives the mouse back.
pub struct PointerCapture {
    thread_id: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl PointerCapture {
    pub fn start() -> Result<(Self, Receiver<Motion>), String> {
        let (tx, rx) = mpsc::channel();
        let thread_id = Arc::new(AtomicU32::new(0));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread = {
            let thread_id = thread_id.clone();
            thread::Builder::new()
                .name("pointer-capture".into())
                .spawn(move || {
                    let tx = tx;
                    SINK.set(Some(&tx as *const _));
                    TOTAL.set((0, 0));
                    // SAFETY: a window class and message-only window owned by this thread,
                    // destroyed before it ends; raw input registered for that window and
                    // removed before it is destroyed; the cursor clip released on the way out.
                    unsafe {
                        thread_id.store(GetCurrentThreadId(), SeqCst);
                        let instance = GetModuleHandleW(None).unwrap_or_default();
                        let class = WNDCLASSW {
                            lpfnWndProc: Some(window),
                            hInstance: instance.into(),
                            lpszClassName: w!("NexaDeskPointerLock"),
                            ..Default::default()
                        };
                        RegisterClassW(&class); // fails harmlessly when already registered
                        let hwnd = match CreateWindowExW(
                            WINDOW_EX_STYLE(0),
                            w!("NexaDeskPointerLock"),
                            None,
                            WS_OVERLAPPED,
                            0,
                            0,
                            0,
                            0,
                            Some(HWND_MESSAGE),
                            None,
                            Some(instance.into()),
                            None,
                        ) {
                            Ok(h) => h,
                            Err(e) => {
                                let _ = ready_tx.send(Err(e.to_string()));
                                return;
                            }
                        };
                        let device = |flags, target| RAWINPUTDEVICE {
                            usUsagePage: 0x01, // generic desktop
                            usUsage: 0x02,     // mouse
                            dwFlags: flags,
                            hwndTarget: target,
                        };
                        if let Err(e) = RegisterRawInputDevices(
                            &[device(RIDEV_INPUTSINK, hwnd)],
                            size_of::<RAWINPUTDEVICE>() as u32,
                        ) {
                            let _ = DestroyWindow(hwnd);
                            let _ = ready_tx.send(Err(e.to_string()));
                            return;
                        }
                        // Pinned where it is: the host draws its own cursor in game mode.
                        let mut at = POINT::default();
                        let _ = GetCursorPos(&mut at);
                        let _ = ClipCursor(Some(&RECT {
                            left: at.x,
                            top: at.y,
                            right: at.x + 1,
                            bottom: at.y + 1,
                        }));
                        let _ = ready_tx.send(Ok(()));
                        let mut msg = MSG::default();
                        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        let _ = ClipCursor(None);
                        let _ = RegisterRawInputDevices(
                            &[device(RIDEV_REMOVE, HWND::default())],
                            size_of::<RAWINPUTDEVICE>() as u32,
                        );
                        let _ = DestroyWindow(hwnd);
                    }
                    SINK.set(None);
                })
                .map_err(|e| e.to_string())?
        };
        ready_rx
            .recv()
            .map_err(|_| "the pointer thread ended".to_owned())??;
        Ok((
            Self {
                thread_id,
                thread: Some(thread),
            },
            rx,
        ))
    }
}

impl Drop for PointerCapture {
    fn drop(&mut self) {
        // SAFETY: the cursor clip is process-wide, so it is released here at once too;
        // then the thread's message loop is asked to end.
        unsafe {
            let _ = ClipCursor(None);
            let _ = PostThreadMessageW(self.thread_id.load(SeqCst), WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
