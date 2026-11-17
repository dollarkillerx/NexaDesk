//! H.264 through Media Foundation's decoder MFT, in low-latency mode so every access
//! unit comes back as a picture at once. Annex-B in, NV12 out, cropped to the picture
//! (without the coded size's padding).
//!
//! On the GPU when it can: the decoder gets the viewer's shared D3D11 device
//! ([`viewer_device`]) and decodes with the GPU's video engine (DXVA); each picture is
//! copied, still on the GPU, into one of a few NV12 textures that the presenter draws.
//! Without that device, or if the decoder refuses it, it decodes on the CPU into memory.

use std::{
    mem::ManuallyDrop,
    sync::Arc,
    time::{Duration, Instant},
};

use nexa_wire::control::Codec;
use windows::{
    Win32::{
        Graphics::{
            Direct3D11::{D3D11_BOX, ID3D11Device, ID3D11Texture2D},
            Dxgi::Common::DXGI_FORMAT_NV12,
        },
        Media::MediaFoundation::{
            CLSID_MSH264DecoderMFT, CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer,
            IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFTransform,
            MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
            MF_LOW_LATENCY, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE,
            MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_VERSION, MFCreateDXGIDeviceManager,
            MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
            MFSTARTUP_LITE, MFStartup, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
            MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
            MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoArea,
            MFVideoFormat_H264, MFVideoFormat_NV12,
        },
        System::{
            Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx},
            Variant::VARIANT,
        },
    },
    core::Interface,
};

use super::gpu::{GpuFrame, GpuTexture, Kind, texture, viewer_device};
use crate::{CpuFrame, DecodedFrame, Decoder, FrameMeta, MediaError, NativeImage, Result, diag};

fn os(what: &'static str) -> impl FnOnce(windows::core::Error) -> MediaError {
    move |e| MediaError::Os {
        what,
        status: e.code().0,
    }
}

/// Pictures handed to the presenter take turns in this many textures.
const POOL: usize = 3;

/// The GPU side of a decoder: the shared device, its manager, and the textures
/// pictures are copied into.
struct Gpu {
    device: ID3D11Device,
    _manager: IMFDXGIDeviceManager,
    pool: Vec<Arc<GpuTexture>>,
    next: usize,
}

/// One decoded picture, on the GPU or in memory.
enum Picture {
    Cpu(CpuFrame),
    Gpu(GpuFrame),
}

/// Counts for the viewer log: a line every few seconds, and the first errors.
struct Tally {
    since: Instant,
    pictures: u32,
    busy: Duration,
    errors: u32,
    first: bool,
}

pub struct MfDecoder {
    transform: IMFTransform,
    gpu: Option<Gpu>,
    tally: Tally,
    /// The coded size of the current output (with padding, e.g. 1920×1088).
    coded: (u32, u32),
    /// The picture inside it (e.g. 1920×1080).
    shown: (u32, u32),
    provides_samples: bool,
    out_size: u32,
    sample_time: i64,
}

// SAFETY: the transform is created and used on the decode thread, which initialised COM
// for the multithreaded apartment; the MFT is free-threaded within it.
unsafe impl Send for MfDecoder {}

impl MfDecoder {
    pub fn new(codec: Codec) -> Result<Self> {
        if codec != Codec::H264 {
            return Err(MediaError::Unsupported("only H.264 is decoded on Windows"));
        }
        if let Some(device) = viewer_device() {
            match Self::open(Some(device)) {
                Ok(d) => {
                    diag!("decoder: H.264 on the GPU (D3D11)");
                    return Ok(d);
                }
                Err(e) => diag!("decoder: the GPU path failed ({e}); decoding on the CPU"),
            }
        } else {
            diag!("decoder: no shared D3D11 device; decoding on the CPU");
        }
        let d = Self::open(None)?;
        diag!("decoder: H.264 on the CPU");
        Ok(d)
    }

