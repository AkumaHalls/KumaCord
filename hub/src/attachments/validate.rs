//! Checking an uploaded chat image: never trust the app that sent it.
//!
//! 1. The format comes from the file's magic bytes (JPEG, PNG, GIF, WebP), never from a header.
//! 2. Every metadata block is cut out of the container *without re-encoding* (so the app's
//!    careful compression is kept): JPEG APPn (EXIF, XMP, ICC...) and comments; PNG ancillary
//!    chunks (text, EXIF, ICC, APNG frames...); GIF comments, plain text and application
//!    extensions other than the loop count; WebP `EXIF` / `XMP ` / `ICCP` chunks (and their
//!    `VP8X` flags). A JPEG whose EXIF asked for a rotation is re-encoded upright instead, since
//!    cutting the EXIF would show it sideways.
//! 3. The cleaned file is decoded with [`decode_untrusted`] (header dimensions checked before any
//!    pixel buffer, allocation cap), and an animation is decoded frame by frame within a pixel
//!    budget, so a file that only *looks* fine is refused.
//! 4. Dimensions (at most [`ATTACHMENT_MAX_SIDE`]) and sizes ([`ATTACHMENT_MAX_BYTES`],
//!    [`ATTACHMENT_MAX_ANIMATED_BYTES`]) are checked on what will be stored.

use std::io::Cursor;

use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageEncoder, Limits};
use vox_proto::{ATTACHMENT_MAX_ANIMATED_BYTES, ATTACHMENT_MAX_BYTES, ATTACHMENT_MAX_SIDE, ATTACHMENT_MAX_THUMB_BYTES, ATTACHMENT_THUMB_BOX};

use crate::linkpreview::thumb::{decode_untrusted, ThumbError};

/// Frames of one animation, at most.
pub const MAX_FRAMES: usize = 2000;
/// Canvas pixels summed over every frame of an animation (e.g. 480x480 x 650 frames).
pub const MAX_ANIMATION_PIXELS: u64 = 150_000_000;
const MAX_ALLOC: u64 = 160 * 1024 * 1024;
/// A thumbnail the hub makes itself (the app sent none): JPEG of this quality.
const THUMB_JPEG_QUALITY: u8 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Jpeg,
    Png,
    Gif,
    WebP,
}

impl Kind {
    pub fn mime(self) -> &'static str {
        match self {
            Kind::Jpeg => "image/jpeg",
            Kind::Png => "image/png",
            Kind::Gif => "image/gif",
            Kind::WebP => "image/webp",
        }
    }

    pub fn from_mime(m: &str) -> Option<Kind> {
        [Kind::Jpeg, Kind::Png, Kind::Gif, Kind::WebP].into_iter().find(|k| k.mime() == m)
    }

    pub fn ext(self) -> &'static str {
        match self {
            Kind::Jpeg => "jpg",
            Kind::Png => "png",
            Kind::Gif => "gif",
            Kind::WebP => "webp",
        }
    }
}

/// Why an upload was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not JPEG / PNG / GIF / WebP.
    Unsupported,
    /// The container or the picture data is broken.
    Damaged,
    /// Wider or taller than allowed, or a decompression bomb.
    Dimensions,
    /// More frames (or frame pixels) than an animation may have.
    TooManyFrames,
    /// Bigger than the size cap (`animated` says which one).
    TooBig { animated: bool },
    /// The thumbnail does not match the image (shape, size, format).
    BadThumb,
}

/// A checked file, ready to store.
#[derive(Debug, Clone)]
pub struct Clean {
    pub bytes: Vec<u8>,
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
    pub animated: bool,
}

/// The magic bytes say which format this is.
pub fn sniff(b: &[u8]) -> Option<Kind> {
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Kind::Jpeg)
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Kind::Png)
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(Kind::Gif)
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(Kind::WebP)
    } else {
        None
    }
}

fn thumb_err(e: ThumbError) -> Refusal {
    match e {
        ThumbError::Unsupported => Refusal::Unsupported,
        ThumbError::TooLarge => Refusal::Dimensions,
        ThumbError::Decode => Refusal::Damaged,
    }
}

