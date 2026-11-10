//! VideoToolbox encoder, per docs/DETAILED_DESIGN.md §6.5 and docs/spikes/M0-report.md.
//!
//! Every mode uses low-latency rate control with LTR, which the M0 spike measured
//! working for both HEVC and H.264. `MaxFrameDelayCount` and
//! `PrioritizeEncodingSpeedOverQuality` are rejected in this configuration and
//! are deliberately never set.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use nexa_wire::control::{Chroma, Codec};
use objc2_core_foundation::{CFArray, CFBoolean, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, CMVideoCodecType,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex, kCMSampleAttachmentKey_NotSync, kCMTimeInvalid,
    kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_EnableLTR, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_AcknowledgedLTRTokens, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTEncodeFrameOptionKey_ForceLTRRefresh, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_HEVC_Main_AutoLevel, kVTSampleAttachmentKey_RequireLTRAcknowledgementToken,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use super::bitstream::{self, NalCodec};
use super::{Nv12Pool, PixelBuffer, cf_dict, number_i64, os, sample_attachment};
use crate::{
    CapturedFrame, EncodeOpts, EncodedMeta, Encoder, EncoderCaps, EncoderConfig, FrameId, MediaError, NativeImage,
    Recovery, Result,
};

const ENCODE_TIMEOUT: Duration = Duration::from_secs(1);
/// How many outstanding `frame_id -> token` entries we remember. Tokens older
/// than this are almost certainly evicted from the encoder's own LTR slots.
const MAX_REMEMBERED_TOKENS: usize = 240;
/// Recently acknowledged frames, kept only to answer "is there a usable LTR".
const MAX_REMEMBERED_ACKS: usize = 32;

struct EncodeResult {
    frame_id: FrameId,
    status: i32,
    dropped: bool,
    sample: Option<CFRetained<CMSampleBuffer>>,
}

// SAFETY: a CMSampleBuffer is an immutable-after-creation CF object, safe to
// retain and release across threads.
unsafe impl Send for EncodeResult {}

/// Single-frame rendezvous between `encode` and the session callback.
#[derive(Default)]
struct OutputSlot {
    result: Mutex<Option<EncodeResult>>,
    filled: Condvar,
}

unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `refcon` is the Arc<OutputSlot> pointer the encoder keeps alive
    // until after the session is invalidated, and `sample` is valid for this call.
    let slot = unsafe { &*(refcon as *const OutputSlot) };
    let sample = NonNull::new(sample).map(|s| unsafe { CFRetained::retain(s) });
    let result = EncodeResult {
        frame_id: frame_refcon as usize as FrameId,
        status,
        dropped: flags.contains(VTEncodeInfoFlags::FrameDropped),
        sample,
    };
    *slot.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    slot.filled.notify_all();
}

/// `a` is older than `b` on the wrapping `FrameId` clock.
fn is_before(a: FrameId, b: FrameId) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    slot: Arc<OutputSlot>,
    config: EncoderConfig,
    codec: Codec,
    nal: NalCodec,
    caps: EncoderCaps,
    /// Property name and status of everything the session rejected.
    rejected: Vec<(&'static str, i32)>,
    /// Lazily created for `NativeImage::Cpu` input.
    cpu_pool: Option<Nv12Pool>,
    first_captured_at: Option<Instant>,
    last_pts_us: i64,
    /// Token per encoded frame, oldest first. Only frames still eligible to be
    /// acknowledged stay here.
    tokens: VecDeque<(FrameId, CFRetained<CFNumber>)>,
    /// Acknowledged, not yet handed to the encoder.
    pending_acks: Vec<(FrameId, CFRetained<CFNumber>)>,
    acked: VecDeque<FrameId>,
    force_key: bool,
    force_ltr_refresh: bool,
    recovery_pending: bool,
    scratch: Vec<u8>,
}

