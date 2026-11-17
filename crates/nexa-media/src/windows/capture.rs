//! Screen capture with DXGI desktop duplication: the display's latest frame as a
//! D3D11 texture. With a hardware encoder it stays on the GPU and becomes NV12 there
//! ([`GpuConverter`]); otherwise it is copied to memory and converted to NV12 at the
//! stream's size on the CPU.
//!
//! DXGI allows one duplication of a display per process: a second `DuplicateOutput`
//! fails with E_INVALIDARG. So every stream of a display (one per viewer, app or
//! browser) reads the same [`Duplication`], pumped by a [`Fanout`] that copies each new
//! desktop into a texture; each [`DxgiCapturer`] scales and converts that on its own, at
//! its own size, frame rate and cursor.
//!
//! Duplication leaves the cursor out; when the stream must carry it (`show_cursor`),
//! it is drawn in from [`WinCursorSource`].

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
                D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice,
                ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
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

use super::{
    Display, WinCursorSource,
    gpu::{GpuConverter, Kind, texture},
    power::DisplayAwake,
};
use crate::{
    CaptureConfig, CaptureEvent, CapturedFrame, Capturer, CursorSource, DisplayMode, MediaError,
    NativeImage, Result,
    convert::bgra_to_nv12,
    fanout::{Fanout, Feed, Source, Update},
};

fn os(what: &'static str) -> impl FnOnce(windows::core::Error) -> MediaError {
    move |e| MediaError::Os {
        what,
        status: e.code().0,
    }
}

/// Set once the GPU path failed in this process: later captures use the CPU path.
static GPU_FAILED: AtomicBool = AtomicBool::new(false);

/// The one duplication of each display that someone is watching.
static DUPLICATIONS: Fanout<Duplication> = Fanout::new();

/// How often the pump looks for a new desktop. It never waits inside `AcquireNextFrame`:
/// on a device the streams' hardware encoders share, a frame wait of a few milliseconds,
/// over and over, starves the encoder ("stopped asking for frames", seen on an NVIDIA
/// GPU). A look with no wait, then a short sleep, leaves it the device.
const POLL: Duration = Duration::from_millis(2);

/// A display's desktop duplication, run on its fanout's thread.
struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    /// The display's size, as duplicated.
    mode: (u32, u32),
    /// The last desktop, for every stream to read.
    latest: Option<Desktop>,
    /// Taken on the pump's thread, which Windows ties it to.
    awake: Option<DisplayAwake>,
}

// SAFETY: made on one thread, then moved to the pump's and used only there; D3D11
// devices are free-threaded.
unsafe impl Send for Duplication {}

/// A display's last desktop, in video memory on its duplication's device.
#[derive(Clone)]
struct Desktop {
    texture: ID3D11Texture2D,
    size: (u32, u32),
}

// SAFETY: D3D11 resources are free-threaded. Streams only copy from it, through the
// multithread-protected immediate context, which orders those copies with the pump's.
unsafe impl Send for Desktop {}

/// What the streams of a display share: the device its desktops are on, and its size.
struct Device {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    mode: (u32, u32),
}

// SAFETY: as for `Desktop`: the device is free-threaded and the context protected.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Duplication {
    /// Duplicates display `index` (`Display::id - 1`).
    fn open(index: u32) -> Result<Self> {
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
            let (mut device, mut context) = (None, None);
            // Video support for the GPU path; a device without it still duplicates.
            let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
            for attempt in 0..2 {
                let made = D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                );
                match made {
                    Ok(()) => break,
                    Err(_) if attempt == 0 => flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Err(e) => return Err(os("D3D11CreateDevice")(e)),
                }
            }
            let device: ID3D11Device = device.ok_or(MediaError::Other("no D3D11 device".into()))?;
            let context = context.ok_or(MediaError::Other("no D3D11 context".into()))?;
            // The pump, every stream of the display and their hardware encoders' own
            // threads all use this device.
            if let Ok(m) = device.cast::<ID3D11Multithread>() {
                let _ = m.SetMultithreadProtected(true);
            }
            let dup = output
                .DuplicateOutput(&device)
                .map_err(os("DuplicateOutput"))?;
            (device, context, dup)
        };
        // SAFETY: a valid duplication.
        let desc = unsafe { dup.GetDesc() };
        Ok(Self {
            device,
            context,
            dup,
            mode: (desc.ModeDesc.Width, desc.ModeDesc.Height),
            latest: None,
            awake: None,
        })
    }

    /// Copies a new desktop out of the duplication, for the streams to read.
    fn keep(&mut self, resource: IDXGIResource) -> Result<Desktop> {
        let frame: ID3D11Texture2D = resource.cast().map_err(os("ID3D11Texture2D"))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a valid texture.
        unsafe { frame.GetDesc(&mut desc) };
        let size = (desc.Width, desc.Height);
        let latest = match self.latest.take() {
            Some(d) if d.size == size => d,
            // The first desktop, or one of a new size: the streams see a mode change in it.
            _ => Desktop {
                texture: texture(&self.device, size.0, size.1, desc.Format, Kind::Target)?,
                size,
            },
        };
        // SAFETY: a copy between two textures of one size and format.
        unsafe { self.context.CopyResource(&latest.texture, &frame) };
        self.latest = Some(latest.clone());
        Ok(latest)
    }
}

impl Source for Duplication {
    type Picture = Desktop;
    type Info = Device;

    fn info(&self) -> Device {
        Device {
            device: self.device.clone(),
            context: self.context.clone(),
            mode: self.mode,
        }
    }