/// Check (and clean) an uploaded image. Returns the file to store and its first frame (for a
/// thumbnail the hub may have to make).
pub fn image(raw: &[u8]) -> Result<(Clean, DynamicImage), Refusal> {
    let kind = sniff(raw).ok_or(Refusal::Unsupported)?;
    let (mut bytes, container_animated) = strip(kind, raw).ok_or(Refusal::Damaged)?;
    let mut kind_out = kind;
    let first = if kind == Kind::Jpeg && needs_rotation(raw) {
        // EXIF said "rotate": re-encode upright (the EXIF itself is gone either way).
        let img = decode_untrusted(raw).map_err(thumb_err)?;
        bytes = encode_jpeg(&img, 90).ok_or(Refusal::Damaged)?;
        kind_out = Kind::Jpeg;
        img
    } else {
        decode_untrusted(&bytes).map_err(thumb_err)?
    };
    let (width, height) = (first.width(), first.height());
    if width == 0 || height == 0 || width > ATTACHMENT_MAX_SIDE || height > ATTACHMENT_MAX_SIDE {
        return Err(Refusal::Dimensions);
    }
    let animated = container_animated && verify_animation(kind, &bytes, width, height)? > 1;
    let cap = if animated { ATTACHMENT_MAX_ANIMATED_BYTES } else { ATTACHMENT_MAX_BYTES };
    if bytes.len() > cap {
        return Err(Refusal::TooBig { animated });
    }
    Ok((Clean { bytes, kind: kind_out, width, height, animated }, first))
}

/// Check (and clean) the thumbnail the app sent for an image of `w`x`h`: a still JPEG / PNG /
/// WebP inside the thumbnail box, with the image's shape.
pub fn thumb(raw: &[u8], w: u32, h: u32) -> Result<Clean, Refusal> {
    if raw.len() > ATTACHMENT_MAX_THUMB_BYTES {
        return Err(Refusal::BadThumb);
    }
    let kind = sniff(raw).ok_or(Refusal::BadThumb)?;
    if kind == Kind::Gif {
        return Err(Refusal::BadThumb);
    }
    let (bytes, animated) = strip(kind, raw).ok_or(Refusal::BadThumb)?;
    if animated {
        return Err(Refusal::BadThumb);
    }
    let img = decode_untrusted(&bytes).map_err(|_| Refusal::BadThumb)?;
    let (tw, th) = (img.width(), img.height());
    if tw == 0 || th == 0 || tw > ATTACHMENT_THUMB_BOX || th > ATTACHMENT_THUMB_BOX || tw > w || th > h {
        return Err(Refusal::BadThumb);
    }
    // Same shape as the image (rounding allows a pixel or two).
    let expect_h = (f64::from(tw) * f64::from(h) / f64::from(w)).round();
    let expect_w = (f64::from(th) * f64::from(w) / f64::from(h)).round();
    if (expect_h - f64::from(th)).abs() > 2.0 && (expect_w - f64::from(tw)).abs() > 2.0 {
        return Err(Refusal::BadThumb);
    }
    Ok(Clean { bytes, kind, width: tw, height: th, animated: false })
}

/// The hub's own thumbnail of `first` (the app sent none and the image is bigger than the
/// thumbnail box): JPEG, or PNG when it has transparency.
pub fn make_thumb(first: &DynamicImage) -> Option<Clean> {
    let t = first.thumbnail(ATTACHMENT_THUMB_BOX, ATTACHMENT_THUMB_BOX);
    let transparent = t.color().has_alpha() && t.to_rgba8().pixels().any(|p| p[3] < 255);
    let (bytes, kind) = if transparent {
        let rgba = t.to_rgba8();
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out).write_image(rgba.as_raw(), rgba.width(), rgba.height(), image::ExtendedColorType::Rgba8).ok()?;
        (out, Kind::Png)
    } else {
        (encode_jpeg(&t, THUMB_JPEG_QUALITY)?, Kind::Jpeg)
    };
    Some(Clean { bytes, kind, width: t.width(), height: t.height(), animated: false })
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(&rgb).ok()?;
    Some(out)
}

