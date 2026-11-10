//! ScreenCaptureKit capture, per docs/DETAILED_DESIGN.md §1.1 and §6.5.
//!
//! The stream callback only moves the newest complete frame into a single-slot
//! mailbox; everything else happens on the `next` caller's thread.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchQoS, DispatchRetained};
use nexa_wire::control::Chroma;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString};
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID, CGPreflightScreenCaptureAccess,
    CGRectMakeWithDictionaryRepresentation, kCGColorSpaceSRGB, kCGDisplayStreamYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamFrameInfoDirtyRects, SCStreamFrameInfoStatus, SCStreamOutput,
    SCStreamOutputType,
};

use super::{PixelBuffer, sample_attachment};
use crate::{CaptureConfig, CaptureEvent, CapturedFrame, Capturer, DisplayMode, MediaError, NativeImage, Rect, Result};

/// §6.5: the default depth of 8 adds latency.
const QUEUE_DEPTH: isize = 3;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const MODE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// True if this process already has Screen Recording permission. This never
/// shows a prompt, and a capture attempted without it fails.
pub fn has_screen_capture_permission() -> bool {
    CGPreflightScreenCaptureAccess()
}

struct Arrived {
    sample: CFRetained<CMSampleBuffer>,
    image: PixelBuffer,
    captured_at: Instant,
}

// SAFETY: CMSampleBuffer is an immutable-after-creation CF object, safe to
// retain and release across threads.
unsafe impl Send for Arrived {}

#[derive(Default)]
struct MailboxState {
    frame: Option<Arrived>,
    stream_stopped: bool,
}

#[derive(Default)]
struct Mailbox {
    state: Mutex<MailboxState>,
    arrived: Condvar,
}

impl Mailbox {
    fn lock(&self) -> MutexGuard<'_, MailboxState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "NexaScreenCaptureOutput"]
    #[ivars = Arc<Mailbox>]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn on_sample(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Screen {
                on_screen_sample(self.ivars(), sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn on_stop(&self, _stream: &SCStream, _error: &NSError) {
            let mailbox = self.ivars();
            mailbox.lock().stream_stopped = true;
            mailbox.arrived.notify_all();
        }
    }
);

impl StreamOutput {
    fn new(mailbox: Arc<Mailbox>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(mailbox);
        // SAFETY: NSObject's init takes no arguments.
        unsafe { msg_send![super(this), init] }
    }
}

fn on_screen_sample(mailbox: &Mailbox, sample: &CMSampleBuffer) {
    // SAFETY: the keys are ScreenCaptureKit statics, and the status attachment
    // is a CFNumber when present.
    let complete = unsafe {
        let status = sample_attachment(sample, std::ptr::from_ref(SCStreamFrameInfoStatus).cast());
        !status.is_null()
            && (*(status as *const CFNumber)).as_i64() == Some(SCFrameStatus::Complete.0 as i64)
    };
    if !complete {
        return;
    }
    // SAFETY: `sample` is valid for this call and the buffer is retained.
    let Some(image) = (unsafe { sample.image_buffer() }).map(PixelBuffer::from_retained) else { return };
    // SCStreamFrameInfoDisplayTime is no use here: measured on macOS 27 it is the
    // scheduled display time, 7 to 11 ms *after* this callback runs.
    let captured_at = Instant::now();
    // SAFETY: `sample` is valid for this call.
    let sample = unsafe { CFRetained::retain(NonNull::from(sample)) };
    let mut state = mailbox.lock();
    // A frame nobody picked up in time is replaced, never queued.
    state.frame = Some(Arrived { sample, image, captured_at });
    drop(state);
    mailbox.arrived.notify_one();
}

fn current_mode(display: u32) -> Option<DisplayMode> {
    let mode = CGDisplayCopyDisplayMode(display)?;
    Some(DisplayMode {
        width: CGDisplayMode::pixel_width(Some(&mode)) as u32,
        height: CGDisplayMode::pixel_height(Some(&mode)) as u32,
        refresh_hz: CGDisplayMode::refresh_rate(Some(&mode)).round() as u32,
    })
}

