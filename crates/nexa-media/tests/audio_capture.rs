//! One short real system-audio capture through ScreenCaptureKit: two seconds, samples
//! counted and dropped, never stored or printed. Skipped unless Screen Recording is
//! already granted, so it cannot be the thing that raises the permission prompt.
//!
//! `cargo test -p nexa-media --test audio_capture -- --ignored --nocapture`
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use nexa_media::macos::{SystemAudioCapture, has_screen_capture_permission};
use nexa_media::{AUDIO_CHANNELS, AudioCapture};

const RUN: Duration = Duration::from_secs(2);

#[test]
#[ignore = "captures the real system audio for two seconds"]
fn system_audio_arrives_as_interleaved_stereo_and_stops() {
    if !has_screen_capture_permission() {
        eprintln!("skipped: no Screen Recording permission (not requested)");
        return;
    }
    let mut capture = SystemAudioCapture::start(0).expect("start");
    let start = Instant::now();
    let (mut chunks, mut samples, mut odd) = (0usize, 0usize, 0usize);
    while start.elapsed() < RUN {
        if let Some(chunk) = capture.next(Duration::from_millis(100)).expect("capture") {
            chunks += 1;
            samples += chunk.samples.len();
            odd += chunk.samples.len() % AUDIO_CHANNELS;
        }
    }
    let (dropped, unsupported) = (capture.dropped(), capture.unsupported());
    drop(capture);
    // Only counts leave this test.
    println!("{chunks} buffers, {} frames, {dropped} dropped, {unsupported} unsupported", samples / 2);
    assert_eq!(unsupported, 0, "every buffer was a format we convert");
    assert_eq!(odd, 0, "whole stereo frames");
    // ScreenCaptureKit delivers buffers even for silence, at about 48k frames a second.
    assert!(chunks > 10, "{chunks} buffers in {RUN:?}");
    let seconds = RUN.as_secs_f64();
    let rate = samples as f64 / 2.0 / seconds;
    assert!((30_000.0..60_000.0).contains(&rate), "{rate} frames/s");
}
