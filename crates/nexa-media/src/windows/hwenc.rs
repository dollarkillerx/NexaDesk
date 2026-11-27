//! The GPU's own H.264 encoder (NVENC, Quick Sync, AMF), as Media Foundation exposes
//! it: a hardware MFT, usually asynchronous, reading NV12 textures through a DXGI
//! device manager on the capturer's D3D11 device. Low latency, no B-frames, constant
//! bitrate, Annex-B out — the same stream as the software encoder.
//!
//! Every hardware MFT the system lists is tried in turn (on a laptop with two GPUs only
//! the one driving the display takes this device). [`WinEncoder`] falls back to the
//! software encoder when none works, and [`hardware_h264`] decides once per process,
//! with a real 4K test frame, whether this PC advertises the hardware limits.

use std::{
    mem::ManuallyDrop,
    sync::{
        Arc, OnceLock,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use nexa_wire::control::{Chroma, Codec, Mode};
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
                ID3D11Texture2D,
            },
            Dxgi::Common::DXGI_FORMAT_NV12,
        },
        Media::MediaFoundation::{
            CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
            CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
            CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer,
            IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFSample, IMFShutdown,
            IMFTransform, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MEError, METransformHaveOutput,
            METransformNeedInput, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
            MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE,
            MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateDXGIDeviceManager,
            MFCreateDXGISurfaceBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_LITE,
            MFSampleExtension_CleanPoint, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
            MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_STREAMING,
            MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
            MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
            MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, eAVEncCommonRateControlMode_CBR,
            eAVEncH264VProfile_High,
        },
        System::{
            Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree},
            Variant::VARIANT,
        },
    },
    core::{GUID, Interface},
};

use super::{
    encoder::{MfEncoder, TICKS, has_sps, os, video_type},
    gpu::{GpuFrame, GpuTexture, Kind, texture},
};
use crate::{
    CapturedFrame, EncodeOpts, EncodedMeta, Encoder, EncoderCaps, EncoderConfig, FrameId,
    MediaError, NativeImage, Recovery, Result,
};

/// How long one frame may take to come out before it counts as held back.
const OUTPUT_WAIT: Duration = Duration::from_millis(40);
/// How long the encoder may take to ask for the next frame before it counts as stuck.
const INPUT_WAIT: Duration = Duration::from_millis(500);

/// A COM pointer moved to the event thread; the MFT is free-threaded (MTA).
struct SendGenerator(IMFMediaEventGenerator);
// SAFETY: see above.
unsafe impl Send for SendGenerator {}

enum Event {
    NeedInput,
    HaveOutput,
    Failed(i32),
}

pub struct HwEncoder {
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    device: ID3D11Device,
    /// `None` for a synchronous hardware MFT.
    events: Option<Receiver<Event>>,
    pump: Option<JoinHandle<()>>,
    need_input: u32,
    have_output: u32,
    width: u32,
    height: u32,
    fps: u32,
    frame_index: i64,
    force_key: bool,
    header: Vec<u8>,
    provides_samples: bool,
    out_size: u32,
}

// SAFETY: the MFT and its interfaces live in the multithreaded apartment and are
// free-threaded; this value is used from one thread at a time (`&mut self`).
unsafe impl Send for HwEncoder {}

fn set(api: &ICodecAPI, key: &GUID, value: VARIANT) {
    // SAFETY: a documented codec property; encoders ignore ones they do not have.
    let _ = unsafe { api.SetValue(key, &value) };
}

