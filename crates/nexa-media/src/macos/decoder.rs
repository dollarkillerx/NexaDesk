//! VideoToolbox decoder taking Annex-B access units.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex};

use nexa_wire::control::Codec;
use objc2_core_foundation::{CFBoolean, CFRetained};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    CMVideoFormatDescriptionGetDimensions,
};
use objc2_core_video::{
    CVImageBuffer, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
};

use super::bitstream::{self, NalCodec, ParameterSets};
use super::{PixelBuffer, cf_dict, number_i64, os};
use crate::{DecodedFrame, Decoder, FrameMeta, MediaError, NativeImage, Result};

/// kVTInvalidSessionErr: the OS tore the session down (sleep, GPU reset).
const INVALID_SESSION: i32 = -12903;
/// A sanity bound against a hostile SPS asking for an absurd surface.
const MAX_DIMENSION: i32 = 16384;

/// What kind of buffers the decoder hands back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecoderOutput {
    /// The default: GPU-compressed buffers on Apple Silicon. Cheapest for display,
    /// but CPU access to the planes crashes.
    #[default]
    Display,
    /// `420v` buffers that can be read from the CPU, for tests.
    CpuReadable420v,
}

struct DecodeResult {
    status: i32,
    dropped: bool,
    image: Option<PixelBuffer>,
}

type OutputSlot = Mutex<Option<DecodeResult>>;

unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: `refcon` is the Arc<OutputSlot> pointer the decoder keeps alive
    // until after the session is invalidated, and `image` is valid for this call.
    let slot = unsafe { &*(refcon as *const OutputSlot) };
    let image = NonNull::new(image).map(|p| unsafe { PixelBuffer::retain(p) });
    let result = DecodeResult { status, dropped: flags.contains(VTDecodeInfoFlags::FrameDropped), image };
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
}

struct Session {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    sets: ParameterSets,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: no callback can run after invalidation, so the slot may go.
        unsafe { self.session.invalidate() };
    }
}

pub struct VtDecoder {
    nal: NalCodec,
    output: DecoderOutput,
    session: Option<Session>,
    slot: Arc<OutputSlot>,
    sample: Vec<u8>,
}

// SAFETY: a VTDecompressionSession may be used from any thread, one at a time,
// which `&mut self` guarantees.
unsafe impl Send for VtDecoder {}

impl VtDecoder {
    pub fn new(codec: Codec, output: DecoderOutput) -> Result<Self> {
        let nal = match codec {
            Codec::H264 => NalCodec::H264,
            Codec::Hevc => NalCodec::Hevc,
            Codec::Av1 | Codec::Unknown => return Err(MediaError::Unsupported("decoder codec must be H.264 or HEVC")),
        };
        Ok(Self { nal, output, session: None, slot: Arc::new(Mutex::new(None)), sample: Vec::new() })
    }

    fn create_format(&self, sets: &ParameterSets) -> Result<CFRetained<CMFormatDescription>> {
        let slices: Vec<&[u8]> = sets.ordered().collect();
        let mut pointers: Vec<NonNull<u8>> = Vec::with_capacity(slices.len());
        for s in &slices {
            pointers.push(NonNull::new(s.as_ptr() as *mut u8).ok_or(MediaError::Other("empty parameter set".into()))?);
        }
        let mut sizes: Vec<usize> = slices.iter().map(|s| s.len()).collect();
        let pointers_ptr = NonNull::new(pointers.as_mut_ptr()).ok_or(MediaError::Other("no parameter sets".into()))?;
        let sizes_ptr = NonNull::new(sizes.as_mut_ptr()).ok_or(MediaError::Other("no parameter sets".into()))?;
        let mut raw: *const CMFormatDescription = ptr::null();
        // SAFETY: the pointer and size arrays are parallel and live across the
        // call, which copies the bytes. A length field of 4 matches the samples
        // written by `payload_to_length_prefixed`.
        let status = unsafe {
            match self.nal {
                NalCodec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None,
                    pointers.len(),
                    pointers_ptr,
                    sizes_ptr,
                    4,
                    NonNull::from(&mut raw),
                ),
                NalCodec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    None,
                    pointers.len(),
                    pointers_ptr,
                    sizes_ptr,
                    4,
                    None,
                    NonNull::from(&mut raw),
                ),
            }
        };
        os("CMVideoFormatDescriptionCreateFromParameterSets", status)?;
        let format = NonNull::new(raw as *mut CMFormatDescription)
            .map(|f| unsafe { CFRetained::from_raw(f) })
            .ok_or(MediaError::Other("format description is null".into()))?;
        // SAFETY: `format` is valid.
        let dims = unsafe { CMVideoFormatDescriptionGetDimensions(&format) };
        if dims.width <= 0 || dims.height <= 0 || dims.width > MAX_DIMENSION || dims.height > MAX_DIMENSION {
            return Err(MediaError::Other(format!("implausible stream size {}x{}", dims.width, dims.height)));
        }
        Ok(format)
    }

    fn ensure_session(&mut self, sets: ParameterSets) -> Result<()> {
        if !sets.is_complete(self.nal) {
            return Err(MediaError::Other("incomplete parameter sets".into()));
        }
        if self.session.as_ref().is_some_and(|s| s.sets == sets) {
            return Ok(());
        }
        let format = self.create_format(&sets)?;
        // Tear down first: the old session owns references to its surfaces.
        self.session = None;
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let session = unsafe { create_session(&format, self.output, Arc::as_ptr(&self.slot))? };
        self.session = Some(Session { session, format, sets });
        Ok(())
    }

    fn create_sample(&self, format: &CMFormatDescription, frame_id: u32) -> Result<CFRetained<CMSampleBuffer>> {
        let bytes = self.sample.as_ptr() as *mut c_void;
        let source = NonNull::new(bytes).ok_or(MediaError::Other("empty sample".into()))?;
        // SAFETY: the block allocates its own memory (NULL block + NULL block
        // allocator) and receives a copy of `self.sample`.
        unsafe {
            let mut block_raw = ptr::null_mut();
            let flags = objc2_core_media::kCMBlockBufferAssureMemoryNowFlag;
            os(
                "CMBlockBufferCreateWithMemoryBlock",
                CMBlockBuffer::create_with_memory_block(
                    None,
                    ptr::null_mut(),
                    self.sample.len(),
                    None,
                    ptr::null(),
                    0,
                    self.sample.len(),
                    flags,
                    NonNull::from(&mut block_raw),
                ),
            )?;
            let block: CFRetained<CMBlockBuffer> = NonNull::new(block_raw)
                .map(|b| CFRetained::from_raw(b))
                .ok_or(MediaError::Other("block buffer is null".into()))?;
            os(
                "CMBlockBufferReplaceDataBytes",
                CMBlockBuffer::replace_data_bytes(source, &block, 0, self.sample.len()),
            )?;

            let timing = CMSampleTimingInfo {
                duration: objc2_core_media::kCMTimeInvalid,
                presentationTimeStamp: CMTime::new(i64::from(frame_id), 1000),
                decodeTimeStamp: objc2_core_media::kCMTimeInvalid,
            };
            let size = self.sample.len();
            let mut sample_raw = ptr::null_mut();
            os(
                "CMSampleBufferCreateReady",
                CMSampleBuffer::create_ready(
                    None,
                    Some(&block),
                    Some(format),
                    1,
                    1,
                    &timing,
                    1,
                    &size,
                    NonNull::from(&mut sample_raw),
                ),
            )?;
            NonNull::new(sample_raw)
                .map(|s| CFRetained::from_raw(s))
                .ok_or(MediaError::Other("sample buffer is null".into()))
        }
    }
}

