//! Codec negotiation (§3.5): the host takes `encode ∩ decode` and scores by mode.

use nexa_wire::control::{Caps, Chroma, Codec, CodecCap, Mode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chosen {
    pub codec: Codec,
    pub chroma: Chroma,
    pub max_w: u32,
    pub max_h: u32,
}

/// §3.5's order for a mode, best first. Game mode prefers encoders with reference
/// frame invalidation; every entry here is the §3.5 order among codecs that have it.
fn ranking(mode: Mode) -> &'static [(Codec, Chroma)] {
    use Chroma::{Yuv420, Yuv444};
    use Codec::{Av1, H264, Hevc};
    match mode {
        Mode::Office => &[
            (Hevc, Yuv444),
            (Av1, Yuv444),
            (Av1, Yuv420),
            (Hevc, Yuv420),
            (H264, Yuv420),
        ],
        Mode::Balanced => &[(Av1, Yuv420), (Hevc, Yuv420), (H264, Yuv420)],
        Mode::Game => &[(Hevc, Yuv420), (Av1, Yuv420), (H264, Yuv420)],
    }
}

fn find(list: &[CodecCap], codec: Codec, chroma: Chroma) -> Option<&CodecCap> {
    list.iter().find(|c| {
        c.codec == codec as i32 && c.chroma == chroma as i32 && c.max_w > 0 && c.max_h > 0
    })
}

/// The best format for `mode`, or `None` if the two ends share nothing.
pub fn choose(host: &Caps, viewer: &Caps, mode: Mode) -> Option<Chosen> {
    ranking(mode).iter().find_map(|&(codec, chroma)| {
        let e = find(&host.encode, codec, chroma)?;
        let d = find(&viewer.decode, codec, chroma)?;
        Some(Chosen {
            codec,
            chroma,
            max_w: e.max_w.min(d.max_w),
            max_h: e.max_h.min(d.max_h),
        })
    })
}

/// Whether any mode could stream, so a doomed session is refused before anyone is asked.
pub fn any_common(host: &Caps, viewer: &Caps) -> bool {
    [Mode::Office, Mode::Balanced, Mode::Game]
        .into_iter()
        .any(|m| choose(host, viewer, m).is_some())
}

/// Largest even size with the display's aspect ratio that fits the codec limits.
pub fn fit_size(native_w: u32, native_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (nw, nh) = (native_w.max(2) as f64, native_h.max(2) as f64);
    let scale = (f64::from(max_w) / nw).min(f64::from(max_h) / nh).min(1.0);
    let even = |v: f64| ((v.floor() as u32) & !1).max(2);
    (even(nw * scale), even(nh * scale))
}

/// `requested` of 0 means the mode's default; every limit that is 0 is ignored.
pub fn choose_fps(requested: u32, mode: Mode, host_max: u32, viewer_max: u32, refresh: u32) -> u32 {
    let default = if mode == Mode::Game { 120 } else { 60 };
    [host_max, viewer_max, refresh]
        .into_iter()
        .filter(|&v| v > 0)
        .fold(if requested == 0 { default } else { requested }, u32::min)
        .clamp(1, 240)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(codec: Codec, chroma: Chroma, w: u32, h: u32) -> CodecCap {
        CodecCap {
            codec: codec as i32,
            chroma: chroma as i32,
            max_w: w,
            max_h: h,
        }
    }

    fn caps(encode: Vec<CodecCap>, decode: Vec<CodecCap>) -> Caps {
        Caps {
            encode,
            decode,
            rfi: false,
            ltr: true,
            max_fps: 120,
        }
    }

    #[test]
    fn intersection_of_encode_and_decode() {
        let host = caps(
            vec![
                cap(Codec::Hevc, Chroma::Yuv420, 3840, 2160),
                cap(Codec::H264, Chroma::Yuv420, 3840, 2160),
            ],
            vec![],
        );
        let viewer = caps(vec![], vec![cap(Codec::H264, Chroma::Yuv420, 1920, 1080)]);
        assert_eq!(
            choose(&host, &viewer, Mode::Game),
            Some(Chosen {
                codec: Codec::H264,
                chroma: Chroma::Yuv420,
                max_w: 1920,
                max_h: 1080,
            })
        );
    }

    #[test]
    fn nothing_in_common() {
        let host = caps(vec![cap(Codec::Hevc, Chroma::Yuv420, 1, 1)], vec![]);
        let viewer = caps(vec![], vec![cap(Codec::H264, Chroma::Yuv420, 1, 1)]);
        assert_eq!(choose(&host, &viewer, Mode::Office), None);
        assert!(!any_common(&host, &viewer));
    }

    #[test]
    fn scoring_follows_mode() {
        let all = vec![
            cap(Codec::Av1, Chroma::Yuv420, 8, 8),
            cap(Codec::Hevc, Chroma::Yuv420, 8, 8),
            cap(Codec::Hevc, Chroma::Yuv444, 8, 8),
            cap(Codec::H264, Chroma::Yuv420, 8, 8),
        ];
        let (h, v) = (caps(all.clone(), vec![]), caps(vec![], all));
        let pick = |m| {
            let c = choose(&h, &v, m).unwrap();
            (c.codec, c.chroma)
        };
        assert_eq!(pick(Mode::Office), (Codec::Hevc, Chroma::Yuv444));
        assert_eq!(pick(Mode::Balanced), (Codec::Av1, Chroma::Yuv420));
        assert_eq!(pick(Mode::Game), (Codec::Hevc, Chroma::Yuv420));
    }

    #[test]
    fn zero_sized_caps_are_unusable() {
        let h = caps(vec![cap(Codec::H264, Chroma::Yuv420, 0, 0)], vec![]);
        let v = caps(vec![], vec![cap(Codec::H264, Chroma::Yuv420, 8, 8)]);
        assert_eq!(choose(&h, &v, Mode::Office), None);
    }

    #[test]
    fn size_keeps_aspect_and_stays_even() {
        assert_eq!(fit_size(1920, 1080, 3840, 2160), (1920, 1080));
        assert_eq!(fit_size(5120, 2880, 3840, 2160), (3840, 2160));
        assert_eq!(fit_size(3024, 1964, 1920, 1080), (1662, 1080));
    }

    #[test]
    fn fps_takes_the_lowest_limit() {
        assert_eq!(choose_fps(0, Mode::Game, 120, 120, 60), 60);
        assert_eq!(choose_fps(0, Mode::Office, 120, 120, 120), 60);
        assert_eq!(choose_fps(30, Mode::Office, 120, 0, 0), 30);
    }
}
