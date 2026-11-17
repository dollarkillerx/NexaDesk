//! The GPU path for a hardware encoder (docs/WINDOWS.md): the desktop stays in video
//! memory from duplication to encoder. A D3D11 video processor turns the BGRA desktop
//! into NV12 at the stream's size (BT.709 video range, scaled on the GPU); the encoder
//! reads that texture through the same device.
//!
//! When the stream must carry the cursor, only the cursor's own rectangle is read back,
//! blended on the CPU and written again: a few kilobytes, not a 4K frame.
//!
//! The viewer uses the same frame type the other way round: the decoder copies each
//! picture into one of its NV12 textures on [`viewer_device`], which the presenter draws.

use std::{
    mem::ManuallyDrop,
    sync::{Arc, OnceLock},
};

use windows::{
    Win32::Graphics::{
        Direct3D::D3D_DRIVER_TYPE_HARDWARE,
        Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VIDEO_ENCODER,
            D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
            D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
            D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
            D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
            D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
            D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice,
            ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
            ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
            ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
            ID3D11VideoProcessorOutputView,
        },
        Dxgi::Common::{
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL,
            DXGI_SAMPLE_DESC,
        },
    },
    core::{BOOL, Interface},
};

use crate::{CpuFrame, CursorSample, MediaError, Result};

fn os(what: &'static str) -> impl FnOnce(windows::core::Error) -> MediaError {
    move |e| MediaError::Os {
        what,
        status: e.code().0,
    }
}

/// An NV12 frame in video memory, and the device it belongs to. Cheap to clone.
#[derive(Clone)]
pub struct GpuFrame(Arc<GpuTexture>);

pub(crate) struct GpuTexture {
    pub texture: ID3D11Texture2D,
    pub device: ID3D11Device,
    pub width: u32,
    pub height: u32,
}

// SAFETY: D3D11 devices are free-threaded, and the capturer turns on multithread
// protection for the immediate context, which the encoder's device manager also uses.
unsafe impl Send for GpuTexture {}
unsafe impl Sync for GpuTexture {}

impl GpuFrame {
    pub(crate) fn from_texture(t: Arc<GpuTexture>) -> Self {
        Self(t)
    }
    pub fn width(&self) -> u32 {
        self.0.width
    }
    pub fn height(&self) -> u32 {
        self.0.height
    }
    pub(crate) fn texture(&self) -> &ID3D11Texture2D {
        &self.0.texture
    }
    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.0.device
    }

    /// The frame in memory, for the software encoder.
    pub fn read(&self) -> Result<CpuFrame> {
        let (w, h) = (self.0.width, self.0.height);
        // SAFETY: a staging copy of this texture, mapped read-only and unmapped before
        // returning; both planes are read within the mapped rows.
        unsafe {
            let device = &self.0.device;
            let context = device
                .GetImmediateContext()
                .map_err(os("GetImmediateContext"))?;
            let staging = texture(device, w, h, DXGI_FORMAT_NV12, Kind::Staging)?;
            context.CopyResource(&staging, &self.0.texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(os("Map NV12"))?;
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            let (w, h) = (w as usize, h as usize);
            let mut y = vec![0u8; w * h];
            let mut uv = vec![0u8; w * h / 2];
            for r in 0..h {
                y[r * w..(r + 1) * w]
                    .copy_from_slice(std::slice::from_raw_parts(base.add(r * pitch), w));
            }
            // The chroma plane follows the luma rows, at the same pitch.
            let chroma = base.add(pitch * h);
            for r in 0..h / 2 {
                uv[r * w..(r + 1) * w]
                    .copy_from_slice(std::slice::from_raw_parts(chroma.add(r * pitch), w));
            }
            context.Unmap(&staging, 0);
            Ok(CpuFrame {
                width: w as u32,
                height: h as u32,
                y,
                uv,
            })
        }
    }
}

pub(crate) enum Kind {
    /// A video processor's input or output, readable by the encoder.
    Target,
    /// A decoded picture the presenter's shader reads.
    Shader,
    Staging,
}

/// A device pointer kept in a static; D3D11 devices are free-threaded.
struct SharedDevice(ID3D11Device);
// SAFETY: see above; the immediate context is multithread-protected.
unsafe impl Send for SharedDevice {}
unsafe impl Sync for SharedDevice {}

/// The viewer's D3D11 device, shared by the decoder and the presenter so a decoded
/// picture never leaves the GPU. `None` without a hardware device with video support;
/// then each makes its own and pictures go through memory.
pub fn viewer_device() -> Option<ID3D11Device> {
    static DEVICE: OnceLock<Option<SharedDevice>> = OnceLock::new();
    DEVICE
        .get_or_init(|| {
            let mut device = None;
            // SAFETY: a hardware device on the default adapter; the out pointer is a local.
            let made = unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    Default::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )
            };
            let device: ID3D11Device = made.ok().and(device)?;
            // The decoder's thread copies while the render thread draws.
            let protected = device
                .cast::<ID3D11Multithread>()
                // SAFETY: a valid interface of this device.
                .map(|m| unsafe { m.SetMultithreadProtected(true) })
                .is_ok();
            protected.then_some(SharedDevice(device))
        })
        .as_ref()
        .map(|d| d.0.clone())
}

