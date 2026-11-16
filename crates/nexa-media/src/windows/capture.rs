//! Screen capture with DXGI desktop duplication: the display's latest frame as a
//! D3D11 texture, copied to memory and converted to NV12 at the stream's size.
//!
//! Duplication leaves the cursor out; when the stream must carry it (`show_cursor`),
//! it is drawn in from [`WinCursorSource`].

use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Texture2D,
            },
            Dxgi::Common::DXGI_SAMPLE_DESC,
            Dxgi::{
                CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_FRAME_INFO, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
                IDXGIResource,
            },
        },
    },
    core::Interface,
};

use super::{Display, WinCursorSource, power::DisplayAwake};
use crate::{
    CaptureConfig, CaptureEvent, CapturedFrame, Capturer, CursorSource, DisplayMode, MediaError,
    NativeImage, Result, convert::bgra_to_nv12,
};

fn os(what: &'static str) -> impl FnOnce(windows::core::Error) -> MediaError {
    move |e| MediaError::Os {
        what,
        status: e.code().0,
    }
}

pub struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// The display's size and place on the desktop.
    source: (u32, u32),
    origin: (i32, i32),
    /// The stream's size.
    out: (u32, u32),
    interval: Duration,
    next_at: Instant,
    show_cursor: bool,
    cursor: WinCursorSource,
    /// The last frame's pixels, tight BGRA rows: re-sent with the cursor when only it moved.
    pixels: Vec<u8>,
    have_frame: bool,
    _awake: DisplayAwake,
}

// SAFETY: the D3D11 device and context and the duplication are used from one thread at
// a time (`next` takes `&mut self`); D3D11 devices are free-threaded.
unsafe impl Send for DxgiCapturer {}

