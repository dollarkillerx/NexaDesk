//! Windows backends (docs/WINDOWS.md): DXGI desktop duplication, Media Foundation's
//! H.264 encoder and decoder, D3D11 presentation, the system cursor, system audio
//! (WASAPI and libopus), and keeping the display awake.
//!
//! Every size and position here is in physical pixels: the agent declares itself
//! per-monitor DPI aware, so the desktop, the cursor and injected input all use them.

mod audio;
mod capture;
mod cursor;
mod decoder;
mod desktop;
mod encoder;
mod gpu;
mod hwenc;
mod power;
mod present;
mod screenshot;

use nexa_wire::control::{Caps, Chroma, Codec, CodecCap};
use windows::{
    Win32::{
        Graphics::{
            Dxgi::{CreateDXGIFactory1, IDXGIFactory1},
            Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW},
        },
        UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        },
    },
    core::PCWSTR,
};

pub use audio::{OpusDecoder, OpusEncoder, Render as AudioRender, WasapiLoopback, WasapiOutput};
pub use capture::DxgiCapturer;
pub use cursor::WinCursorSource;
pub use decoder::MfDecoder;
pub use desktop::follow_input_desktop;
pub use encoder::MfEncoder;
pub use gpu::{GpuFrame, viewer_device};
pub use hwenc::{HwEncoder, WinEncoder, encoder, hardware_h264};
pub use power::{DisplayAwake, wake_display};
pub use present::D3dPresenter;
pub use screenshot::screenshot_rgba;

/// A monitor on the desktop.
#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    /// `(adapter << 8 | output) + 1`, so 0 can mean "the main display".
    pub id: u32,
    /// Its place on the virtual desktop: x, y, width, height.
    pub rect: (i32, i32, u32, u32),
    pub refresh_hz: u32,
    pub name: String,
    /// The one at the desktop's origin.
    pub primary: bool,
}

/// The monitors DXGI can duplicate, the primary one first.
pub fn displays() -> Result<Vec<Display>, String> {
    // SAFETY: a plain factory constructor.
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| format!("DXGI: {e}"))?;
    let mut found = Vec::new();
    for a in 0..16u32 {
        // SAFETY: enumerating until the factory says there are no more.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else {
            break;
        };
        for o in 0..16u32 {
            // SAFETY: as above, per adapter.
            let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else {
                break;
            };
            // SAFETY: a valid output.
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let r = desc.DesktopCoordinates;
            let name_end = desc
                .DeviceName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.DeviceName.len());
            let device = String::from_utf16_lossy(&desc.DeviceName[..name_end]);
            found.push(Display {
                id: ((a << 8) | o) + 1,
                rect: (
                    r.left,
                    r.top,
                    (r.right - r.left).max(0) as u32,
                    (r.bottom - r.top).max(0) as u32,
                ),
                refresh_hz: refresh_of(&desc.DeviceName),
                name: device.trim_start_matches(r"\\.\").to_owned(),
                primary: r.left == 0 && r.top == 0,
            });
        }
    }
    if found.is_empty() {
        return Err("no display to capture".into());
    }
    found.sort_by_key(|d| !d.primary);
    Ok(found)
}

fn refresh_of(device: &[u16; 32]) -> u32 {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: a NUL-terminated device name and a sized DEVMODEW.
    let ok =
        unsafe { EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };
    if ok.as_bool() && mode.dmDisplayFrequency > 1 {
        mode.dmDisplayFrequency
    } else {
        60
    }
}

/// The whole virtual desktop: x, y, width, height, in pixels.
pub fn virtual_desktop() -> (i32, i32, i32, i32) {
    // SAFETY: plain metric queries.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

fn h264(max_w: u32, max_h: u32) -> CodecCap {
    CodecCap {
        codec: Codec::H264 as i32,
        chroma: Chroma::Yuv420 as i32,
        max_w,
        max_h,
    }
}

/// What this computer sends as a host: H.264 4:2:0 through Media Foundation. With the
/// GPU's encoder (tried once, [`hardware_h264`]) up to 4096×2304 at 120 fps; with the
/// software one up to 1080p at 60. Decoded up to 4096×2304.
pub fn caps() -> Caps {
    let (max_w, max_h, max_fps) = if hardware_h264() {
        (4096, 2304, 120)
    } else {
        (1920, 1080, 60)
    };
    Caps {
        encode: vec![h264(max_w, max_h)],
        decode: vec![h264(4096, 2304)],
        rfi: false,
        ltr: false,
        max_fps,
    }
}

/// What this computer shows as a viewer: H.264 up to 4096×2304 (a Retina Mac's screen
/// included), at up to 120 fps when it decodes on the GPU. It never touches the
/// encoder: a viewer has no use for one, and trying it costs time and risk.
pub fn viewer_caps() -> Caps {
    Caps {
        encode: vec![h264(1920, 1080)],
        decode: vec![h264(4096, 2304)],
        rfi: false,
        ltr: false,
        max_fps: if viewer_device().is_some() { 120 } else { 60 },
    }
}