impl HwEncoder {
    /// The first hardware H.264 MFT that takes `device` and `config`.
    pub fn new(device: &ID3D11Device, config: EncoderConfig) -> Result<Self> {
        let config = EncoderConfig {
            width: config.width & !1,
            height: config.height & !1,
            ..config
        };
        // SAFETY: COM and MF start-up for this thread, then the documented enumeration;
        // the array and every activation object in it are released before returning.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(os("MFStartup"))?;
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let output = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut list,
                &mut count,
            )
            .map_err(os("MFTEnumEx"))?;
            let mut found = Err(MediaError::Unsupported("no hardware H.264 encoder"));
            if !list.is_null() {
                let all = std::slice::from_raw_parts_mut(list, count as usize);
                for slot in all.iter_mut() {
                    if found.is_err()
                        && let Some(activate) = slot.as_ref()
                    {
                        found = Self::open(activate, device, &config);
                        if found.is_err() {
                            let _ = activate.ShutdownObject();
                        }
                    }
                    drop(slot.take());
                }
                CoTaskMemFree(Some(list.cast()));
            }
            found
        }
    }

    /// # Safety
    /// COM is initialised on this thread.
    unsafe fn open(
        activate: &IMFActivate,
        device: &ID3D11Device,
        config: &EncoderConfig,
    ) -> Result<Self> {
        // SAFETY: per the contract; documented calls on a fresh MFT.
        unsafe {
            let transform: IMFTransform =
                activate.ActivateObject().map_err(os("ActivateObject"))?;
            let attributes = transform.GetAttributes().map_err(os("GetAttributes"))?;
            let asynchronous = attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if asynchronous {
                attributes
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(os("MF_TRANSFORM_ASYNC_UNLOCK"))?;
            }
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);

            let (mut token, mut manager) = (0u32, None);
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .map_err(os("MFCreateDXGIDeviceManager"))?;
            let manager: IMFDXGIDeviceManager =
                manager.ok_or(MediaError::Other("no device manager".into()))?;
            manager
                .ResetDevice(device, token)
                .map_err(os("ResetDevice"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .map_err(os("MFT_MESSAGE_SET_D3D_MANAGER"))?;

            // Rate control before the types: some encoders read it only then.
            let codec: Option<ICodecAPI> = transform.cast().ok();
            if let Some(api) = &codec {
                set(api, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
                set(
                    api,
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                );
                set(
                    api,
                    &CODECAPI_AVEncCommonMeanBitRate,
                    VARIANT::from(config.bitrate_bps),
                );
                set(
                    api,
                    &CODECAPI_AVEncMPVDefaultBPictureCount,
                    VARIANT::from(0u32),
                );
                // Key frames only when asked for, or every ten seconds.
                set(
                    api,
                    &CODECAPI_AVEncMPVGOPSize,
                    VARIANT::from(config.fps.max(1) * 10),
                );
            }
            let output = video_type(&MFVideoFormat_H264, config)?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)
                .map_err(os("MF_MT_AVG_BITRATE"))?;
            output
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .map_err(os("MF_MT_MPEG2_PROFILE"))?;
            transform
                .SetOutputType(0, &output, 0)
                .map_err(os("SetOutputType"))?;
            let input = video_type(&MFVideoFormat_NV12, config)?;
            if transform.SetInputType(0, &input, 0).is_err() {
                // Some want one of their own offers, with the size and rate filled in.
                let mut set_one = false;
                for i in 0..32 {
                    let Ok(t) = transform.GetInputAvailableType(0, i) else {
                        break;
                    };
                    if t.GetGUID(&windows::Win32::Media::MediaFoundation::MF_MT_SUBTYPE)
                        .ok()
                        != Some(MFVideoFormat_NV12)
                    {
                        continue;
                    }
                    let ours = video_type(&MFVideoFormat_NV12, config)?;
                    let _ = ours.CopyAllItems(&t);
                    let _ = t.SetUINT64(
                        &windows::Win32::Media::MediaFoundation::MF_MT_FRAME_SIZE,
                        (u64::from(config.width) << 32) | u64::from(config.height),
                    );
                    let _ = t.SetUINT64(
                        &windows::Win32::Media::MediaFoundation::MF_MT_FRAME_RATE,
                        (u64::from(config.fps.max(1)) << 32) | 1,
                    );
                    if transform.SetInputType(0, &t, 0).is_ok() {
                        set_one = true;
                        break;
                    }
                }
                if !set_one {
                    return Err(MediaError::Unsupported(
                        "the hardware encoder takes no NV12 at this size",
                    ));
                }
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
            let (events, pump) = if asynchronous {
                let generator: IMFMediaEventGenerator =
                    transform.cast().map_err(os("IMFMediaEventGenerator"))?;
                let (tx, rx) = mpsc::channel();
                let generator = SendGenerator(generator);
                let pump = thread::Builder::new()
                    .name("encoder-events".into())
                    .spawn(move || {
                        let generator = generator;
                        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                        // Ends when the MFT shuts down (GetEvent fails) or the encoder goes.
                        while let Ok(event) = generator
                            .0
                            .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
                        {
                            let kind = event.GetType().unwrap_or(0);
                            let status = event.GetStatus().map(|s| s.0).unwrap_or(0);
                            let sent = if status < 0 || kind == MEError.0 as u32 {
                                tx.send(Event::Failed(status))
                            } else if kind == METransformNeedInput.0 as u32 {
                                tx.send(Event::NeedInput)
                            } else if kind == METransformHaveOutput.0 as u32 {
                                tx.send(Event::HaveOutput)
                            } else {
                                Ok(())
                            };
                            if sent.is_err() {
                                return;
                            }
                        }
                    })
                    .map_err(|e| MediaError::Other(e.to_string()))?;
                (Some(rx), Some(pump))
            } else {
                (None, None)
            };
            let mut this = Self {
                transform,
                codec,
                _manager: manager,
                device: device.clone(),
                events,
                pump,
                need_input: 0,
                have_output: 0,
                width: config.width,
                height: config.height,
                fps: config.fps.max(1),
                frame_index: 0,
                force_key: true,
                header: Vec::new(),
                provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                out_size: info.cbSize.max(config.width * config.height * 3 / 2),
            };
            this.read_header();
            Ok(this)
        }
    }

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

    /// Waits up to `wait` for one event. False when none came.
    fn pump(&mut self, wait: Duration) -> Result<bool> {
        let Some(events) = &self.events else {
            return Ok(false);
        };
        match events.recv_timeout(wait) {
            Ok(Event::NeedInput) => self.need_input += 1,
            Ok(Event::HaveOutput) => self.have_output += 1,
            Ok(Event::Failed(status)) => {
                return Err(MediaError::Os {
                    what: "hardware encoder",
                    status,
                });
            }
            Err(RecvTimeoutError::Timeout) => return Ok(false),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(MediaError::Other("the hardware encoder stopped".into()));
            }
        }
        Ok(true)
    }

    fn sample(&self, frame: &GpuFrame) -> Result<IMFSample> {
        // SAFETY: a DXGI buffer over the frame's texture (it holds its own reference),
        // in a fresh sample with this frame's time.
        unsafe {
            let buffer =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, frame.texture(), 0, false)
                    .map_err(os("MFCreateDXGISurfaceBuffer"))?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>()
                && let Ok(len) = b2.GetContiguousLength()
            {
                let _ = buffer.SetCurrentLength(len);
            }
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

    /// Takes one output. `None`: nothing ready after all.
    fn output(&mut self, out: &mut Vec<u8>) -> Result<Option<bool>> {
        for _ in 0..3 {
            // SAFETY: as in the software encoder: an output buffer the encoder fills or
            // provides, read and released before returning.
            unsafe {
                let provided = if self.provides_samples {
                    None
                } else {
                    let buffer =
                        windows::Win32::Media::MediaFoundation::MFCreateMemoryBuffer(self.out_size)
                            .map_err(os("MFCreateMemoryBuffer"))?;
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
                        if let Ok(t) = self.transform.GetOutputAvailableType(0, 0) {
                            let _ = self.transform.SetOutputType(0, &t, 0);
                        }
                        self.read_header();
                        if self.events.is_some() {
                            // The frame itself comes with the next HaveOutput.
                            return Ok(None);
                        }
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
                let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
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

    fn encode_gpu(
        &mut self,
        frame: &GpuFrame,
        frame_id: FrameId,
        opts: EncodeOpts,
        out: &mut Vec<u8>,
    ) -> Result<EncodedMeta> {
        if frame.device().as_raw() != self.device.as_raw() {
            return Err(MediaError::Other("the frame is from another device".into()));
        }
        if (frame.width(), frame.height()) != (self.width, self.height) {
            return Err(MediaError::Other(format!(
                "frame is {}x{}, the encoder {}x{}",
                frame.width(),
                frame.height(),
                self.width,
                self.height
            )));
        }
        if (opts.force_key || self.force_key)
            && let Some(api) = &self.codec
        {
            set(api, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32));
        }
        self.force_key = false;
        let sample = self.sample(frame)?;
        if self.events.is_some() {
            let until = Instant::now() + INPUT_WAIT;
            while self.need_input == 0 {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() || !self.pump(left)? && Instant::now() >= until {
                    return Err(MediaError::Other(
                        "the hardware encoder stopped asking for frames".into(),
                    ));
                }
            }
            self.need_input -= 1;
        }
        // SAFETY: a valid input sample for stream 0, offered when the encoder asked.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }.map_err(os("ProcessInput"))?;
        self.frame_index += 1;
        if self.events.is_some() {
            let until = Instant::now() + OUTPUT_WAIT;
            while self.have_output == 0 {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() || !self.pump(left)? {
                    // Held back: it comes out with a later frame.
                    return Err(MediaError::FrameDropped);
                }
            }
            self.have_output -= 1;
        }
        match self.output(out)? {
            Some(key) => Ok(EncodedMeta {
                frame_id,
                key,
                recovery_point: false,
            }),
            None => Err(MediaError::FrameDropped),
        }
    }

    /// An output that is ready now (see [`Encoder::take_ready`]), without waiting.
    fn take_ready(&mut self, frame_id: FrameId, out: &mut Vec<u8>) -> Result<Option<EncodedMeta>> {
        if self.events.is_none() {
            return Ok(None);
        }
        while self.pump(Duration::ZERO)? {}
        if self.have_output == 0 {
            return Ok(None);
        }
        self.have_output -= 1;
        Ok(self.output(out)?.map(|key| EncodedMeta {
            frame_id,
            key,
            recovery_point: false,
        }))
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if let Some(api) = &self.codec {
            // SAFETY: a documented codec property, changeable while encoding.
            unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps)) }
                .map_err(os("CODECAPI_AVEncCommonMeanBitRate"))?;
        }
        Ok(())
    }
}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        // SAFETY: ending the stream and shutting the MFT down, which also ends the
        // event thread's GetEvent.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            match self.transform.cast::<IMFShutdown>() {
                Ok(s) => {
                    let _ = s.Shutdown();
                    if let Some(t) = self.pump.take() {
                        let _ = t.join();
                    }
                }
                // Without IMFShutdown the event thread cannot be woken: leave it be.
                Err(_) => drop(self.pump.take()),
            }
        }
    }
}

