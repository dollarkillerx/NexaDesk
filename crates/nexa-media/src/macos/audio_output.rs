//! Playback through the default output device: a CoreAudio output unit whose render
//! callback pulls 48 kHz interleaved stereo float from the caller. The unit converts
//! to the device's rate and layout.
//!
//! Only the viewer app creates one; tests render offline instead and never make a sound.

use std::ffi::c_void;
use std::ptr;
use std::time::Instant;

use crate::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, MediaError, Result};

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

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
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

type AudioUnit = *mut c_void;
type RenderProc = unsafe extern "C" fn(
    refcon: *mut c_void,
    action_flags: *mut u32,
    time_stamp: *const c_void,
    bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> i32;

#[repr(C)]
struct RenderCallback {
    proc_: RenderProc,
    refcon: *mut c_void,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(after: *mut c_void, desc: *const AudioComponentDescription) -> *mut c_void;
    fn AudioComponentInstanceNew(component: *mut c_void, out: *mut AudioUnit) -> i32;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> i32;
    fn AudioUnitSetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> i32;
    fn AudioUnitInitialize(unit: AudioUnit) -> i32;
    fn AudioUnitUninitialize(unit: AudioUnit) -> i32;
    fn AudioOutputUnitStart(unit: AudioUnit) -> i32;
    fn AudioOutputUnitStop(unit: AudioUnit) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const PROP_STREAM_FORMAT: u32 = 8;
const PROP_SET_RENDER_CALLBACK: u32 = 23;
const SCOPE_INPUT: u32 = 1;
/// `kAudioUnitRenderAction_OutputIsSilence`.
const OUTPUT_IS_SILENCE: u32 = 1 << 4;

/// Fills interleaved stereo; the argument after the buffer is when it will be heard.
pub type RenderFn = Box<dyn FnMut(&mut [f32], Instant) + Send>;

struct Callback {
    render: RenderFn,
}

unsafe extern "C" fn render_proc(
    refcon: *mut c_void,
    action_flags: *mut u32,
    _time_stamp: *const c_void,
    _bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: `refcon` is the Box<Callback> the unit owner keeps alive until the unit
    // is disposed; `data` holds one interleaved buffer in the format set below.
    unsafe {
        let callback = &mut *(refcon as *mut Callback);
        let Some(list) = data.as_mut() else { return 0 };
        if list.number_buffers == 0 || list.buffers[0].data.is_null() {
            return 0;
        }
        let want = (frames as usize * AUDIO_CHANNELS).min(list.buffers[0].data_byte_size as usize / 4);
        let out = std::slice::from_raw_parts_mut(list.buffers[0].data as *mut f32, want);
        // A panic must not unwind into CoreAudio.
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (callback.render)(out, Instant::now())));
        if ok.is_err() {
            out.fill(0.0);
            if let Some(f) = action_flags.as_mut() {
                *f |= OUTPUT_IS_SILENCE;
            }
        }
        0
    }
}

/// A running output. Dropping it stops playback.
pub struct AudioOutputUnit {
    unit: AudioUnit,
    _callback: Box<Callback>,
}

// SAFETY: the unit is only started and stopped (thread-safe AudioUnit calls); the
// callback runs on CoreAudio's thread and only touches `Callback`, which is Send.
unsafe impl Send for AudioOutputUnit {}

fn check(what: &'static str, status: i32) -> Result<()> {
    if status == 0 { Ok(()) } else { Err(MediaError::Os { what, status }) }
}

impl AudioOutputUnit {
    /// Starts the default output device pulling from `render`.
    pub fn start(render: RenderFn) -> Result<Self> {
        let desc = AudioComponentDescription {
            component_type: fourcc(b"auou"),
            component_sub_type: fourcc(b"def "),
            component_manufacturer: fourcc(b"appl"),
            component_flags: 0,
            component_flags_mask: 0,
        };
        let mut callback = Box::new(Callback { render });
        // SAFETY: documented AudioToolbox calls with valid pointers; on any error the
        // unit is disposed before returning, and `callback` outlives the unit.
        unsafe {
            let component = AudioComponentFindNext(ptr::null_mut(), &desc);
            if component.is_null() {
                return Err(MediaError::Unsupported("no default audio output"));
            }
            let mut unit: AudioUnit = ptr::null_mut();
            check("AudioComponentInstanceNew", AudioComponentInstanceNew(component, &mut unit))?;
            let bytes = (AUDIO_CHANNELS * 4) as u32;
            let format = AudioStreamBasicDescription {
                sample_rate: f64::from(AUDIO_SAMPLE_RATE),
                format_id: fourcc(b"lpcm"),
                format_flags: 1 | 8, // float, packed
                bytes_per_packet: bytes,
                frames_per_packet: 1,
                bytes_per_frame: bytes,
                channels_per_frame: AUDIO_CHANNELS as u32,
                bits_per_channel: 32,
                reserved: 0,
            };
            let cb = RenderCallback { proc_: render_proc, refcon: (&raw mut *callback).cast() };
            let setup = check(
                "AudioUnit stream format",
                AudioUnitSetProperty(
                    unit,
                    PROP_STREAM_FORMAT,
                    SCOPE_INPUT,
                    0,
                    (&raw const format).cast(),
                    std::mem::size_of_val(&format) as u32,
                ),
            )
            .and_then(|_| {
                check(
                    "AudioUnit render callback",
                    AudioUnitSetProperty(
                        unit,
                        PROP_SET_RENDER_CALLBACK,
                        SCOPE_INPUT,
                        0,
                        (&raw const cb).cast(),
                        std::mem::size_of_val(&cb) as u32,
                    ),
                )
            })
            .and_then(|_| check("AudioUnitInitialize", AudioUnitInitialize(unit)))
            .and_then(|_| check("AudioOutputUnitStart", AudioOutputUnitStart(unit)));
            if let Err(e) = setup {
                AudioUnitUninitialize(unit);
                AudioComponentInstanceDispose(unit);
                return Err(e);
            }
            Ok(Self { unit, _callback: callback })
        }
    }
}

impl Drop for AudioOutputUnit {
    fn drop(&mut self) {
        // SAFETY: stopping waits for any render in progress; after disposal no
        // callback can run, so `_callback` may be dropped afterwards.
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
        }
    }
}
