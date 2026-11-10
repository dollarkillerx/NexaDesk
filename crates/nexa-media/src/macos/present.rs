//! Metal presenter: NV12 from CVMetalTextureCache to a CAMetalLayer.
//!
//! Attach on the UI thread, then present from a render thread. S1 confirmed this
//! keeps running while the UI thread is blocked.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2::MainThreadMarker;
use objc2_app_kit::NSView;
use objc2_core_foundation::{CFRetained, CGSize};
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
use objc2_core_video::{
    CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetWidthOfPlane,
};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLDrawable, MTLLibrary, MTLLoadAction, MTLOrigin, MTLPixelFormat, MTLPrimitiveType, MTLRegion,
    MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLSize,
    MTLStorageMode, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage, MTLViewport,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer, CATransaction};

use super::{PixelBuffer, os};
use crate::{DecodedFrame, MediaError, NativeImage, PresentMode, PresentStat, Presenter, Result};

const SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;
struct V { float4 p [[position]]; float2 uv; };
vertex V vs(uint id [[vertex_id]]) {
    float2 uv = float2((id << 1) & 2, id & 2);
    V o; o.p = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1); o.uv = uv; return o;
}
fragment float4 fs(V in [[stage_in]], texture2d<float> y [[texture(0)]], texture2d<float> c [[texture(1)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float Y = y.sample(s, in.uv).r - 16.0 / 255.0;
    float2 C = c.sample(s, in.uv).rg - float2(128.0 / 255.0);
    // BT.709 limited range.
    float3 rgb = float3(1.164384 * Y + 1.792741 * C.y,
                        1.164384 * Y - 0.213249 * C.x - 0.532909 * C.y,
                        1.164384 * Y + 2.112402 * C.x);
    return float4(saturate(rgb), 1);
}
"#;

const PIXEL_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;
/// §6.6: at most this many frames between us and the display.
const MAX_IN_FLIGHT: u32 = 2;
const IN_FLIGHT_WAIT: Duration = Duration::from_millis(100);

/// Largest centered rectangle with the frame's aspect ratio that fits the
/// drawable, as `(x, y, w, h)` in drawable pixels.
pub fn fit_viewport(drawable: (u32, u32), frame: (u32, u32)) -> (f64, f64, f64, f64) {
    let (dw, dh) = (f64::from(drawable.0.max(1)), f64::from(drawable.1.max(1)));
    let (fw, fh) = (f64::from(frame.0.max(1)), f64::from(frame.1.max(1)));
    let scale = (dw / fw).min(dh / fh);
    let (w, h) = ((fw * scale).round().min(dw), (fh * scale).round().min(dh));
    (((dw - w) / 2.0).round(), ((dh - h) / 2.0).round(), w, h)
}

/// Counts submitted frames not yet finished by the GPU.
#[derive(Default)]
struct InFlight {
    count: Mutex<u32>,
    done: Condvar,
}

impl InFlight {
    /// Takes a slot, waiting up to `timeout` for one to free up.
    fn acquire(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        while *count >= MAX_IN_FLIGHT {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, _) = self.done.wait_timeout(count, deadline - now).unwrap_or_else(|e| e.into_inner());
            count = next;
        }
        *count += 1;
        true
    }

    fn release(&self) {
        *self.count.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.done.notify_one();
    }
}

type Texture = Retained<ProtocolObject<dyn MTLTexture>>;

/// A frame's plane textures, which must outlive the GPU's use of them.
struct BoundFrame {
    _y: CFRetained<CVMetalTexture>,
    _uv: CFRetained<CVMetalTexture>,
    y_texture: Texture,
    uv_texture: Texture,
    _image: PixelBuffer,
}

pub struct MetalPresenter {
    layer: Retained<CAMetalLayer>,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    cache: CFRetained<CVMetalTextureCache>,
    in_flight: Arc<InFlight>,
    offscreen_target: Option<(u32, u32, Texture)>,
}

// SAFETY: after attach only the render thread touches the layer's drawables
// and the Metal objects; S1 verified this model, and Metal is thread-safe here.
unsafe impl Send for MetalPresenter {}

impl MetalPresenter {
    /// Attaches to an `NSView`, replacing its layer. Must run on the main thread.
    ///
    /// # Safety
    /// `view` must be a valid `NSView*`, and must outlive the presenter.
    pub unsafe fn attach(view: *mut c_void, width_px: u32, height_px: u32, scale: f32, mode: PresentMode) -> Result<Self> {
        if MainThreadMarker::new().is_none() {
            return Err(MediaError::Other("MetalPresenter::attach must run on the main thread".into()));
        }
        let view = NonNull::new(view as *mut NSView).ok_or(MediaError::Other("null NSView".into()))?;
        let presenter = Self::create(width_px, height_px, scale, mode)?;
        // SAFETY: the caller guarantees `view` is a live NSView.
        unsafe {
            let view = view.as_ref();
            view.setWantsLayer(true);
            view.setLayer(Some(&presenter.layer));
        }
        Ok(presenter)
    }

    /// A presenter whose layer is in no view, so nothing appears on screen.
    pub fn offscreen(width_px: u32, height_px: u32) -> Result<Self> {
        Self::create(width_px, height_px, 1.0, PresentMode::Immediate)
    }

    fn create(width_px: u32, height_px: u32, scale: f32, mode: PresentMode) -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().ok_or(MediaError::Unsupported("no Metal device"))?;
        // SAFETY: documented setters with valid values on fresh objects; the
        // layer is not yet visible to anything else.
        unsafe {
            let layer = CAMetalLayer::new();
            layer.setDevice(Some(&device));
            layer.setPixelFormat(PIXEL_FORMAT);
            layer.setFramebufferOnly(true);
            layer.setMaximumDrawableCount(2);
            layer.setDisplaySyncEnabled(mode == PresentMode::Vsync);
            layer.setContentsScale(f64::from(scale));
            layer.setDrawableSize(CGSize::new(f64::from(width_px.max(1)), f64::from(height_px.max(1))));
            if let Some(space) = CGColorSpace::with_name(Some(kCGColorSpaceSRGB)) {
                layer.setColorspace(Some(&space));
            }

            let queue = device.newCommandQueue().ok_or(MediaError::Other("no command queue".into()))?;
            let library = device
                .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
                .map_err(|e| MediaError::Other(format!("shader compile: {e:?}")))?;
            let descriptor = MTLRenderPipelineDescriptor::new();
            descriptor.setVertexFunction(library.newFunctionWithName(&NSString::from_str("vs")).as_deref());
            descriptor.setFragmentFunction(library.newFunctionWithName(&NSString::from_str("fs")).as_deref());
            descriptor.colorAttachments().objectAtIndexedSubscript(0).setPixelFormat(PIXEL_FORMAT);
            let pipeline = device
                .newRenderPipelineStateWithDescriptor_error(&descriptor)
                .map_err(|e| MediaError::Other(format!("pipeline: {e:?}")))?;

            let mut raw = ptr::null_mut();
            os(
                "CVMetalTextureCacheCreate",
                CVMetalTextureCache::create(None, None, &device, None, NonNull::from(&mut raw)),
            )?;
            let cache = NonNull::new(raw)
                .map(|c| CFRetained::from_raw(c))
                .ok_or(MediaError::Other("texture cache is null".into()))?;

            Ok(Self {
                layer,
                device,
                queue,
                pipeline,
                cache,
                in_flight: Arc::new(InFlight::default()),
                offscreen_target: None,
            })
        }
    }

    fn plane_texture(&self, image: &PixelBuffer, plane: usize, format: MTLPixelFormat) -> Result<(CFRetained<CVMetalTexture>, Texture)> {
        let pb = image.raw();
        let mut raw = ptr::null_mut();
        // SAFETY: `pb` is a valid image buffer and the out-pointer is valid. The
        // returned texture references the IOSurface until it is released.
        unsafe {
            os(
                "CVMetalTextureCacheCreateTextureFromImage",
                CVMetalTextureCache::create_texture_from_image(
                    None,
                    &self.cache,
                    pb,
                    None,
                    format,
                    CVPixelBufferGetWidthOfPlane(pb, plane),
                    CVPixelBufferGetHeightOfPlane(pb, plane),
                    plane,
                    NonNull::from(&mut raw),
                ),
            )?;
            let cv = NonNull::new(raw)
                .map(|t| CFRetained::from_raw(t))
                .ok_or(MediaError::Other("texture is null".into()))?;
            let texture = CVMetalTextureGetTexture(&cv).ok_or(MediaError::Other("CVMetalTexture has no texture".into()))?;
            Ok((cv, texture))
        }
    }

    fn bind(&self, frame: &DecodedFrame) -> Result<BoundFrame> {
        let image = match &frame.image {
            NativeImage::PixelBuffer(pb) => pb.clone(),
            NativeImage::Cpu(_) => return Err(MediaError::Unsupported("MetalPresenter takes pixel buffers only")),
        };
        let (y, y_texture) = self.plane_texture(&image, 0, MTLPixelFormat::R8Unorm)?;
        let (uv, uv_texture) = self.plane_texture(&image, 1, MTLPixelFormat::RG8Unorm)?;
        Ok(BoundFrame { _y: y, _uv: uv, y_texture, uv_texture, _image: image })
    }

    /// Records the NV12 draw into `cb`, letterboxed into `target`.
    fn draw(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        target: &ProtocolObject<dyn MTLTexture>,
        bound: &BoundFrame,
    ) -> Result<()> {
        // SAFETY: the textures and pipeline are valid and compatible with the
        // descriptor; the shader's bindings match the indices set here.
        unsafe {
            let pass = MTLRenderPassDescriptor::new();
            let attachment = pass.colorAttachments().objectAtIndexedSubscript(0);
            attachment.setTexture(Some(target));
            attachment.setLoadAction(MTLLoadAction::Clear);
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 1.0 });

            let viewport = fit_viewport(
                (target.width() as u32, target.height() as u32),
                (bound.y_texture.width() as u32, bound.y_texture.height() as u32),
            );
            let encoder = cb.renderCommandEncoderWithDescriptor(&pass).ok_or(MediaError::Other("no render encoder".into()))?;
            encoder.setRenderPipelineState(&self.pipeline);
            encoder.setViewport(MTLViewport {
                originX: viewport.0,
                originY: viewport.1,
                width: viewport.2,
                height: viewport.3,
                znear: 0.0,
                zfar: 1.0,
            });
            encoder.setFragmentTexture_atIndex(Some(&bound.y_texture), 0);
            encoder.setFragmentTexture_atIndex(Some(&bound.uv_texture), 1);
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
            encoder.endEncoding();
        }
        Ok(())
    }

    fn offscreen_target(&mut self, width: u32, height: u32) -> Result<Texture> {
        if let Some((w, h, t)) = &self.offscreen_target
            && (*w, *h) == (width, height)
        {
            return Ok(t.clone());
        }
        // SAFETY: a shared, render-target texture of this size is valid to create.
        let target = unsafe {
            let descriptor = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                PIXEL_FORMAT,
                width as usize,
                height as usize,
                false,
            );
            descriptor.setUsage(MTLTextureUsage::RenderTarget);
            descriptor.setStorageMode(MTLStorageMode::Shared);
            self.device.newTextureWithDescriptor(&descriptor).ok_or(MediaError::Other("no offscreen texture".into()))?
        };
        self.offscreen_target = Some((width, height, target.clone()));
        Ok(target)
    }

    /// Binds and draws a frame into an offscreen target and waits for the GPU,
    /// without touching the layer. Used to measure the binding path.
    pub fn render_offscreen(&mut self, frame: &DecodedFrame, width: u32, height: u32) -> Result<()> {
        let target = self.offscreen_target(width, height)?;
        autoreleasepool(|_| {
            let bound = self.bind(frame)?;
            let cb = self.queue.commandBuffer().ok_or(MediaError::Other("no command buffer".into()))?;
            self.draw(&cb, &target, &bound)?;
            cb.commit();
            // The GPU finishes here, before `bound` drops.
            cb.waitUntilCompleted();
            Ok(())
        })
    }

    /// Renders a frame offscreen and returns the BGRA pixels, for verifying that
    /// GPU-compressed buffers reach the shader intact.
    pub fn render_to_bgra(&mut self, frame: &DecodedFrame, width: u32, height: u32) -> Result<Vec<u8>> {
        self.render_offscreen(frame, width, height)?;
        let target = self.offscreen_target(width, height)?;
        let stride = width as usize * 4;
        let mut bytes = vec![0u8; stride * height as usize];
        let dest = NonNull::new(bytes.as_mut_ptr().cast()).ok_or(MediaError::Other("zero-sized render".into()))?;
        // SAFETY: the GPU has finished and `bytes` is exactly the size of the region.
        unsafe {
            target.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                dest,
                stride,
                MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize { width: width as usize, height: height as usize, depth: 1 },
                },
                0,
            );
        }
        Ok(bytes)
    }
}

