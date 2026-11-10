//! macOS backends: ScreenCaptureKit, VideoToolbox, Metal.

pub mod bitstream;
mod capture;
mod caps;
mod decoder;
mod encoder;
mod opus;
mod audio_capture;
mod audio_output;
mod cursor;
mod present;

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};

use crate::{CpuFrame, MediaError, Result};

pub use capture::{ScreenCapturer, has_screen_capture_permission};
pub use caps::{cached_caps, probe_caps, system_key};
pub use decoder::{DecoderOutput, VtDecoder};
pub use encoder::VtEncoder;
pub use opus::{OpusDecoder, OpusEncoder};
pub use audio_capture::SystemAudioCapture;
pub use audio_output::{AudioOutputUnit, RenderFn};
pub use cursor::SystemCursorSource;
pub use present::{MetalPresenter, fit_viewport};

/// A retained CVPixelBuffer.
#[derive(Clone)]
pub struct PixelBuffer(CFRetained<CVPixelBuffer>);

// SAFETY: CoreVideo buffers are reference counted thread-safely, and the only
// mutation this type exposes takes the buffer's base-address lock.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

impl PixelBuffer {
    pub(crate) fn from_retained(pb: CFRetained<CVPixelBuffer>) -> Self {
        Self(pb)
    }

    /// # Safety
    /// `ptr` must be a valid CVPixelBuffer, which this retains.
    pub(crate) unsafe fn retain(ptr: NonNull<CVPixelBuffer>) -> Self {
        Self(unsafe { CFRetained::retain(ptr) })
    }

    pub(crate) fn raw(&self) -> &CVPixelBuffer {
        &self.0
    }

    pub fn width(&self) -> u32 {
        CVPixelBufferGetWidth(&self.0) as u32
    }

    pub fn height(&self) -> u32 {
        CVPixelBufferGetHeight(&self.0) as u32
    }

    /// The CoreVideo FourCC, e.g. `420v`.
    pub fn pixel_format(&self) -> u32 {
        CVPixelBufferGetPixelFormatType(&self.0)
    }

    /// Whether the planes can be read through `lock_base_address`. GPU-compressed
    /// buffers (the default decoder output on Apple Silicon) cannot, and touching
    /// their base addresses crashes.
    pub fn is_cpu_readable(&self) -> bool {
        let f = self.pixel_format();
        f == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            || f == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
    }

    /// Every `step`-th luma sample in both directions, or `None` if the buffer is
    /// not CPU readable.
    pub fn sample_luma(&self, step: usize) -> Option<Vec<u8>> {
        if !self.is_cpu_readable() || step == 0 {
            return None;
        }
        let lock = CVPixelBufferLockFlags::ReadOnly;
        // SAFETY: format checked above; plane 0 stays locked while it is read.
        unsafe {
            if CVPixelBufferLockBaseAddress(&self.0, lock) != 0 {
                return None;
            }
            let base = CVPixelBufferGetBaseAddressOfPlane(&self.0, 0) as *const u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&self.0, 0);
            let width = CVPixelBufferGetWidthOfPlane(&self.0, 0);
            let height = CVPixelBufferGetHeightOfPlane(&self.0, 0);
            let mut out = Vec::with_capacity(width / step * (height / step + 1));
            if !base.is_null() {
                for y in (0..height).step_by(step) {
                    for x in (0..width).step_by(step) {
                        out.push(*base.add(y * stride + x));
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&self.0, lock);
            (!out.is_empty()).then_some(out)
        }
    }

    /// Runs `f` with mutable access to both NV12 planes as
    /// `(luma, luma_stride, chroma, chroma_stride)`. For synthetic sources.
    pub fn with_planes_mut<R>(
        &self,
        f: impl FnOnce(&mut [u8], usize, &mut [u8], usize) -> R,
    ) -> Result<R> {
        if !self.is_cpu_readable() {
            return Err(MediaError::Unsupported("buffer is not CPU readable"));
        }
        let lock = CVPixelBufferLockFlags::empty();
        // SAFETY: format checked above, so plane base addresses and sizes are
        // valid while the buffer is locked, and the two planes do not overlap.
        unsafe {
            os("CVPixelBufferLockBaseAddress", CVPixelBufferLockBaseAddress(&self.0, lock))?;
            let plane = |i| -> (*mut u8, usize, usize) {
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&self.0, i);
                let base = CVPixelBufferGetBaseAddressOfPlane(&self.0, i) as *mut u8;
                (base, stride, stride * CVPixelBufferGetHeightOfPlane(&self.0, i))
            };
            let (y, y_stride, y_len) = plane(0);
            let (uv, uv_stride, uv_len) = plane(1);
            let ret = if y.is_null() || uv.is_null() {
                Err(MediaError::Other("pixel buffer has no CPU mapping".into()))
            } else {
                Ok(f(
                    std::slice::from_raw_parts_mut(y, y_len),
                    y_stride,
                    std::slice::from_raw_parts_mut(uv, uv_len),
                    uv_stride,
                ))
            };
            CVPixelBufferUnlockBaseAddress(&self.0, lock);
            ret
        }
    }
}

