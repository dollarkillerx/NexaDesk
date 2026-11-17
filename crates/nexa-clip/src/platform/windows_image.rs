//! Pictures between the sync engine's PNG and what Windows' clipboard holds, with WIC.
//! A PNG goes on the clipboard as itself (the registered "PNG" format, which browsers and
//! Office read) and as a CF_DIBV5 (32-bit, straight alpha, bottom-up), from which Windows
//! makes CF_DIB and CF_BITMAP for programs that read only bitmaps. A bitmap copied on
//! Windows without a PNG beside it becomes one.

use windows::Win32::{
    Foundation::HGLOBAL,
    Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat24bppBGR,
        GUID_WICPixelFormat32bppBGRA, IWICBitmapFrameEncode, IWICImagingFactory,
        WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapPaletteTypeCustom,
        WICDecodeMetadataCacheOnDemand,
    },
    System::{
        Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            STREAM_SEEK_CUR,
            StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream, IPropertyBag2},
        },
        Memory::{GlobalLock, GlobalUnlock},
    },
};

/// No picture larger than this is taken apart: 8192² pixels are already 256 MiB.
const MAX_PIXELS: u64 = 8192 * 8192;
/// BITMAPV5HEADER.
const V5_HEADER: usize = 124;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;
/// 'sRGB'.
const LCS_SRGB: u32 = 0x7352_4742;
const LCS_GM_IMAGES: u32 = 4;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Whether `data` starts as a PNG file does.
pub(super) fn is_png(data: &[u8]) -> bool {
    data.starts_with(PNG_SIGNATURE)
}

fn factory() -> windows::core::Result<IWICImagingFactory> {
    thread_local! {
        // SAFETY: COM for this thread, for WIC; a thread that already has it keeps its own.
        static COM: () = { let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }; };
    }
    COM.with(|_| ());
    // SAFETY: an in-process COM object, created on a thread with COM.
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
}

/// A picture file (PNG, BMP…) as straight-alpha BGRA rows, top first.
pub(super) fn decode(file: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let wic = factory().ok()?;
    // SAFETY: WIC objects used within this call; the stream reads `file`, which outlives
    // them; the pixel buffer is the size CopyPixels is told.
    unsafe {
        let stream = wic.CreateStream().ok()?;
        stream.InitializeFromMemory(file).ok()?;
        let decoder = wic
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let converter = wic.CreateFormatConverter().ok()?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;
        let (mut w, mut h) = (0, 0);
        converter.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
            return None;
        }
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        converter
            .CopyPixels(std::ptr::null(), w * 4, &mut pixels)
            .ok()?;
        Some((w, h, pixels))
    }
}

/// A PNG of straight-alpha BGRA rows, top first.
pub(super) fn encode_png(w: u32, h: u32, bgra: &[u8]) -> Option<Vec<u8>> {
    encode(&GUID_ContainerFormatPng, w, h, bgra)
}

/// Any picture WIC reads (JPEG, TIFF, BMP…) as PNG.
pub(super) fn png_of_picture(file: &[u8]) -> Option<Vec<u8>> {
    let (w, h, bgra) = decode(file)?;
    encode_png(w, h, &bgra)
}

/// A `container` file (PNG; JPEG in tests) of straight-alpha BGRA rows, top first. An
/// encoder that wants 24-bit BGR (JPEG) is given that.
fn encode(container: &windows::core::GUID, w: u32, h: u32, bgra: &[u8]) -> Option<Vec<u8>> {
    if bgra.len() != w as usize * h as usize * 4 {
        return None;
    }
    let wic = factory().ok()?;
    // SAFETY: WIC objects used within this call, writing into a stream on a global block
    // it owns; the block is read, locked, within the length written.
    unsafe {
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true).ok()?;
        let encoder = wic.CreateEncoder(container, std::ptr::null()).ok()?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache).ok()?;
        let mut frame: Option<IWICBitmapFrameEncode> = None;
        encoder
            .CreateNewFrame(&mut frame, std::ptr::null_mut())
            .ok()?;
        let frame = frame?;
        frame.Initialize(None::<&IPropertyBag2>).ok()?;
        frame.SetSize(w, h).ok()?;
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut format).ok()?;
        // PNG takes BGRA with alpha as it is; JPEG asks for BGR.
        if format == GUID_WICPixelFormat32bppBGRA {
            frame.WritePixels(h, w * 4, bgra).ok()?;
        } else if format == GUID_WICPixelFormat24bppBGR {
            let bgr: Vec<u8> = (0..bgra.len())
                .step_by(4)
                .flat_map(|i| [bgra[i], bgra[i + 1], bgra[i + 2]])
                .collect();
            frame.WritePixels(h, w * 3, &bgr).ok()?;
        } else {
            return None;
        }
        frame.Commit().ok()?;
        encoder.Commit().ok()?;
        let mut written = 0u64;
        stream
            .Seek(0, STREAM_SEEK_CUR, Some(&raw mut written))
            .ok()?;
        let global = GetHGlobalFromStream(&stream).ok()?;
        let ptr = GlobalLock(global) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let png = std::slice::from_raw_parts(ptr, written as usize).to_vec();
        let _ = GlobalUnlock(global);
        Some(png)
    }
}