impl Presenter for MetalPresenter {
    fn resize(&mut self, width_px: u32, height_px: u32, scale: f32) -> Result<()> {
        // The explicit transaction keeps the change atomic and lets it commit
        // from a render thread with no run loop.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.layer.setContentsScale(f64::from(scale));
        self.layer.setDrawableSize(CGSize::new(f64::from(width_px.max(1)), f64::from(height_px.max(1))));
        CATransaction::commit();
        Ok(())
    }

    fn set_mode(&mut self, mode: PresentMode) -> Result<()> {
        self.layer.setDisplaySyncEnabled(mode == PresentMode::Vsync);
        Ok(())
    }

    fn present(&mut self, frame: &DecodedFrame) -> Result<PresentStat> {
        let started = Instant::now();
        autoreleasepool(|_| {
            let bound = self.bind(frame)?;
            if !self.in_flight.acquire(IN_FLIGHT_WAIT) {
                return Err(MediaError::Other("GPU has not finished recent frames".into()));
            }
            // SAFETY: drawable, queue and textures are valid; the textures and image
            // live in the completion block until the GPU is done with them.
            let submitted: Result<()> = unsafe {
                let Some(drawable) = self.layer.nextDrawable() else {
                    self.in_flight.release();
                    return Err(MediaError::Other("no drawable".into()));
                };
                let Some(cb) = self.queue.commandBuffer() else {
                    self.in_flight.release();
                    return Err(MediaError::Other("no command buffer".into()));
                };
                if let Err(e) = self.draw(&cb, &drawable.texture(), &bound) {
                    self.in_flight.release();
                    return Err(e);
                }

                let in_flight = self.in_flight.clone();
                let handler = RcBlock::new(move |_cb: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                    // Mentions the whole struct so the closure owns all of it.
                    let _ = &bound;
                    in_flight.release();
                });
                cb.addCompletedHandler(RcBlock::as_ptr(&handler).cast());
                let as_drawable: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&*drawable);
                cb.presentDrawable(as_drawable);
                cb.commit();
                let _: &ProtocolObject<dyn CAMetalDrawable> = &drawable;
                Ok(())
            };
            submitted?;
            self.cache.flush(0);
            Ok(PresentStat { present_call_us: started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32 })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fit_viewport;

    #[test]
    fn exact_fit_uses_whole_drawable() {
        assert_eq!(fit_viewport((1920, 1080), (1920, 1080)), (0.0, 0.0, 1920.0, 1080.0));
    }

    #[test]
    fn wide_drawable_pillarboxes() {
        // 16:9 video in a 21:9-ish drawable: full height, centered horizontally.
        assert_eq!(fit_viewport((2560, 1080), (1920, 1080)), (320.0, 0.0, 1920.0, 1080.0));
    }

    #[test]
    fn tall_drawable_letterboxes() {
        assert_eq!(fit_viewport((1920, 1600), (1920, 1080)), (0.0, 260.0, 1920.0, 1080.0));
    }

    #[test]
    fn scales_up_and_down() {
        assert_eq!(fit_viewport((960, 540), (1920, 1080)), (0.0, 0.0, 960.0, 540.0));
        assert_eq!(fit_viewport((3840, 2160), (1920, 1080)), (0.0, 0.0, 3840.0, 2160.0));
    }

    #[test]
    fn degenerate_sizes_do_not_divide_by_zero() {
        let (x, y, w, h) = fit_viewport((0, 0), (0, 0));
        assert!(x.is_finite() && y.is_finite() && w > 0.0 && h > 0.0);
    }
}
