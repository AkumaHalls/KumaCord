//! Thumbnail pipeline: untrusted image bytes in, a small re-encoded JPEG out.
//!
//! Decoding untrusted images is where decompression bombs live, so the dimensions are read from
//! the header and checked *before* any pixel buffer is allocated, and the decoder gets an
//! allocation limit on top. The output is a fresh JPEG: no EXIF, no ICC, no animation, nothing
//! from the original container survives.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};

pub const MAX_W: u32 = 400;
pub const MAX_H: u32 = 225;
/// Header dimensions above this are refused outright.
pub const MAX_SIDE: u32 = 10_000;
/// A 20 MP photo is 5000x4000; bigger than that is not a link thumbnail.
pub const MAX_PIXELS: u64 = 20_000_000;
const MAX_ALLOC: u64 = 160 * 1024 * 1024;
const JPEG_QUALITY: u8 = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumb {
    pub jpeg: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ThumbError {
    Unsupported,
    TooLarge,
    Decode,
}

/// Untrusted bytes in, a decoded picture out, or the reason it was refused. The header's
/// dimensions are checked before any pixel buffer exists, the decoder runs under an allocation
/// limit, the first frame of animations is taken, and an EXIF orientation is applied (the
/// metadata itself is dropped with the original container). Shared by the link-preview
/// thumbnails and the profile pictures.
pub fn decode_untrusted(bytes: &[u8]) -> Result<DynamicImage, ThumbError> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|_| ThumbError::Unsupported)?;
    if reader.format().is_none() {
        return Err(ThumbError::Unsupported);
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(map_err)?;
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(ThumbError::TooLarge);
    }
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(map_err)?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// `crop_16_9`: cut the picture to 16:9 around the centre first (YouTube's 4:3 `hqdefault` has
/// black bars above and below the real frame).
pub fn make(bytes: &[u8], crop_16_9: bool) -> Result<Thumb, ThumbError> {
    let mut img = decode_untrusted(bytes)?;
    if crop_16_9 {
        img = crop_to_16_9(img);
    }
    let rgb = flatten_on_white(&img);
    let (w, h) = (rgb.width(), rgb.height());
    let rgb = if w > MAX_W || h > MAX_H {
        DynamicImage::ImageRgb8(rgb).thumbnail(MAX_W, MAX_H).to_rgb8()
    } else {
        rgb
    };
    let mut out = Vec::with_capacity(32 * 1024);
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY).encode_image(&rgb).map_err(|_| ThumbError::Decode)?;
    Ok(Thumb { jpeg: out, w: rgb.width(), h: rgb.height() })
}

fn map_err(e: image::ImageError) -> ThumbError {
    match e {
        image::ImageError::Limits(_) => ThumbError::TooLarge,
        image::ImageError::Unsupported(_) => ThumbError::Unsupported,
        _ => ThumbError::Decode,
    }
}

fn crop_to_16_9(img: DynamicImage) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let target_h = (u64::from(w) * 9 / 16) as u32;
    if target_h == 0 || target_h >= h {
        return img;
    }
    img.crop_imm(0, (h - target_h) / 2, w, target_h)
}

/// Transparent PNG / WebP / GIF pixels become white instead of black.
fn flatten_on_white(img: &DynamicImage) -> image::RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    let mut out = image::RgbImage::new(rgba.width(), rgba.height());
    for (o, p) in out.pixels_mut().zip(rgba.pixels()) {
        let a = u32::from(p[3]);
        let blend = |c: u8| ((u32::from(c) * a + 255 * (255 - a)) / 255) as u8;
        *o = image::Rgb([blend(p[0]), blend(p[1]), blend(p[2])]);
    }
    out
}

#[cfg(test)]
pub mod test_images {
    use super::*;
    use image::{ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};

