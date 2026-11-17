//! Device samples into the wire format (48 kHz stereo float): for a Windows loopback
//! that only captures in the device's own format. Portable, so it is tested everywhere.

use crate::AUDIO_SAMPLE_RATE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sample {
    F32,
    I16,
    I24,
    I32,
}

/// The device's format as a converter into the wire format: channels after the first
/// two are dropped (mono is doubled), and the rate is resampled linearly to 48 kHz.
#[derive(Debug)]
pub(crate) struct Convert {
    sample: Sample,
    channels: usize,
    /// Source frames per output frame.
    step: f64,
    /// Where the next output frame falls, in source frames after `prev`.
    pos: f64,
    prev: [f32; 2],
    frames: Vec<[f32; 2]>,
}

impl Convert {
    pub(crate) fn new(sample: Sample, channels: usize, rate: u32) -> Self {
        Self {
            sample,
            channels: channels.max(1),
            step: rate as f64 / AUDIO_SAMPLE_RATE as f64,
            pos: 1.0,
            prev: [0.0; 2],
            frames: Vec::new(),
        }
    }

    pub(crate) fn bytes_per_frame(&self) -> usize {
        self.channels
            * match self.sample {
                Sample::F32 | Sample::I32 => 4,
                Sample::I16 => 2,
                Sample::I24 => 3,
            }
    }

    fn read(&self, b: &[u8]) -> f32 {
        match self.sample {
            Sample::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Sample::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0,
            Sample::I24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            Sample::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        }
    }

    /// Appends `bytes` (whole frames, or `silent_frames` of silence) to `out`.
    pub(crate) fn push(&mut self, bytes: Option<&[u8]>, silent_frames: usize, out: &mut Vec<f32>) {
        self.frames.clear();
        match bytes {
            Some(bytes) => {
                let (size, width) = (
                    self.bytes_per_frame(),
                    self.bytes_per_frame() / self.channels,
                );
                for frame in bytes.chunks_exact(size) {
                    let l = self.read(&frame[..width]);
                    let r = if self.channels > 1 {
                        self.read(&frame[width..2 * width])
                    } else {
                        l
                    };
                    self.frames.push([l, r]);
                }
            }
            None => self.frames.resize(silent_frames, [0.0; 2]),
        }
        if self.step == 1.0 {
            out.extend(self.frames.iter().flatten());
            return;
        }
        // Source frame i (from 1) is self.frames[i - 1]; frame 0 is the previous block's last.
        let len = self.frames.len() as f64;
        while self.pos <= len {
            let i = self.pos.floor() as usize;
            let t = (self.pos - i as f64) as f32;
            let a = if i == 0 {
                self.prev
            } else {
                self.frames[i - 1]
            };
            let b = if i < self.frames.len() {
                self.frames[i]
            } else {
                a
            };
            out.extend([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            self.pos += self.step;
        }
        self.pos -= len;
        if let Some(&last) = self.frames.last() {
            self.prev = last;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_float_at_48k_passes_through() {
        let mut c = Convert::new(Sample::F32, 2, 48_000);
        let bytes: Vec<u8> = [0.25f32, -0.5, 1.0, 0.0]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut out = Vec::new();
        c.push(Some(&bytes), 0, &mut out);
        assert_eq!(out, [0.25, -0.5, 1.0, 0.0]);
    }

    #[test]
    fn extra_channels_are_dropped_and_ints_scaled() {
        let mut c = Convert::new(Sample::I16, 6, 48_000);
        let frame: Vec<u8> = [16_384i16, -32_768, 1, 2, 3, 4]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut out = Vec::new();
        c.push(Some(&frame), 0, &mut out);
        assert_eq!(out, [0.5, -1.0]);
        let mut mono = Convert::new(Sample::I24, 1, 48_000);
        out.clear();
        mono.push(Some(&[0x00, 0x00, 0xC0]), 0, &mut out); // -0.5 in 24 bits
        assert_eq!(out, [-0.5, -0.5]);
        let mut wide = Convert::new(Sample::I32, 2, 48_000);
        out.clear();
        let frame: Vec<u8> = [i32::MIN, 1 << 30]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        wide.push(Some(&frame), 0, &mut out);
        assert_eq!(out, [-1.0, 0.5]);
    }

    #[test]
    fn a_44k1_device_comes_out_at_48k_without_gaps() {
        let mut c = Convert::new(Sample::F32, 2, 44_100);
        let mut out = Vec::new();
        // One second in uneven blocks, a ramp so continuity is checkable.
        let (mut n, mut sizes) = (0usize, [441usize, 1000, 37, 2000].iter().cycle());
        while n < 44_100 {
            let k = (*sizes.next().unwrap()).min(44_100 - n);
            let bytes: Vec<u8> = (n..n + k)
                .flat_map(|i| {
                    let s = i as f32 / 44_100.0;
                    [s, -s]
                })
                .flat_map(|s| s.to_le_bytes())
                .collect();
            c.push(Some(&bytes), 0, &mut out);
            n += k;
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_000).contains(&frames), "{frames}");
        for w in out.as_chunks::<2>().0.windows(2) {
            let d = w[1][0] - w[0][0];
            assert!((0.0..0.0001).contains(&d), "{d}");
            assert_eq!(w[1][1], -w[1][0]);
        }
        // Silence resamples too.
        let before = out.len();
        c.push(None, 441, &mut out);
        assert!(
            (958..=962).contains(&(out.len() - before)),
            "{}",
            out.len() - before
        );
    }
}