/// Whether this PC has a working hardware H.264 encoder, tried once with a 4K frame.
pub fn hardware_h264() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| probe().is_ok())
}

fn probe() -> Result<()> {
    // SAFETY: a device on the default adapter with video support, and one NV12 frame
    // encoded on it; everything is released before returning.
    let device = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut device = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .map_err(os("D3D11CreateDevice"))?;
        let device: ID3D11Device = device.ok_or(MediaError::Other("no device".into()))?;
        if let Ok(m) = device.cast::<ID3D11Multithread>() {
            let _ = m.SetMultithreadProtected(true);
        }
        device
    };
    let (w, h) = (3840, 2160);
    let mut encoder = HwEncoder::new(
        &device,
        EncoderConfig {
            codec: Codec::H264,
            chroma: Chroma::Yuv420,
            mode: Mode::Balanced,
            width: w,
            height: h,
            fps: 60,
            bitrate_bps: 30_000_000,
        },
    )?;
    let frame = GpuFrame::from_texture(Arc::new(GpuTexture {
        texture: texture(&device, w, h, DXGI_FORMAT_NV12, Kind::Target)?,
        device: device.clone(),
        width: w,
        height: h,
    }));
    let mut out = Vec::new();
    // A held-back first frame is fine; the second must come out.
    for id in 0..3 {
        match encoder.encode_gpu(&frame, id, EncodeOpts::default(), &mut out) {
            Ok(_) if !out.is_empty() => return Ok(()),
            Ok(_) | Err(MediaError::FrameDropped) => {}
            Err(e) => return Err(e),
        }
    }
    Err(MediaError::Other(
        "the hardware encoder gave no output".into(),
    ))
}