fn apple_error(what: &'static str, error: &NSError) -> MediaError {
    MediaError::Os { what, status: error.code() as i32 }
}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        // SAFETY: both are valid or null; content is retained by the wrapper.
        let result = unsafe {
            match (Retained::retain(content), error.as_ref()) {
                (Some(content), _) => Ok(content),
                (None, Some(error)) => Err(apple_error("SCShareableContent", error)),
                (None, None) => Err(MediaError::Other("SCShareableContent returned nothing".into())),
            }
        };
        let _ = tx.send(result);
    });
    // SAFETY: the block is valid for the call, which copies it.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    rx.recv_timeout(STARTUP_TIMEOUT)
        .map_err(|_| MediaError::Other("timed out listing shareable content".into()))?
}

fn start_stream(stream: &SCStream) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: valid or null for the duration of the block.
        let code = unsafe { error.as_ref() }.map(|e| e.code() as i32);
        let _ = tx.send(code);
    });
    // SAFETY: the block is valid for the call, which copies it.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match rx.recv_timeout(STARTUP_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(status)) => Err(MediaError::Os { what: "SCStream start", status }),
        Err(_) => Err(MediaError::Other("timed out starting capture".into())),
    }
}

fn stop_stream(stream: &SCStream) {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |_error: *mut NSError| {
        let _ = tx.send(());
    });
    // SAFETY: the block is valid for the call, which copies it.
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };
    let _ = rx.recv_timeout(Duration::from_secs(2));
}

/// Parses the dirty-rect array, clipping each rect to the frame. Empty means unknown.
fn dirty_rects(sample: &CMSampleBuffer, frame_width: u32, frame_height: u32) -> Vec<Rect> {
    use objc2_core_foundation::{CFArray, CGRect};
    // SAFETY: the attachment, when present, is a CFArray of CGRect dictionaries.
    unsafe {
        let value = sample_attachment(sample, std::ptr::from_ref(SCStreamFrameInfoDirtyRects).cast());
        let Some(array) = NonNull::new(value as *mut CFArray) else { return Vec::new() };
        let array = array.as_ref();
        let mut rects = Vec::with_capacity(array.len());
        for i in 0..array.len() {
            let dict = array.value_at_index(i as isize) as *const CFDictionary;
            let Some(dict) = dict.as_ref() else { continue };
            let mut r = CGRect::default();
            if !CGRectMakeWithDictionaryRepresentation(Some(dict), &mut r) {
                continue;
            }
            let x = (r.origin.x.max(0.0) as u32).min(frame_width);
            let y = (r.origin.y.max(0.0) as u32).min(frame_height);
            let w = (r.size.width.max(0.0).ceil() as u32).min(frame_width - x);
            let h = (r.size.height.max(0.0).ceil() as u32).min(frame_height - y);
            if w > 0 && h > 0 {
                rects.push(Rect { x, y, w, h });
            }
        }
        rects
    }
}

pub struct ScreenCapturer {
    mailbox: Arc<Mailbox>,
    stream: Retained<SCStream>,
    // Kept alive for the stream, which only holds these weakly.
    _output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    configuration: Retained<SCStreamConfiguration>,
    display: u32,
    /// `width`/`height` of 0 asked for native size, so the stream follows mode changes.
    follows_native: bool,
    mode: DisplayMode,
    /// Frame size the stream is currently configured to deliver.
    output_size: (u32, u32),
    next_mode_poll: Instant,
}

// SAFETY: SCStream's methods may be called from any thread, and `next` takes
// `&mut self`. The mailbox is what the callback thread shares.
unsafe impl Send for ScreenCapturer {}

