//! System audio on Windows (docs/WINDOWS.md stage 4): WASAPI loopback of the default
//! output device on the host, WASAPI playback on the viewer, Opus through libopus
//! (`nexa-opus`). Same contract as macOS: 48 kHz stereo interleaved float, 10 ms frames.
//!
//! Each stream runs on its own thread, which owns every COM object (they are not
//! `Send`). A stream that fails (the default device changed, headphones unplugged) is
//! opened again on the new default device; only the first open's failure is reported.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_FAILED},
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
            MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
        },
        System::{
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                CoUninitialize,
            },
            Threading::{CreateEventW, WaitForSingleObject},
        },
    },
    core::GUID,
};

use crate::{
    AUDIO_CHANNELS, AUDIO_FRAME_LEN, AUDIO_SAMPLE_RATE, AudioCapture, AudioChunk, AudioDecoder,
    AudioEncoder, MediaError, Result,
    resample::{Convert, Sample},
};

fn os(what: &'static str) -> impl FnOnce(windows::core::Error) -> MediaError {
    move |e| MediaError::Os {
        what,
        status: e.code().0,
    }
}

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// The tail all KSDATAFORMAT_SUBTYPE GUIDs share; `data1` is the format tag.
const SUBTYPE_TAIL: (u16, u16, [u8; 8]) = (
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);
/// 100 ns units: the buffer each stream asks for.
const BUFFER_100NS: i64 = 200_000;
/// How long the loopback may deliver nothing before silence is sent in its place.
const SILENCE_AFTER: Duration = Duration::from_millis(20);

/// What travels: 48 kHz stereo 32-bit float.
fn wire_format() -> WAVEFORMATEX {
    let block = (AUDIO_CHANNELS * 4) as u16;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: AUDIO_CHANNELS as u16,
        nSamplesPerSec: AUDIO_SAMPLE_RATE,
        nAvgBytesPerSec: AUDIO_SAMPLE_RATE * block as u32,
        nBlockAlign: block,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// COM for this thread, released when the thread's stream ends.
struct Com;

impl Com {
    fn init() -> Self {
        // SAFETY: COM for this thread only, released in Drop.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Com
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: pairs with the CoInitializeEx above.
        unsafe { CoUninitialize() }
    }
}

fn default_output() -> Result<IAudioClient> {
    // SAFETY: plain COM activation of the default output device's audio client.
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(os("MMDeviceEnumerator"))?;
        let device = devices
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(os("no default audio output"))?;
        device
            .Activate::<IAudioClient>(CLSCTX_ALL, None)
            .map_err(os("IAudioClient"))
    }
}

// ---------------------------------------------------------------------------------
// Sample conversion, for a loopback that only works in the device's own format.
// ---------------------------------------------------------------------------------

/// `None` for formats [`Convert`] does not read.
///
/// # Safety
/// `format` must point at a valid WAVEFORMATEX, with the extensible part when its tag
/// says so.
unsafe fn convert_of(format: *const WAVEFORMATEX) -> Option<Convert> {
    // SAFETY: per the contract.
    let f = unsafe { *format };
    let tag = if f.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
        // SAFETY: an extensible format carries the longer struct.
        let x = unsafe { std::ptr::read_unaligned(format.cast::<WAVEFORMATEXTENSIBLE>()) };
        let g: GUID = x.SubFormat;
        if (g.data2, g.data3, g.data4) != SUBTYPE_TAIL {
            return None;
        }
        g.data1 as u16
    } else {
        f.wFormatTag
    };
    let sample = match (tag, f.wBitsPerSample) {
        (WAVE_FORMAT_IEEE_FLOAT, 32) => Sample::F32,
        (WAVE_FORMAT_PCM, 16) => Sample::I16,
        (WAVE_FORMAT_PCM, 24) => Sample::I24,
        (WAVE_FORMAT_PCM, 32) => Sample::I32,
        _ => return None,
    };
    Some(Convert::new(sample, f.nChannels as usize, f.nSamplesPerSec))
}

// ---------------------------------------------------------------------------------
// Loopback capture (host).
// ---------------------------------------------------------------------------------

struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// `None` when Windows converts into the wire format for us.
    convert: Option<Convert>,
}

