//! One picture of a display on demand, for an AI agent (docs/MCP.md §13, G2): GDI
//! copies the desktop's rectangle, scaled to the size asked for, and draws the cursor
//! in, as the macOS screenshot has it. One-off pictures need none of the duplication
//! that video keeps running.

use std::ffi::c_void;

use windows::Win32::{
    Foundation::POINT,
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HALFTONE, HGDIOBJ, ReleaseDC, SRCCOPY,
        SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt,
    },
    UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo,
        GetSystemMetrics, HICON, ICONINFO, SM_CXCURSOR, SM_CYCURSOR,
    },
};

use crate::{MediaError, Result};

/// The desktop's rectangle `rect` (physical pixels: x, y, width, height) at
/// `width`×`height` pixels, the cursor drawn in: straight RGBA, top row first.
pub fn screenshot_rgba(
    rect: (i32, i32, u32, u32),
    width: u32,
    height: u32,
) -> Result<(u32, u32, Vec<u8>)> {
    let (w, h) = (width.max(1) as i32, height.max(1) as i32);
    let (sx, sy, sw, sh) = (rect.0, rect.1, rect.2.max(1) as i32, rect.3.max(1) as i32);
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            // Negative: top row first.
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: plain GDI calls on objects made here and released before returning; the
    // section's bits are `w`×`h` 32-bit pixels while it is alive.
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err(MediaError::Other("no desktop to copy".into()));
        }
        let memory = CreateCompatibleDC(Some(screen));
        let mut bits: *mut c_void = std::ptr::null_mut();
        let section = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0);
        let result = match section {
            Ok(section) if !bits.is_null() => {
                let old = SelectObject(memory, HGDIOBJ(section.0));
                SetStretchBltMode(memory, HALFTONE);
                let _ = SetBrushOrgEx(memory, 0, 0, None);
                let copied = StretchBlt(
                    memory,
                    0,
                    0,
                    w,
                    h,
                    Some(screen),
                    sx,
                    sy,
                    sw,
                    sh,
                    SRCCOPY | CAPTUREBLT,
                );
                if copied.as_bool() {
                    draw_cursor(
                        memory,
                        (sx, sy),
                        (f64::from(w) / f64::from(sw), f64::from(h) / f64::from(sh)),
                    );
                }
                let pixels = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
                let mut rgba = Vec::with_capacity(pixels.len());
                for p in pixels.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
                }
                SelectObject(memory, old);
                let _ = DeleteObject(HGDIOBJ(section.0));
                if copied.as_bool() {
                    Ok((w as u32, h as u32, rgba))
                } else {
                    Err(MediaError::Other("could not copy the desktop".into()))
                }
            }
            Ok(section) => {
                let _ = DeleteObject(HGDIOBJ(section.0));
                Err(MediaError::Other("no memory for the screenshot".into()))
            }
            Err(e) => Err(MediaError::Other(format!(
                "no memory for the screenshot: {e}"
            ))),
        };
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        result
    }
}

/// The cursor as it is now, scaled as the picture is, onto `dc`, whose top left is
/// the desktop's `origin`.
///
/// # Safety
/// `dc` is a valid memory DC with a bitmap selected.
unsafe fn draw_cursor(
    dc: windows::Win32::Graphics::Gdi::HDC,
    origin: (i32, i32),
    scale: (f64, f64),
) {
    let mut cursor = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: as the caller promises; the icon's bitmaps are deleted after use.
    unsafe {
        if GetCursorInfo(&mut cursor).is_err() || cursor.flags.0 & CURSOR_SHOWING.0 == 0 {
            return;
        }
        let icon = HICON(cursor.hCursor.0);
        let mut info = ICONINFO::default();
        let hotspot = if GetIconInfo(icon, &mut info).is_ok() {
            if !info.hbmMask.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
            }
            if !info.hbmColor.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
            }
            (info.xHotspot as i32, info.yHotspot as i32)
        } else {
            (0, 0)
        };
        let POINT { x, y } = cursor.ptScreenPos;
        let size = (
            (f64::from(GetSystemMetrics(SM_CXCURSOR)) * scale.0)
                .round()
                .max(8.0) as i32,
            (f64::from(GetSystemMetrics(SM_CYCURSOR)) * scale.1)
                .round()
                .max(8.0) as i32,
        );
        let at = (
            (f64::from(x - origin.0 - hotspot.0) * scale.0).round() as i32,
            (f64::from(y - origin.1 - hotspot.1) * scale.1).round() as i32,
        );
        let _ = DrawIconEx(dc, at.0, at.1, icon, size.0, size.1, 0, None, DI_NORMAL);
    }
}
