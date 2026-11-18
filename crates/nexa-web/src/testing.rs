//! A host for this crate's tests: each part it is asked for comes from a closure the test
//! gives, and everything else is refused.

use std::time::Duration;

use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig, MediaError};
use nexa_session::input::InputSink;

use crate::{
    BrowserAnswer, BrowserRequest, WebClipboard, WebDisplay, WebEvent, WebHost, WebScreen,
};

type Make<T> = Box<dyn Fn() -> T + Send + Sync>;
type MakeCapturer =
    Box<dyn Fn(&CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> + Send + Sync>;

pub(crate) struct FakeHost {
    capturer: MakeCapturer,
    encoder: Make<nexa_media::Result<Box<dyn Encoder>>>,
    input_sink: Make<Result<Box<dyn InputSink>, String>>,
    clipboard: Make<Result<Box<dyn WebClipboard>, String>>,
    displays: Vec<WebDisplay>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self {
            capturer: Box::new(|_| Err(MediaError::Unsupported("test"))),
            encoder: Box::new(|| Err(MediaError::Unsupported("test"))),
            input_sink: Box::new(|| Err("test".into())),
            clipboard: Box::new(|| Err("test".into())),
            displays: Vec::new(),
        }
    }
}

/// A display with 2 pixels per point.
pub(crate) fn display(id: u32, name: &str, origin: (f64, f64), pixels: (u32, u32)) -> WebDisplay {
    WebDisplay {
        name: name.into(),
        screen: WebScreen {
            display: id,
            origin_points: origin,
            size_points: (f64::from(pixels.0) / 2.0, f64::from(pixels.1) / 2.0),
            size_pixels: pixels,
        },
    }
}

impl FakeHost {
    pub(crate) fn with_capturer(
        self,
        make: impl Fn() -> nexa_media::Result<Box<dyn Capturer>> + Send + Sync + 'static,
    ) -> Self {
        self.with_capturer_for(move |_| make())
    }

    /// A capturer made for what is asked of it: which display, how large.
    pub(crate) fn with_capturer_for(
        mut self,
        make: impl Fn(&CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> + Send + Sync + 'static,
    ) -> Self {
        self.capturer = Box::new(make);
        self
    }

    pub(crate) fn with_encoder(
        mut self,
        make: impl Fn() -> nexa_media::Result<Box<dyn Encoder>> + Send + Sync + 'static,
    ) -> Self {
        self.encoder = Box::new(make);
        self
    }

    pub(crate) fn with_input_sink(
        mut self,
        make: impl Fn() -> Result<Box<dyn InputSink>, String> + Send + Sync + 'static,
    ) -> Self {
        self.input_sink = Box::new(make);
        self
    }

    pub(crate) fn with_clipboard(
        mut self,
        make: impl Fn() -> Result<Box<dyn WebClipboard>, String> + Send + Sync + 'static,
    ) -> Self {
        self.clipboard = Box::new(make);
        self
    }

    /// The displays it has, the first the main one.
    pub(crate) fn with_displays(mut self, displays: Vec<WebDisplay>) -> Self {
        self.displays = displays;
        self
    }
}

impl WebHost for FakeHost {
    fn screen(&self) -> Result<WebScreen, String> {
        self.displays
            .first()
            .map(|d| d.screen)
            .ok_or_else(|| "test".into())
    }
    fn displays(&self) -> Result<Vec<WebDisplay>, String> {
        Ok(self.displays.clone())
    }
    fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        (self.capturer)(&config)
    }
    fn encoder(&self, _: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        (self.encoder)()
    }
    fn input_available(&self) -> bool {
        true
    }
    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        (self.input_sink)()
    }
    fn clipboard(&self) -> Result<Box<dyn WebClipboard>, String> {
        (self.clipboard)()
    }
    fn ask(&self, _: &BrowserRequest, _: Duration) -> BrowserAnswer {
        BrowserAnswer::DENY
    }
    fn event(&self, _: WebEvent) {}
}