impl Loopback {
    fn open() -> Result<Self> {
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK;
        let convert_flags =
            flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let wire = wire_format();
        // SAFETY: initialising a fresh client with a valid format; the mix format is
        // freed with CoTaskMemFree once read.
        unsafe {
            let mut client = default_output()?;
            let mut convert = None;
            if client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    convert_flags,
                    BUFFER_100NS,
                    0,
                    &wire,
                    None,
                )
                .is_err()
            {
                // Older Windows: capture in the device's format and convert here.
                client = default_output()?;
                let mix = client.GetMixFormat().map_err(os("GetMixFormat"))?;
                let c = convert_of(mix);
                let result =
                    client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_100NS, 0, mix, None);
                CoTaskMemFree(Some(mix.cast()));
                result.map_err(os("IAudioClient::Initialize (loopback)"))?;
                convert =
                    Some(c.ok_or(MediaError::Unsupported("the output device's sample format"))?);
            }
            let capture: IAudioCaptureClient =
                client.GetService().map_err(os("IAudioCaptureClient"))?;
            client.Start().map_err(os("IAudioClient::Start"))?;
            Ok(Self {
                client,
                capture,
                convert,
            })
        }
    }

    /// Everything captured since the last call, as wire samples.
    fn drain(&mut self, out: &mut Vec<f32>) -> Result<()> {
        // SAFETY: GetBuffer/ReleaseBuffer in pairs; the buffer is read only in between,
        // for the frame count it reports.
        unsafe {
            loop {
                let next = self
                    .capture
                    .GetNextPacketSize()
                    .map_err(os("GetNextPacketSize"))?;
                if next == 0 {
                    return Ok(());
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(os("IAudioCaptureClient::GetBuffer"))?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                let n = frames as usize;
                match &mut self.convert {
                    None if silent => out.resize(out.len() + n * AUDIO_CHANNELS, 0.0),
                    None => out.extend_from_slice(std::slice::from_raw_parts(
                        data.cast::<f32>(),
                        n * AUDIO_CHANNELS,
                    )),
                    Some(c) if silent => c.push(None, n, out),
                    Some(c) => {
                        let bytes = std::slice::from_raw_parts(data, n * c.bytes_per_frame());
                        c.push(Some(bytes), 0, out);
                    }
                }
                self.capture
                    .ReleaseBuffer(frames)
                    .map_err(os("IAudioCaptureClient::ReleaseBuffer"))?;
            }
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: stopping the stream this value started.
        let _ = unsafe { self.client.Stop() };
    }
}

/// The host's system audio: what the default output device plays.
pub struct WasapiLoopback {
    rx: Receiver<AudioChunk>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WasapiLoopback {
    pub fn start() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("audio-loopback".into())
                .spawn(move || loopback_thread(tx, ready_tx, stop))
                .map_err(|e| MediaError::Other(e.to_string()))?
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                rx,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(MediaError::Other("the audio thread ended".into())),
        }
    }
}