/// Does this JPEG's EXIF ask for a rotation / flip?
fn needs_rotation(raw: &[u8]) -> bool {
    let Ok(reader) = image::ImageReader::with_format(Cursor::new(raw), image::ImageFormat::Jpeg).into_decoder() else { return false };
    let mut reader = reader;
    !matches!(reader.orientation(), Ok(image::metadata::Orientation::NoTransforms) | Err(_))
}

fn limits() -> Limits {
    let mut l = Limits::default();
    l.max_image_width = Some(ATTACHMENT_MAX_SIDE);
    l.max_image_height = Some(ATTACHMENT_MAX_SIDE);
    l.max_alloc = Some(MAX_ALLOC);
    l
}

/// Decode every frame of an animation within the budget; returns the frame count.
fn verify_animation(kind: Kind, bytes: &[u8], w: u32, h: u32) -> Result<usize, Refusal> {
    let per_frame = u64::from(w) * u64::from(h);
    let frames = match kind {
        Kind::Gif => {
            let mut d = GifDecoder::new(Cursor::new(bytes)).map_err(|_| Refusal::Damaged)?;
            d.set_limits(limits()).map_err(|_| Refusal::Dimensions)?;
            d.into_frames()
        }
        Kind::WebP => {
            let mut d = WebPDecoder::new(Cursor::new(bytes)).map_err(|_| Refusal::Damaged)?;
            d.set_limits(limits()).map_err(|_| Refusal::Dimensions)?;
            if !d.has_animation() {
                return Ok(1);
            }
            d.into_frames()
        }
        _ => return Ok(1),
    };
    let mut n = 0usize;
    for f in frames {
        f.map_err(|e| match e {
            image::ImageError::Limits(_) => Refusal::Dimensions,
            _ => Refusal::Damaged,
        })?;
        n += 1;
        if n > MAX_FRAMES || per_frame.saturating_mul(n as u64) > MAX_ANIMATION_PIXELS {
            return Err(Refusal::TooManyFrames);
        }
    }
    if n == 0 {
        return Err(Refusal::Damaged);
    }
    Ok(n)
}

// ---------------------------------------------------------------- container cleaning

/// The file without metadata, and whether its container says it is animated. `None`: the
/// container is broken.
pub fn strip(kind: Kind, b: &[u8]) -> Option<(Vec<u8>, bool)> {
    match kind {
        Kind::Jpeg => strip_jpeg(b).map(|v| (v, false)),
        Kind::Png => strip_png(b).map(|v| (v, false)),
        Kind::Gif => strip_gif(b).map(|(v, frames)| (v, frames > 1)),
        Kind::WebP => strip_webp(b),
    }
}

fn be16(b: &[u8], i: usize) -> Option<usize> {
    Some(usize::from(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?])))
}

fn le32(b: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize)
}

fn be32(b: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize)
}

/// JPEG: keep the picture's segments (tables, frame, scans, restart intervals) and Adobe's APP14
/// (colour transform); drop APP0-APP15 otherwise (JFIF, EXIF, XMP, ICC, maker notes...) and
/// comments, and anything after the end marker.
fn strip_jpeg(b: &[u8]) -> Option<Vec<u8>> {
    if !b.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len());
    out.extend([0xff, 0xd8]);
    let mut i = 2;
    let mut seen_scan = false;
    loop {
        if i >= b.len() {
            // Truncated after the picture data: close it and let the decoder judge.
            if !seen_scan {
                return None;
            }
            out.extend([0xff, 0xd9]);
            return Some(out);
        }
        if b[i] != 0xff {
            return None;
        }
        while i < b.len() && b[i] == 0xff {
            i += 1;
        }
        let m = *b.get(i)?;
        i += 1;
        match m {
            0xd9 => {
                out.extend([0xff, 0xd9]);
                return Some(out);
            }
            0xd0..=0xd7 | 0x01 => out.extend([0xff, m]),
            0xd8 | 0x00 => return None,
            _ => {
                let len = be16(b, i)?;
                if len < 2 || i + len > b.len() {
                    return None;
                }
                let seg = &b[i..i + len];
                let keep = match m {
                    0xee => seg.get(2..7) == Some(b"Adobe"),
                    0xe0..=0xef | 0xfe => false,
                    _ => true,
                };
                if keep {
                    out.extend([0xff, m]);
                    out.extend(seg);
                }
                i += len;
                if m == 0xda {
                    seen_scan = true;
                    // Entropy-coded data runs to the next marker that is not a stuffed byte or a
                    // restart marker.
                    let start = i;
                    while i + 1 < b.len() {
                        if b[i] == 0xff {
                            let n = b[i + 1];
                            if n == 0x00 || (0xd0..=0xd7).contains(&n) {
                                i += 2;
                                continue;
                            }
                            if n != 0xff {
                                break;
                            }
                        }
                        i += 1;
                    }
                    if i + 1 >= b.len() {
                        i = b.len();
                    }
                    out.extend(&b[start..i]);
                }
            }
        }
    }
}

