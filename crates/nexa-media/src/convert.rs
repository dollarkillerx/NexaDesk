//! BGRA screen pixels to the NV12 the encoders take (BT.709, video range), scaled to
//! the stream's size on the way. Used where the capturer hands over plain memory:
//! Windows' desktop duplication, for now.

use crate::CpuFrame;

/// Converts `src` (BGRA rows of `stride` bytes, `sw` × `sh`) to NV12 at `dw` × `dh`
/// (both even). Scaling picks the nearest source pixel for luma and averages a 2×2
/// block for chroma: cheap, and sharp enough for text at the sizes a stream uses.
pub fn bgra_to_nv12(src: &[u8], sw: u32, sh: u32, stride: usize, dw: u32, dh: u32) -> CpuFrame {
    let (dw, dh) = (dw.max(2) & !1, dh.max(2) & !1);
    let (sw, sh) = (sw.max(1) as usize, sh.max(1) as usize);
    let (w, h) = (dw as usize, dh as usize);
    // Source column and row for each destination one.
    let xs: Vec<usize> = (0..w).map(|x| ((x * sw) / w).min(sw - 1)).collect();
    let ys: Vec<usize> = (0..h).map(|y| ((y * sh) / h).min(sh - 1)).collect();
    let px = |x: usize, y: usize| -> (i32, i32, i32) {
        let i = y * stride + x * 4;
        match src.get(i..i + 3) {
            Some(p) => (i32::from(p[2]), i32::from(p[1]), i32::from(p[0])),
            None => (0, 0, 0),
        }
    };
    let mut y_plane = vec![0u8; w * h];
    let mut uv = vec![0u8; w * h / 2];
    for (dy, &sy) in ys.iter().enumerate() {
        let row = &mut y_plane[dy * w..(dy + 1) * w];
        for (dx, &sx) in xs.iter().enumerate() {
            let (r, g, b) = px(sx, sy);
            row[dx] = luma(r, g, b);
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (mut r, mut g, mut b) = (0, 0, 0);
            for (ox, oy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (pr, pg, pb) = px(xs[cx * 2 + ox], ys[cy * 2 + oy]);
                r += pr;
                g += pg;
                b += pb;
            }
            let (u, v) = chroma(r / 4, g / 4, b / 4);
            uv[cy * w + cx * 2] = u;
            uv[cy * w + cx * 2 + 1] = v;
        }
    }
    CpuFrame {
        width: dw,
        height: dh,
        y: y_plane,
        uv,
    }
}

/// BT.709 video range, in fixed point (×256).
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16).clamp(16, 235) as u8
}

fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    let u = ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    (u.clamp(16, 240) as u8, v.clamp(16, 240) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((w * h) as usize)
    }

    #[test]
    fn black_white_and_grey_land_on_the_video_range() {
        let black = bgra_to_nv12(&solid(4, 4, [0, 0, 0, 255]), 4, 4, 16, 4, 4);
        assert!(black.y.iter().all(|&y| y == 16));
        assert!(black.uv.iter().all(|&c| c == 128));
        let white = bgra_to_nv12(&solid(4, 4, [255; 4]), 4, 4, 16, 4, 4);
        assert!(white.y.iter().all(|&y| y == 235));
        assert!(white.uv.iter().all(|&c| (127..=129).contains(&c)));
    }

    #[test]
    fn colours_go_the_right_way() {
        // Pure blue: low luma, high U; pure red: high V.
        let blue = bgra_to_nv12(&solid(2, 2, [255, 0, 0, 255]), 2, 2, 8, 2, 2);
        assert!(blue.y[0] < 40 && blue.uv[0] > 200 && blue.uv[1] < 128);
        let red = bgra_to_nv12(&solid(2, 2, [0, 0, 255, 255]), 2, 2, 8, 2, 2);
        assert!(red.uv[1] > 200);
    }

    #[test]
    fn scales_to_the_stream_size_with_padded_rows() {
        // A 6×4 source whose rows are padded to 32 bytes, down to 2×2.
        let mut src = vec![0u8; 32 * 4];
        for y in 0..4 {
            for x in 0..6 {
                let v = if x < 3 { 0 } else { 255 };
                src[y * 32 + x * 4..y * 32 + x * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let f = bgra_to_nv12(&src, 6, 4, 32, 2, 2);
        assert_eq!((f.width, f.height, f.y.len(), f.uv.len()), (2, 2, 4, 2));
        assert_eq!(f.y[0], 16);
        assert_eq!(f.y[1], 235);
    }

    #[test]
    fn odd_sizes_round_down_to_even() {
        let f = bgra_to_nv12(&solid(5, 5, [0, 0, 0, 255]), 5, 5, 20, 5, 3);
        assert_eq!((f.width, f.height), (4, 2));
    }
}