    fn acquire(&mut self, timeout: Duration) -> Update<Desktop> {
        // The display stays on for as long as anyone watches it.
        self.awake.get_or_insert_with(DisplayAwake::hold);
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: valid out pointers; the frame is released below on every path.
        match unsafe { self.dup.AcquireNextFrame(0, &mut info, &mut resource) } {
            Ok(()) => {}
            // Nothing new: look again soon (see `POLL` for why not wait in there).
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                thread::sleep(timeout.min(POLL));
                return Update::Idle;
            }
            // A mode change, a secure desktop (UAC, lock screen), a full-screen switch.
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Update::Lost,
            Err(e) => return Update::Failed(os("AcquireNextFrame")(e)),
        }
        let update = match resource {
            Some(r) if info.LastPresentTime != 0 || self.latest.is_none() => {
                self.keep(r).map_or_else(Update::Failed, Update::Picture)
            }
            // Only the pointer moved: the picture is the last one.
            _ => Update::Pointer,
        };
        // SAFETY: the frame acquired above.
        let _ = unsafe { self.dup.ReleaseFrame() };
        update
    }
}

/// One stream of a display: its share of the display's duplication, made into frames
/// at the stream's own size and rate.
pub struct DxgiCapturer {
    feed: Feed<Duplication>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
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
    /// The GPU path, when a hardware encoder will take its frames.
    gpu: Option<GpuConverter>,
    /// Whether a desktop has been received since the capture began.
    got_desktop: bool,
    /// The last frame's pixels, tight BGRA rows: re-sent with the cursor when only it moved.
    pixels: Vec<u8>,
    have_frame: bool,
}

// SAFETY: the D3D11 device and context are free-threaded and multithread-protected; the
// display's other streams and its pump share them, and this stream's own textures are
// used from one thread at a time (`next` takes `&mut self`).
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
        // The duplication another stream of this display runs, or a new one.
        let feed = DUPLICATIONS.subscribe(display.id, || Duplication::open(display.id - 1))?;
        let (device, context, mode) = {
            let shared = feed.info();
            (shared.device.clone(), shared.context.clone(), shared.mode)
        };
        // Its size now: its latest desktop's, when another stream already watches it.
        let source = feed.latest().map_or(mode, |d| d.size);
        let out = if config.width == 0 || config.height == 0 {
            source
        } else {
            (config.width, config.height)
        };
        let gpu = (super::hardware_h264() && !GPU_FAILED.load(Relaxed))
            .then(|| GpuConverter::new(&device, &context, source, out, config.fps).ok())
            .flatten();
        Ok(Self {
            feed,
            device,
            context,
            staging: None,
            source,
            origin: (display.rect.0, display.rect.1),
            out,
            interval: Duration::from_secs_f64(1.0 / f64::from(config.fps)),
            next_at: Instant::now(),
            show_cursor: config.show_cursor,
            cursor: WinCursorSource::new(),
            gpu,
            got_desktop: false,
            pixels: Vec::new(),
            have_frame: false,
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

    /// Keeps a new desktop on the GPU, or notes a new size.
    fn take(&mut self, desktop: &Desktop) -> Result<()> {
        let Some(gpu) = self.gpu.as_mut() else {
            return self.read(desktop);
        };
        if desktop.size == gpu.source() {
            gpu.take(&desktop.texture);
            self.got_desktop = true;
        }
        // A different size is a mode change: `next` reports it.
        self.source = desktop.size;
        Ok(())
    }

    /// Copies a new desktop's pixels into `self.pixels`.
    fn read(&mut self, desktop: &Desktop) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a valid texture.
        unsafe { desktop.texture.GetDesc(&mut desc) };
        let staging = self.staging(desc.Width, desc.Height, desc.Format)?;
        let (w, h) = (desc.Width as usize, desc.Height as usize);
        // SAFETY: a copy between textures of one size and format, then a read-only map
        // released before returning; rows are copied within `RowPitch`.
        unsafe {
            self.context.CopyResource(&staging, &desktop.texture);
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
        self.got_desktop = true;
        Ok(())
    }

    /// The last pixels, with the cursor if asked, as the stream's frame.
    fn frame(&mut self) -> CaptureEvent {
        let sample = (self.show_cursor && self.gpu.is_some()).then(|| self.cursor.sample());
        if let Some(gpu) = self.gpu.as_mut() {
            return match gpu.convert(sample.as_ref(), self.origin) {
                Ok(f) => CaptureEvent::Frame(CapturedFrame {
                    image: NativeImage::Texture(f),
                    captured_at: Instant::now(),
                    dirty: Vec::new(),
                }),
                // The capture is rebuilt, on the CPU path from then on.
                Err(_) => {
                    GPU_FAILED.store(true, Relaxed);
                    CaptureEvent::AccessLost
                }
            };
        }
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
        let desktop = match self.feed.next(timeout, self.show_cursor) {
            Update::Picture(d) => d,
            // Nothing new on screen, or only the pointer moved: a cursor drawn in still
            // needs a frame. (Streams that do not draw it are not woken for the pointer.)
            Update::Idle | Update::Pointer => {
                return Ok(if self.show_cursor && self.have_frame {
                    self.next_at = Instant::now() + self.interval;
                    self.frame()
                } else {
                    CaptureEvent::Idle
                });
            }
            Update::Lost => return Ok(CaptureEvent::AccessLost),
            Update::Failed(e) => return Err(e),
        };
        let before = self.source;
        self.take(&desktop)?;
        if !self.have_frame && !self.got_desktop {
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