/// PNG: only the chunks that make the (first) picture: `IHDR`, `PLTE`, `tRNS`, `IDAT`, `sRGB`,
/// `IEND`. Text, EXIF, ICC, time stamps and APNG frames are dropped (an APNG becomes its still
/// default image).
fn strip_png(b: &[u8]) -> Option<Vec<u8>> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !b.starts_with(SIG) {
        return None;
    }
    let mut out = SIG.to_vec();
    let mut i = SIG.len();
    let mut ihdr = false;
    loop {
        let len = be32(b, i)?;
        let end = i.checked_add(12)?.checked_add(len)?;
        if end > b.len() {
            return None;
        }
        let kind = &b[i + 4..i + 8];
        if kind == b"IHDR" {
            ihdr = true;
        }
        if matches!(kind, b"IHDR" | b"PLTE" | b"tRNS" | b"IDAT" | b"sRGB" | b"IEND") {
            out.extend(&b[i..end]);
        }
        i = end;
        if kind == b"IEND" {
            return ihdr.then_some(out);
        }
    }
}

/// Copy GIF data sub-blocks starting at `i` (up to and including the terminator) when `out` is
/// given; returns the index after them.
fn gif_sub_blocks(b: &[u8], mut i: usize, mut out: Option<&mut Vec<u8>>) -> Option<usize> {
    loop {
        let n = usize::from(*b.get(i)?);
        let end = i + 1 + n;
        if end > b.len() {
            return None;
        }
        if let Some(o) = out.as_deref_mut() {
            o.extend(&b[i..end]);
        }
        i = end;
        if n == 0 {
            return Some(i);
        }
    }
}

/// GIF: header, palettes, frames and graphic-control extensions (frame timing, transparency) and
/// the loop-count application extension; comments, plain text and other application extensions
/// (XMP lives there) are dropped. Returns the file and its frame count.
fn strip_gif(b: &[u8]) -> Option<(Vec<u8>, u32)> {
    if b.len() < 13 || !(b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return None;
    }
    let mut out = b[..13].to_vec();
    let mut i = 13;
    let flags = b[10];
    if flags & 0x80 != 0 {
        let n = 3 * (1usize << ((flags & 7) + 1));
        out.extend(b.get(i..i + n)?);
        i += n;
    }
    let mut frames = 0u32;
    loop {
        let Some(&block) = b.get(i) else {
            // Truncated after at least one frame: close it and let the decoder judge.
            if frames == 0 {
                return None;
            }
            out.push(0x3b);
            return Some((out, frames));
        };
        match block {
            0x3b => {
                out.push(0x3b);
                return (frames > 0).then_some((out, frames));
            }
            0x2c => {
                let desc = b.get(i..i + 10)?;
                out.extend(desc);
                i += 10;
                let lflags = desc[9];
                if lflags & 0x80 != 0 {
                    let n = 3 * (1usize << ((lflags & 7) + 1));
                    out.extend(b.get(i..i + n)?);
                    i += n;
                }
                out.push(*b.get(i)?); // LZW minimum code size
                i = gif_sub_blocks(b, i + 1, Some(&mut out))?;
                frames += 1;
            }
            0x21 => {
                let label = *b.get(i + 1)?;
                let keep = match label {
                    0xf9 => true,
                    0xff => {
                        let id = b.get(i + 2..i + 14)?;
                        id[0] == 11 && (&id[1..] == b"NETSCAPE2.0" || &id[1..] == b"ANIMEXTS1.0")
                    }
                    _ => false,
                };
                if keep {
                    out.extend([0x21, label]);
                    i = gif_sub_blocks(b, i + 2, Some(&mut out))?;
                } else {
                    i = gif_sub_blocks(b, i + 2, None)?;
                }
            }
            _ => return None,
        }
    }
}

