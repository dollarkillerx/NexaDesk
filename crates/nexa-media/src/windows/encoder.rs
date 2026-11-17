//! H.264 through Media Foundation's encoder MFT, set for low latency: no B-frames,
//! constant bitrate, one output per input. NV12 in, Annex-B out.
//!
//! This is Microsoft's software encoder, synchronous and the same on every PC, used up
//! to 1080p. The GPU's own encoder (`hwenc`) takes over where there is one.

use std::mem::ManuallyDrop;

use nexa_wire::control::Codec;
use windows::{
    Win32::{
        Media::MediaFoundation::{
            CLSID_MSH264EncoderMFT, CODECAPI_AVEncCommonMeanBitRate,
            CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
            CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
            ICodecAPI, IMFMediaType, IMFSample, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
            MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
            MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER,
            MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_VERSION,
            MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
            MFSTARTUP_LITE, MFSampleExtension_CleanPoint, MFStartup,
            MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
            MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264,
            MFVideoFormat_NV12, MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR,
            eAVEncH264VProfile_High,
        },
        System::{
            Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx},
            Variant::VARIANT,
        },
    },
    core::{GUID, Interface},
};

use crate::{
    CapturedFrame, EncodeOpts, EncodedMeta, Encoder, EncoderCaps, EncoderConfig, FrameId,
    MediaError, NativeImage, Recovery, Result,
};

pub(super) fn os(what: &'static str) -> impl FnOnce(windows::core::Error) -> MediaError {
    move |e| MediaError::Os {
        what,
        status: e.code().0,
    }
}

/// 100-nanosecond units per second: Media Foundation's clock.
pub(super) const TICKS: i64 = 10_000_000;

pub struct MfEncoder {
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    width: u32,
    height: u32,
    fps: u32,
    frame_index: i64,
    force_key: bool,
    /// SPS and PPS, Annex-B, for key frames that come without them.
    header: Vec<u8>,
    provides_samples: bool,
    out_size: u32,
}

// SAFETY: the transform is created and used on the media thread, which initialised
// COM for the multithreaded apartment; the MFT is free-threaded within it.
unsafe impl Send for MfEncoder {}

pub(super) fn video_type(subtype: &GUID, config: &EncoderConfig) -> Result<IMFMediaType> {
    // SAFETY: setting documented attributes on a fresh media type.
    unsafe {
        let t = MFCreateMediaType().map_err(os("MFCreateMediaType"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(os("MF_MT_MAJOR_TYPE"))?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)
            .map_err(os("MF_MT_SUBTYPE"))?;
        t.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(config.width) << 32) | u64::from(config.height),
        )
        .map_err(os("MF_MT_FRAME_SIZE"))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(config.fps.max(1)) << 32) | 1)
            .map_err(os("MF_MT_FRAME_RATE"))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)
            .map_err(os("MF_MT_PIXEL_ASPECT_RATIO"))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(os("MF_MT_INTERLACE_MODE"))?;
        Ok(t)
    }
}