impl DxgiCapturer {
    /// `config.display` is a [`Display::id`], 0 meaning the primary display;
    /// `width`/`height` of 0 capture at the display's own size.
    pub fn new(config: CaptureConfig) -> Result<Self> {
        if config.fps == 0 {
            return Err(MediaError::Other("fps must be non-zero".into()));
        }
        let displays = super::displays().map_err(MediaError::Other)?;
        let display: &Display = match config.display {
            0 => displays.first(),
            id => displays.iter().find(|d| d.id == id),
        }
        .ok_or_else(|| MediaError::Other(format!("display {} not found", config.display)))?;
        let index = display.id - 1;
        // SAFETY: DXGI and D3D11 constructors with valid arguments; every out pointer
        // is a local that lives through the call.
        let (device, context, dup) = unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(os("CreateDXGIFactory1"))?;
            let adapter = factory
                .EnumAdapters1(index >> 8)
                .map_err(os("EnumAdapters1"))?;
            let output = adapter
                .EnumOutputs(index & 0xff)
                .map_err(os("EnumOutputs"))?;
            let output: IDXGIOutput1 = output.cast().map_err(os("IDXGIOutput1"))?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(os("D3D11CreateDevice"))?;
            let device: ID3D11Device = device.ok_or(MediaError::Other("no D3D11 device".into()))?;
            let context = context.ok_or(MediaError::Other("no D3D11 context".into()))?;
            let dup = output
                .DuplicateOutput(&device)
                .map_err(os("DuplicateOutput"))?;
            (device, context, dup)
        };
        // SAFETY: a valid duplication.
        let desc = unsafe { dup.GetDesc() };
        let source = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        let out = if config.width == 0 || config.height == 0 {
            source
        } else {
            (config.width, config.height)
        };
        Ok(Self {
            device,
            context,
            dup,
            staging: None,
            source,
            origin: (display.rect.0, display.rect.1),
            out,
            interval: Duration::from_secs_f64(1.0 / f64::from(config.fps)),
            next_at: Instant::now(),
            show_cursor: config.show_cursor,
            cursor: WinCursorSource::new(),
            pixels: Vec::new(),
            have_frame: false,
            _awake: DisplayAwake::hold(),
        })
    }

    fn staging(
        &mut self,
        w: u32,
        h: u32,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> Result<ID3D11Texture2D> {
        if let Some((t, sw, sh)) = &self.staging
            && (*sw, *sh) == (w, h)
        {
            return Ok(t.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: a valid description; the texture is written on success.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(os("CreateTexture2D"))?;
        let texture = texture.ok_or(MediaError::Other("no staging texture".into()))?;
        self.staging = Some((texture.clone(), w, h));
        Ok(texture)
    }

    /// Copies the acquired frame's pixels into `self.pixels`.
    fn read(&mut self, resource: IDXGIResource) -> Result<()> {
        let texture: ID3D11Texture2D = resource.cast().map_err(os("ID3D11Texture2D"))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a valid texture.
        unsafe { texture.GetDesc(&mut desc) };
        let staging = self.staging(desc.Width, desc.Height, desc.Format)?;
        let (w, h) = (desc.Width as usize, desc.Height as usize);
        // SAFETY: a copy between textures of one size and format, then a read-only map
        // released before returning; rows are copied within `RowPitch`.
        unsafe {
            self.context.CopyResource(&staging, &texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(os("Map"))?;
            self.pixels.resize(w * h * 4, 0);
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            for y in 0..h {
                let row = std::slice::from_raw_parts(base.add(y * pitch), w * 4);
                self.pixels[y * w * 4..(y + 1) * w * 4].copy_from_slice(row);
            }
            self.context.Unmap(&staging, 0);
        }
        self.source = (desc.Width, desc.Height);
        Ok(())
    }

    /// The last pixels, with the cursor if asked, as the stream's frame.
    fn frame(&mut self) -> CaptureEvent {
        let (w, h) = self.source;
        let mut composed;
        let pixels: &[u8] = if self.show_cursor {
            composed = self.pixels.clone();
            draw_cursor(&mut composed, w, h, self.origin, &self.cursor.sample());
            &composed
        } else {
            &self.pixels
        };
        let frame = bgra_to_nv12(pixels, w, h, w as usize * 4, self.out.0, self.out.1);
        CaptureEvent::Frame(CapturedFrame {
            image: NativeImage::Cpu(Arc::new(frame)),
            captured_at: Instant::now(),
            dirty: Vec::new(),
        })
    }
}

/// Blends the cursor over BGRA pixels of a display whose top-left is `origin`.
fn draw_cursor(
    pixels: &mut [u8],
    w: u32,
    h: u32,
    origin: (i32, i32),
    sample: &crate::CursorSample,
) {
    let (Some(image), Some((x, y)), true) = (&sample.image, sample.position, sample.visible) else {
        return;
    };
    let left = x as i32 - origin.0 - image.hot_x as i32;
    let top = y as i32 - origin.1 - image.hot_y as i32;
    for cy in 0..image.height as i32 {
        let py = top + cy;
        if py < 0 || py >= h as i32 {
            continue;
        }
        for cx in 0..image.width as i32 {
            let px = left + cx;
            if px < 0 || px >= w as i32 {
                continue;
            }
            let s = ((cy as u32 * image.width + cx as u32) * 4) as usize;
            let a = u32::from(image.rgba[s + 3]);
            if a == 0 {
                continue;
            }
            let d = ((py as u32 * w + px as u32) * 4) as usize;
            for (c, from) in [(0, 2), (1, 1), (2, 0)] {
                let under = u32::from(pixels[d + c]);
                let over = u32::from(image.rgba[s + from]);
                pixels[d + c] = ((over * a + under * (255 - a)) / 255) as u8;
            }
        }
    }
}

impl Capturer for DxgiCapturer {
    fn next(&mut self, timeout: Duration) -> Result<CaptureEvent> {
        // No faster than the stream's frame rate.
        let now = Instant::now();
        if self.next_at > now {
            thread::sleep((self.next_at - now).min(timeout));
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let wait = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        // SAFETY: valid out pointers; the frame is released below on every path.
        match unsafe { self.dup.AcquireNextFrame(wait, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                // Nothing new on screen; a moving cursor still needs a frame when drawn in.
                return Ok(if self.show_cursor && self.have_frame {
                    self.next_at = Instant::now() + self.interval;
                    self.frame()
                } else {
                    CaptureEvent::Idle
                });
            }
            // A mode change, a secure desktop (UAC, lock screen), a full-screen switch.
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Ok(CaptureEvent::AccessLost),
            Err(e) => return Err(os("AcquireNextFrame")(e)),
        }
        let before = self.source;
        let read = match resource {
            // Only the pointer moved: the picture is the last one.
            Some(_) if info.LastPresentTime == 0 && self.have_frame => Ok(()),
            Some(r) => self.read(r),
            None => Ok(()),
        };
        // SAFETY: the frame acquired above.
        let _ = unsafe { self.dup.ReleaseFrame() };
        read?;
        if !self.have_frame && self.pixels.is_empty() {
            return Ok(CaptureEvent::Idle);
        }
        self.have_frame = true;
        if self.source != before {
            return Ok(CaptureEvent::ModeChanged(DisplayMode {
                width: self.source.0,
                height: self.source.1,
                refresh_hz: 0,
            }));
        }
        self.next_at = Instant::now() + self.interval;
        Ok(self.frame())
    }
}
