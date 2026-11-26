//! The host cursor (§6.9) on Windows: shape from GetIconInfo, visibility and position
//! from GetCursorInfo. Read only: nothing here moves or changes it.

use std::sync::Arc;

use windows::Win32::{
    Foundation::POINT,
    Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
        DeleteObject, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ,
    },
    UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetIconInfo, HCURSOR, HICON, ICONINFO,
    },
};

use crate::{CursorImage, CursorSample, CursorSource};

/// The most a cursor image may be on a side.
const MOST: i32 = 256;

#[derive(Default)]
pub struct WinCursorSource {
    last: Option<(isize, Arc<CursorImage>)>,
}

impl WinCursorSource {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CursorSource for WinCursorSource {
    fn sample(&mut self) -> CursorSample {
        // The cursor of the desktop receiving input (docs/WINDOWS_SERVICE.md §4).
        super::desktop::follow_input_desktop(false);
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: a sized CURSORINFO.
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            return CursorSample::default();
        }
        let visible = info.flags.0 & CURSOR_SHOWING.0 != 0 && !info.hCursor.is_invalid();
        let key = info.hCursor.0 as isize;
        let image = match &self.last {
            Some((k, img)) if *k == key => Some(img.clone()),
            _ if visible => {
                let read = image_of(info.hCursor).map(Arc::new);
                if let Some(img) = &read {
                    self.last = Some((key, img.clone()));
                }
                read
            }
            _ => None,
        };
        let POINT { x, y } = info.ptScreenPos;
        CursorSample {
            image,
            visible,
            position: Some((f64::from(x), f64::from(y))),
        }
    }
}

/// A bitmap's pixels as 32-bit BGRA rows, top first.
fn pixels(bitmap: HBITMAP) -> Option<(i32, i32, Vec<u8>)> {
    let mut b = BITMAP::default();
    // SAFETY: asks for the BITMAP of a valid bitmap handle.
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            size_of::<BITMAP>() as i32,
            Some(&mut b as *mut _ as *mut _),
        )
    };
    if got == 0 || b.bmWidth <= 0 || b.bmHeight <= 0 || b.bmWidth > MOST || b.bmHeight > MOST * 2 {
        return None;
    }
    let (w, h) = (b.bmWidth, b.bmHeight);
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut out = vec![0u8; (w * h * 4) as usize];
    // SAFETY: a memory DC for GetDIBits, released below; `out` holds w × h × 4 bytes.
    unsafe {
        let dc = CreateCompatibleDC(None);
        let lines = GetDIBits(
            dc,
            bitmap,
            0,
            h as u32,
            Some(out.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        (lines == h).then_some((w, h, out))
    }
}

fn image_of(cursor: HCURSOR) -> Option<CursorImage> {
    let mut icon = ICONINFO::default();
    // SAFETY: a valid cursor handle; the bitmaps it returns are ours to delete.
    unsafe { GetIconInfo(HICON(cursor.0), &mut icon) }.ok()?;
    let color = (!icon.hbmColor.is_invalid())
        .then(|| pixels(icon.hbmColor))
        .flatten();
    let mask = pixels(icon.hbmMask);
    // SAFETY: deleting the bitmaps GetIconInfo made for us.
    unsafe {
        if !icon.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon.hbmColor.0));
        }
        let _ = DeleteObject(HGDIOBJ(icon.hbmMask.0));
    }
    let (hot_x, hot_y) = (f64::from(icon.xHotspot), f64::from(icon.yHotspot));
    let rgba = match (color, mask) {
        // A colour cursor: its own alpha, or the mask's where it has none.
        (Some((w, h, bgra)), mask) => {
            let has_alpha = bgra.chunks(4).any(|p| p[3] != 0);
            let mut rgba = Vec::with_capacity(bgra.len());
            for (i, p) in bgra.chunks(4).enumerate() {
                let a = if has_alpha {
                    p[3]
                } else {
                    // AND mask: black (0) where the colour shows.
                    match &mask {
                        Some((_, _, m)) if m.get(i * 4).copied() == Some(0) => 255,
                        Some(_) => 0,
                        None => 255,
                    }
                };
                rgba.extend_from_slice(&[p[2], p[1], p[0], a]);
            }
            (w, h, rgba)
        }
        // Monochrome: the mask is twice as tall, AND above XOR.
        (None, Some((w, h2, m))) => {
            let h = h2 / 2;
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for i in 0..(w * h) as usize {
                let and = m[i * 4] != 0;
                let xor = m[(i + (w * h) as usize) * 4] != 0;
                rgba.extend_from_slice(match (and, xor) {
                    (false, false) => &[0, 0, 0, 255],
                    (false, true) => &[255, 255, 255, 255],
                    (true, false) => &[0, 0, 0, 0],
                    // Inverts the screen: shown as black, the usual stand-in.
                    (true, true) => &[0, 0, 0, 255],
                });
            }
            (w, h, rgba)
        }
        (None, None) => return None,
    };
    let (w, h, rgba) = rgba;
    Some(CursorImage {
        width: w as u32,
        height: h as u32,
        rgba,
        hot_x,
        hot_y,
        scale: 1.0,
    })
}
