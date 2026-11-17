//! The host's cursor over the Windows video view (§6.9). Reached only from `RealDeps`.
//!
//! The video is a STATIC child window that lets the mouse through, so `WM_SETCURSOR`
//! for it reaches the session's top-level window. A subclass on that window, installed
//! from the UI thread when the view is attached, answers it with the host's shape while
//! the pointer is over the video, and leaves everywhere else to the UI. Shapes are PNGs
//! decoded with WIC into `HCURSOR`s.
//!
//! `show`, `reset` and `warp` may come from any thread: they update shared state and
//! post a message, and the subclass applies it on the UI thread. Dropping the display
//! posts the subclass's removal, which also happens by itself if the window goes first.

use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{
            Gdi::{
                BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection,
                DIB_RGB_COLORS, DeleteObject, HBITMAP,
            },
            Imaging::{
                CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory,
                WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
                WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
            },
        },
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        },
        UI::{
            HiDpi::GetDpiForWindow,
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::{
                CreateIconIndirect, DestroyCursor, GA_ROOT, GetAncestor, GetCursorPos,
                GetWindowRect, HCURSOR, HTCLIENT, ICONINFO, IDC_ARROW, IsWindowVisible,
                LoadCursorW, PostMessageW, RegisterWindowMessageW, SetCursor, SetCursorPos,
                WM_NCDESTROY, WM_SETCURSOR,
            },
        },
    },
    core::{BOOL, w},
};

use crate::deps::{CursorDisplay, LocalCursor};

/// The largest cursor made, per side, in pixels.
const MAX_SIDE: u32 = 256;
/// Shapes kept ready; the host switches between a handful.
const KEEP: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    /// Whatever the UI shows.
    Default,
    Hidden,
    /// An `HCURSOR` owned by [`Shared::cursors`].
    Host(isize),
}

struct Made {
    id: u64,
    size: (u32, u32),
    cursor: isize,
}

struct Shared {
    video: isize,
    look: Mutex<Look>,
    cursors: Mutex<VecDeque<Made>>,
}

impl Shared {
    fn video(&self) -> HWND {
        HWND(self.video as *mut c_void)
    }

    /// Whether the pointer is over the video now.
    fn over_video(&self) -> bool {
        let (mut at, mut rect) = (POINT::default(), RECT::default());
        // SAFETY: reading the cursor position and a window's rectangle.
        let read = unsafe {
            IsWindowVisible(self.video()).as_bool()
                && GetCursorPos(&mut at).is_ok()
                && GetWindowRect(self.video(), &mut rect).is_ok()
        };
        read && (rect.left..rect.right).contains(&at.x) && (rect.top..rect.bottom).contains(&at.y)
    }