/// A CF_DIBV5 of straight-alpha BGRA rows, top first.
pub(super) fn dibv5(w: u32, h: u32, bgra: &[u8]) -> Vec<u8> {
    let row = w as usize * 4;
    let mut out = Vec::with_capacity(V5_HEADER + bgra.len());
    // Size, width, and a positive height: rows bottom first, as most programs expect.
    for v in [V5_HEADER as u32, w, h] {
        out.extend(v.to_le_bytes());
    }
    // One plane, 32 bits a pixel.
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    // Bit fields with an alpha mask; the image's size; no resolution or palette; masks
    // for BGRA in memory; sRGB.
    for v in [
        BI_BITFIELDS,
        (row * h as usize) as u32,
        0,
        0,
        0,
        0,
        0x00FF_0000,
        0x0000_FF00,
        0x0000_00FF,
        0xFF00_0000,
        LCS_SRGB,
    ] {
        out.extend(v.to_le_bytes());
    }
    // No end points; no gamma; for pictures; no profile.
    out.extend([0u8; 36]);
    for v in [0u32, 0, 0, LCS_GM_IMAGES, 0, 0, 0] {
        out.extend(v.to_le_bytes());
    }
    debug_assert_eq!(out.len(), V5_HEADER);
    for y in (0..h as usize).rev() {
        out.extend_from_slice(&bgra[y * row..(y + 1) * row]);
    }
    out
}

/// A PNG of a clipboard bitmap (CF_DIB or CF_DIBV5: a header and pixels, no file
/// header). Read as the BMP file it would be.
pub(super) fn png_of_dib(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let u32_at = |i: usize| u32::from_le_bytes([dib[i], dib[i + 1], dib[i + 2], dib[i + 3]]);
    let header = u32_at(0) as usize;
    let bits = u32::from(u16::from_le_bytes([dib[14], dib[15]]));
    let compression = u32_at(16);
    let used = u32_at(32) as usize;
    // A plain BITMAPINFOHEADER keeps its bit fields after it; later headers inside.
    let masks = match (header, compression) {
        (40, BI_BITFIELDS) => 12,
        (40, BI_ALPHABITFIELDS) => 16,
        _ => 0,
    };
    let palette = match (bits, used) {
        (1..=8, 0) => (1usize << bits) * 4,
        (_, n) => n * 4,
    };
    let offset = 14 + header + masks + palette;
    if header < 40 || offset > 14 + dib.len() {
        return None;
    }
    let mut file = Vec::with_capacity(14 + dib.len());
    file.extend(b"BM");
    file.extend(((14 + dib.len()) as u32).to_le_bytes());
    file.extend(0u32.to_le_bytes());
    file.extend((offset as u32).to_le_bytes());
    file.extend_from_slice(dib);
    let (w, h, bgra) = decode(&file)?;
    encode_png(w, h, &bgra)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3×2, every pixel different, one half transparent: rows top first.
    fn picture() -> Vec<u8> {
        let mut px = Vec::new();
        for (i, a) in [255u8, 255, 255, 128, 0, 255].into_iter().enumerate() {
            px.extend([i as u8 * 40, 255 - i as u8 * 40, 7, a]);
        }
        px
    }

    #[test]
    fn a_png_comes_back_pixel_for_pixel() {
        let png = encode_png(3, 2, &picture()).expect("encode");
        assert!(is_png(&png));
        assert_eq!(decode(&png), Some((3, 2, picture())));
    }

    #[test]
    fn the_bitmap_is_bottom_up_with_an_alpha_mask() {
        let dib = dibv5(3, 2, &picture());
        assert_eq!(dib.len(), V5_HEADER + 3 * 2 * 4);
        assert_eq!(&dib[0..4], &124u32.to_le_bytes());
        assert_eq!(&dib[8..12], &2u32.to_le_bytes(), "positive height");
        assert_eq!(&dib[52..56], &0xFF00_0000u32.to_le_bytes(), "alpha mask");
        // The first row in the bitmap is the picture's last.
        assert_eq!(&dib[V5_HEADER..V5_HEADER + 12], &picture()[12..24]);
    }

    #[test]
    fn a_copied_bitmap_becomes_the_same_png() {
        let png = png_of_dib(&dibv5(3, 2, &picture())).expect("convert");
        assert_eq!(decode(&png), Some((3, 2, picture())));
    }

    #[test]
    fn a_plain_24_bit_bitmap_becomes_an_opaque_png() {
        // BITMAPINFOHEADER, 2×1, 24-bit, rows padded to 4 bytes.
        let mut dib = Vec::new();
        for v in [40u32, 2, 1] {
            dib.extend(v.to_le_bytes());
        }
        dib.extend(1u16.to_le_bytes());
        dib.extend(24u16.to_le_bytes());
        dib.extend([0u8; 24]);
        dib.extend([1, 2, 3, 4, 5, 6, 0, 0]);
        let png = png_of_dib(&dib).expect("convert");
        assert_eq!(decode(&png), Some((2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255])));
    }

    #[test]
    fn a_jpeg_becomes_a_png_of_the_same_size() {
        use windows::Win32::Graphics::Imaging::GUID_ContainerFormatJpeg;
        let px = picture();
        let opaque: Vec<u8> = (0..px.len())
            .step_by(4)
            .flat_map(|i| [px[i], px[i + 1], px[i + 2], 255])
            .collect();
        let jpeg = encode(&GUID_ContainerFormatJpeg, 3, 2, &opaque).expect("jpeg");
        assert!(jpeg.starts_with(&[0xFF, 0xD8, 0xFF]));
        let png = png_of_picture(&jpeg).expect("convert");
        assert!(is_png(&png));
        let (w, h, px) = decode(&png).expect("decode");
        assert_eq!((w, h), (3, 2));
        assert!(px.iter().skip(3).step_by(4).all(|&a| a == 255), "opaque");
    }

    #[test]
    fn what_is_not_a_picture_is_refused() {
        assert_eq!(decode(b"not a picture"), None);
        assert_eq!(png_of_dib(&[0; 12]), None);
        assert!(!is_png(b"GIF89a"));
    }
}
