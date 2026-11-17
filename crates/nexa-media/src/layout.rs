//! Placing a picture in a view, the same on every platform.

/// Largest centered rectangle with the frame's aspect ratio that fits the
/// drawable, as `(x, y, w, h)` in drawable pixels.
pub fn fit_viewport(drawable: (u32, u32), frame: (u32, u32)) -> (f64, f64, f64, f64) {
    let (dw, dh) = (f64::from(drawable.0.max(1)), f64::from(drawable.1.max(1)));
    let (fw, fh) = (f64::from(frame.0.max(1)), f64::from(frame.1.max(1)));
    let scale = (dw / fw).min(dh / fh);
    let (w, h) = ((fw * scale).round().min(dw), (fh * scale).round().min(dh));
    (((dw - w) / 2.0).round(), ((dh - h) / 2.0).round(), w, h)
}

#[cfg(test)]
mod tests {
    use super::fit_viewport;

    #[test]
    fn exact_fit_uses_whole_drawable() {
        assert_eq!(
            fit_viewport((1920, 1080), (1920, 1080)),
            (0.0, 0.0, 1920.0, 1080.0)
        );
    }

    #[test]
    fn wide_drawable_pillarboxes() {
        // 16:9 video in a 21:9-ish drawable: full height, centered horizontally.
        assert_eq!(
            fit_viewport((2560, 1080), (1920, 1080)),
            (320.0, 0.0, 1920.0, 1080.0)
        );
    }

    #[test]
    fn tall_drawable_letterboxes() {
        assert_eq!(
            fit_viewport((1920, 1600), (1920, 1080)),
            (0.0, 260.0, 1920.0, 1080.0)
        );
    }

    #[test]
    fn scales_up_and_down() {
        assert_eq!(
            fit_viewport((960, 540), (1920, 1080)),
            (0.0, 0.0, 960.0, 540.0)
        );
        assert_eq!(
            fit_viewport((3840, 2160), (1920, 1080)),
            (0.0, 0.0, 3840.0, 2160.0)
        );
    }

    #[test]
    fn degenerate_sizes_do_not_divide_by_zero() {
        let (x, y, w, h) = fit_viewport((0, 0), (0, 0));
        assert!(x.is_finite() && y.is_finite() && w > 0.0 && h > 0.0);
    }
}
