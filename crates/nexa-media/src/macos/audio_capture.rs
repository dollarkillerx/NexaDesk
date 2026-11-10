//! System audio through ScreenCaptureKit (`capturesAudio`), without this process's
//! own audio. A stream of its own, separate from the video stream, so muting stops
//! it without touching video and video rebuilds do not interrupt audio.
//!
//! Needs Screen Recording permission like video; macOS may show its own prompt the
//! first time. Nothing here stores or logs samples: they go to the caller only.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchQoS, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGMainDisplayID;
use objc2_core_media::{CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};

use super::capture::{apple_error, shareable_content, start_stream, stop_stream};
use crate::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, AudioCapture, AudioChunk, MediaError, Result};

/// About 320 ms of 10–20 ms buffers; older audio is dropped, never queued without bound.
const QUEUE: usize = 32;

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

/// An AudioBufferList with room for two buffers (non-interleaved stereo).
#[repr(C)]
struct AudioBufferList2 {
    number_buffers: u32,
    buffers: [AudioBuffer; 2],
}

const FLAG_FLOAT: u32 = 1 << 0;
const FLAG_NON_INTERLEAVED: u32 = 1 << 5;
const FORMAT_LPCM: u32 = u32::from_be_bytes(*b"lpcm");
const ASSURE_16_BYTE_ALIGNMENT: u32 = 1 << 0;

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sample: &CMSampleBuffer,
        size_needed: *mut usize,
        list: *mut AudioBufferList2,
        list_size: usize,
        structure_allocator: *const c_void,
        block_allocator: *const c_void,
        flags: u32,
        block_out: *mut *mut CMBlockBuffer,
    ) -> i32;
    fn CMAudioFormatDescriptionGetStreamBasicDescription(desc: &CMFormatDescription) -> *const AudioStreamBasicDescription;
}

struct Shared {
    tx: SyncSender<AudioChunk>,
    stopped: AtomicBool,
    dropped: AtomicU64,
    unsupported: AtomicU64,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "NexaSystemAudioOutput"]
    #[ivars = Arc<Shared>]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn on_sample(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Audio {
                on_audio(self.ivars(), sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn on_stop(&self, _stream: &SCStream, _error: &NSError) {
            self.ivars().stopped.store(true, Relaxed);
        }
    }
);

impl AudioOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        // SAFETY: NSObject's init takes no arguments.
        unsafe { msg_send![super(this), init] }
    }
}

/// Copies one sample buffer out as interleaved stereo f32.
fn on_audio(shared: &Shared, sample: &CMSampleBuffer) {
    let captured_at = Instant::now();
    // SAFETY: `sample` is valid for this call; the description belongs to it.
    let format = unsafe {
        let Some(desc) = sample.format_description() else { return };
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&desc);
        let Some(asbd) = asbd.as_ref() else { return };
        (asbd.format_id, asbd.format_flags, asbd.channels_per_frame, asbd.bits_per_channel, asbd.sample_rate)
    };
    let (format_id, flags, channels, bits, rate) = format;
    if format_id != FORMAT_LPCM
        || flags & FLAG_FLOAT == 0
        || bits != 32
        || !(1..=2).contains(&channels)
        || rate != f64::from(AUDIO_SAMPLE_RATE)
    {
        shared.unsupported.fetch_add(1, Relaxed);
        return;
    }
    let mut list = AudioBufferList2 {
        number_buffers: 0,
        buffers: [AudioBuffer { number_channels: 0, data_byte_size: 0, data: ptr::null_mut() }; 2],
    };
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: the list has room for two buffers, which covers both layouts accepted
    // above; the retained block buffer keeps the data alive until released below.
    let status = unsafe {
        CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sample,
            ptr::null_mut(),
            &mut list,
            std::mem::size_of::<AudioBufferList2>(),
            ptr::null(),
            ptr::null(),
            ASSURE_16_BYTE_ALIGNMENT,
            &mut block,
        )
    };
    let block = NonNull::new(block).map(|b| unsafe { CFRetained::from_raw(b) });
    if status != 0 || block.is_none() {
        return;
    }
    let buffers = &list.buffers[..(list.number_buffers as usize).min(2)];
    // SAFETY: each buffer's data is `data_byte_size` bytes of f32 while `block` lives.
    let planes: Vec<&[f32]> = buffers
        .iter()
        .filter(|b| !b.data.is_null())
        .map(|b| unsafe { std::slice::from_raw_parts(b.data as *const f32, b.data_byte_size as usize / 4) })
        .collect();
    let mut samples = Vec::new();
    match (flags & FLAG_NON_INTERLEAVED != 0, channels, planes.as_slice()) {
        (true, 2, [l, r]) => {
            samples.reserve(l.len() * 2);
            for (a, b) in l.iter().zip(r.iter()) {
                samples.push(*a);
                samples.push(*b);
            }
        }
        (true, 1, [m]) | (false, 1, [m]) => {
            samples.reserve(m.len() * 2);
            for v in m.iter() {
                samples.push(*v);
                samples.push(*v);
            }
        }
        (false, 2, [i]) => samples.extend_from_slice(i),
        _ => {
            shared.unsupported.fetch_add(1, Relaxed);
            return;
        }
    }
    drop(block);
    debug_assert_eq!(AUDIO_CHANNELS, 2);
    match shared.tx.try_send(AudioChunk { samples, captured_at }) {
        Ok(()) | Err(TrySendError::Disconnected(_)) => {}
        Err(TrySendError::Full(_)) => {
            shared.dropped.fetch_add(1, Relaxed);
        }
    }
}