impl MfEncoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        if config.codec != Codec::H264 {
            return Err(MediaError::Unsupported("only H.264 on Windows for now"));
        }
        let (width, height) = (config.width & !1, config.height & !1);
        // SAFETY: COM and Media Foundation start-up for this thread (repeat calls are
        // harmless), then the encoder MFT and its types, all with documented arguments.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(os("MFStartup"))?;
            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(os("H.264 encoder"))?;
            let config = EncoderConfig {
                width,
                height,
                ..config
            };
            // The output type first: the encoder offers inputs to match it.
            let output = video_type(&MFVideoFormat_H264, &config)?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)
                .map_err(os("MF_MT_AVG_BITRATE"))?;
            output
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .map_err(os("MF_MT_MPEG2_PROFILE"))?;
            transform
                .SetOutputType(0, &output, 0)
                .map_err(os("SetOutputType"))?;
            let input = video_type(&MFVideoFormat_NV12, &config)?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(os("SetInputType"))?;

            let codec: Option<ICodecAPI> = transform.cast().ok();
            if let Some(api) = &codec {
                let set = |key: &GUID, value: VARIANT| {
                    let _ = api.SetValue(key, &value);
                };
                set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
                set(
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                );
                set(
                    &CODECAPI_AVEncCommonMeanBitRate,
                    VARIANT::from(config.bitrate_bps),
                );
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                // Key frames only when asked for, or every ten seconds.
                set(
                    &CODECAPI_AVEncMPVGOPSize,
                    VARIANT::from(config.fps.max(1) * 10),
                );
            }
            let info = transform
                .GetOutputStreamInfo(0)
                .map_err(os("GetOutputStreamInfo"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(os("BEGIN_STREAMING"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(os("START_OF_STREAM"))?;
            let mut this = Self {
                transform,
                codec,
                width,
                height,
                fps: config.fps.max(1),
                frame_index: 0,
                force_key: true,
                header: Vec::new(),
                provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                out_size: info.cbSize.max(width * height * 3 / 2),
            };
            this.read_header();
            Ok(this)
        }
    }

    /// SPS and PPS from the current output type, when the encoder puts them there.
    fn read_header(&mut self) {
        // SAFETY: reading a blob attribute of the encoder's own output type.
        unsafe {
            let Ok(t) = self.transform.GetOutputCurrentType(0) else {
                return;
            };
            let Ok(len) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else {
                return;
            };
            let mut blob = vec![0u8; len as usize];
            if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
                .is_ok()
            {
                self.header = blob;
            }
        }
    }

    fn input_sample(&self, frame: &CapturedFrame) -> Result<IMFSample> {
        let read;
        let f = match &frame.image {
            NativeImage::Cpu(f) => &**f,
            // A frame on the GPU, when the hardware encoder could not take it.
            NativeImage::Texture(t) => {
                read = t.read()?;
                &read
            }
        };
        if f.width != self.width || f.height != self.height {
            return Err(MediaError::Other(format!(
                "frame is {}x{}, the encoder {}x{}",
                f.width, f.height, self.width, self.height
            )));
        }
        let len = (f.y.len() + f.uv.len()) as u32;
        // SAFETY: a fresh buffer of `len` bytes, locked for the copy and unlocked after.
        unsafe {
            let buffer = MFCreateMemoryBuffer(len).map_err(os("MFCreateMemoryBuffer"))?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None).map_err(os("Lock"))?;
            std::ptr::copy_nonoverlapping(f.y.as_ptr(), data, f.y.len());
            std::ptr::copy_nonoverlapping(f.uv.as_ptr(), data.add(f.y.len()), f.uv.len());
            buffer.Unlock().map_err(os("Unlock"))?;
            buffer
                .SetCurrentLength(len)
                .map_err(os("SetCurrentLength"))?;
            let sample = MFCreateSample().map_err(os("MFCreateSample"))?;
            sample.AddBuffer(&buffer).map_err(os("AddBuffer"))?;
            sample
                .SetSampleTime(self.frame_index * TICKS / i64::from(self.fps))
                .map_err(os("SetSampleTime"))?;
            sample
                .SetSampleDuration(TICKS / i64::from(self.fps))
                .map_err(os("SetSampleDuration"))?;
            Ok(sample)
        }
    }

    /// One encoded frame, if the encoder has one ready.
    fn output(&mut self, out: &mut Vec<u8>) -> Result<Option<bool>> {
        for _ in 0..2 {
            // SAFETY: an output buffer the encoder fills (or one it provides), read and
            // released before returning; the sample is dropped through ManuallyDrop.
            unsafe {
                let provided = if self.provides_samples {
                    None
                } else {
                    let buffer =
                        MFCreateMemoryBuffer(self.out_size).map_err(os("MFCreateMemoryBuffer"))?;
                    let sample = MFCreateSample().map_err(os("MFCreateSample"))?;
                    sample.AddBuffer(&buffer).map_err(os("AddBuffer"))?;
                    Some(sample)
                };
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(provided),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
                let sample = ManuallyDrop::take(&mut buffers[0].pSample);
                drop(ManuallyDrop::take(&mut buffers[0].pEvents));
                match result {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        // The encoder settled its output format: take what it offers.
                        if let Ok(t) = self.transform.GetOutputAvailableType(0, 0) {
                            let _ = self.transform.SetOutputType(0, &t, 0);
                        }
                        self.read_header();
                        continue;
                    }
                    Err(e) => return Err(os("ProcessOutput")(e)),
                }
                let Some(sample) = sample else {
                    return Ok(None);
                };
                let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                let buffer = sample
                    .ConvertToContiguousBuffer()
                    .map_err(os("ConvertToContiguousBuffer"))?;
                let mut data = std::ptr::null_mut();
                let mut len = 0u32;
                buffer
                    .Lock(&mut data, None, Some(&mut len))
                    .map_err(os("Lock"))?;
                let bytes = std::slice::from_raw_parts(data, len as usize);
                out.clear();
                if key && !has_sps(bytes) {
                    out.extend_from_slice(&self.header);
                }
                out.extend_from_slice(bytes);
                let _ = buffer.Unlock();
                return Ok(Some(key));
            }
        }
        Ok(None)
    }
}

/// Whether an Annex-B stream carries a sequence parameter set (NAL type 7).
pub(super) fn has_sps(annex_b: &[u8]) -> bool {
    annex_b
        .windows(4)
        .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)
}

impl Encoder for MfEncoder {
    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            rfi: false,
            ltr: false,
            dynamic_bitrate: self.codec.is_some(),
            hardware: false,
        }
    }

    fn encode(
        &mut self,
        frame: &CapturedFrame,
        frame_id: FrameId,
        opts: EncodeOpts,
        out: &mut Vec<u8>,
    ) -> Result<EncodedMeta> {
        if (opts.force_key || self.force_key)
            && let Some(api) = &self.codec
        {
            // SAFETY: a documented codec property.
            let _ =
                unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
        }
        self.force_key = false;
        let sample = self.input_sample(frame)?;
        // SAFETY: a valid input sample for stream 0.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }.map_err(os("ProcessInput"))?;
        self.frame_index += 1;
        match self.output(out)? {
            Some(key) => Ok(EncodedMeta {
                frame_id,
                key,
                recovery_point: false,
            }),
            // The encoder held this frame back: nothing to send for it.
            None => Err(MediaError::FrameDropped),
        }
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if let Some(api) = &self.codec {
            // SAFETY: a documented codec property, changeable while encoding.
            unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps)) }
                .map_err(os("CODECAPI_AVEncCommonMeanBitRate"))?;
        }
        Ok(())
    }

    fn invalidate_from(&mut self, _first_lost: FrameId) -> Result<Recovery> {
        self.force_key = true;
        Ok(Recovery::Idr)
    }

    fn on_frame_acked(&mut self, _frame: FrameId) {}

    fn force_idr(&mut self) {
        self.force_key = true;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_a_sequence_parameter_set() {
        assert!(super::has_sps(&[0, 0, 0, 1, 0x67, 1, 2]));
        assert!(!super::has_sps(&[0, 0, 0, 1, 0x65, 1, 2]));
    }
}
