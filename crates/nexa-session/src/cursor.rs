//! Host cursor tracking (§6.9): shape and visibility, de-duplicated by content hash,
//! and sending a position back only when the cursor moved on its own.
//!
//! Everything here is platform-free. The platform supplies a [`CursorSource`]; the
//! agent runs [`CursorTracker`] on its own thread and forwards what it emits on the
//! Control stream. See `crate::proto` for what the messages mean.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub use nexa_media::{CursorImage, CursorSample, CursorSource};
use nexa_wire::control::{CursorSet, CursorShape};

use crate::proto::{ControlBody, CursorMoved};

/// Where the streamed display is, for converting the host cursor into stream pixels.
pub trait StreamGeometry: Send + Sync {
    /// Global host points to (display index, stream pixels). `None` when the point is
    /// not on the streamed display.
    fn points_to_stream(&self, x: f64, y: f64) -> Option<(u32, i32, i32)>;
    /// Stream pixels per host point on the streamed display.
    fn stream_pixels_per_point(&self) -> f64;
}

/// How close (host points) the cursor must be to an injected position to count as
/// where the viewer put it.
const SAME_POSITION_POINTS: f64 = 1.5;
/// Injected positions stay "explanations" this long: the cursor reaches them a little
/// after the event is posted, and a sample can land in between.
const INJECT_MEMORY: Duration = Duration::from_millis(250);
const MAX_REMEMBERED_INJECTS: usize = 64;
/// Shapes the viewer is known to have. A session rarely sees more than a dozen.
const MAX_REMEMBERED_SHAPES: usize = 64;
/// Images beyond this are not sent (a cursor is normally at most 256 px square).
const MAX_CURSOR_PIXELS: u32 = 512;

/// Absolute positions the host injected recently, in global points. Shared between
/// the inject thread (writer) and the cursor thread (reader).
#[derive(Default)]
pub struct InjectHistory {
    recent: Mutex<VecDeque<(Instant, f64, f64)>>,
}

impl InjectHistory {
    pub fn record(&self, x: f64, y: f64, now: Instant) {
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        if recent.len() == MAX_REMEMBERED_INJECTS {
            recent.pop_front();
        }
        recent.push_back((now, x, y));
    }

    /// Whether `(x, y)` is, or recently was, where an injected event put the cursor.
    /// The newest injected position counts however old it is: the cursor stays there
    /// until something else moves it.
    pub fn explains(&self, x: f64, y: f64, now: Instant) -> bool {
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        let newest = recent.back().copied();
        while recent.len() > 1
            && recent
                .front()
                .is_some_and(|(t, ..)| now.saturating_duration_since(*t) > INJECT_MEMORY)
        {
            recent.pop_front();
        }
        let near = |(_, ix, iy): &(Instant, f64, f64)| {
            (ix - x).abs() <= SAME_POSITION_POINTS && (iy - y).abs() <= SAME_POSITION_POINTS
        };
        newest.as_ref().is_some_and(near) || recent.iter().any(near)
    }