    pub fn png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbImage::from_fn(w, h, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    pub fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = RgbImage::from_fn(w, h, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 200]));
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 90).encode_image(&img).unwrap();
        out
    }

    pub fn transparent_png() -> Vec<u8> {
        let img = RgbaImage::from_pixel(20, 20, Rgba([255, 0, 0, 0]));
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(img.as_raw(), 20, 20, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(data);
        out.extend(&body);
        out.extend(crc32(&body).to_be_bytes());
    }

    /// A PNG whose header claims `w`x`h` (1-bit grey) followed by a tiny junk data stream. Real
    /// bombs are valid streams that compress to almost nothing; the header is what we judge.
    pub fn png_claiming(w: u32, h: u32) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend(w.to_be_bytes());
        ihdr.extend(h.to_be_bytes());
        ihdr.extend([1, 0, 0, 0, 0]); // 1-bit greyscale, deflate, no filter, no interlace
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &[0x78, 0x9c, 0x01, 0x00, 0x00, 0xff, 0xff]);
        chunk(&mut out, b"IEND", &[]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_images::*;
    use super::*;

    fn decode(t: &Thumb) -> image::RgbImage {
        image::load_from_memory_with_format(&t.jpeg, image::ImageFormat::Jpeg).unwrap().to_rgb8()
    }

    #[test]
    fn large_images_are_scaled_down_keeping_the_aspect() {
        let t = make(&png(1200, 630), false).unwrap();
        assert!(t.w <= MAX_W && t.h <= MAX_H, "{}x{}", t.w, t.h);
        assert!((f64::from(t.w) / f64::from(t.h) - 1200.0 / 630.0).abs() < 0.05);
        assert_eq!((decode(&t).width(), decode(&t).height()), (t.w, t.h));
        let t = make(&jpeg(600, 1800), false).unwrap();
        assert!(t.h <= MAX_H && t.w < t.h);
    }

    #[test]
    fn small_images_are_not_upscaled() {
        let t = make(&png(120, 80), false).unwrap();
        assert_eq!((t.w, t.h), (120, 80));
    }

    #[test]
    fn output_is_a_plain_jpeg_without_metadata_segments() {
        let t = make(&png(300, 300), false).unwrap();
        assert_eq!(&t.jpeg[..3], &[0xff, 0xd8, 0xff]);
        // No EXIF (APP1) or ICC (APP2) segment in the re-encoded file.
        assert!(!t.jpeg.windows(6).any(|w| w == b"Exif\0\0"));
        assert!(!t.jpeg.windows(11).any(|w| w == b"ICC_PROFILE"));
    }

    #[test]
    fn crop_removes_letterbox_bars() {
        let t = make(&jpeg(320, 240), true).unwrap();
        assert_eq!((t.w, t.h), (320, 180));
        // A 4:3 hqdefault (480x360) ends up as a 16:9 frame inside the size box.
        let t = make(&jpeg(480, 360), true).unwrap();
        assert_eq!((t.w, t.h), (400, 225));
    }

    #[test]
    fn transparency_becomes_white() {
        let img = decode(&make(&transparent_png(), false).unwrap());
        let p = img.get_pixel(10, 10);
        assert!(p[0] > 240 && p[1] > 240 && p[2] > 240, "{p:?}");
    }

    #[test]
    fn decompression_bombs_and_junk_are_refused() {
        assert_eq!(make(&png_claiming(20000, 20000), false), Err(ThumbError::TooLarge));
        assert_eq!(make(&png_claiming(9000, 9000), false), Err(ThumbError::TooLarge)); // 81 MP
        assert_eq!(make(&png_claiming(65535, 1), false), Err(ThumbError::TooLarge));
        assert_eq!(make(b"<html>not an image</html>", false), Err(ThumbError::Unsupported));
        assert_eq!(make(b"", false), Err(ThumbError::Unsupported));
        let mut cut = png(800, 600);
        cut.truncate(cut.len() / 2);
        assert_eq!(make(&cut, false), Err(ThumbError::Decode));
    }
}
