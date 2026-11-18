//! One picture of a display on demand, for an AI agent (docs/MCP.md §13.4):
//! ScreenCaptureKit's screenshot (macOS 14), at the size asked for, in BGRA with the
//! pointer drawn, handed back as straight RGBA.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGMainDisplayID, kCGColorSpaceSRGB};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::kCVPixelFormatType_32BGRA;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCScreenshotManager, SCStreamConfiguration,
};

use super::{
    PixelBuffer,
    capture::{apple_error, shareable_content},
};
use crate::{MediaError, Result};

const SHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Display `display_id` (0: the main one) at `width`×`height` pixels, the pointer
/// drawn in: straight RGBA, top row first.
pub fn screenshot_rgba(display_id: u32, width: u32, height: u32) -> Result<(u32, u32, Vec<u8>)> {
    let display_id = if display_id == 0 {
        CGMainDisplayID()
    } else {
        display_id
    };
    let content = shareable_content()?;
    // SAFETY: plain getters on a valid object.
    let display = unsafe {
        content
            .displays()
            .iter()
            .find(|d: &Retained<SCDisplay>| d.displayID() == display_id)
    }
    .ok_or_else(|| MediaError::Other(format!("display {display_id} is not available")))?;
    // SAFETY: plain setters with values of the documented types on a fresh
    // configuration; the colour space is a CF static.
    let configuration = unsafe {
        let c = SCStreamConfiguration::new();
        c.setWidth(width.max(1) as usize);
        c.setHeight(height.max(1) as usize);
        c.setPixelFormat(kCVPixelFormatType_32BGRA);
        c.setShowsCursor(true);
        c.setCapturesAudio(false);
        c.setColorSpaceName(kCGColorSpaceSRGB);
        c
    };
    // SAFETY: the display outlives the call, which copies what it needs.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::new(),
        )
    };
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |sample: *mut CMSampleBuffer, error: *mut NSError| {
        // SAFETY: both are valid or null for the duration of the call; the sample is
        // retained before it leaves.
        let result = unsafe {
            match (NonNull::new(sample), error.as_ref()) {
                (Some(sample), _) => Ok(CFRetained::retain(sample)),
                (None, Some(error)) => Err(apple_error("SCScreenshotManager", error)),
                (None, None) => Err(MediaError::Other("the screenshot came back empty".into())),
            }
        };
        let _ = tx.send(result);
    });
    // SAFETY: filter, configuration and block are valid for the call, which copies the
    // block.
    unsafe {
        SCScreenshotManager::captureSampleBufferWithFilter_configuration_completionHandler(
            &filter,
            &configuration,
            Some(&handler),
        );
    }
    let sample = rx
        .recv_timeout(SHOT_TIMEOUT)
        .map_err(|_| MediaError::Other("timed out taking a screenshot".into()))??;
    // SAFETY: the sample is valid and its image buffer is retained by the wrapper.
    let image = unsafe { sample.image_buffer() }
        .map(PixelBuffer::from_retained)
        .ok_or_else(|| MediaError::Other("the screenshot has no picture".into()))?;
    image.bgra_to_rgba()
}