// SAFETY: a VTCompressionSession may be used from any thread, one at a time,
// which `&mut self` guarantees; the retained CF objects are thread-safe.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    /// Creates a session. `Codec::Unknown` means HEVC, falling back to H.264.
    pub fn new(config: EncoderConfig) -> Result<Self> {
        if config.width == 0 || config.height == 0 || config.fps == 0 || config.bitrate_bps == 0 {
            return Err(MediaError::Other(format!("invalid encoder config {config:?}")));
        }
        if config.chroma == Chroma::Yuv444 {
            // M0: a 4:4:4 input still produces chroma_format_idc=1.
            return Err(MediaError::Unsupported("VideoToolbox has no 4:4:4 hardware encode"));
        }
        match config.codec {
            Codec::Hevc => Self::new_with_codec(config, Codec::Hevc),
            Codec::H264 => Self::new_with_codec(config, Codec::H264),
            Codec::Unknown => {
                Self::new_with_codec(config, Codec::Hevc).or_else(|_| Self::new_with_codec(config, Codec::H264))
            }
            Codec::Av1 => Err(MediaError::Unsupported("VideoToolbox has no AV1 encoder")),
        }
    }

    fn new_with_codec(config: EncoderConfig, codec: Codec) -> Result<Self> {
        let (codec_type, nal): (CMVideoCodecType, _) = match codec {
            Codec::Hevc => (kCMVideoCodecType_HEVC, NalCodec::Hevc),
            _ => (kCMVideoCodecType_H264, NalCodec::H264),
        };
        let slot = Arc::new(OutputSlot::default());
        // `UsingHardwareAcceleratedVideoEncoder` is unsupported (-12900) on
        // low-latency sessions, so hardware is proven by requiring it at creation.
        // SAFETY: `slot` outlives the session, which `Drop` invalidates first.
        let (session, hardware) = match unsafe { create_session(&config, codec_type, Arc::as_ptr(&slot), true) } {
            Ok(session) => (session, true),
            Err(_) => (unsafe { create_session(&config, codec_type, Arc::as_ptr(&slot), false)? }, false),
        };

        let mut rejected = Vec::new();
        // Required by §6.5; without these the session is not what we measured.
        // SAFETY: the keys are VideoToolbox statics and the values their documented types.
        unsafe {
            set_required(&session, kVTCompressionPropertyKey_RealTime, "RealTime", CFBoolean::new(true).as_ref())?;
            set_required(
                &session,
                kVTCompressionPropertyKey_AllowFrameReordering,
                "AllowFrameReordering",
                CFBoolean::new(false).as_ref(),
            )?;
            set_required(
                &session,
                kVTCompressionPropertyKey_AverageBitRate,
                "AverageBitRate",
                &number_i64(config.bitrate_bps as i64),
            )?;

            let profile = if nal == NalCodec::Hevc {
                kVTProfileLevel_HEVC_Main_AutoLevel
            } else {
                kVTProfileLevel_H264_High_AutoLevel
            };
            let fps = number_i64(config.fps as i64);
            let optional: [(&CFString, &str, &CFType); 5] = [
                (kVTCompressionPropertyKey_ProfileLevel, "ProfileLevel", profile.as_ref()),
                (kVTCompressionPropertyKey_ExpectedFrameRate, "ExpectedFrameRate", &fps),
                (
                    kVTCompressionPropertyKey_ColorPrimaries,
                    "ColorPrimaries",
                    kCVImageBufferColorPrimaries_ITU_R_709_2.as_ref(),
                ),
                (
                    kVTCompressionPropertyKey_TransferFunction,
                    "TransferFunction",
                    kCVImageBufferTransferFunction_ITU_R_709_2.as_ref(),
                ),
                (
                    kVTCompressionPropertyKey_YCbCrMatrix,
                    "YCbCrMatrix",
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2.as_ref(),
                ),
            ];
            for (key, name, value) in optional {
                let status = VTSessionSetProperty(&session, key, Some(value));
                if status != 0 {
                    rejected.push((name, status));
                }
            }
        }

        // LTR is not fatal: without it `invalidate_from` degrades to an IDR and
        // caps() says so, which the negotiation layer already handles.
        // SAFETY: as above.
        let ltr = unsafe {
            let status = VTSessionSetProperty(
                &session,
                kVTCompressionPropertyKey_EnableLTR,
                Some(CFBoolean::new(true).as_ref()),
            );
            if status != 0 {
                rejected.push(("EnableLTR", status));
            }
            status == 0
        };

        // SAFETY: the session is valid and fully configured.
        unsafe {
            os("VTCompressionSessionPrepareToEncodeFrames", session.prepare_to_encode_frames())?;
        }
        Ok(Self {
            session,
            slot,
            config,
            codec,
            nal,
            caps: EncoderCaps { rfi: false, ltr, dynamic_bitrate: true, hardware },
            rejected,
            cpu_pool: None,
            first_captured_at: None,
            last_pts_us: -1,
            tokens: VecDeque::new(),
            pending_acks: Vec::new(),
            acked: VecDeque::new(),
            force_key: false,
            force_ltr_refresh: false,
            recovery_pending: false,
            scratch: Vec::new(),
        })
    }

    /// The codec actually in use.
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Properties the session rejected, for diagnostics. `MaxFrameDelayCount` and
    /// friends are never attempted, so this is normally empty.
    pub fn rejected_properties(&self) -> &[(&'static str, i32)] {
        &self.rejected
    }

    fn next_timestamps(&mut self, captured_at: Instant) -> (CMTime, CMTime) {
        let first = *self.first_captured_at.get_or_insert(captured_at);
        let us = captured_at.saturating_duration_since(first).as_micros() as i64;
        // The refinement path re-encodes one image, so the same instant can repeat.
        let us = us.max(self.last_pts_us + 1);
        self.last_pts_us = us;
        // SAFETY: plain value constructors.
        unsafe { (CMTime::new(us, 1_000_000), CMTime::new(1, self.config.fps as i32)) }
    }

    fn pixel_buffer(&mut self, frame: &CapturedFrame) -> Result<PixelBuffer> {
        match &frame.image {
            NativeImage::PixelBuffer(pb) => Ok(pb.clone()),
            NativeImage::Cpu(cpu) => {
                if self.cpu_pool.is_none() {
                    self.cpu_pool = Some(Nv12Pool::new(self.config.width, self.config.height)?);
                }
                self.cpu_pool.as_ref().expect("just created").upload(cpu)
            }
        }
    }

    fn frame_properties(&self, opts: EncodeOpts) -> Option<CFRetained<objc2_core_foundation::CFDictionary>> {
        let force_key = opts.force_key || self.force_key;
        let tokens: Vec<_> = self.pending_acks.iter().map(|(_, t)| t.clone()).collect();
        if !force_key && !self.force_ltr_refresh && tokens.is_empty() {
            return None;
        }
        // SAFETY: the keys are VideoToolbox statics and the values their documented types.
        unsafe {
            let true_value: &CFType = CFBoolean::new(true).as_ref();
            let tokens = (!tokens.is_empty() && !force_key).then(|| CFArray::from_retained_objects(&tokens));
            let mut pairs: Vec<(&CFString, &CFType)> = Vec::with_capacity(2);
            if let Some(tokens) = &tokens {
                pairs.push((kVTEncodeFrameOptionKey_AcknowledgedLTRTokens, tokens.as_ref()));
            }
            if force_key {
                pairs.push((kVTEncodeFrameOptionKey_ForceKeyFrame, true_value));
            } else if self.force_ltr_refresh {
                pairs.push((kVTEncodeFrameOptionKey_ForceLTRRefresh, true_value));
            }
            Some(cf_dict(&pairs))
        }
    }

    fn wait_for_output(&self, frame_id: FrameId) -> Result<EncodeResult> {
        let deadline = Instant::now() + ENCODE_TIMEOUT;
        let mut guard = self.slot.result.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match guard.take() {
                Some(r) if r.frame_id == frame_id => return Ok(r),
                // A late result from a frame we already timed out on.
                Some(_) => {}
                None => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(MediaError::Other("encoder did not produce output in time".into()));
            }
            let (next, _) = self
                .slot
                .filled
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
        }
    }

    fn remember_token(&mut self, frame_id: FrameId, token: CFRetained<CFNumber>) {
        if self.tokens.len() == MAX_REMEMBERED_TOKENS {
            self.tokens.pop_front();
        }
        self.tokens.push_back((frame_id, token));
    }

    fn reset_ltr_state(&mut self) {
        self.tokens.clear();
        self.pending_acks.clear();
        self.acked.clear();
    }
}

