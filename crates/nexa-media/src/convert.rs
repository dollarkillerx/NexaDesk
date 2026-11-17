//! BGRA screen pixels to the NV12 the encoders take (BT.709, video range), scaled to
//! the stream's size on the way. Used where the capturer hands over plain memory:
//! Windows' desktop duplication, for now.

use crate::CpuFrame;

/// Fixed-point weights sum to this.
const ONE: u32 = 1 << 14;

/// For each destination pixel along one axis: the first source pixel it covers and
/// the share of each source pixel from there (area averaging, summing to [`ONE`]).
fn taps(src: usize, dst: usize) -> Vec<(usize, Vec<u32>)> {
    let scale = src as f64 / dst as f64;
    (0..dst)
        .map(|i| {
            let (lo, hi) = (i as f64 * scale, ((i + 1) as f64 * scale).min(src as f64));
            let first = (lo.floor() as usize).min(src - 1);
            let last = (hi.ceil() as usize).clamp(first + 1, src);
            let mut weights: Vec<u32> = (first..last)
                .map(|s| {
                    let covered = hi.min(s as f64 + 1.0) - lo.max(s as f64);
                    (covered.max(0.0) / (hi - lo) * f64::from(ONE)).round() as u32
                })
                .collect();
            // Rounding must not brighten or darken: the largest share takes the rest.
            let sum: u32 = weights.iter().sum();
            if let Some(max) = weights.iter_mut().max() {
                *max = (*max + ONE).saturating_sub(sum);
            }
            (first, weights)
        })
        .collect()
}

/// How many threads share one resample: a 4K desktop is a lot for one core.
fn workers(rows: usize) -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 4)
        .min(rows.max(1))
}

/// `src` (BGRA rows of `stride` bytes) as tight RGB at `w` × `h`, each output pixel
/// the average of the source area it covers: text stays whole, unlike picking pixels.
fn resample(src: &[u8], sw: usize, sh: usize, stride: usize, w: usize, h: usize) -> Vec<[u8; 3]> {
    let (xt, yt) = (taps(sw, w), taps(sh, h));
    // Horizontal pass, one row per source row, in 8.8 fixed point (fits u16:
    // 255 × ONE >> 6 = 65280).
    let mut rows = vec![[0u16; 3]; w * sh];
    let band = sh.div_ceil(workers(sh));
    std::thread::scope(|scope| {
        for (i, chunk) in rows.chunks_mut(band * w).enumerate() {
            let xt = &xt;
            scope.spawn(move || {
                for (r, out) in chunk.chunks_mut(w).enumerate() {
                    let sy = i * band + r;
                    let Some(line) = src.get(sy * stride..sy * stride + sw * 4) else {
                        continue;
                    };
                    for ((first, weights), o) in xt.iter().zip(out.iter_mut()) {
                        let mut acc = [0u32; 3];
                        for (k, &wgt) in weights.iter().enumerate() {
                            let p = &line[(first + k) * 4..(first + k) * 4 + 3];
                            acc[0] += u32::from(p[2]) * wgt;
                            acc[1] += u32::from(p[1]) * wgt;
                            acc[2] += u32::from(p[0]) * wgt;
                        }
                        *o = acc.map(|a| (a >> 6) as u16);
                    }
                }
            });
        }
    });
    // Vertical pass back to 8 bits: 65280 × ONE fits u32.
    let mut out = vec![[0u8; 3]; w * h];
    let band = h.div_ceil(workers(h));
    std::thread::scope(|scope| {
        for (i, chunk) in out.chunks_mut(band * w).enumerate() {
            let (yt, rows) = (&yt, &rows);
            scope.spawn(move || {
                for (r, line) in chunk.chunks_mut(w).enumerate() {
                    let (first, weights) = &yt[i * band + r];
                    for (dx, o) in line.iter_mut().enumerate() {
                        let mut acc = [0u32; 3];
                        for (k, &wgt) in weights.iter().enumerate() {
                            let p = rows[(first + k) * w + dx];
                            acc[0] += u32::from(p[0]) * wgt;
                            acc[1] += u32::from(p[1]) * wgt;
                            acc[2] += u32::from(p[2]) * wgt;
                        }
                        *o = acc.map(|a| ((a + (1 << 21)) >> 22).min(255) as u8);
                    }
                }
            });
        }
    });
    out
}