    /// Sets the cursor for the video; false leaves it to the UI. UI thread only.
    fn apply(&self, restore_arrow: bool) -> bool {
        let look = *self.look.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: setting the cursor to one this owns (kept alive while it is the
        // look), none, or the system arrow.
        unsafe {
            match look {
                Look::Default => {
                    if restore_arrow {
                        SetCursor(LoadCursorW(None, IDC_ARROW).ok());
                    }
                    false
                }
                Look::Hidden => {
                    SetCursor(None);
                    true
                }
                Look::Host(c) => {
                    SetCursor(Some(HCURSOR(c as *mut c_void)));
                    true
                }
            }
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let cursors = self.cursors.get_mut().unwrap_or_else(|e| e.into_inner());
        for m in cursors.drain(..) {
            // SAFETY: made by CreateIconIndirect here, destroyed once, after the subclass
            // that could still set it is gone.
            let _ = unsafe { DestroyCursor(HCURSOR(m.cursor as *mut c_void)) };
        }
    }
}

fn message(name: windows::core::PCWSTR, slot: &OnceLock<u32>) -> u32 {
    // SAFETY: registering a message name.
    *slot.get_or_init(|| unsafe { RegisterWindowMessageW(name) })
}

/// "Apply the look now", with the subclass id in `wparam`.
fn refresh_message() -> u32 {
    static M: OnceLock<u32> = OnceLock::new();
    message(w!("NexaDeskCursorRefresh"), &M)
}

/// "Remove yourself", with the subclass id in `wparam`.
fn remove_message() -> u32 {
    static M: OnceLock<u32> = OnceLock::new();
    message(w!("NexaDeskCursorRemove"), &M)
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: `data` is the Arc<Shared> given to SetWindowSubclass, released only below
    // when the subclass is removed.
    let shared = unsafe { &*(data as *const Shared) };
    if msg == WM_SETCURSOR
        && (lparam.0 & 0xFFFF) as u32 == HTCLIENT
        && shared.over_video()
        && shared.apply(false)
    {
        return LRESULT(1);
    }
    if msg == refresh_message() && wparam.0 == id {
        if shared.over_video() {
            shared.apply(true);
        }
        return LRESULT(0);
    }
    if (msg == remove_message() && wparam.0 == id) || msg == WM_NCDESTROY {
        // SAFETY: removing this subclass from its own window, on its thread, then
        // releasing the reference it held.
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(subclass), id);
            if msg != WM_NCDESTROY && shared.over_video() {
                SetCursor(LoadCursorW(None, IDC_ARROW).ok());
            }
            drop(Arc::from_raw(data as *const Shared));
        }
        if msg != WM_NCDESTROY {
            return LRESULT(0);
        }
    }
    // SAFETY: everything else goes on to the window's own procedure.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Decodes `png` at `w`×`h` into straight-alpha BGRA rows, top first.
fn decode(png: &[u8], w: u32, h: u32) -> windows::core::Result<Vec<u8>> {
    thread_local! {
        // SAFETY: COM for this thread, for WIC; a thread that already has it keeps its own.
        static COM: () = { let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }; };
    }
    COM.with(|_| ());
    // SAFETY: WIC objects used within this call; the stream reads `png`, which outlives it.
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = wic.CreateStream()?;
        stream.InitializeFromMemory(png)?;
        let decoder =
            wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;
        let scaler = wic.CreateBitmapScaler()?;
        scaler.Initialize(&frame, w, h, WICBitmapInterpolationModeFant)?;
        let converter = wic.CreateFormatConverter()?;
        converter.Initialize(
            &scaler,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        converter.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
        Ok(pixels)
    }
}

/// An `HCURSOR` of `w`×`h` from straight-alpha BGRA rows, with its hot spot.
fn make_cursor(bgra: &[u8], w: u32, h: u32, hot: (u32, u32)) -> windows::core::Result<HCURSOR> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: a DIB section of the stated size, filled within its bounds; both bitmaps
    // are deleted once the cursor has its own copies.
    unsafe {
        let mut bits = std::ptr::null_mut();
        let color: HBITMAP = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), bgra.len());
        // With an alpha channel the mask is ignored, but it must exist.
        let mask = CreateBitmap(w as i32, h as i32, 1, 1, None);
        let made = CreateIconIndirect(&ICONINFO {
            fIcon: BOOL(0),
            xHotspot: hot.0,
            yHotspot: hot.1,
            hbmMask: mask,
            hbmColor: color,
        });
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        Ok(HCURSOR(made?.0))
    }
}

/// Pixels from local points at the video's DPI, within what a cursor may be.
fn pixels(pt: f64, dpi: u32) -> u32 {
    ((pt * f64::from(dpi) / 96.0).round() as u32).clamp(1, MAX_SIDE)
}

pub struct WinCursorDisplay {
    shared: Arc<Shared>,
    root: isize,
    id: usize,
}