fn loopback_thread(tx: Sender<AudioChunk>, ready: SyncSender<Result<()>>, stop: Arc<AtomicBool>) {
    let _com = Com::init();
    let mut ready = Some(ready);
    while !stop.load(Relaxed) {
        let mut stream = match Loopback::open() {
            Ok(s) => s,
            Err(e) => {
                if let Some(r) = ready.take() {
                    crate::diag!("system sound capture could not start: {e}");
                    let _ = r.send(Err(e));
                    return;
                }
                thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        if let Some(r) = ready.take() {
            let _ = r.send(Ok(()));
            crate::diag!("system sound capture started (WASAPI)");
        }
        // Windows sends nothing while nothing plays; the viewer gets silence instead.
        let mut last = Instant::now();
        let mut samples = Vec::new();
        while !stop.load(Relaxed) {
            thread::sleep(Duration::from_millis(5));
            samples.clear();
            if stream.drain(&mut samples).is_err() {
                break; // the device went away: open the new default
            }
            let now = Instant::now();
            if samples.is_empty() {
                let quiet = now - last;
                if quiet < SILENCE_AFTER {
                    continue;
                }
                let frames = (quiet.as_secs_f64() * AUDIO_SAMPLE_RATE as f64) as usize;
                samples.resize(frames * AUDIO_CHANNELS, 0.0);
            }
            last = now;
            let chunk = AudioChunk {
                samples: std::mem::take(&mut samples),
                captured_at: now,
            };
            if tx.send(chunk).is_err() {
                return;
            }
        }
    }
}

impl AudioCapture for WasapiLoopback {
    fn next(&mut self, timeout: Duration) -> Result<Option<AudioChunk>> {
        match self.rx.recv_timeout(timeout) {
            Ok(chunk) => Ok(Some(chunk)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(MediaError::Other("system audio capture ended".into()))
            }
        }
    }
}

impl Drop for WasapiLoopback {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------------
// Playback (viewer).
// ---------------------------------------------------------------------------------

/// Fills interleaved wire samples; the `Instant` is when the first one is heard.
pub type Render = Box<dyn FnMut(&mut [f32], Instant) + Send>;

struct Playback {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    frames: u32,
    latency: Duration,
}

impl Playback {
    fn open() -> Result<Self> {
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        // SAFETY: a fresh client initialised with a valid format; the event handle is
        // owned by the value and closed in Drop.
        unsafe {
            let client = default_output()?;
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    BUFFER_100NS,
                    0,
                    &wire_format(),
                    None,
                )
                .map_err(os("IAudioClient::Initialize (playback)"))?;
            let event = CreateEventW(None, false, false, None).map_err(os("CreateEventW"))?;
            let stream = Self {
                render: match client.GetService() {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = CloseHandle(event);
                        return Err(os("IAudioRenderClient")(e));
                    }
                },
                frames: client.GetBufferSize().unwrap_or(0),
                latency: Duration::from_nanos(
                    client.GetStreamLatency().unwrap_or(0).max(0) as u64 * 100,
                ),
                client,
                event,
            };
            stream
                .client
                .SetEventHandle(stream.event)
                .map_err(os("SetEventHandle"))?;
            if stream.frames == 0 {
                return Err(MediaError::Other("the audio output has no buffer".into()));
            }
            Ok(stream)
        }
    }

    /// Fills whatever room the buffer has.
    fn fill(&mut self, render: &mut Render) -> Result<()> {
        // SAFETY: GetBuffer/ReleaseBuffer in pairs, writing exactly the frames asked for
        // in the wire format the stream was opened with.
        unsafe {
            let queued = self
                .client
                .GetCurrentPadding()
                .map_err(os("GetCurrentPadding"))?;
            let room = self.frames.saturating_sub(queued);
            if room == 0 {
                return Ok(());
            }
            let data = self
                .render
                .GetBuffer(room)
                .map_err(os("IAudioRenderClient::GetBuffer"))?;
            let out =
                std::slice::from_raw_parts_mut(data.cast::<f32>(), room as usize * AUDIO_CHANNELS);
            let ahead = Duration::from_secs_f64(queued as f64 / AUDIO_SAMPLE_RATE as f64);
            render(out, Instant::now() + ahead + self.latency);
            self.render
                .ReleaseBuffer(room, 0)
                .map_err(os("IAudioRenderClient::ReleaseBuffer"))
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        // SAFETY: stopping this stream and closing the event it owns.
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

/// Plays the session's audio on the default output device. Dropping it stops.
pub struct WasapiOutput {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WasapiOutput {
    pub fn start(render: Render) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("audio-output".into())
                .spawn(move || playback_thread(render, ready_tx, stop))
                .map_err(|e| MediaError::Other(e.to_string()))?
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(MediaError::Other("the audio thread ended".into())),
        }
    }
}

fn playback_thread(mut render: Render, ready: SyncSender<Result<()>>, stop: Arc<AtomicBool>) {
    let _com = Com::init();
    let mut ready = Some(ready);
    while !stop.load(Relaxed) {
        let started = Playback::open().and_then(|mut s| {
            s.fill(&mut render)?;
            // SAFETY: starting the stream just filled.
            unsafe { s.client.Start() }.map_err(os("IAudioClient::Start"))?;
            Ok(s)
        });
        let mut stream = match started {
            Ok(s) => s,
            Err(e) => {
                if let Some(r) = ready.take() {
                    crate::diag!("sound playback could not start: {e}");
                    let _ = r.send(Err(e));
                    return;
                }
                thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        if let Some(r) = ready.take() {
            let _ = r.send(Ok(()));
            crate::diag!("sound playback started (WASAPI)");
        }
        while !stop.load(Relaxed) {
            // SAFETY: waiting on the event this stream owns.
            if unsafe { WaitForSingleObject(stream.event, 200) } == WAIT_FAILED {
                break;
            }
            if stream.fill(&mut render).is_err() {
                break; // the device went away: open the new default
            }
        }
    }
}

impl Drop for WasapiOutput {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------------
// Opus.
// ---------------------------------------------------------------------------------

fn opus(e: nexa_opus::Error) -> MediaError {
    MediaError::Os {
        what: e.what,
        status: e.code,
    }
}

pub struct OpusEncoder(nexa_opus::Encoder);

impl OpusEncoder {
    pub fn new(bitrate_bps: u32) -> Result<Self> {
        nexa_opus::Encoder::new(bitrate_bps).map(Self).map_err(opus)
    }
}

impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<()> {
        self.0.encode(pcm, out).map_err(opus)
    }
}

pub struct OpusDecoder(nexa_opus::Decoder);

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        nexa_opus::Decoder::new().map(Self).map_err(opus)
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<()> {
        debug_assert_eq!(out.len(), AUDIO_FRAME_LEN);
        self.0.decode(packet, out).map_err(opus)
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}
