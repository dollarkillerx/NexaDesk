//! Lets the view come and go while the render thread keeps running (§10.2).

use std::sync::{Arc, Mutex, MutexGuard};

use nexa_media::{DecodedFrame, PresentMode, PresentStat, Presenter};

type Attached = Arc<Mutex<Box<dyn Presenter>>>;

fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Owned by the session; [`Proxy`] is what the render thread holds.
#[derive(Default)]
pub struct PresenterSlot {
    attached: Mutex<Option<Attached>>,
    pending_size: Mutex<Option<(u32, u32, f32)>>,
    mode: Mutex<Option<PresentMode>>,
}

impl PresenterSlot {
    pub fn attach(&self, presenter: Box<dyn Presenter>) {
        *lock(&self.attached) = Some(Arc::new(Mutex::new(presenter)));
    }

    /// Never waits for the render thread, which may be inside a present; the presenter
    /// itself keeps the layer alive until that call returns.
    pub fn detach(&self) {
        *lock(&self.attached) = None;
    }

    pub fn is_attached(&self) -> bool {
        lock(&self.attached).is_some()
    }

    pub fn resize(&self, w: u32, h: u32, scale: f32) {
        *lock(&self.pending_size) = Some((w, h, scale));
    }

    pub fn set_mode(&self, mode: PresentMode) {
        *lock(&self.mode) = Some(mode);
    }

    pub fn current_mode(&self) -> PresentMode {
        lock(&self.mode).unwrap_or(PresentMode::Vsync)
    }

    pub fn proxy(self: &Arc<Self>) -> Box<dyn Presenter> {
        Box::new(Proxy(self.clone()))
    }
}

struct Proxy(Arc<PresenterSlot>);

impl Presenter for Proxy {
    fn resize(&mut self, width_px: u32, height_px: u32, scale: f32) -> nexa_media::Result<()> {
        self.0.resize(width_px, height_px, scale);
        Ok(())
    }

    fn set_mode(&mut self, mode: PresentMode) -> nexa_media::Result<()> {
        self.0.set_mode(mode);
        Ok(())
    }

    fn present(&mut self, frame: &DecodedFrame) -> nexa_media::Result<PresentStat> {
        let attached = lock(&self.0.attached).clone();
        let Some(attached) = attached else {
            return Ok(PresentStat::default()); // no view yet; the frame stays available for repaint
        };
        let mut presenter = lock(&attached);
        if let Some((w, h, scale)) = lock(&self.0.pending_size).take() {
            presenter.resize(w, h, scale)?;
        }
        if let Some(mode) = *lock(&self.0.mode) {
            presenter.set_mode(mode)?;
        }
        presenter.present(frame)
    }
}