/// Creates the session with the output callback pointed at `slot`.
///
/// # Safety
/// `slot` must outlive the session.
unsafe fn create_session(
    config: &EncoderConfig,
    codec_type: CMVideoCodecType,
    slot: *const OutputSlot,
    require_hardware: bool,
) -> Result<CFRetained<VTCompressionSession>> {
    // SAFETY: the specification keys are VideoToolbox statics, the out pointer
    // is valid, and the caller keeps `slot` alive.
    unsafe {
        let true_value: &CFType = CFBoolean::new(true).as_ref();
        let mut pairs = vec![(kVTVideoEncoderSpecification_EnableLowLatencyRateControl, true_value)];
        if require_hardware {
            pairs.push((kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, true_value));
        }
        let spec = cf_dict(&pairs);
        let mut raw = ptr::null_mut();
        os(
            "VTCompressionSessionCreate",
            VTCompressionSession::create(
                None,
                config.width as i32,
                config.height as i32,
                codec_type,
                Some(&spec),
                None,
                None,
                Some(output_callback),
                slot as *mut c_void,
                NonNull::from(&mut raw),
            ),
        )?;
        NonNull::new(raw)
            .map(|s| CFRetained::from_raw(s))
            .ok_or(MediaError::Other("VTCompressionSessionCreate returned null".into()))
    }
}