/// Converts `src` (BGRA rows of `stride` bytes, `sw` × `sh`) to NV12 at `dw` × `dh`
/// (both even). A different size is reached by area averaging; chroma averages each
/// 2×2 block of the result.
pub fn bgra_to_nv12(src: &[u8], sw: u32, sh: u32, stride: usize, dw: u32, dh: u32) -> CpuFrame {
    let (dw, dh) = (dw.max(2) & !1, dh.max(2) & !1);
    let (sw, sh) = (sw.max(1) as usize, sh.max(1) as usize);
    let (w, h) = (dw as usize, dh as usize);
    let scaled = ((sw, sh) != (w, h)).then(|| resample(src, sw, sh, stride, w, h));
    let px = |x: usize, y: usize| -> (i32, i32, i32) {
        if let Some(rgb) = &scaled {
            let p = rgb[y * w + x];
            return (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
        }
        let i = y * stride + x * 4;
        match src.get(i..i + 3) {
            Some(p) => (i32::from(p[2]), i32::from(p[1]), i32::from(p[0])),
            None => (0, 0, 0),
        }
    };
    let mut y_plane = vec![0u8; w * h];
    let mut uv = vec![0u8; w * h / 2];
    for dy in 0..h {
        let row = &mut y_plane[dy * w..(dy + 1) * w];
        for (dx, out) in row.iter_mut().enumerate() {
            let (r, g, b) = px(dx, dy);
            *out = luma(r, g, b);
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (mut r, mut g, mut b) = (0, 0, 0);
            for (ox, oy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (pr, pg, pb) = px(cx * 2 + ox, cy * 2 + oy);
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
    fn a_one_pixel_line_survives_shrinking_instead_of_vanishing() {
        // 2560 wide to 1920 (4:3): picking pixels drops every fourth column, so a thin
        // vertical line there disappeared; averaging keeps it as a lighter line.
        let (sw, sh, dw, dh) = (2560usize, 4usize, 1920u32, 4u32);
        let mut src = solid(sw as u32, sh as u32, [0, 0, 0, 255]);
        for line_x in [3usize, 7, 11] {
            for y in 0..sh {
                src[(y * sw + line_x) * 4..(y * sw + line_x) * 4 + 3].copy_from_slice(&[255; 3]);
            }
        }
        let f = bgra_to_nv12(&src, sw as u32, sh as u32, sw * 4, dw, dh);
        for line_x in [3usize, 7, 11] {
            let around = (line_x * 3 / 4).saturating_sub(1)..=(line_x * 3 / 4 + 1);
            let brightest = around.map(|x| f.y[x]).max().unwrap();
            assert!(brightest > 100, "line at {line_x}: {brightest}");
        }
    }

    #[test]
    fn averaging_keeps_the_overall_brightness() {
        // A fine checkerboard shrunk 3:2 is a flat mid grey, not stripes.
        let (sw, sh) = (30usize, 30usize);
        let mut src = vec![0u8; sw * sh * 4];
        for y in 0..sh {
            for x in 0..sw {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                src[(y * sw + x) * 4..(y * sw + x) * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let f = bgra_to_nv12(&src, sw as u32, sh as u32, sw * 4, 20, 20);
        let mid = luma(128, 128, 128);
        assert!(
            f.y.iter().all(|&y| y.abs_diff(mid) <= 24),
            "{:?}",
            &f.y[..20]
        );
        for (src, dst) in [(2560, 1920), (3840, 1920), (1366, 1280), (7, 3)] {
            for (_, weights) in taps(src, dst) {
                assert_eq!(weights.iter().sum::<u32>(), ONE);
            }
        }
    }

    #[test]
    fn odd_sizes_round_down_to_even() {
        let f = bgra_to_nv12(&solid(5, 5, [0, 0, 0, 255]), 5, 5, 20, 5, 3);
        assert_eq!((f.width, f.height), (4, 2));
    }
}
