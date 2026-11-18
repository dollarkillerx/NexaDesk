//! Screenshots as PNG, and the mapping between their pixels and the stream's.

use nexa_client::snapshot::Rgb;

/// Longest side of a screenshot: what models take in without scaling it down
/// themselves, so larger only costs tokens (docs/MCP.md §3.2).
pub const MAX_SIDE: u32 = 1568;

/// The size `w`×`h` comes out at when fitted within `max_side` (as
/// `nexa_client::snapshot::fit` scales it).
pub fn fitted(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    if w.max(h) <= max_side || w == 0 || h == 0 {
        return (w, h);
    }
    let k = f64::from(max_side) / f64::from(w.max(h));
    (
        ((f64::from(w) * k).round() as u32).max(1),
        ((f64::from(h) * k).round() as u32).max(1),
    )
}

pub fn png(rgb: &Rgb) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, rgb.width, rgb.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer
        .write_image_data(&rgb.data)
        .map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// A point in a screenshot `shot` (its pixels) to the stream's pixels.
pub fn to_stream(
    x: f64,
    y: f64,
    shot: (u32, u32),
    stream: (u32, u32),
) -> Result<(i32, i32), String> {
    let (sw, sh) = (f64::from(shot.0), f64::from(shot.1));
    if !(x.is_finite() && y.is_finite()) || x < 0.0 || y < 0.0 || x >= sw || y >= sh {
        return Err(format!(
            "({x}, {y}) is outside the screenshot, which is {}×{} pixels",
            shot.0, shot.1
        ));
    }
    let px = (x * f64::from(stream.0) / sw).floor() as i32;
    let py = (y * f64::from(stream.1) / sh).floor() as i32;
    Ok((
        px.clamp(0, stream.0 as i32 - 1),
        py.clamp(0, stream.1 as i32 - 1),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_matches_the_scaler() {
        assert_eq!(fitted(2560, 1600, MAX_SIDE), (1568, 980));
        assert_eq!(fitted(1280, 800, MAX_SIDE), (1280, 800));
        assert_eq!(fitted(1080, 1920, MAX_SIDE), (882, 1568));
        let rgb = Rgb {
            width: 2560,
            height: 1600,
            data: vec![0; 2560 * 1600 * 3],
        };
        let small = nexa_client::snapshot::fit(rgb, MAX_SIDE);
        assert_eq!((small.width, small.height), fitted(2560, 1600, MAX_SIDE));
    }

    #[test]
    fn points_scale_to_the_stream_and_stay_inside() {
        assert_eq!(
            to_stream(0.0, 0.0, (784, 490), (1568, 980)).unwrap(),
            (0, 0)
        );
        assert_eq!(
            to_stream(392.0, 245.0, (784, 490), (1568, 980)).unwrap(),
            (784, 490)
        );
        assert_eq!(
            to_stream(783.9, 489.9, (784, 490), (1568, 980)).unwrap(),
            (1567, 979)
        );
        assert!(to_stream(784.0, 0.0, (784, 490), (1568, 980)).is_err());
        assert!(to_stream(-1.0, 0.0, (784, 490), (1568, 980)).is_err());
        assert!(to_stream(f64::NAN, 0.0, (784, 490), (1568, 980)).is_err());
    }

    #[test]
    fn png_has_the_signature_and_size() {
        let rgb = Rgb {
            width: 3,
            height: 2,
            data: vec![200; 18],
        };
        let png = png(&rgb).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 3);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 2);
    }
}