impl ScreenCapturer {
    /// Starts capturing. `config.display` is a CGDirectDisplayID, 0 meaning the
    /// main display; `width`/`height` of 0 capture at the native pixel size.
    ///
    /// Without Screen Recording permission this fails, and macOS may show its
    /// permission prompt; use [`has_screen_capture_permission`] to check first.
    pub fn new(config: CaptureConfig) -> Result<Self> {
        if config.chroma != Chroma::Yuv420 {
            return Err(MediaError::Unsupported("macOS capture is 4:2:0 only"));
        }
        if config.fps == 0 {
            return Err(MediaError::Other("fps must be non-zero".into()));
        }
        let display_id = if config.display == 0 { CGMainDisplayID() } else { config.display };
        let content = shareable_content()?;
        // SAFETY: plain getters on a valid object.
        let display = unsafe {
            content
                .displays()
                .iter()
                .find(|d: &Retained<SCDisplay>| d.displayID() == display_id)
                .ok_or_else(|| MediaError::Other(format!("display {display_id} not found")))?
        };
        let mode = current_mode(display_id)
            .ok_or_else(|| MediaError::Other(format!("display {display_id} has no current mode")))?;
        let follows_native = config.width == 0 || config.height == 0;
        let (width, height) =
            if follows_native { (mode.width, mode.height) } else { (config.width, config.height) };

        // SAFETY: plain setters with values of the documented types on a fresh
        // configuration; the color-matrix and color-space keys are CF statics.
        let configuration = unsafe {
            let c = SCStreamConfiguration::new();
            c.setWidth(width as usize);
            c.setHeight(height as usize);
            c.setMinimumFrameInterval(CMTime::new(1, config.fps as i32));
            c.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            c.setQueueDepth(QUEUE_DEPTH);
            c.setShowsCursor(config.show_cursor);
            c.setCapturesAudio(false);
            // The presenter and the encoder's VUI both assume BT.709 limited range.
            let matrix: &CFString = kCGDisplayStreamYCbCrMatrix_ITU_R_709_2;
            c.setColorMatrix(matrix);
            c.setColorSpaceName(kCGColorSpaceSRGB);
            c
        };

        let mailbox = Arc::new(Mailbox::default());
        let output = StreamOutput::new(mailbox.clone());
        // The QoS has to be part of the attribute: libdispatch aborts the process
        // if a floor is set on a queue that is already active.
        let attribute = DispatchQueueAttr::with_qos_class(None, DispatchQoS::UserInteractive, 0);
        let queue = DispatchQueue::new("nexa.media.capture", Some(&attribute));

        // SAFETY: the filter, configuration, delegate and output all outlive the
        // stream start; `Self` keeps the output and queue for the stream's life.
        let stream = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &configuration,
                Some(ProtocolObject::from_ref(&*output)),
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| apple_error("SCStream addStreamOutput", &e))?;
            stream
        };
        start_stream(&stream)?;

        Ok(Self {
            mailbox,
            stream,
            _output: output,
            _queue: queue,
            configuration,
            display: display_id,
            follows_native,
            mode,
            output_size: (width, height),
            next_mode_poll: Instant::now() + MODE_POLL_INTERVAL,
        })
    }

    /// Size the stream currently delivers.
    pub fn output_size(&self) -> (u32, u32) {
        self.output_size
    }

    fn poll_mode_change(&mut self) -> Option<DisplayMode> {
        let now = Instant::now();
        if now < self.next_mode_poll {
            return None;
        }
        self.next_mode_poll = now + MODE_POLL_INTERVAL;
        let mode = current_mode(self.display)?;
        if mode == self.mode {
            return None;
        }
        self.mode = mode;
        if self.follows_native {
            self.output_size = (mode.width, mode.height);
            // SAFETY: plain setters, then the stream copies the configuration.
            unsafe {
                self.configuration.setWidth(mode.width as usize);
                self.configuration.setHeight(mode.height as usize);
                self.stream.updateConfiguration_completionHandler(&self.configuration, None);
            }
        }
        Some(mode)
    }
}

impl Capturer for ScreenCapturer {
    fn next(&mut self, timeout: Duration) -> Result<CaptureEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(mode) = self.poll_mode_change() {
                return Ok(CaptureEvent::ModeChanged(mode));
            }
            let mut state = self.mailbox.lock();
            if state.stream_stopped {
                return Ok(CaptureEvent::AccessLost);
            }
            if let Some(arrived) = state.frame.take() {
                drop(state);
                let (w, h) = (arrived.image.width(), arrived.image.height());
                // Frames still in flight at the old size after a mode change.
                if self.follows_native && (w, h) != self.output_size {
                    continue;
                }
                let dirty = dirty_rects(&arrived.sample, w, h);
                return Ok(CaptureEvent::Frame(CapturedFrame {
                    image: NativeImage::PixelBuffer(arrived.image),
                    captured_at: arrived.captured_at,
                    dirty,
                }));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(CaptureEvent::Idle);
            }
            let wait = deadline.min(self.next_mode_poll).saturating_duration_since(now);
            let (_state, _) = self
                .mailbox
                .arrived
                .wait_timeout(state, wait.max(Duration::from_millis(1)))
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Drop for ScreenCapturer {
    fn drop(&mut self) {
        stop_stream(&self.stream);
    }
}
