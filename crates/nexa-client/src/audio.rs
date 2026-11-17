//! Viewer audio: AudioPacket datagrams into the jitter buffer, the output device
//! pulling from it. Exists only in sessions granted the audio permission.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering::Relaxed},
};

use nexa_session::{
    audio::{AudioDecoder, Playout, PlayoutConfig, PlayoutStats},
    viewer::AudioSink,
};

use crate::deps::{AudioOutput, AudioRender};

pub struct AudioPlayback {
    playout: Arc<Mutex<Playout>>,
    enabled: Arc<AtomicBool>,
    output: Option<Box<dyn AudioOutput>>,
}

impl AudioPlayback {
    pub fn new(decoder: Box<dyn AudioDecoder>) -> Self {
        Self {
            playout: Arc::new(Mutex::new(Playout::new(decoder, PlayoutConfig::default()))),
            enabled: Arc::new(AtomicBool::new(false)),
            output: None,
        }
    }

    /// For `ViewerOptions::audio`: packets are kept only while playing.
    pub fn sink(&self) -> AudioSink {
        let (playout, enabled) = (self.playout.clone(), self.enabled.clone());
        Arc::new(move |h, payload, at| {
            if enabled.load(Relaxed)
                && let Ok(mut p) = playout.lock()
            {
                p.push(h, payload, at);
            }
        })
    }

    /// What the output device calls. Never waits: a busy buffer plays silence.
    pub fn render(&self) -> AudioRender {
        let playout = self.playout.clone();
        Box::new(move |out, at| match playout.try_lock() {
            Ok(mut p) => p.render(out, at),
            Err(_) => out.fill(0.0),
        })
    }

    /// Unmuting, first half: an empty buffer and what the new output device will pull
    /// from, or `None` if already playing. Starting the device can take a while (a
    /// Bluetooth or AirPlay output), so the caller does that without holding this.
    pub fn prepare_enable(&mut self) -> Option<AudioRender> {
        if self.enabled.load(Relaxed) {
            return None;
        }
        self.playout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset();
        Some(self.render())
    }

    /// Unmuting, second half: the started device plays from now on.
    pub fn install(&mut self, output: Box<dyn AudioOutput>) {
        self.output = Some(output);
        self.enabled.store(true, Relaxed);
    }

    /// Mutes. Returns the output device to stop, which the caller drops outside any lock.
    #[must_use]
    pub fn disable(&mut self) -> Option<Box<dyn AudioOutput>> {
        self.enabled.store(false, Relaxed);
        self.output.take()
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Relaxed)
    }

    pub fn stats(&self) -> PlayoutStats {
        self.playout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stats()
    }
}