/// A pool of IOSurface-backed, Metal-compatible 420v buffers, for synthetic
/// sources and for uploading `NativeImage::Cpu` frames.
pub struct Nv12Pool {
    pool: CFRetained<CVPixelBufferPool>,
    width: u32,
    height: u32,
}

// SAFETY: CVPixelBufferPool is thread-safe.
unsafe impl Send for Nv12Pool {}
unsafe impl Sync for Nv12Pool {}

impl Nv12Pool {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(MediaError::Other("zero-sized pool".into()));
        }
        // SAFETY: the keys are CoreVideo-provided statics and every value is the
        // CF type those keys expect.
        unsafe {
            let attrs = cf_dict(&[
                (
                    kCVPixelBufferPixelFormatTypeKey,
                    &number_i64(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i64),
                ),
                (kCVPixelBufferWidthKey, &number_i64(width as i64)),
                (kCVPixelBufferHeightKey, &number_i64(height as i64)),
                (kCVPixelBufferIOSurfacePropertiesKey, cf_dict(&[]).as_ref()),
                (kCVPixelBufferMetalCompatibilityKey, CFBoolean::new(true).as_ref()),
            ]);
            let mut out = ptr::null_mut();
            os(
                "CVPixelBufferPoolCreate",
                CVPixelBufferPool::create(None, None, Some(&attrs), NonNull::from(&mut out)),
            )?;
            let pool = NonNull::new(out)
                .map(|p| CFRetained::from_raw(p))
                .ok_or(MediaError::Other("CVPixelBufferPoolCreate returned null".into()))?;
            Ok(Self { pool, width, height })
        }
    }

    pub fn get(&self) -> Result<PixelBuffer> {
        let mut out = ptr::null_mut();
        // SAFETY: `out` is a valid out-parameter for the created buffer.
        unsafe {
            os(
                "CVPixelBufferPoolCreatePixelBuffer",
                CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut out)),
            )?;
            NonNull::new(out)
                .map(|p| PixelBuffer::from_retained(CFRetained::from_raw(p)))
                .ok_or(MediaError::Other("pixel buffer pool returned null".into()))
        }
    }

    /// Copies a tightly packed CPU frame into a fresh buffer.
    pub fn upload(&self, frame: &CpuFrame) -> Result<PixelBuffer> {
        if (frame.width, frame.height) != (self.width, self.height) {
            return Err(MediaError::Other("CpuFrame size does not match the pool".into()));
        }
        let w = frame.width as usize;
        let h = frame.height as usize;
        let uv_rows = h.div_ceil(2);
        let expected_uv = w.div_ceil(2) * 2 * uv_rows;
        if w == 0 || h == 0 || frame.y.len() < w * h || frame.uv.len() < expected_uv {
            return Err(MediaError::Other("CpuFrame planes do not match its size".into()));
        }
        let pb = self.get()?;
        let (uv_width, frame_h) = (w.div_ceil(2) * 2, h);
        pb.with_planes_mut(|y, y_stride, uv, uv_stride| {
            for (dst, src) in y.chunks_exact_mut(y_stride).zip(frame.y.chunks_exact(w)).take(frame_h) {
                dst[..w.min(y_stride)].copy_from_slice(&src[..w.min(y_stride)]);
            }
            for (dst, src) in uv.chunks_exact_mut(uv_stride).zip(frame.uv.chunks_exact(uv_width)).take(uv_rows) {
                dst[..uv_width.min(uv_stride)].copy_from_slice(&src[..uv_width.min(uv_stride)]);
            }
        })?;
        Ok(pb)
    }
}

/// Converts an OS status into a `MediaError::Os`.
pub(crate) fn os(what: &'static str, status: i32) -> Result<()> {
    if status == 0 { Ok(()) } else { Err(MediaError::Os { what, status }) }
}

pub(crate) fn number_i64(v: i64) -> CFRetained<CFType> {
    // SAFETY: upcasting to CFType.
    unsafe { CFRetained::cast_unchecked(CFNumber::new_i64(v)) }
}

pub(crate) fn cf_dict(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary> {
    let keys: Vec<&CFString> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<&CFType> = pairs.iter().map(|p| p.1).collect();
    // SAFETY: only the key and value type parameters are erased.
    unsafe { CFRetained::cast_unchecked(CFDictionary::<CFString, CFType>::from_slices(&keys, &values)) }
}

/// Looks up `key` in the attachments of the first sample. Returns null when
/// absent. `key` must be a pointer to a CFString-compatible object.
///
/// # Safety
/// The returned pointer is only valid for as long as `sample`.
pub(crate) unsafe fn sample_attachment(sample: &CMSampleBuffer, key: *const c_void) -> *const c_void {
    // SAFETY: the array, when present, holds one dictionary per sample.
    unsafe {
        let Some(array) = sample.sample_attachments_array(false) else { return ptr::null() };
        if array.is_empty() {
            return ptr::null();
        }
        let dict = array.value_at_index(0) as *const CFDictionary;
        if dict.is_null() { ptr::null() } else { (*dict).value(key) }
    }
}