    fn open(device: Option<ID3D11Device>) -> Result<Self> {
        // SAFETY: COM and Media Foundation start-up for this thread (repeat calls are
        // harmless), then the decoder MFT and its types, all with documented arguments.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(os("MFStartup"))?;
            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(os("H.264 decoder"))?;
            // One picture out per access unit in, nothing held back for reordering.
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if let Ok(api) = transform.cast::<ICodecAPI>() {
                let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            }
            // The device before the types: the decoder picks its outputs by it.
            let gpu = match device {
                Some(device) => {
                    let aware = transform
                        .GetAttributes()
                        .and_then(|a| a.GetUINT32(&MF_SA_D3D11_AWARE))
                        .unwrap_or(0)
                        != 0;
                    if !aware {
                        return Err(MediaError::Unsupported("the decoder is not D3D11-aware"));
                    }
                    let (mut token, mut manager) = (0u32, None);
                    MFCreateDXGIDeviceManager(&mut token, &mut manager)
                        .map_err(os("MFCreateDXGIDeviceManager"))?;
                    let manager: IMFDXGIDeviceManager =
                        manager.ok_or(MediaError::Other("no device manager".into()))?;
                    manager
                        .ResetDevice(&device, token)
                        .map_err(os("ResetDevice"))?;
                    transform
                        .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                        .map_err(os("MFT_MESSAGE_SET_D3D_MANAGER"))?;
                    Some(Gpu {
                        device,
                        _manager: manager,
                        pool: Vec::new(),
                        next: 0,
                    })
                }
                None => None,
            };
            let input = MFCreateMediaType().map_err(os("MFCreateMediaType"))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(os("MF_MT_MAJOR_TYPE"))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(os("MF_MT_SUBTYPE"))?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(os("SetInputType"))?;
            let mut this = Self {
                transform,
                gpu,
                tally: Tally {
                    since: Instant::now(),
                    pictures: 0,
                    busy: Duration::ZERO,
                    errors: 0,
                    first: true,
                },
                coded: (0, 0),
                shown: (0, 0),
                provides_samples: false,
                out_size: 0,
                sample_time: 0,
            };
            this.choose_output()?;
            this.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(os("BEGIN_STREAMING"))?;
            this.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(os("START_OF_STREAM"))?;
            Ok(this)
        }
    }

    /// Picks NV12 among the outputs the decoder offers, and reads its sizes.
    fn choose_output(&mut self) -> Result<()> {
        // SAFETY: enumerating and setting the decoder's own media types.
        unsafe {
            let mut chosen: Option<IMFMediaType> = None;
            for i in 0..64 {
                let Ok(t) = self.transform.GetOutputAvailableType(0, i) else {
                    break;
                };
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    chosen = Some(t);
                    break;
                }
            }
            let t = chosen.ok_or(MediaError::Unsupported("the decoder offers no NV12 output"))?;
            self.transform
                .SetOutputType(0, &t, 0)
                .map_err(os("SetOutputType"))?;
            let size = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            self.coded = ((size >> 32) as u32, size as u32);
            self.shown = aperture(&t).unwrap_or(self.coded);
            let info = self
                .transform
                .GetOutputStreamInfo(0)
                .map_err(os("GetOutputStreamInfo"))?;
            self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.out_size = info.cbSize.max(self.coded.0 * self.coded.1 * 3 / 2).max(1);
            Ok(())
        }
    }

    fn input_sample(&mut self, au: &[u8]) -> Result<IMFSample> {
        // SAFETY: a fresh buffer of `au.len()` bytes, locked for the copy and unlocked after.
        unsafe {
            let buffer =
                MFCreateMemoryBuffer(au.len().max(1) as u32).map_err(os("MFCreateMemoryBuffer"))?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None).map_err(os("Lock"))?;
            std::ptr::copy_nonoverlapping(au.as_ptr(), data, au.len());
            buffer.Unlock().map_err(os("Unlock"))?;
            buffer
                .SetCurrentLength(au.len() as u32)
                .map_err(os("SetCurrentLength"))?;
            let sample = MFCreateSample().map_err(os("MFCreateSample"))?;
            sample.AddBuffer(&buffer).map_err(os("AddBuffer"))?;
            // Any increasing time will do: the decoder only orders by it.
            self.sample_time += 1;
            sample
                .SetSampleTime(self.sample_time)
                .map_err(os("SetSampleTime"))?;
            Ok(sample)
        }
    }

    /// The picture in a GPU sample, copied out of the decoder's own surfaces (which it
    /// reuses) into the next pool texture.
    fn copy_gpu(&mut self, sample: &IMFSample) -> Result<GpuFrame> {
        let (w, h) = (self.shown.0 & !1, self.shown.1 & !1);
        let gpu = self
            .gpu
            .as_mut()
            .ok_or(MediaError::Other("no GPU".into()))?;
        if gpu
            .pool
            .first()
            .is_none_or(|t| (t.width, t.height) != (w, h))
        {
            gpu.pool.clear();
            for _ in 0..POOL {
                gpu.pool.push(Arc::new(GpuTexture {
                    texture: texture(&gpu.device, w, h, DXGI_FORMAT_NV12, Kind::Shader)?,
                    device: gpu.device.clone(),
                    width: w,
                    height: h,
                }));
            }
            gpu.next = 0;
        }
        let target = gpu.pool[gpu.next].clone();
        gpu.next = (gpu.next + 1) % gpu.pool.len();
        // SAFETY: the sample's DXGI buffer names a texture (array slice) of the shared
        // device; a box within the picture is copied into a texture of its size.
        unsafe {
            let buffer = sample.GetBufferByIndex(0).map_err(os("GetBufferByIndex"))?;
            let dxgi: IMFDXGIBuffer = buffer.cast().map_err(os("IMFDXGIBuffer"))?;
            let mut raw = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)
                .map_err(os("IMFDXGIBuffer::GetResource"))?;
            let source = ID3D11Texture2D::from_raw(raw);
            let slice = dxgi
                .GetSubresourceIndex()
                .map_err(os("GetSubresourceIndex"))?;
            let context = gpu
                .device
                .GetImmediateContext()
                .map_err(os("GetImmediateContext"))?;
            let area = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w,
                bottom: h,
                back: 1,
            };
            context.CopySubresourceRegion(&target.texture, 0, 0, 0, 0, &source, slice, Some(&area));
        }
        Ok(GpuFrame::from_texture(target))
    }

    /// One decoded picture, if the decoder has one.
    fn output(&mut self) -> Result<Option<Picture>> {
        for _ in 0..3 {
            // SAFETY: an output sample the decoder fills (or one it provides), read and
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
                        // The first key frame told the decoder the picture's size.
                        self.choose_output()?;
                        continue;
                    }
                    Err(e) => return Err(os("ProcessOutput")(e)),
                }
                let Some(sample) = sample else {
                    return Ok(None);
                };
                let on_gpu = sample
                    .GetBufferByIndex(0)
                    .is_ok_and(|b| b.cast::<IMFDXGIBuffer>().is_ok());
                if self.gpu.is_some() && on_gpu {
                    return self.copy_gpu(&sample).map(|f| Some(Picture::Gpu(f)));
                }
                return self.read(&sample).map(|f| Some(Picture::Cpu(f)));
            }
        }
        Ok(None)
    }

    /// The picture in `sample`, without the coded size's padding.
    fn read(&self, sample: &IMFSample) -> Result<CpuFrame> {
        let (cw, ch) = self.coded;
        let (w, h) = (self.shown.0 & !1, self.shown.1 & !1);
        if w == 0 || h == 0 || cw < w || ch < h {
            return Err(MediaError::Other(format!(
                "decoder output {cw}x{ch}, picture {w}x{h}"
            )));
        }
        // SAFETY: the sample's buffer is locked for reading, rows are copied within its
        // pitch and plane offsets, and it is unlocked before returning.
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(os("ConvertToContiguousBuffer"))?;
            let mut frame = CpuFrame {
                width: w,
                height: h,
                y: vec![0; (w * h) as usize],
                uv: vec![0; (w * h / 2) as usize],
            };
            let copy = |base: *const u8, pitch: usize, frame: &mut CpuFrame| {
                let (w, h, ch) = (w as usize, h as usize, ch as usize);
                for row in 0..h {
                    let src = std::slice::from_raw_parts(base.add(row * pitch), w);
                    frame.y[row * w..(row + 1) * w].copy_from_slice(src);
                }
                // NV12: the interleaved chroma plane follows the luma plane's coded rows.
                let uv_base = base.add(pitch * ch);
                for row in 0..h / 2 {
                    let src = std::slice::from_raw_parts(uv_base.add(row * pitch), w);
                    frame.uv[row * w..(row + 1) * w].copy_from_slice(src);
                }
            };
            if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                let mut line0 = std::ptr::null_mut();
                let mut pitch = 0i32;
                two_d.Lock2D(&mut line0, &mut pitch).map_err(os("Lock2D"))?;
                if pitch > 0 {
                    copy(line0, pitch as usize, &mut frame);
                }
                let _ = two_d.Unlock2D();
                if pitch <= 0 {
                    return Err(MediaError::Other("bottom-up NV12".into()));
                }
            } else {
                let mut data = std::ptr::null_mut();
                let mut len = 0u32;
                buffer
                    .Lock(&mut data, None, Some(&mut len))
                    .map_err(os("Lock"))?;
                let needed = cw as usize * ch as usize * 3 / 2;
                if (len as usize) >= needed {
                    copy(data, cw as usize, &mut frame);
                }
                let _ = buffer.Unlock();
                if (len as usize) < needed {
                    return Err(MediaError::Other(format!(
                        "decoded {len} bytes, need {needed}"
                    )));
                }
            }
            Ok(frame)
        }
    }
}