pub(crate) fn texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    kind: Kind,
) -> Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    if let Kind::Staging = kind {
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    } else if let Kind::Shader = kind {
        desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
    } else if format == DXGI_FORMAT_NV12 {
        // Some encoders want to bind it themselves; not every driver allows that.
        desc.BindFlags |= D3D11_BIND_VIDEO_ENCODER.0 as u32;
    }
    let mut made = None;
    // SAFETY: a valid description; the texture is written on success.
    let mut result = unsafe { device.CreateTexture2D(&desc, None, Some(&mut made)) };
    if result.is_err() && desc.BindFlags & D3D11_BIND_VIDEO_ENCODER.0 as u32 != 0 {
        desc.BindFlags &= !(D3D11_BIND_VIDEO_ENCODER.0 as u32);
        // SAFETY: as above.
        result = unsafe { device.CreateTexture2D(&desc, None, Some(&mut made)) };
    }
    result.map_err(os("CreateTexture2D"))?;
    made.ok_or(MediaError::Other("no texture".into()))
}

/// NV12 textures handed to the encoder take turns: one may still be in the encoder
/// while the next frame is made.
const POOL: usize = 4;

/// BGRA desktop to NV12 at the stream's size, with the cursor drawn in when asked.
pub(crate) struct GpuConverter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    /// Kept for as long as the processor and views made from it.
    _enumerator: ID3D11VideoProcessorEnumerator,
    /// The last desktop, as duplicated, and a copy that gets the cursor.
    desktop: ID3D11Texture2D,
    desktop_view: ID3D11VideoProcessorInputView,
    composed: ID3D11Texture2D,
    composed_view: ID3D11VideoProcessorInputView,
    pool: Vec<(Arc<GpuTexture>, ID3D11VideoProcessorOutputView)>,
    next: usize,
    source: (u32, u32),
}

