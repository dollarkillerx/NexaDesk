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

use crate::deps::{AudioOutput, AudioRender, Deps};

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

    /// Mute (`false`) stops the output device; unmute starts it on an empty buffer.
    pub fn set_enabled(&mut self, deps: &dyn Deps, enabled: bool) -> Result<(), String> {
        if enabled == self.enabled.load(Relaxed) {
            return Ok(());
        }
        if enabled {
            self.playout
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
            self.output = Some(deps.audio_output(self.render())?);
            self.enabled.store(true, Relaxed);
        } else {
            self.enabled.store(false, Relaxed);
            self.output = None;
        }
        Ok(())
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