/// # Safety
/// `key` must be a VideoToolbox property key and `value` the type it expects.
unsafe fn set_required(
    session: &VTCompressionSession,
    key: &CFString,
    name: &'static str,
    value: &CFType,
) -> Result<()> {
    let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if status != 0 {
        return Err(MediaError::Os { what: name, status });
    }
    Ok(())
}

/// Appends the format description's parameter sets as Annex-B and returns the
/// NAL length-field size of the samples.
///
/// # Safety
/// `format` must be a valid video format description for `codec`.
unsafe fn write_parameter_sets(
    format: &CMFormatDescription,
    codec: NalCodec,
    write_sets: bool,
    out: &mut Vec<u8>,
) -> Result<usize> {
    let mut count = 0usize;
    let mut length_size = 0i32;
    let mut index = 0usize;
    loop {
        let mut ptr: *const u8 = ptr::null();
        let mut size = 0usize;
        // SAFETY: all out-pointers are valid.
        let status = unsafe {
            match codec {
                NalCodec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    format, index, &mut ptr, &mut size, &mut count, &mut length_size,
                ),
                NalCodec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    format, index, &mut ptr, &mut size, &mut count, &mut length_size,
                ),
            }
        };
        os("CMVideoFormatDescriptionGetParameterSetAtIndex", status)?;
        if write_sets && !ptr.is_null() && size > 0 {
            // SAFETY: CoreMedia returned a pointer and length into the description.
            bitstream::append_annexb_nal(unsafe { slice::from_raw_parts(ptr, size) }, out);
        }
        index += 1;
        if !write_sets || index >= count {
            return Ok(length_size as usize);
        }
    }
}

/// # Safety
/// `sample` must be valid.
unsafe fn sample_is_key(sample: &CMSampleBuffer) -> bool {
    // SAFETY: the key is a CoreMedia static.
    let not_sync = unsafe { sample_attachment(sample, kCMSampleAttachmentKey_NotSync as *const CFString as _) };
    if not_sync.is_null() {
        return true;
    }
    // SAFETY: the attachment is a CFBoolean per the header.
    !unsafe { &*(not_sync as *const CFBoolean) }.as_bool()
}

/// # Safety
/// `sample` must be valid.
unsafe fn sample_ltr_token(sample: &CMSampleBuffer) -> Option<CFRetained<CFNumber>> {
    // SAFETY: the key is a VideoToolbox static; the value is a CFNumber per the header.
    unsafe {
        let v = sample_attachment(
            sample,
            kVTSampleAttachmentKey_RequireLTRAcknowledgementToken as *const CFString as _,
        );
        NonNull::new(v as *mut CFNumber).map(|p| CFRetained::retain(p))
    }
}

/// The bytes of a sample's data buffer, as one contiguous slice.
///
/// # Safety
/// The result is valid for as long as `block`.
unsafe fn block_bytes<'a>(block: &'a CMBlockBuffer, scratch: &'a mut Vec<u8>) -> Result<&'a [u8]> {
    // SAFETY: the out-parameters are valid, and a contiguous range stays mapped
    // for the life of the block buffer.
    unsafe {
        let mut length_here = 0usize;
        let mut total = 0usize;
        let mut ptr = ptr::null_mut();
        os("CMBlockBufferGetDataPointer", block.data_pointer(0, &mut length_here, &mut total, &mut ptr))?;
        if length_here == total {
            return Ok(if ptr.is_null() { &[] } else { slice::from_raw_parts(ptr as *const u8, total) });
        }
        scratch.resize(total, 0);
        let dest = NonNull::new(scratch.as_mut_ptr().cast())
            .ok_or(MediaError::Other("empty scratch buffer".into()))?;
        os("CMBlockBufferCopyDataBytes", block.copy_data_bytes(0, total, dest))?;
        Ok(scratch)
    }
}