/// # Safety
/// `slot` must outlive the session.
unsafe fn create_session(
    format: &CMFormatDescription,
    output: DecoderOutput,
    slot: *const OutputSlot,
) -> Result<CFRetained<VTDecompressionSession>> {
    // SAFETY: the record and attributes live across the call, which copies
    // them, and `slot` outlives the session per the caller.
    unsafe {
        let attrs = match output {
            DecoderOutput::Display => None,
            DecoderOutput::CpuReadable420v => Some(cf_dict(&[
                (
                    kCVPixelBufferPixelFormatTypeKey,
                    &number_i64(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i64),
                ),
                (kCVPixelBufferIOSurfacePropertiesKey, cf_dict(&[]).as_ref()),
                (kCVPixelBufferMetalCompatibilityKey, CFBoolean::new(true).as_ref()),
            ])),
        };
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback),
            decompressionOutputRefCon: slot as *mut c_void,
        };
        let mut raw = ptr::null_mut();
        os(
            "VTDecompressionSessionCreate",
            VTDecompressionSession::create(
                None,
                format,
                None,
                attrs.as_deref(),
                &record,
                NonNull::from(&mut raw),
            ),
        )?;
        NonNull::new(raw)
            .map(|s| CFRetained::from_raw(s))
            .ok_or(MediaError::Other("VTDecompressionSessionCreate returned null".into()))
    }
}

impl Decoder for VtDecoder {
    fn decode(&mut self, au: &[u8], meta: FrameMeta) -> Result<DecodedFrame> {
        let parsed =
            bitstream::parse_access_unit(self.nal, au).map_err(|e| MediaError::Other(format!("bad access unit: {e}")))?;
        if !parsed.sets.is_empty() {
            self.ensure_session(parsed.sets)?;
        }
        if parsed.payload.is_empty() {
            return Err(MediaError::Other("access unit has no slices".into()));
        }
        if self.session.is_none() {
            return Err(MediaError::Other("decoder needs a key frame first".into()));
        }
        bitstream::payload_to_length_prefixed(&parsed.payload, &mut self.sample)
            .map_err(|e| MediaError::Other(format!("bad access unit: {e}")))?;

        let session = self.session.as_ref().expect("checked above");
        let sample = self.create_sample(&session.format, meta.frame_id)?;
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let mut flags_out = VTDecodeInfoFlags::empty();
        // SAFETY: both objects are valid. Without EnableAsynchronousDecompression
        // the callback has run by the time this returns.
        let status = unsafe {
            session.session.decode_frame(&sample, VTDecodeFrameFlags::empty(), ptr::null_mut(), &mut flags_out)
        };
        if status != 0 {
            if status == INVALID_SESSION {
                self.session = None;
            }
            return Err(MediaError::Os { what: "VTDecompressionSessionDecodeFrame", status });
        }

        let result = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        match result {
            None => Err(MediaError::Other("decoder returned no output".into())),
            Some(r) if r.status != 0 => {
                if r.status == INVALID_SESSION {
                    self.session = None;
                }
                Err(MediaError::Os { what: "VTDecompressionSession output", status: r.status })
            }
            Some(DecodeResult { image: Some(image), .. }) => {
                Ok(DecodedFrame { image: NativeImage::PixelBuffer(image), meta })
            }
            Some(DecodeResult { dropped: true, .. }) => Err(MediaError::FrameDropped),
            Some(_) => Err(MediaError::Other("decoder produced no image".into())),
        }
    }
}