/// The encoder for a stream: hardware when the frames are on the GPU and an encoder
/// takes them, otherwise Microsoft's software encoder. Decided on the first frame,
/// since a hardware encoder must share the frames' device.
pub struct WinEncoder {
    config: EncoderConfig,
    state: State,
    force_key: bool,
}

enum State {
    Waiting,
    Hardware(HwEncoder),
    Software(MfEncoder),
}

/// The encoder for `config` on this PC.
pub fn encoder(config: EncoderConfig) -> Result<Box<dyn Encoder>> {
    if config.codec == Codec::H264 && hardware_h264() {
        return Ok(Box::new(WinEncoder {
            config,
            state: State::Waiting,
            force_key: true,
        }));
    }
    Ok(Box::new(MfEncoder::new(config)?))
}

impl WinEncoder {
    fn start(&mut self, frame: &CapturedFrame) -> Result<()> {
        if !matches!(self.state, State::Waiting) {
            return Ok(());
        }
        self.state = match &frame.image {
            NativeImage::Texture(f) => match HwEncoder::new(f.device(), self.config) {
                Ok(h) => State::Hardware(h),
                Err(_) => State::Software(MfEncoder::new(self.config)?),
            },
            NativeImage::Cpu(_) => State::Software(MfEncoder::new(self.config)?),
        };
        Ok(())
    }
}

