//! The host cursor (§6.9): shape from `NSCursor.currentSystemCursor`, visibility from
//! `CGCursorIsVisible`, position from a null `CGEvent`. Read only: nothing here
//! moves, hides or changes the cursor.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use objc2::rc::autoreleasepool;
use objc2_app_kit::NSCursor;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
#[allow(deprecated)]
use objc2_core_graphics::{
    CGColorSpace, CGContext, CGCursorIsVisible, CGEvent, CGImage,
    kCGColorSpaceSRGB,
};

use crate::{CursorImage, CursorSample, CursorSource};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *const CGColorSpace,
        bitmap_info: u32,
    ) -> *mut CGContext;
}

/// `kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big`: R, G, B, A in memory.
const RGBA_PREMULTIPLIED: u32 = 1 | (4 << 12);
const MAX_SIDE: usize = 512;

/// Draws `image` into straight RGBA.
fn rgba_of(image: &CGImage) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    let mut pixels = vec![0u8; w * h * 4];
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    // SAFETY: `pixels` is w*h*4 bytes, outlives the context, and matches the format.
    let context = unsafe {
        let raw = CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            w,
            h,
            8,
            w * 4,
            CFRetained::as_ptr(&space).as_ptr(),
            RGBA_PREMULTIPLIED,
        );
        CFRetained::from_raw(NonNull::new(raw)?)
    };
    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    CGContext::draw_image(Some(&context), rect, Some(image));
    drop(context);
    for px in pixels.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Some((w as u32, h as u32, pixels))
}

#[allow(deprecated)] // currentSystemCursor: the only public way to read another app's cursor.
fn current_shape() -> Option<Arc<CursorImage>> {
    let cursor = NSCursor::currentSystemCursor()?;
    let image = cursor.image();
    let size = image.size();
    let hot = cursor.hotSpot();
    if size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    // SAFETY: a null rect, context and hints ask for the best representation.
    let cg = unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }?;
    let (width, height, rgba) = rgba_of(&cg)?;
    let scale = f64::from(width) / size.width;
    Some(Arc::new(CursorImage {
        width,
        height,
        rgba,
        hot_x: hot.x * scale,
        hot_y: hot.y * scale,
        scale,
    }))
}

/// Polls the system cursor. Cheap enough at 60 Hz: one small image per call.
#[derive(Default)]
pub struct SystemCursorSource {
    last: Option<Arc<CursorImage>>,
}

impl SystemCursorSource {
    pub fn new() -> Self {
        Self::default()
    }
}

// CGCursorIsVisible is deprecated without a replacement; it is the only public answer.
#[allow(deprecated)]
impl CursorSource for SystemCursorSource {
    fn sample(&mut self) -> CursorSample {
        autoreleasepool(|_| {
            let position = CGEvent::new(None).map(|e| {
                let p = CGEvent::location(Some(&e));
                (p.x, p.y)
            });
            let image = current_shape();
            // Keep handing out the same allocation for an unchanged image, so the
            // tracker's pointer comparison short-cuts the content comparison.
            let image = match (image, &self.last) {
                (Some(new), Some(old)) if *new == **old => Some(old.clone()),
                (Some(new), _) => {
                    self.last = Some(new.clone());
                    Some(new)
                }
                (None, _) => None,
            };
            CursorSample { image, visible: CGCursorIsVisible(), position }
        })
    }
}