impl GpuConverter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: (u32, u32),
        out: (u32, u32),
        fps: u32,
    ) -> Result<Self> {
        let out = (out.0.max(2) & !1, out.1.max(2) & !1);
        // SAFETY: video interfaces of a device made with video support; descriptions
        // are complete and every out pointer lives through its call.
        unsafe {
            let video_device: ID3D11VideoDevice = device.cast().map_err(os("ID3D11VideoDevice"))?;
            let video: ID3D11VideoContext = context.cast().map_err(os("ID3D11VideoContext"))?;
            let rate = DXGI_RATIONAL {
                Numerator: fps.max(1),
                Denominator: 1,
            };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: source.0,
                InputHeight: source.1,
                OutputFrameRate: rate,
                OutputWidth: out.0,
                OutputHeight: out.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&desc)
                .map_err(os("CreateVideoProcessorEnumerator"))?;
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(os("CreateVideoProcessor"))?;
            // Exactly the pixels: no driver sharpening or noise reduction.
            video.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            if let Ok(v1) = video.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                v1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
            } else {
                // Usage 0, RGB full range; YCbCr BT.709 (bit 2), 16–235 (nominal range 1).
                video.VideoProcessorSetStreamColorSpace(
                    &processor,
                    0,
                    &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
                );
                video.VideoProcessorSetOutputColorSpace(
                    &processor,
                    &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                        _bitfield: 0b01_0100,
                    },
                );
            }
            let input_view = |t: &ID3D11Texture2D| -> Result<ID3D11VideoProcessorInputView> {
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: 0,
                        },
                    },
                };
                let mut view = None;
                video_device
                    .CreateVideoProcessorInputView(t, &enumerator, &desc, Some(&mut view))
                    .map_err(os("CreateVideoProcessorInputView"))?;
                view.ok_or(MediaError::Other("no input view".into()))
            };
            let desktop = texture(
                device,
                source.0,
                source.1,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                Kind::Target,
            )?;
            let composed = texture(
                device,
                source.0,
                source.1,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                Kind::Target,
            )?;
            let (desktop_view, composed_view) = (input_view(&desktop)?, input_view(&composed)?);
            let mut pool = Vec::with_capacity(POOL);
            for _ in 0..POOL {
                let t = texture(device, out.0, out.1, DXGI_FORMAT_NV12, Kind::Target)?;
                let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut view = None;
                video_device
                    .CreateVideoProcessorOutputView(&t, &enumerator, &desc, Some(&mut view))
                    .map_err(os("CreateVideoProcessorOutputView"))?;
                let view = view.ok_or(MediaError::Other("no output view".into()))?;
                pool.push((
                    Arc::new(GpuTexture {
                        texture: t,
                        device: device.clone(),
                        width: out.0,
                        height: out.1,
                    }),
                    view,
                ));
            }
            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                video,
                processor,
                _enumerator: enumerator,
                desktop,
                desktop_view,
                composed,
                composed_view,
                pool,
                next: 0,
                source,
            })
        }
    }

    pub fn source(&self) -> (u32, u32) {
        self.source
    }

    /// Keeps the duplicated frame (same size and format) as the current desktop.
    pub fn take(&mut self, frame: &ID3D11Texture2D) {
        // SAFETY: a copy between two textures of one size and format.
        unsafe { self.context.CopyResource(&self.desktop, frame) };
    }

    /// The current desktop as an NV12 frame, with `cursor` drawn in when given.
    pub fn convert(
        &mut self,
        cursor: Option<&CursorSample>,
        origin: (i32, i32),
    ) -> Result<GpuFrame> {
        let input = match cursor {
            Some(c) if self.compose(c, origin).unwrap_or(false) => self.composed_view.clone(),
            _ => self.desktop_view.clone(),
        };
        let (target, view) = {
            let (t, v) = &self.pool[self.next];
            (t.clone(), v.clone())
        };
        self.next = (self.next + 1) % self.pool.len();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        // SAFETY: one enabled stream with a valid input view, into a pool texture's view.
        let blt = unsafe {
            self.video
                .VideoProcessorBlt(&self.processor, &view, 0, &streams)
        };
        // SAFETY: releasing the view reference the stream description held.
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        blt.map_err(os("VideoProcessorBlt"))?;
        Ok(GpuFrame(target))
    }

    /// Copies the desktop and blends the cursor into the copy. False: nothing to draw.
    fn compose(&mut self, sample: &CursorSample, origin: (i32, i32)) -> Result<bool> {
        let (Some(image), Some((x, y)), true) = (&sample.image, sample.position, sample.visible)
        else {
            return Ok(false);
        };
        let (sw, sh) = (self.source.0 as i32, self.source.1 as i32);
        let left = x as i32 - origin.0 - image.hot_x as i32;
        let top = y as i32 - origin.1 - image.hot_y as i32;
        let (x0, y0) = (left.max(0), top.max(0));
        let (x1, y1) = (
            (left + image.width as i32).min(sw),
            (top + image.height as i32).min(sh),
        );
        // SAFETY: a full copy of the desktop, then the cursor's box read through a small
        // staging texture, blended and written back to the copy, all within bounds.
        unsafe {
            self.context.CopyResource(&self.composed, &self.desktop);
            if x0 >= x1 || y0 >= y1 {
                return Ok(true);
            }
            let (bw, bh) = ((x1 - x0) as u32, (y1 - y0) as u32);
            let staging = texture(
                &self.device,
                bw,
                bh,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                Kind::Staging,
            )?;
            let area = D3D11_BOX {
                left: x0 as u32,
                top: y0 as u32,
                front: 0,
                right: x1 as u32,
                bottom: y1 as u32,
                back: 1,
            };
            self.context
                .CopySubresourceRegion(&staging, 0, 0, 0, 0, &self.desktop, 0, Some(&area));
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(os("Map cursor"))?;
            let pitch = mapped.RowPitch as usize;
            let mut pixels = vec![0u8; (bw * bh * 4) as usize];
            for r in 0..bh as usize {
                let row = std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(r * pitch),
                    bw as usize * 4,
                );
                pixels[r * bw as usize * 4..(r + 1) * bw as usize * 4].copy_from_slice(row);
            }
            self.context.Unmap(&staging, 0);
            for cy in 0..bh {
                for cx in 0..bw {
                    let (ix, iy) = ((x0 - left) as u32 + cx, (y0 - top) as u32 + cy);
                    let s = ((iy * image.width + ix) * 4) as usize;
                    let a = u32::from(image.rgba[s + 3]);
                    if a == 0 {
                        continue;
                    }
                    let d = ((cy * bw + cx) * 4) as usize;
                    for (c, from) in [(0, 2), (1, 1), (2, 0)] {
                        let under = u32::from(pixels[d + c]);
                        let over = u32::from(image.rgba[s + from]);
                        pixels[d + c] = ((over * a + under * (255 - a)) / 255) as u8;
                    }
                }
            }
            let dest = D3D11_BOX {
                left: x0 as u32,
                top: y0 as u32,
                front: 0,
                right: x1 as u32,
                bottom: y1 as u32,
                back: 1,
            };
            self.context.UpdateSubresource(
                &self.composed,
                0,
                Some(&dest),
                pixels.as_ptr().cast(),
                bw * 4,
                0,
            );
        }
        Ok(true)
    }
}