impl Encoder for WinEncoder {
    fn caps(&self) -> EncoderCaps {
        match &self.state {
            State::Software(s) => s.caps(),
            _ => EncoderCaps {
                rfi: false,
                ltr: false,
                dynamic_bitrate: true,
                hardware: true,
            },
        }
    }

    fn encode(
        &mut self,
        frame: &CapturedFrame,
        frame_id: FrameId,
        opts: EncodeOpts,
        out: &mut Vec<u8>,
    ) -> Result<EncodedMeta> {
        self.start(frame)?;
        let opts = EncodeOpts {
            force_key: opts.force_key || std::mem::take(&mut self.force_key),
        };
        let result = match (&mut self.state, &frame.image) {
            (State::Hardware(h), NativeImage::Texture(f)) => h.encode_gpu(f, frame_id, opts, out),
            (State::Software(s), _) => return s.encode(frame, frame_id, opts, out),
            _ => Err(MediaError::Other(
                "the frame does not suit the encoder".into(),
            )),
        };
        if let Err(e) = &result
            && !matches!(e, MediaError::FrameDropped)
        {
            // The hardware encoder failed: software from the next frame on.
            self.state = State::Software(MfEncoder::new(self.config)?);
            self.force_key = true;
        }
        result
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        self.config.bitrate_bps = bps;
        match &mut self.state {
            State::Hardware(h) => h.set_bitrate(bps),
            State::Software(s) => s.set_bitrate(bps),
            State::Waiting => Ok(()),
        }
    }

    fn invalidate_from(&mut self, first_lost: FrameId) -> Result<Recovery> {
        match &mut self.state {
            State::Software(s) => s.invalidate_from(first_lost),
            _ => {
                self.force_key = true;
                Ok(Recovery::Idr)
            }
        }
    }

    fn on_frame_acked(&mut self, frame: FrameId) {
        if let State::Software(s) = &mut self.state {
            s.on_frame_acked(frame);
        }
    }

    fn force_idr(&mut self) {
        match &mut self.state {
            State::Software(s) => s.force_idr(),
            _ => self.force_key = true,
        }
    }

    fn take_ready(&mut self, frame_id: FrameId, out: &mut Vec<u8>) -> Result<Option<EncodedMeta>> {
        match &mut self.state {
            State::Hardware(h) => h.take_ready(frame_id, out),
            _ => Ok(None),
        }
    }
}