    pub fn is_empty(&self) -> bool {
        self.recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// FNV-1a, 64-bit: stable across runs and builds, unlike `DefaultHasher`.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &b in *part {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// A shape converted for the wire: sizes in stream pixels.
#[derive(Clone, Debug, PartialEq)]
struct WireShape {
    id: u64,
    w: u32,
    h: u32,
    hot_x: i32,
    hot_y: i32,
}

fn wire_shape(image: &CursorImage, stream_px_per_point: f64) -> Option<WireShape> {
    let expected = (image.width as usize)
        .checked_mul(image.height as usize)?
        .checked_mul(4)?;
    if image.width == 0
        || image.height == 0
        || image.width > MAX_CURSOR_PIXELS
        || image.height > MAX_CURSOR_PIXELS
        || image.rgba.len() != expected
        || image.scale.is_nan()
        || image.scale <= 0.0
        || stream_px_per_point.is_nan()
        || stream_px_per_point <= 0.0
    {
        return None;
    }
    let k = stream_px_per_point / image.scale;
    let w = ((f64::from(image.width) * k).round() as u32).max(1);
    let h = ((f64::from(image.height) * k).round() as u32).max(1);
    let hot_x = (image.hot_x * k).round() as i32;
    let hot_y = (image.hot_y * k).round() as i32;
    let id = fnv1a(&[
        &image.width.to_le_bytes(),
        &image.height.to_le_bytes(),
        &w.to_le_bytes(),
        &h.to_le_bytes(),
        &hot_x.to_le_bytes(),
        &hot_y.to_le_bytes(),
        &image.rgba,
    ]);
    Some(WireShape {
        id,
        w,
        h,
        hot_x,
        hot_y,
    })
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorStats {
    pub shapes_sent: u64,
    pub sets_sent: u64,
    pub moves_sent: u64,
    pub shapes_rejected: u64,
}

/// Turns samples into the messages the viewer needs, sending each shape once.
pub struct CursorTracker {
    /// Shape ids the viewer has, oldest first.
    sent: VecDeque<u64>,
    /// What the viewer was last told: `Some(id)` showing, `None` hidden. Outer `None`:
    /// nothing yet.
    told: Option<Option<u64>>,
    current: Option<WireShape>,
    /// The image `current` was made from, so an unchanged image is not hashed again.
    last_image: Option<Arc<CursorImage>>,
    last_scale: f64,
    last_reported: Option<(u32, i32, i32)>,
    pub stats: CursorStats,
}

impl Default for CursorTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorTracker {
    pub fn new() -> Self {
        Self {
            sent: VecDeque::new(),
            told: None,
            current: None,
            last_image: None,
            last_scale: 0.0,
            last_reported: None,
            stats: CursorStats::default(),
        }
    }

    /// The viewer's picture of the cursor may be stale (a new display, back from game
    /// mode): say the current state again. Shapes it already has are not resent.
    pub fn resync(&mut self) {
        self.told = None;
        self.last_reported = None;
    }

    /// Processes one sample, appending any messages for the viewer to `out`.
    pub fn update(
        &mut self,
        sample: &CursorSample,
        geometry: &dyn StreamGeometry,
        injected: &InjectHistory,
        now: Instant,
        out: &mut Vec<ControlBody>,
    ) {
        let scale = geometry.stream_pixels_per_point();
        if let Some(image) = &sample.image {
            let same = self
                .last_image
                .as_ref()
                .is_some_and(|l| Arc::ptr_eq(l, image) || **l == **image);
            if !same || scale != self.last_scale {
                match wire_shape(image, scale) {
                    Some(shape) => {
                        if !self.sent.contains(&shape.id) {
                            out.push(ControlBody::CursorShape(CursorShape {
                                id: shape.id,
                                w: shape.w,
                                h: shape.h,
                                hot_x: shape.hot_x,
                                hot_y: shape.hot_y,
                                png: encode_png(image.width, image.height, &image.rgba),
                            }));
                            self.stats.shapes_sent += 1;
                            if self.sent.len() == MAX_REMEMBERED_SHAPES {
                                self.sent.pop_front();
                            }
                            self.sent.push_back(shape.id);
                        }
                        self.current = Some(shape);
                    }
                    None => self.stats.shapes_rejected += 1,
                }
                self.last_image = Some(image.clone());
                self.last_scale = scale;
            }
        }

        let showing = match (&self.current, sample.visible) {
            (Some(shape), true) => Some(shape.id),
            _ => None,
        };
        if self.told != Some(showing) {
            out.push(ControlBody::CursorSet(CursorSet {
                id: showing.unwrap_or(0),
                hidden: showing.is_none(),
            }));
            self.stats.sets_sent += 1;
            self.told = Some(showing);
        }

        let Some((x, y)) = sample.position else {
            return;
        };
        if injected.explains(x, y, now) {
            // Where the viewer put it. A later move away is news again.
            self.last_reported = None;
            return;
        }
        if injected.is_empty() {
            // The viewer has not moved the pointer yet; nothing to correct.
            return;
        }
        if let Some(at) = geometry.points_to_stream(x, y)
            && self.last_reported != Some(at)
        {
            out.push(ControlBody::CursorMoved(CursorMoved {
                x: at.1,
                y: at.2,
                display: at.0,
            }));
            self.stats.moves_sent += 1;
            self.last_reported = Some(at);
        }
    }
}

// ---------------------------------------------------------------------------------
// PNG
// ---------------------------------------------------------------------------------

fn crc32(data: &[&[u8]]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for part in data {
        for &b in *part {
            crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    crc ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out.extend_from_slice(&crc32(&[kind, body]).to_be_bytes());
}

/// Run-length codes a scanline-filtered image with DEFLATE's fixed Huffman code
/// (RFC 1951 §3.2.6): literals plus distance-1..4 matches, which covers the large
/// transparent runs cursors are made of. Small and dependency-free.
fn deflate_fixed(data: &[u8], bpp: usize) -> Vec<u8> {
    struct Bits {
        out: Vec<u8>,
        acc: u64,
        n: u32,
    }
    impl Bits {
        fn put(&mut self, v: u32, len: u32) {
            self.acc |= u64::from(v) << self.n;
            self.n += len;
            while self.n >= 8 {
                self.out.push(self.acc as u8);
                self.acc >>= 8;
                self.n -= 8;
            }
        }
        /// Huffman codes are sent most significant bit first.
        fn put_rev(&mut self, code: u32, len: u32) {
            let mut r = 0;
            for i in 0..len {
                r |= ((code >> i) & 1) << (len - 1 - i);
            }
            self.put(r, len);
        }
        fn finish(mut self) -> Vec<u8> {
            if self.n > 0 {
                self.out.push(self.acc as u8);
            }
            self.out
        }
    }
    fn literal(b: &mut Bits, v: u32) {
        match v {
            0..=143 => b.put_rev(0x30 + v, 8),
            144..=255 => b.put_rev(0x190 + v - 144, 9),
            256..=279 => b.put_rev(v - 256, 7),
            _ => b.put_rev(0xC0 + v - 280, 8),
        }
    }
    const LEN_BASE: [u16; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LEN_EXTRA: [u8; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    fn length(b: &mut Bits, len: usize) {
        let i = LEN_BASE
            .iter()
            .rposition(|&base| usize::from(base) <= len)
            .expect("len >= 3");
        literal(b, 257 + i as u32);
        b.put(
            (len - usize::from(LEN_BASE[i])) as u32,
            u32::from(LEN_EXTRA[i]),
        );
    }

    let mut b = Bits {
        out: Vec::with_capacity(data.len() / 4 + 16),
        acc: 0,
        n: 0,
    };
    b.put(1, 1); // final block
    b.put(1, 2); // fixed Huffman
    let dist = bpp.clamp(1, 4);
    let mut i = 0;
    while i < data.len() {
        let mut run = 0;
        if i >= dist {
            while i + run < data.len() && run < 258 && data[i + run] == data[i + run - dist] {
                run += 1;
            }
        }
        if run >= 3 {
            length(&mut b, run);
            // Distance codes 0..=3 are distances 1..=4, five bits, no extra bits.
            b.put_rev((dist - 1) as u32, 5);
            i += run;
        } else {
            literal(&mut b, u32::from(data[i]));
            i += 1;
        }
    }
    literal(&mut b, 256);
    b.finish()
}

/// Encodes straight RGBA as an 8-bit RGBA PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let row = width as usize * 4;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for line in rgba.chunks_exact(row).take(height as usize) {
        raw.push(0); // filter: none
        raw.extend_from_slice(line);
    }
    let mut zlib = vec![0x78, 0x01];
    zlib.extend_from_slice(&deflate_fixed(&raw, 4));
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, adaptive, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal inflater for what `deflate_fixed` produces, to check it without a
    /// dependency. Fixed-Huffman blocks only.
    pub(crate) fn inflate_fixed(data: &[u8]) -> Vec<u8> {
        struct R<'a> {
            d: &'a [u8],
            pos: usize,
        }
        impl R<'_> {
            fn bit(&mut self) -> u32 {
                let b = (self.d[self.pos / 8] >> (self.pos % 8)) & 1;
                self.pos += 1;
                u32::from(b)
            }
            fn bits(&mut self, n: u32) -> u32 {
                (0..n).fold(0, |v, i| v | (self.bit() << i))
            }
            fn code(&mut self, n: u32) -> u32 {
                (0..n).fold(0, |v, _| (v << 1) | self.bit())
            }
        }
        const LEN_BASE: [u32; 29] = [
            3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99,
            115, 131, 163, 195, 227, 258,
        ];
        const LEN_EXTRA: [u32; 29] = [
            0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
        ];
        let mut r = R { d: data, pos: 0 };
        assert_eq!(r.bits(1), 1, "final");
        assert_eq!(r.bits(2), 1, "fixed");
        let mut out: Vec<u8> = Vec::new();
        loop {
            // Read 7 bits, extend as the fixed code requires.
            let mut c = r.code(7);
            let sym = if c <= 0x17 {
                256 + c
            } else {
                c = (c << 1) | r.bit();
                if (0x30..=0xBF).contains(&c) {
                    c - 0x30
                } else if (0xC0..=0xC7).contains(&c) {
                    280 + c - 0xC0
                } else {
                    c = (c << 1) | r.bit();
                    144 + c - 0x190
                }
            };
            match sym {
                0..=255 => out.push(sym as u8),
                256 => return out,
                _ => {
                    let i = (sym - 257) as usize;
                    let len = LEN_BASE[i] + r.bits(LEN_EXTRA[i]);
                    let dist = r.code(5) + 1;
                    for _ in 0..len {
                        let b = out[out.len() - dist as usize];
                        out.push(b);
                    }
                }
            }
        }
    }

    /// Extracts the RGBA pixels from a PNG made by `encode_png`.
    pub(crate) fn decode_png(png: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let mut o = 8;
        let (mut w, mut h, mut idat) = (0, 0, Vec::new());
        while o < png.len() {
            let len = u32::from_be_bytes(png[o..o + 4].try_into().unwrap()) as usize;
            let kind = &png[o + 4..o + 8];
            let body = &png[o + 8..o + 8 + len];
            let crc = u32::from_be_bytes(png[o + 8 + len..o + 12 + len].try_into().unwrap());
            assert_eq!(crc, crc32(&[kind, body]), "chunk CRC");
            match kind {
                b"IHDR" => {
                    w = u32::from_be_bytes(body[0..4].try_into().unwrap());
                    h = u32::from_be_bytes(body[4..8].try_into().unwrap());
                    assert_eq!(&body[8..13], &[8, 6, 0, 0, 0]);
                }
                b"IDAT" => idat.extend_from_slice(body),
                _ => {}
            }
            o += 12 + len;
        }
        let raw = inflate_fixed(&idat[2..idat.len() - 4]);
        let adler = u32::from_be_bytes(idat[idat.len() - 4..].try_into().unwrap());
        assert_eq!(adler, adler32(&raw));
        let row = w as usize * 4;
        let mut rgba = Vec::new();
        for line in raw.chunks_exact(row + 1) {
            assert_eq!(line[0], 0);
            rgba.extend_from_slice(&line[1..]);
        }
        (w, h, rgba)
    }

    pub(crate) fn arrow(scale: f64, tint: u8) -> Arc<CursorImage> {
        let (w, h) = ((16.0 * scale) as u32, (24.0 * scale) as u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w.min(y + 1) {
                let i = ((y * w + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[tint, 0, 0, 255]);
            }
        }
        Arc::new(CursorImage {
            width: w,
            height: h,
            rgba,
            hot_x: 1.0 * scale,
            hot_y: 1.0 * scale,
            scale,
        })
    }

    /// Host display of 1000×500 points at (100, 0), streamed at 2000×1000 pixels.
    struct Geo;
    impl StreamGeometry for Geo {
        fn points_to_stream(&self, x: f64, y: f64) -> Option<(u32, i32, i32)> {
            let (px, py) = ((x - 100.0) * 2.0, y * 2.0);
            ((0.0..2000.0).contains(&px) && (0.0..1000.0).contains(&py))
                .then_some((0, px as i32, py as i32))
        }
        fn stream_pixels_per_point(&self) -> f64 {
            2.0
        }
    }

    struct HalfGeo;
    impl StreamGeometry for HalfGeo {
        fn points_to_stream(&self, x: f64, y: f64) -> Option<(u32, i32, i32)> {
            Some((1, x as i32, y as i32))
        }
        fn stream_pixels_per_point(&self) -> f64 {
            1.0
        }
    }

    fn sample(image: &Arc<CursorImage>, visible: bool, at: Option<(f64, f64)>) -> CursorSample {
        CursorSample {
            image: Some(image.clone()),
            visible,
            position: at,
        }
    }

    fn kinds(out: &[ControlBody]) -> Vec<&'static str> {
        out.iter()
            .map(|b| match b {
                ControlBody::CursorShape(_) => "shape",
                ControlBody::CursorSet(s) if s.hidden => "hidden",
                ControlBody::CursorSet(_) => "set",
                ControlBody::CursorMoved(_) => "moved",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn each_shape_is_sent_once_and_switching_back_only_sets() {
        let (a, b) = (arrow(2.0, 10), arrow(2.0, 20));
        let mut t = CursorTracker::new();
        let inj = InjectHistory::default();
        let now = Instant::now();
        let mut out = Vec::new();
        t.update(&sample(&a, true, None), &Geo, &inj, now, &mut out);
        assert_eq!(kinds(&out), ["shape", "set"]);
        out.clear();
        // The same image again, even as a different allocation: nothing to say.
        let a_copy = Arc::new((*a).clone());
        t.update(&sample(&a_copy, true, None), &Geo, &inj, now, &mut out);
        assert!(out.is_empty());
        t.update(&sample(&b, true, None), &Geo, &inj, now, &mut out);
        assert_eq!(kinds(&out), ["shape", "set"]);
        out.clear();
        t.update(&sample(&a, true, None), &Geo, &inj, now, &mut out);
        assert_eq!(kinds(&out), ["set"], "the viewer already has shape a");
        assert_eq!(t.stats.shapes_sent, 2);
    }

    #[test]
    fn ids_are_content_hashes_and_sizes_are_stream_pixels() {
        let a = arrow(2.0, 10);
        let (mut t1, mut t2) = (CursorTracker::new(), CursorTracker::new());
        let inj = InjectHistory::default();
        let (mut o1, mut o2) = (Vec::new(), Vec::new());
        t1.update(&sample(&a, true, None), &Geo, &inj, Instant::now(), &mut o1);
        t2.update(
            &sample(&Arc::new((*a).clone()), true, None),
            &Geo,
            &inj,
            Instant::now(),
            &mut o2,
        );
        let (ControlBody::CursorShape(s1), ControlBody::CursorShape(s2)) = (&o1[0], &o2[0]) else {
            panic!("expected shapes");
        };
        assert_eq!(s1.id, s2.id, "same content, same id in any session");
        // 32×48 image pixels at 2 px/pt is 16×24 points, which is 32×48 stream pixels.
        assert_eq!((s1.w, s1.h, s1.hot_x, s1.hot_y), (32, 48, 2, 2));
        // On a stream at 1 px/pt the same cursor covers half as many stream pixels,
        // and needs its own id because the viewer's copy has another size.
        let mut t3 = CursorTracker::new();
        let mut o3 = Vec::new();
        t3.update(
            &sample(&a, true, None),
            &HalfGeo,
            &inj,
            Instant::now(),
            &mut o3,
        );
        let ControlBody::CursorShape(s3) = &o3[0] else {
            panic!()
        };
        assert_eq!((s3.w, s3.h, s3.hot_x), (16, 24, 1));
        assert_ne!(s3.id, s1.id);
        // The PNG carries every image pixel.
        let (w, h, rgba) = decode_png(&s1.png);
        assert_eq!((w, h), (32, 48));
        assert_eq!(rgba, a.rgba);
    }

    #[test]
    fn hiding_and_showing_send_sets_only() {
        let a = arrow(1.0, 1);
        let mut t = CursorTracker::new();
        let inj = InjectHistory::default();
        let mut out = Vec::new();
        let now = Instant::now();
        t.update(&sample(&a, true, None), &Geo, &inj, now, &mut out);
        out.clear();
        t.update(&sample(&a, false, None), &Geo, &inj, now, &mut out);
        assert_eq!(kinds(&out), ["hidden"]);
        out.clear();
        t.update(&sample(&a, false, None), &Geo, &inj, now, &mut out);
        assert!(out.is_empty());
        t.update(&sample(&a, true, None), &Geo, &inj, now, &mut out);
        assert_eq!(kinds(&out), ["set"]);
        out.clear();
        t.resync();
        t.update(&sample(&a, true, None), &Geo, &inj, now, &mut out);
        assert_eq!(
            kinds(&out),
            ["set"],
            "resync repeats the state, not the shape"
        );
    }

    #[test]
    fn unreadable_or_absurd_images_are_not_sent() {
        let mut huge = (*arrow(1.0, 1)).clone();
        huge.width = 4096;
        huge.rgba = vec![0; 4096 * 24 * 4];
        let mut short = (*arrow(1.0, 1)).clone();
        short.rgba.pop();
        let mut t = CursorTracker::new();
        let inj = InjectHistory::default();
        let mut out = Vec::new();
        for bad in [huge, short] {
            t.update(
                &sample(&Arc::new(bad), true, None),
                &Geo,
                &inj,
                Instant::now(),
                &mut out,
            );
        }
        assert_eq!(
            kinds(&out),
            ["hidden"],
            "no shape yet, so the cursor is hidden"
        );
        assert_eq!(t.stats.shapes_rejected, 2);
    }

    #[test]
    fn position_syncs_back_only_when_it_differs_from_the_last_injection() {
        let a = arrow(1.0, 1);
        let mut t = CursorTracker::new();
        let inj = InjectHistory::default();
        let t0 = Instant::now();
        let mut out = Vec::new();
        t.update(
            &sample(&a, true, Some((300.0, 100.0))),
            &Geo,
            &inj,
            t0,
            &mut out,
        );
        assert!(
            !kinds(&out).contains(&"moved"),
            "before the viewer moved the pointer there is nothing to correct"
        );
        out.clear();

        inj.record(300.0, 100.0, t0);
        t.update(
            &sample(&a, true, Some((300.0, 100.0))),
            &Geo,
            &inj,
            t0,
            &mut out,
        );
        t.update(
            &sample(&a, true, Some((300.5, 100.0))),
            &Geo,
            &inj,
            t0,
            &mut out,
        );
        assert!(out.is_empty(), "where the viewer put it, within rounding");

        // The cursor lags an injection slightly: an older injected position explains it.
        let t1 = t0 + Duration::from_millis(10);
        inj.record(310.0, 100.0, t1);
        t.update(
            &sample(&a, true, Some((300.0, 100.0))),
            &Geo,
            &inj,
            t1,
            &mut out,
        );
        assert!(out.is_empty());

        // A program warps it elsewhere.
        let t2 = t1 + Duration::from_secs(1);
        t.update(
            &sample(&a, true, Some((600.0, 50.0))),
            &Geo,
            &inj,
            t2,
            &mut out,
        );
        let [ControlBody::CursorMoved(m)] = out.as_slice() else {
            panic!("{out:?}")
        };
        assert_eq!((m.x, m.y, m.display), (1000, 100, 0));
        out.clear();
        t.update(
            &sample(&a, true, Some((600.0, 50.0))),
            &Geo,
            &inj,
            t2,
            &mut out,
        );
        assert!(out.is_empty(), "reported once");

        // Off the streamed display: nothing the viewer could show.
        t.update(
            &sample(&a, true, Some((5000.0, 50.0))),
            &Geo,
            &inj,
            t2,
            &mut out,
        );
        assert!(out.is_empty());

        // Once old, an injected position no longer explains anything but the newest.
        let t3 = t2 + Duration::from_secs(1);
        t.update(
            &sample(&a, true, Some((300.0, 100.0))),
            &Geo,
            &inj,
            t3,
            &mut out,
        );
        assert_eq!(kinds(&out), ["moved"]);
        out.clear();
        t.update(
            &sample(&a, true, Some((310.0, 100.0))),
            &Geo,
            &inj,
            t3,
            &mut out,
        );
        assert!(
            out.is_empty(),
            "the newest injection always explains its position"
        );
    }

    #[test]
    fn png_compresses_transparent_runs_and_round_trips() {
        let a = arrow(2.0, 200);
        let png = encode_png(a.width, a.height, &a.rgba);
        assert!(png.len() < a.rgba.len() / 4, "{} bytes", png.len());
        let (w, h, rgba) = decode_png(&png);
        assert_eq!((w, h), (a.width, a.height));
        assert_eq!(rgba, a.rgba);
        // Noise has no runs and still round-trips.
        let mut x = 0x9E37_79B9u32;
        let noise: Vec<u8> = (0..8 * 8 * 4)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        assert_eq!(decode_png(&encode_png(8, 8, &noise)).2, noise);
    }
}