/// The picture's area within the coded frame, when the decoder says.
fn aperture(t: &IMFMediaType) -> Option<(u32, u32)> {
    let mut area = MFVideoArea::default();
    // SAFETY: the blob is an MFVideoArea, read into one of exactly its size.
    let ok = unsafe {
        t.GetBlob(
            &MF_MT_MINIMUM_DISPLAY_APERTURE,
            std::slice::from_raw_parts_mut(
                (&mut area as *mut MFVideoArea).cast::<u8>(),
                size_of::<MFVideoArea>(),
            ),
            None,
        )
    }
    .is_ok();
    (ok && area.Area.cx > 0 && area.Area.cy > 0)
        .then_some((area.Area.cx as u32, area.Area.cy as u32))
}

impl MfDecoder {
    fn decode_one(&mut self, au: &[u8]) -> Result<Picture> {
        let sample = self.input_sample(au)?;
        // SAFETY: feeding the decoder its own input type's sample.
        let fed = unsafe { self.transform.ProcessInput(0, &sample, 0) };
        let mut picture = None;
        match fed {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                // Full: take what it has, then give it this one.
                picture = self.output()?;
                // SAFETY: as above.
                unsafe { self.transform.ProcessInput(0, &sample, 0) }
                    .map_err(os("ProcessInput"))?;
            }
            Err(e) => return Err(os("ProcessInput")(e)),
        }
        if let Some(newer) = self.output()? {
            picture = Some(newer);
        }
        // In low-latency mode nothing comes out only before the first key frame.
        picture.ok_or(MediaError::FrameDropped)
    }

    /// The viewer log: the first picture, the first errors, and a line every 5 s.
    fn note(&mut self, result: &Result<Picture>, took: Duration) {
        let t = &mut self.tally;
        match result {
            Ok(_) => {
                t.pictures += 1;
                t.busy += took;
                if t.first {
                    t.first = false;
                    let (w, h) = self.shown;
                    diag!("decoder: first picture {w}x{h} ({} ms)", took.as_millis());
                }
            }
            Err(MediaError::FrameDropped) => {}
            Err(e) => {
                t.errors += 1;
                if t.errors <= 5 {
                    diag!("decoder error: {e}");
                }
            }
        }
        let elapsed = t.since.elapsed();
        if elapsed >= Duration::from_secs(5) {
            if t.pictures > 0 || t.errors > 0 {
                diag!(
                    "decoder: {:.1} pictures/s, {:.1} ms each, {} errors, on the {}",
                    f64::from(t.pictures) / elapsed.as_secs_f64(),
                    t.busy.as_secs_f64() * 1000.0 / f64::from(t.pictures.max(1)),
                    t.errors,
                    if self.gpu.is_some() { "GPU" } else { "CPU" }
                );
            }
            *t = Tally {
                since: Instant::now(),
                pictures: 0,
                busy: Duration::ZERO,
                errors: t.errors,
                first: false,
            };
        }
    }
}

impl Decoder for MfDecoder {
    fn decode(&mut self, au: &[u8], meta: FrameMeta) -> Result<DecodedFrame> {
        let started = Instant::now();
        let result = self.decode_one(au);
        self.note(&result, started.elapsed());
        let image = match result? {
            Picture::Cpu(f) => NativeImage::Cpu(Arc::new(f)),
            Picture::Gpu(f) => NativeImage::Texture(f),
        };
        Ok(DecodedFrame { image, meta })
    }
}
