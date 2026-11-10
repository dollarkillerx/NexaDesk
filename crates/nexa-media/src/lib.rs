//! Capture, encode, decode and present. See docs/DETAILED_DESIGN.md §2 and §6.
//!
//! The traits own their frames rather than borrowing as §2 sketched: on macOS a
//! captured frame is a retained CVPixelBuffer that outlives the capture callback.
//!
//! Encoded frames are always **Annex-B** with parameter sets in-band on key frames.
//! That is the one bitstream form NVENC, VideoToolbox's converted output and
//! browser decoders can all share.

use std::time::{Duration, Instant};

use nexa_wire::control::{Chroma, Codec, Mode};

pub mod caps_cache;
#[cfg(target_os = "macos")]
pub mod macos;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("{what}: OS status {status}")]
    Os { what: &'static str, status: i32 },
    #[error("{0}")]
    Other(String),
    #[error("not supported: {0}")]
    Unsupported(&'static str),
    /// A backend skipped this frame on purpose. Nothing was produced, the
    /// reference chain is intact, and the next frame can proceed normally.
    #[error("frame dropped")]
    FrameDropped,
}

pub type Result<T> = std::result::Result<T, MediaError>;

pub type FrameId = u32;

/// A platform image handle. Cheap to clone: it only retains the underlying surface.
#[derive(Clone)]
pub enum NativeImage {
    #[cfg(target_os = "macos")]
    PixelBuffer(macos::PixelBuffer),
    Cpu(std::sync::Arc<CpuFrame>),
}

/// Tightly packed planes, for synthetic sources and software paths.
#[derive(Clone, Debug)]
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

pub struct CapturedFrame {
    pub image: NativeImage,
    pub captured_at: Instant,
    pub dirty: Vec<Rect>,
}

pub enum CaptureEvent {
    Frame(CapturedFrame),
    /// Nothing new within the timeout.
    Idle,
    /// The encoder must be rebuilt.
    ModeChanged(DisplayMode),
    /// The capturer must be rebuilt.
    AccessLost,
}

#[derive(Clone, Copy, Debug)]
pub struct CaptureConfig {
    pub display: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub show_cursor: bool,
    pub chroma: Chroma,
}

pub trait Capturer: Send {
    fn next(&mut self, timeout: Duration) -> Result<CaptureEvent>;
}

#[derive(Clone, Copy, Debug)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub chroma: Chroma,
    pub mode: Mode,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EncoderCaps {
    pub rfi: bool,
    pub ltr: bool,
    pub dynamic_bitrate: bool,
    pub hardware: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EncodeOpts {
    pub force_key: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct EncodedMeta {
    pub frame_id: FrameId,
    pub key: bool,
    /// This frame is a loss-recovery point requested through `invalidate_from`.
    pub recovery_point: bool,
}

/// What `Encoder::invalidate_from` managed to arrange for the next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    RefInvalidated,
    IntraRefresh,
    Idr,
}

pub trait Encoder: Send {
    fn caps(&self) -> EncoderCaps;
    /// Synchronous: on return `out` holds the frame as Annex-B.
    fn encode(&mut self, frame: &CapturedFrame, frame_id: FrameId, opts: EncodeOpts, out: &mut Vec<u8>) -> Result<EncodedMeta>;
    fn set_bitrate(&mut self, bps: u32) -> Result<()>;
    fn invalidate_from(&mut self, first_lost: FrameId) -> Result<Recovery>;
    /// The viewer confirmed this frame arrived complete. LTR backends use it; others ignore it.
    fn on_frame_acked(&mut self, frame: FrameId);
    fn force_idr(&mut self);
}

#[derive(Clone, Copy, Debug)]
pub struct FrameMeta {
    pub frame_id: FrameId,
    pub capture_ts_us: u32,
}

pub struct DecodedFrame {
    pub image: NativeImage,
    pub meta: FrameMeta,
}

pub trait Decoder: Send {
    /// `au` is one Annex-B access unit. May be fed non-key frames only after a key frame.
    fn decode(&mut self, au: &[u8], meta: FrameMeta) -> Result<DecodedFrame>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentMode {
    /// Office and balanced modes.
    Vsync,
    /// Game mode.
    Immediate,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PresentStat {
    pub present_call_us: u32,
}

/// Presents on the render thread after being attached to a native view on the UI thread.
pub trait Presenter: Send {
    fn resize(&mut self, width_px: u32, height_px: u32, scale: f32) -> Result<()>;
    fn set_mode(&mut self, mode: PresentMode) -> Result<()>;
    fn present(&mut self, frame: &DecodedFrame) -> Result<PresentStat>;
}

// ---------------------------------------------------------------------------------
// Cursor (§6.9)
// ---------------------------------------------------------------------------------

/// An RGBA cursor image as the host draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// Straight (not premultiplied) RGBA, `width * height * 4` bytes, top row first.
    pub rgba: Vec<u8>,
    /// Hot spot in image pixels.
    pub hot_x: f64,
    pub hot_y: f64,
    /// Image pixels per host point: 2.0 for a Retina cursor.
    pub scale: f64,
}

/// One look at the host's cursor.
#[derive(Clone, Debug, Default)]
pub struct CursorSample {
    /// `None` when the shape could not be read; the last one stays in effect.
    pub image: Option<std::sync::Arc<CursorImage>>,
    pub visible: bool,
    /// Global host points (CoreGraphics: top-left origin of the main display).
    pub position: Option<(f64, f64)>,
}

/// Reads the host cursor. Implementations must never move or change it.
pub trait CursorSource: Send {
    fn sample(&mut self) -> CursorSample;
}

// ---------------------------------------------------------------------------------
// System audio (DESIGN §6.1): 48 kHz stereo, interleaved f32, 10 ms Opus packets.
// ---------------------------------------------------------------------------------

pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: usize = 2;
/// Samples per channel in one 10 ms frame.
pub const AUDIO_FRAME_SAMPLES: usize = 480;
/// Interleaved samples in one frame.
pub const AUDIO_FRAME_LEN: usize = AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS;

/// Interleaved stereo float samples at 48 kHz, as captured.
pub struct AudioChunk {
    pub samples: Vec<f32>,
    /// When the last sample was captured.
    pub captured_at: Instant,
}

/// The host's system audio. Creating one starts capture; dropping it stops capture.
pub trait AudioCapture: Send {
    /// The next chunk, or `None` after `timeout` with nothing new.
    fn next(&mut self, timeout: Duration) -> Result<Option<AudioChunk>>;
}

pub trait AudioEncoder: Send {
    /// Encodes exactly one frame (`AUDIO_FRAME_LEN` interleaved samples) into `out`.
    fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<()>;
}

pub trait AudioDecoder: Send {
    /// Decodes one packet into exactly one frame (`AUDIO_FRAME_LEN` samples). `None`
    /// asks the codec to conceal a lost packet; a codec that cannot returns `Err`.
    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<()>;
    fn reset(&mut self);
}