impl WinCursorDisplay {
    /// # Safety
    /// UI thread; `video` is the attached view's live HWND.
    pub unsafe fn attach(video: *mut c_void) -> Option<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let video_hwnd = HWND(video);
        // SAFETY: per the contract; the subclass holds one reference to `shared`.
        unsafe {
            let root = GetAncestor(video_hwnd, GA_ROOT);
            if root.is_invalid() {
                return None;
            }
            let shared = Arc::new(Shared {
                video: video as isize,
                look: Mutex::new(Look::Default),
                cursors: Mutex::new(VecDeque::new()),
            });
            let id = NEXT.fetch_add(1, Relaxed);
            let data = Arc::into_raw(shared.clone()) as usize;
            if !SetWindowSubclass(root, Some(subclass), id, data).as_bool() {
                drop(Arc::from_raw(data as *const Shared));
                return None;
            }
            Some(Self {
                shared,
                root: root.0 as isize,
                id,
            })
        }
    }

    fn post(&self, msg: u32) {
        // SAFETY: posting to the session window; a window already gone just fails.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(self.root as *mut c_void)),
                msg,
                WPARAM(self.id),
                LPARAM(0),
            )
        };
    }

    fn set(&self, look: Look) {
        *self.shared.look.lock().unwrap_or_else(|e| e.into_inner()) = look;
        self.post(refresh_message());
    }

    /// The `HCURSOR` for `c` at this view's DPI, made once per shape and size.
    fn cursor_for(&self, c: &LocalCursor) -> Option<isize> {
        // SAFETY: reading the DPI of a live window.
        let dpi = unsafe { GetDpiForWindow(self.shared.video()) }.max(96);
        let size = (pixels(c.width_pt, dpi), pixels(c.height_pt, dpi));
        let current = *self.shared.look.lock().unwrap_or_else(|e| e.into_inner());
        let mut cursors = self
            .shared
            .cursors
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(m) = cursors.iter().find(|m| m.id == c.id && m.size == size) {
            return Some(m.cursor);
        }
        let at = |pt: f64, side: u32| {
            ((pt.max(0.0) * f64::from(dpi) / 96.0).round() as u32).min(side - 1)
        };
        let hot = (at(c.hot_x_pt, size.0), at(c.hot_y_pt, size.1));
        let bgra = decode(&c.png, size.0, size.1).ok()?;
        let cursor = make_cursor(&bgra, size.0, size.1, hot).ok()?.0 as isize;
        if cursors.len() >= KEEP {
            // The oldest that is not on screen now.
            if let Some(i) = cursors.iter().position(|m| Look::Host(m.cursor) != current) {
                let old = cursors.remove(i).map(|m| m.cursor);
                if let Some(old) = old {
                    // SAFETY: one of ours, not the cursor in use.
                    let _ = unsafe { DestroyCursor(HCURSOR(old as *mut c_void)) };
                }
            }
        }
        cursors.push_back(Made {
            id: c.id,
            size,
            cursor,
        });
        Some(cursor)
    }
}

impl CursorDisplay for WinCursorDisplay {
    fn show(&mut self, cursor: Option<&LocalCursor>) {
        match cursor {
            None => self.set(Look::Hidden),
            Some(c) => match self.cursor_for(c) {
                Some(h) => self.set(Look::Host(h)),
                // A shape Windows cannot read: the ordinary cursor rather than none.
                None => self.set(Look::Default),
            },
        }
    }

    fn reset(&mut self) {
        self.set(Look::Default);
    }

    fn warp(&mut self, x_pt: f64, y_pt: f64) {
        let mut rect = RECT::default();
        // SAFETY: reading the view's place and DPI, then moving the cursor there.
        unsafe {
            if GetWindowRect(self.shared.video(), &mut rect).is_err() {
                return;
            }
            let scale = f64::from(GetDpiForWindow(self.shared.video()).max(96)) / 96.0;
            let _ = SetCursorPos(
                rect.left + (x_pt * scale).round() as i32,
                rect.top + (y_pt * scale).round() as i32,
            );
        }
    }
}

impl Drop for WinCursorDisplay {
    fn drop(&mut self) {
        *self.shared.look.lock().unwrap_or_else(|e| e.into_inner()) = Look::Default;
        self.post(remove_message());
    }
}
