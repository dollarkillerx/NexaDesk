//! System audio on the host: started and stopped by the viewer's `AudioControl`, and
//! only ever created for a session granted the audio permission (`session::run`
//! makes no [`AudioSwitch`] otherwise).

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering::Relaxed},
};

use nexa_session::host::HostMedia;
use tokio::{sync::watch, task::JoinHandle};

use crate::{AgentEvent, AgentUi, Platform};

pub(crate) struct AudioSwitch {
    want: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl AudioSwitch {
    pub fn spawn(
        platform: Arc<dyn Platform>,
        ui: Arc<dyn AgentUi>,
        media: Arc<HostMedia>,
        display_id: Arc<AtomicU32>,
    ) -> Self {
        let (want, mut rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut running = false;
            // Changes are applied one at a time, in order; starting can take a while.
            while rx.changed().await.is_ok() {
                let wanted = *rx.borrow_and_update();
                if wanted == running {
                    continue;
                }
                let (p, m, id) = (platform.clone(), media.clone(), display_id.load(Relaxed));
                let result = tokio::task::spawn_blocking(move || {
                    if !wanted {
                        m.stop_audio();
                        return Ok(());
                    }
                    let capture = p.audio_capture(id)?;
                    let encoder = p.audio_encoder()?;
                    m.start_audio(capture, encoder);
                    Ok::<(), String>(())
                })
                .await
                .unwrap_or_else(|_| Err("audio setup panicked".into()));
                match result {
                    Ok(()) => running = wanted,
                    Err(e) => ui.event(AgentEvent::Warning(format!(
                        "system audio unavailable: {e}"
                    ))),
                }
            }
            if running {
                let _ = tokio::task::spawn_blocking(move || media.stop_audio()).await;
            }
        });
        Self { want, task }
    }

    pub fn set(&self, enabled: bool) {
        self.want.send_replace(enabled);
    }

    /// Stops audio and waits until it has.
    pub async fn shutdown(self) {
        drop(self.want);
        let _ = self.task.await;
    }
}