impl Encoder for VtEncoder {
    fn caps(&self) -> EncoderCaps {
        self.caps
    }

    fn encode(
        &mut self,
        frame: &CapturedFrame,
        frame_id: FrameId,
        opts: EncodeOpts,
        out: &mut Vec<u8>,
    ) -> Result<EncodedMeta> {
        out.clear();
        let image = self.pixel_buffer(frame)?;
        let (pts, duration) = self.next_timestamps(frame.captured_at);
        let properties = self.frame_properties(opts);

        // SAFETY: `session` and `image` are valid; the callback only touches the
        // slot, which outlives the session.
        let status = unsafe {
            self.session.encode_frame(
                image.raw(),
                pts,
                duration,
                properties.as_deref(),
                frame_id as usize as *mut c_void,
                ptr::null_mut(),
            )
        };
        os("VTCompressionSessionEncodeFrame", status)?;

        let result = self.wait_for_output(frame_id)?;
        os("VTCompressionSession output", result.status)?;
        if result.dropped {
            return Err(MediaError::FrameDropped);
        }
        let sample = result
            .sample
            .ok_or_else(|| MediaError::Other("encoder returned no sample and no drop flag".into()))?;

        // The request has been honoured, so the one-shot flags go only now.
        self.pending_acks.clear();
        let recovery_point = self.recovery_pending;
        self.force_key = false;
        self.force_ltr_refresh = false;
        self.recovery_pending = false;

        // SAFETY: `sample` is a valid sample buffer for the rest of this block.
        let key = unsafe {
            let key = sample_is_key(&sample);
            let format = sample
                .format_description()
                .ok_or_else(|| MediaError::Other("sample has no format description".into()))?;
            let length_size = write_parameter_sets(&format, self.nal, key, out)?;
            let block = sample
                .data_buffer()
                .ok_or_else(|| MediaError::Other("sample has no data buffer".into()))?;
            let bytes = block_bytes(&block, &mut self.scratch)?;
            bitstream::length_prefixed_to_annexb(bytes, length_size, out)
                .map_err(|e| MediaError::Other(format!("encoder output: {e}")))?;
            key
        };

        // An IDR clears the reference buffers, so everything older is unusable.
        if key {
            self.reset_ltr_state();
        }
        // SAFETY: `sample` is valid.
        if let Some(token) = unsafe { sample_ltr_token(&sample) } {
            self.remember_token(frame_id, token);
        }

        Ok(EncodedMeta { frame_id, key, recovery_point })
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if bps == 0 {
            return Err(MediaError::Other("bitrate must be non-zero".into()));
        }
        // SAFETY: the key is a VideoToolbox static and the value a CFNumber.
        unsafe {
            set_required(
                &self.session,
                kVTCompressionPropertyKey_AverageBitRate,
                "AverageBitRate",
                &number_i64(bps as i64),
            )?;
        }
        self.config.bitrate_bps = bps;
        Ok(())
    }

    fn invalidate_from(&mut self, first_lost: FrameId) -> Result<Recovery> {
        // Anything at or after the first loss may reference it, so it can never
        // be a safe long-term reference even if an ack for it shows up later.
        self.tokens.retain(|(id, _)| is_before(*id, first_lost));
        self.pending_acks.retain(|(id, _)| is_before(*id, first_lost));
        self.acked.retain(|id| is_before(*id, first_lost));
        self.recovery_pending = true;
        if self.caps.ltr && !self.acked.is_empty() {
            self.force_ltr_refresh = true;
            Ok(Recovery::RefInvalidated)
        } else {
            // VideoToolbox would produce an IDR anyway; say so instead of
            // claiming a reference was invalidated.
            self.force_key = true;
            Ok(Recovery::Idr)
        }
    }

    fn on_frame_acked(&mut self, frame: FrameId) {
        if let Some(i) = self.tokens.iter().position(|(id, _)| *id == frame) {
            let (id, token) = self.tokens.remove(i).expect("position is valid");
            self.pending_acks.push((id, token));
            if self.acked.len() == MAX_REMEMBERED_ACKS {
                self.acked.pop_front();
            }
            self.acked.push_back(id);
        }
    }

    fn force_idr(&mut self) {
        self.force_key = true;
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // SAFETY: the session is valid; after invalidation no callback can run,
        // so `slot` may be dropped afterwards.
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
        }
    }
}
