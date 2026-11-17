//! A host for this crate's tests: each part it is asked for comes from a closure the test
//! gives, and everything else is refused.

use std::time::Duration;

use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig, MediaError};
use nexa_session::input::InputSink;

use crate::{BrowserAnswer, BrowserRequest, WebClipboard, WebEvent, WebHost, WebScreen};

type Make<T> = Box<dyn Fn() -> T + Send + Sync>;

pub(crate) struct FakeHost {
    capturer: Make<nexa_media::Result<Box<dyn Capturer>>>,
    encoder: Make<nexa_media::Result<Box<dyn Encoder>>>,
    input_sink: Make<Result<Box<dyn InputSink>, String>>,
    clipboard: Make<Result<Box<dyn WebClipboard>, String>>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self {
            capturer: Box::new(|| Err(MediaError::Unsupported("test"))),
            encoder: Box::new(|| Err(MediaError::Unsupported("test"))),
            input_sink: Box::new(|| Err("test".into())),
            clipboard: Box::new(|| Err("test".into())),
        }
    }
}

impl FakeHost {
    pub(crate) fn with_capturer(
        mut self,
        make: impl Fn() -> nexa_media::Result<Box<dyn Capturer>> + Send + Sync + 'static,
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
}

impl WebHost for FakeHost {
    fn screen(&self) -> Result<WebScreen, String> {
        Err("test".into())
    }
    fn capturer(&self, _: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        (self.capturer)()
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