/// WebP: keep `VP8 `, `VP8L`, `VP8X`, `ALPH`, `ANIM`, `ANMF`; drop `ICCP`, `EXIF`, `XMP ` and
/// anything unknown (clearing their flags in `VP8X`). Returns the file and whether it is an
/// animation.
fn strip_webp(b: &[u8]) -> Option<(Vec<u8>, bool)> {
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return None;
    }
    let end = 8usize.checked_add(le32(b, 4)?)?.min(b.len());
    let mut out = Vec::with_capacity(b.len());
    out.extend(b"RIFF\0\0\0\0WEBP");
    let mut i = 12;
    let (mut anim_flag, mut anim_frames, mut image_chunk) = (false, 0u32, false);
    while i + 8 <= end {
        let fourcc: [u8; 4] = b[i..i + 4].try_into().ok()?;
        let size = le32(b, i + 4)?;
        let start = i + 8;
        let stop = start.checked_add(size)?;
        if stop > end {
            return None;
        }
        let keep = matches!(&fourcc, b"VP8 " | b"VP8L" | b"VP8X" | b"ALPH" | b"ANIM" | b"ANMF");
        if keep {
            out.extend(fourcc);
            out.extend((size as u32).to_le_bytes());
            if &fourcc == b"VP8X" {
                let mut d = b[start..stop].to_vec();
                if let Some(f) = d.first_mut() {
                    anim_flag = *f & 0x02 != 0;
                    *f &= !(0x20 | 0x08 | 0x04); // ICC, EXIF, XMP
                }
                out.extend(d);
            } else {
                out.extend(&b[start..stop]);
            }
            if size & 1 == 1 {
                out.push(0);
            }
            match &fourcc {
                b"ANMF" => anim_frames += 1,
                b"VP8 " | b"VP8L" => image_chunk = true,
                _ => {}
            }
        }
        i = stop + (size & 1);
    }
    if !image_chunk && anim_frames == 0 {
        return None;
    }
    let riff = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff.to_le_bytes());
    Some((out, anim_flag && anim_frames > 0))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::linkpreview::thumb::test_images::{jpeg, png, png_claiming};
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    pub fn with_app1(jpeg: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut dirty = jpeg[..2].to_vec();
        dirty.extend([0xff, 0xe1]);
        dirty.extend(((payload.len() + 2) as u16).to_be_bytes());
        dirty.extend(payload);
        dirty.extend([0xff, 0xfe, 0x00, 0x0a]);
        dirty.extend(b"COMMENT!");
        dirty.extend(&jpeg[2..]);
        dirty
    }

    pub fn gif(frames: &[Rgba<u8>], w: u32, h: u32) -> Vec<u8> {
        use image::codecs::gif::{GifEncoder, Repeat};
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut out);
            enc.set_repeat(Repeat::Infinite).unwrap();
            for c in frames {
                enc.encode_frame(image::Frame::new(RgbaImage::from_pixel(w, h, *c))).unwrap();
            }
        }
        out
    }

    /// A GIF with a comment and an XMP application extension spliced in after the header.
    fn gif_with_junk(g: &[u8]) -> Vec<u8> {
        let flags = g[10];
        let gct = if flags & 0x80 != 0 { 3 * (1usize << ((flags & 7) + 1)) } else { 0 };
        let at = 13 + gct;
        let mut out = g[..at].to_vec();
        out.extend([0x21, 0xfe, 7]);
        out.extend(b"SECRET1");
        out.push(0);
        out.extend([0x21, 0xff, 11]);
        out.extend(b"XMP DataXMP");
        out.extend([7]);
        out.extend(b"SECRET2");
        out.push(0);
        out.extend(&g[at..]);
        out
    }

    /// Lossless WebP made by `image`, then wrapped as VP8X with EXIF / XMP / ICCP chunks.
    fn webp_with_metadata() -> Vec<u8> {
        let img = RgbImage::from_fn(64, 48, |x, y| Rgb([(x * 4) as u8, (y * 5) as u8, 90]));
        let mut plain = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut plain).encode(img.as_raw(), 64, 48, image::ExtendedColorType::Rgb8).unwrap();
        let vp8l = &plain[12..]; // the VP8L chunk
        let chunk = |fourcc: &[u8], data: &[u8]| {
            let mut c = fourcc.to_vec();
            c.extend((data.len() as u32).to_le_bytes());
            c.extend(data);
            if data.len() % 2 == 1 {
                c.push(0);
            }
            c
        };
        let mut vp8x = vec![0x20 | 0x08 | 0x04, 0, 0, 0];
        vp8x.extend(&63u32.to_le_bytes()[..3]);
        vp8x.extend(&47u32.to_le_bytes()[..3]);
        let mut body = b"WEBP".to_vec();
        body.extend(chunk(b"VP8X", &vp8x));
        body.extend(chunk(b"ICCP", b"SECRET-ICC"));
        body.extend(vp8l);
        body.extend(chunk(b"EXIF", b"SECRET-GPS"));
        body.extend(chunk(b"XMP ", b"SECRET-XMP!"));
        let mut out = b"RIFF".to_vec();
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    fn has(b: &[u8], needle: &[u8]) -> bool {
        b.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn jpeg_metadata_is_cut_and_the_picture_kept() {
        let src = jpeg(320, 200);
        let dirty = with_app1(&src, b"Exif\0\0SECRET-GPS-DATA");
        let (clean, first) = image(&dirty).unwrap();
        assert_eq!((clean.kind, clean.width, clean.height, clean.animated), (Kind::Jpeg, 320, 200, false));
        assert!(!has(&clean.bytes, b"SECRET") && !has(&clean.bytes, b"COMMENT"));
        assert!(clean.bytes.len() <= src.len(), "not re-encoded: no bigger than the original");
        assert_eq!((first.width(), first.height()), (320, 200));
    }

    #[test]
    fn a_rotated_jpeg_is_re_encoded_upright() {
        let src = RgbImage::from_fn(400, 200, |x, _| if x < 200 { Rgb([255, 0, 0]) } else { Rgb([0, 0, 255]) });
        let mut j = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut j, 95).encode_image(&src).unwrap();
        let exif = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0";
        let (clean, _) = image(&with_app1(&j, exif)).unwrap();
        assert_eq!((clean.width, clean.height), (200, 400));
        assert!(!has(&clean.bytes, b"Exif"));
        let back = image::load_from_memory(&clean.bytes).unwrap();
        assert_eq!((back.width(), back.height()), (200, 400));
    }

    #[test]
    fn png_text_chunks_are_dropped() {
        let p = png(100, 80);
        // Splice a tEXt chunk after IHDR (8 + 25 bytes).
        let mut dirty = p[..33].to_vec();
        let mut text = b"tEXtComment\0SECRET".to_vec();
        let crc = 0u32; // a broken CRC is fine: the chunk goes away
        dirty.extend(((text.len() - 4) as u32).to_be_bytes());
        dirty.append(&mut text);
        dirty.extend(crc.to_be_bytes());
        dirty.extend(&p[33..]);
        let (clean, _) = image(&dirty).unwrap();
        assert!(!has(&clean.bytes, b"SECRET"));
        assert_eq!((clean.kind, clean.width, clean.height), (Kind::Png, 100, 80));
    }

    #[test]
    fn gif_junk_is_dropped_and_animation_kept() {
        let g = gif(&[Rgba([255, 0, 0, 255]), Rgba([0, 255, 0, 255]), Rgba([0, 0, 255, 255])], 40, 30);
        let dirty = gif_with_junk(&g);
        assert!(has(&dirty, b"SECRET1"));
        let (clean, _) = image(&dirty).unwrap();
        assert!(!has(&clean.bytes, b"SECRET"));
        assert!(clean.animated && clean.kind == Kind::Gif);
        assert!(has(&clean.bytes, b"NETSCAPE2.0"), "the loop count stays");
        // One frame is not an animation.
        let (one, _) = image(&gif(&[Rgba([1, 2, 3, 255])], 10, 10)).unwrap();
        assert!(!one.animated);
    }

    #[test]
    fn webp_metadata_chunks_and_flags_are_dropped() {
        let dirty = webp_with_metadata();
        assert!(image::load_from_memory(&dirty).is_ok(), "the test file itself is valid");
        let (clean, _) = image(&dirty).unwrap();
        assert!(!has(&clean.bytes, b"SECRET") && !has(&clean.bytes, b"EXIF") && !has(&clean.bytes, b"ICCP"));
        assert_eq!(clean.bytes[20] & (0x20 | 0x08 | 0x04), 0, "VP8X flags cleared");
        assert_eq!((clean.width, clean.height, clean.kind), (64, 48, Kind::WebP));
        assert_eq!(u32::from_le_bytes(clean.bytes[4..8].try_into().unwrap()) as usize, clean.bytes.len() - 8);
    }

    #[test]
    fn bombs_junk_and_oversize_are_refused() {
        assert_eq!(image(b"<html>").unwrap_err(), Refusal::Unsupported);
        assert_eq!(image(b"").unwrap_err(), Refusal::Unsupported);
        assert_eq!(image(&png_claiming(30_000, 30_000)).unwrap_err(), Refusal::Dimensions);
        assert_eq!(image(&png(4200, 10)).unwrap_err(), Refusal::Dimensions, "wider than the 4096 cap");
        let mut cut = png(300, 300);
        cut.truncate(cut.len() / 2);
        assert_eq!(image(&cut).unwrap_err(), Refusal::Damaged);
        // A noisy 1800x1800 PNG is way over 3 MB.
        let mut seed = 1u32;
        let noisy = RgbImage::from_fn(1800, 1800, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            Rgb([(seed >> 24) as u8, (seed >> 16) as u8, (seed >> 8) as u8])
        });
        let mut big = Vec::new();
        image::codecs::png::PngEncoder::new(&mut big).write_image(noisy.as_raw(), 1800, 1800, image::ExtendedColorType::Rgb8).unwrap();
        assert!(big.len() > ATTACHMENT_MAX_BYTES);
        assert_eq!(image(&big).unwrap_err(), Refusal::TooBig { animated: false });
    }

    #[test]
    fn thumbnails_must_match_the_image() {
        let t = jpeg(400, 225);
        assert!(thumb(&t, 1920, 1080).is_ok());
        assert_eq!(thumb(&t, 1080, 1920).unwrap_err(), Refusal::BadThumb, "wrong shape");
        assert_eq!(thumb(&jpeg(500, 281), 1920, 1080).unwrap_err(), Refusal::BadThumb, "outside the box");
        assert_eq!(thumb(&gif(&[Rgba([0, 0, 0, 255])], 40, 40), 400, 400).unwrap_err(), Refusal::BadThumb);
        assert_eq!(thumb(b"junk", 10, 10).unwrap_err(), Refusal::BadThumb);
    }

    #[test]
    fn the_hub_thumbnail_fits_the_box_and_keeps_transparency() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(1600, 900, Rgb([10, 20, 30])));
        let t = make_thumb(&img).unwrap();
        assert_eq!((t.width, t.height, t.kind), (400, 225, Kind::Jpeg));
        let alpha = DynamicImage::ImageRgba8(RgbaImage::from_pixel(800, 800, Rgba([0, 0, 0, 0])));
        assert_eq!(make_thumb(&alpha).unwrap().kind, Kind::Png);
    }
}