/// System audio of the whole machine, minus NexaDesk's own.
pub struct SystemAudioCapture {
    stream: Retained<SCStream>,
    _output: Retained<AudioOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    rx: Receiver<AudioChunk>,
}

// SAFETY: SCStream's methods may be called from any thread; the receiver is owned.
unsafe impl Send for SystemAudioCapture {}

impl SystemAudioCapture {
    /// Starts capturing. `display` is a CGDirectDisplayID (0: main), which
    /// ScreenCaptureKit requires for any stream even though audio is system-wide.
    pub fn start(display: u32) -> Result<Self> {
        let display_id = if display == 0 { CGMainDisplayID() } else { display };
        let content = shareable_content()?;
        // SAFETY: plain getters on a valid object.
        let display = unsafe {
            content
                .displays()
                .iter()
                .find(|d: &Retained<SCDisplay>| d.displayID() == display_id)
                .ok_or_else(|| MediaError::Other(format!("display {display_id} not found")))?
        };
        // SAFETY: plain setters with values of the documented types.
        let configuration = unsafe {
            let c = SCStreamConfiguration::new();
            c.setCapturesAudio(true);
            c.setExcludesCurrentProcessAudio(true);
            c.setSampleRate(AUDIO_SAMPLE_RATE as isize);
            c.setChannelCount(AUDIO_CHANNELS as isize);
            // Video is unavoidable on a stream; keep it as cheap as it gets.
            c.setWidth(2);
            c.setHeight(2);
            c.setMinimumFrameInterval(CMTime::new(1, 1));
            c.setQueueDepth(1);
            c.setShowsCursor(false);
            c
        };
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let shared = Arc::new(Shared {
            tx,
            stopped: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            unsupported: AtomicU64::new(0),
        });
        let output = AudioOutput::new(shared.clone());
        let attribute = DispatchQueueAttr::with_qos_class(None, DispatchQoS::UserInteractive, 0);
        let queue = DispatchQueue::new("nexa.media.audio", Some(&attribute));
        // SAFETY: as in `ScreenCapturer::new`; `Self` keeps output and queue alive.
        let stream = unsafe {
            let filter =
                SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new());
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &configuration,
                Some(ProtocolObject::from_ref(&*output)),
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
                .map_err(|e| apple_error("SCStream addStreamOutput (audio)", &e))?;
            stream
        };
        start_stream(&stream)?;
        Ok(Self { stream, _output: output, _queue: queue, shared, rx })
    }

    /// Buffers dropped because nobody read them in time.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Relaxed)
    }

    /// Buffers in a format this does not convert.
    pub fn unsupported(&self) -> u64 {
        self.shared.unsupported.load(Relaxed)
    }
}

impl AudioCapture for SystemAudioCapture {
    fn next(&mut self, timeout: Duration) -> Result<Option<AudioChunk>> {
        if self.shared.stopped.load(Relaxed) {
            return Err(MediaError::Other("system audio capture stopped".into()));
        }
        match self.rx.recv_timeout(timeout) {
            Ok(chunk) => Ok(Some(chunk)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(MediaError::Other("system audio capture ended".into())),
        }
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        stop_stream(&self.stream);
    }
}
