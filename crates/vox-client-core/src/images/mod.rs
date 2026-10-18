//! Chat images: the sending side's compression ("Imagens no chat"). Everything heavy happens
//! here, on the sender's PC, so the host's server only ever stores small files:
//!
//! 1. Decode PNG / JPEG / WebP / GIF / BMP under limits (header dimensions checked before any
//!    pixel buffer: a decompression bomb is refused), apply the EXIF orientation.
//! 2. Resize so the longest side is at most [`Quality::max_side`] (never enlarged): an area
//!    average down to about twice the target, then Lanczos3 for the last step (sharp and fast,
//!    even from 8000x8000).
//! 3. Re-encode from the pixels, so no metadata of the original survives (EXIF, GPS, XMP, ICC,
//!    comments): photos as lossy WebP (q80) or JPEG (q82, 4:2:0, progressive, optimised
//!    Huffman tables), whichever is smaller; screenshot-like pictures (large flat areas, few
//!    colours: text and UI) as lossless WebP, or high-quality lossy WebP with sharp chroma when
//!    lossless would be much bigger, so text stays crisp; small or flat pictures also try
//!    lossless and keep it when it is smaller. Transparency is kept (WebP).
//! 4. A thumbnail (WebP, fits [`ATTACHMENT_THUMB_BOX`]) for the chat list, and a tiny preview
//!    for the composer.
//! 5. Animated GIF / WebP: re-encoded as animated WebP (libwebp, per frame lossy or lossless)
//!    or kept as a metadata-free GIF, whichever is smaller; over [`MAX_ANIMATED_BYTES`] (or too
//!    many frames) only the first frame is sent, as a still.

pub mod webp;

use std::collections::HashSet;
use std::io::Cursor;
use std::time::Instant;

use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, Limits, RgbaImage};
use serde::{Deserialize, Serialize};
use vox_proto::{ATTACHMENT_MAX_BYTES, ATTACHMENT_MAX_SIDE, ATTACHMENT_THUMB_BOX};

/// Files bigger than this are not even read.
pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
/// Header dimensions above this are refused before decoding (bombs).
pub const MAX_INPUT_SIDE: u32 = 20_000;
pub const MAX_INPUT_PIXELS: u64 = 100_000_000;
const MAX_DECODE_ALLOC: u64 = 768 * 1024 * 1024;
/// An animation bigger than this after re-encoding goes as its first frame.
pub const MAX_ANIMATED_BYTES: usize = 8 * 1024 * 1024;
/// Decoded frames of an animation, at most (RGBA bytes summed).
const MAX_ANIMATION_DECODED: u64 = 512 * 1024 * 1024;
const MAX_FRAMES: usize = 1500;
/// The hub decodes every frame within this budget (`hub/src/attachments/validate.rs`).
const HUB_ANIMATION_PIXELS: u64 = 140_000_000;
const WEBP_QUALITY: f32 = 80.0;
const WEBP_TEXT_QUALITY: f32 = 90.0;
const JPEG_QUALITY: u8 = 82;
const THUMB_QUALITY: f32 = 75.0;
const ANIM_QUALITY: f32 = 75.0;
/// Lossless is kept for screenshot-like pictures unless it is more than this times the
/// high-quality lossy size.
const LOSSLESS_SLACK: f64 = 1.25;
/// Pictures up to this many pixels also try lossless.
const SMALL_PIXELS: u64 = 512 * 512;
/// The composer's preview fits in this square.
pub const PREVIEW_BOX: u32 = 160;

/// "Qualidade das imagens enviadas".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Quality {
    /// Longest side 1920 px.
    #[default]
    Normal,
    /// 2560 px.
    High,
    /// As it is, up to 4096 px.
    Original,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Normal, Quality::High, Quality::Original];

    pub fn max_side(self) -> u32 {
        match self {
            Quality::Normal => 1920,
            Quality::High => 2560,
            Quality::Original => ATTACHMENT_MAX_SIDE,
        }
    }

    /// Screenshots (text, UI) get more room, so small text stays readable: a 4K print at 1920
    /// px would be half size.
    pub fn max_side_screenshot(self) -> u32 {
        match self {
            Quality::Normal => 2560,
            Quality::High => 3840,
            Quality::Original => ATTACHMENT_MAX_SIDE,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Quality::Normal => "Normal (até 1920 px, recomendado)",
            Quality::High => "Alta (até 2560 px)",
            Quality::Original => "Original (até 4096 px)",
        }
    }
}

/// How the output was encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    WebpLossy,
    WebpLossless,
    Jpeg,
    AnimatedWebp,
    Gif,
}

impl Encoding {
    pub fn mime(self) -> &'static str {
        match self {
            Encoding::WebpLossy | Encoding::WebpLossless | Encoding::AnimatedWebp => "image/webp",
            Encoding::Jpeg => "image/jpeg",
            Encoding::Gif => "image/gif",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Encoding::WebpLossy => "WebP",
            Encoding::WebpLossless => "WebP sem perdas",
            Encoding::Jpeg => "JPEG",
            Encoding::AnimatedWebp => "WebP animado",
            Encoding::Gif => "GIF",
        }
    }
}

/// A small RGBA picture (the composer's preview).
#[derive(Clone)]
pub struct Preview {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Preview({}x{})", self.w, self.h)
    }
}

/// A picture ready to upload.
#[derive(Debug, Clone)]
pub struct Compressed {
    pub bytes: Vec<u8>,
    pub encoding: Encoding,
    pub width: u32,
    pub height: u32,
    pub animated: bool,
    /// WebP thumbnail inside the thumbnail box; `None` when the picture already fits it.
    pub thumb: Option<Vec<u8>>,
    pub thumb_w: u32,
    pub thumb_h: u32,
    /// The picture looked like a screenshot (text / UI).
    pub screenshot: bool,
    /// Size of what the person chose (file or clipboard image as PNG-equivalent bytes).
    pub original_bytes: u64,
    pub original_w: u32,
    pub original_h: u32,
    pub preview: Preview,
    pub millis: u64,
}

impl Compressed {
    pub fn mime(&self) -> &'static str {
        self.encoding.mime()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressError {
    /// Over [`MAX_INPUT_BYTES`].
    FileTooBig,
    NotImage,
    /// Dimensions over the caps (or a decompression bomb).
    TooLarge,
    Damaged,
    /// Could not be made small enough.
    Failed,
}

impl std::fmt::Display for CompressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CompressError::FileTooBig => "O arquivo é grande demais (mais de 64 MB).",
            CompressError::NotImage => "Não é uma imagem que dê para enviar (use PNG, JPEG, WebP, GIF ou BMP).",
            CompressError::TooLarge => "A imagem tem pixels demais.",
            CompressError::Damaged => "Não deu para ler a imagem (arquivo danificado?).",
            CompressError::Failed => "Não deu para compactar a imagem.",
        })
    }
}

impl std::error::Error for CompressError {}

fn limits() -> Limits {
    let mut l = Limits::default();
    l.max_image_width = Some(MAX_INPUT_SIDE);
    l.max_image_height = Some(MAX_INPUT_SIDE);
    l.max_alloc = Some(MAX_DECODE_ALLOC);
    l
}

fn map_err(e: image::ImageError) -> CompressError {
    match e {
        image::ImageError::Limits(_) => CompressError::TooLarge,
        image::ImageError::Unsupported(_) => CompressError::NotImage,
        _ => CompressError::Damaged,
    }
}

/// The formats we read.
pub fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    image::guess_format(bytes).ok().filter(|f| matches!(f, ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif | ImageFormat::Bmp))
}

/// Decode a still picture (first frame) with the EXIF orientation applied.
pub fn decode(bytes: &[u8]) -> Result<DynamicImage, CompressError> {
    let fmt = sniff(bytes).ok_or(CompressError::NotImage)?;
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), fmt);
    reader.limits(limits());
    let mut dec = reader.into_decoder().map_err(map_err)?;
    let (w, h) = dec.dimensions();
    if w == 0 || h == 0 || w > MAX_INPUT_SIDE || h > MAX_INPUT_SIDE || u64::from(w) * u64::from(h) > MAX_INPUT_PIXELS {
        return Err(CompressError::TooLarge);
    }
    let orientation = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec).map_err(map_err)?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Compress a file the person chose (or dropped). `progress` gets 0..=1.
pub fn compress(bytes: &[u8], quality: Quality, progress: &dyn Fn(f32)) -> Result<Compressed, CompressError> {
    let start = Instant::now();
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(CompressError::FileTooBig);
    }
    let fmt = sniff(bytes).ok_or(CompressError::NotImage)?;
    progress(0.05);
    if matches!(fmt, ImageFormat::Gif | ImageFormat::WebP) {
        if let Some(c) = compress_animation(bytes, fmt, quality, progress, start)? {
            return Ok(c);
        }
    }
    let img = decode(bytes)?;
    progress(0.3);
    compress_still(img, bytes.len() as u64, quality, progress, start)
}

/// Compress a decoded picture (a clipboard image). `original_bytes` is what it weighed before.
pub fn compress_image(img: DynamicImage, original_bytes: u64, quality: Quality, progress: &dyn Fn(f32)) -> Result<Compressed, CompressError> {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_INPUT_PIXELS {
        return Err(CompressError::TooLarge);
    }
    compress_still(img, original_bytes, quality, progress, Instant::now())
}

/// `(w, h)` fitted inside a `side` square (never enlarged).
pub fn fit(w: u32, h: u32, side: u32) -> (u32, u32) {
    if w <= side && h <= side {
        return (w, h);
    }
    let s = f64::from(side) / f64::from(w.max(h));
    (((f64::from(w) * s).round() as u32).clamp(1, side), ((f64::from(h) * s).round() as u32).clamp(1, side))
}

/// Downscale to exactly `w`x`h`: an area average close to the target first (fast on huge
/// inputs), then Lanczos3.
fn downscale(img: DynamicImage, w: u32, h: u32) -> DynamicImage {
    if (img.width(), img.height()) == (w, h) {
        return img;
    }
    let img = if img.width() > w * 3 || img.height() > h * 3 { img.thumbnail_exact(w * 2, h * 2) } else { img };
    img.resize_exact(w, h, FilterType::Lanczos3)
}

fn any_transparent(img: &DynamicImage) -> bool {
    match img {
        DynamicImage::ImageRgba8(b) => b.pixels().any(|p| p[3] < 255),
        DynamicImage::ImageLumaA8(b) => b.pixels().any(|p| p[1] < 255),
        i if i.color().has_alpha() => i.to_rgba8().pixels().any(|p| p[3] < 255),
        _ => false,
    }
}

/// Screenshots, UI and text: large runs of identical neighbours and few colours. Photos (sensor
/// noise, gradients) have almost none.
pub fn looks_like_screenshot(px: &[u8], w: u32, h: u32, bpp: usize) -> bool {
    let (w, h) = (w as usize, h as usize);
    if w < 2 || h < 2 {
        return false;
    }
    let step = (h / 300).max(1);
    let (mut same, mut total) = (0u64, 0u64);
    let mut colors: HashSet<u32> = HashSet::new();
    for y in (0..h).step_by(step) {
        let row = &px[y * w * bpp..(y + 1) * w * bpp];
        for x in 1..w {
            let (a, b) = (&row[(x - 1) * bpp..(x - 1) * bpp + 3], &row[x * bpp..x * bpp + 3]);
            same += u64::from(a == b);
            total += 1;
            if x % 5 == 0 && colors.len() < 4096 {
                colors.insert(u32::from_le_bytes([b[0], b[1], b[2], 0]));
            }
        }
    }
    let flat = same as f64 / total.max(1) as f64;
    flat >= 0.5 || (flat >= 0.3 && colors.len() < 2000)
}

/// JPEG, 4:2:0, progressive, optimised Huffman tables, no metadata (a JFIF header only).
pub fn encode_jpeg(rgb: &[u8], w: u32, h: u32, quality: u8) -> Option<Vec<u8>> {
    if w > u32::from(u16::MAX) || h > u32::from(u16::MAX) {
        return None;
    }
    let mut out = Vec::with_capacity(rgb.len() / 10);
    let mut e = jpeg_encoder::Encoder::new(&mut out, quality);
    e.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
    e.set_progressive(true);
    e.set_optimized_huffman_tables(true);
    e.encode(rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).ok()?;
    Some(out)
}

fn smallest(cands: Vec<Option<(Vec<u8>, Encoding)>>) -> Option<(Vec<u8>, Encoding)> {
    cands.into_iter().flatten().min_by_key(|(b, _)| b.len())
}

/// Encode a resized picture; returns the bytes, how, and whether it looked like a screenshot.
fn encode_still(img: &DynamicImage, alpha: bool, progress: &dyn Fn(f32)) -> Option<(Vec<u8>, Encoding, bool)> {
    let (w, h) = (img.width(), img.height());
    let (px, bpp) = if alpha { (img.to_rgba8().into_raw(), 4) } else { (img.to_rgb8().into_raw(), 3) };
    let screenshot = looks_like_screenshot(&px, w, h, bpp);
    progress(0.55);
    let lossy = |q: f32, sharp: bool| webp::encode(&px, w, h, alpha, webp::Opts { lossless: false, quality: q, method: 4, sharp_yuv: sharp }).map(|b| (b, Encoding::WebpLossy));
    let lossless = || webp::encode(&px, w, h, alpha, webp::Opts::lossless()).map(|b| (b, Encoding::WebpLossless));
    let chosen = if screenshot {
        let l = lossless();
        progress(0.7);
        let hq = lossy(WEBP_TEXT_QUALITY, true);
        match (l, hq) {
            (Some(l), Some(hq)) if l.0.len() as f64 <= hq.0.len() as f64 * LOSSLESS_SLACK => Some(l),
            (l, hq) => hq.or(l),
        }
    } else {
        let w80 = lossy(WEBP_QUALITY, false);
        progress(0.7);
        let jpeg = if alpha { None } else { encode_jpeg(&px, w, h, JPEG_QUALITY).map(|b| (b, Encoding::Jpeg)) };
        let mut best = smallest(vec![w80, jpeg]);
        if u64::from(w) * u64::from(h) <= SMALL_PIXELS {
            best = smallest(vec![best, lossless()]);
        }
        best
    }?;
    progress(0.85);
    Some((chosen.0, chosen.1, screenshot))
}

fn preview_of(img: &DynamicImage) -> Preview {
    let p = img.thumbnail(PREVIEW_BOX, PREVIEW_BOX).to_rgba8();
    Preview { w: p.width(), h: p.height(), rgba: p.into_raw() }
}

/// WebP thumbnail inside the thumbnail box (`None` when the picture already fits it).
fn thumb_of(img: &DynamicImage, alpha: bool) -> Option<(Vec<u8>, u32, u32)> {
    if img.width() <= ATTACHMENT_THUMB_BOX && img.height() <= ATTACHMENT_THUMB_BOX {
        return None;
    }
    let t = img.thumbnail(ATTACHMENT_THUMB_BOX, ATTACHMENT_THUMB_BOX);
    let (tw, th) = (t.width(), t.height());
    let px = if alpha { t.to_rgba8().into_raw() } else { t.to_rgb8().into_raw() };
    webp::encode(&px, tw, th, alpha, webp::Opts::lossy(THUMB_QUALITY)).map(|b| (b, tw, th))
}

/// [`looks_like_screenshot`] on a picture of any size and layout (sampled, so a big photo costs
/// a few milliseconds).
fn screenshot_at_full_size(img: &DynamicImage) -> bool {
    let (w, h) = (img.width(), img.height());
    if u64::from(w) * u64::from(h) > 40_000_000 {
        return false; // 40+ MP is a camera, not a screen
    }
    match img {
        DynamicImage::ImageRgb8(b) => looks_like_screenshot(b.as_raw(), w, h, 3),
        DynamicImage::ImageRgba8(b) => looks_like_screenshot(b.as_raw(), w, h, 4),
        other => {
            let rgb = other.to_rgb8();
            looks_like_screenshot(rgb.as_raw(), w, h, 3)
        }
    }
}

fn compress_still(img: DynamicImage, original_bytes: u64, quality: Quality, progress: &dyn Fn(f32), start: Instant) -> Result<Compressed, CompressError> {
    let (ow, oh) = (img.width(), img.height());
    let screen = (ow > quality.max_side() || oh > quality.max_side()) && screenshot_at_full_size(&img);
    let mut side = if screen { quality.max_side_screenshot() } else { quality.max_side() }.min(ATTACHMENT_MAX_SIDE);
    let (w, h) = fit(ow, oh, side);
    let mut resized = downscale(img, w, h);
    progress(0.45);
    let alpha = any_transparent(&resized);
    let mut out = encode_still(&resized, alpha, progress).ok_or(CompressError::Failed)?;
    // Far too big for the hub (a huge noisy "Original"): smaller until it fits.
    let mut tries = 0;
    while out.0.len() > ATTACHMENT_MAX_BYTES && tries < 4 {
        tries += 1;
        side = (f64::from(side.min(resized.width().max(resized.height()))) * 0.75) as u32;
        let (w, h) = fit(resized.width(), resized.height(), side);
        resized = downscale(resized, w, h);
        let px = if alpha { resized.to_rgba8().into_raw() } else { resized.to_rgb8().into_raw() };
        let b = webp::encode(&px, w, h, alpha, webp::Opts::lossy(WEBP_QUALITY - 5.0)).ok_or(CompressError::Failed)?;
        out = (b, Encoding::WebpLossy, out.2);
    }
    if out.0.len() > ATTACHMENT_MAX_BYTES {
        return Err(CompressError::Failed);
    }
    let thumb = thumb_of(&resized, alpha);
    progress(0.95);
    let (tw, th) = thumb.as_ref().map_or((resized.width(), resized.height()), |t| (t.1, t.2));
    let c = Compressed {
        bytes: out.0,
        encoding: out.1,
        width: resized.width(),
        height: resized.height(),
        animated: false,
        thumb: thumb.map(|t| t.0),
        thumb_w: tw,
        thumb_h: th,
        screenshot: out.2,
        original_bytes,
        original_w: ow,
        original_h: oh,
        preview: preview_of(&resized),
        millis: start.elapsed().as_millis() as u64,
    };
    progress(1.0);
    log(&c);
    Ok(c)
}

fn log(c: &Compressed) {
    tracing::info!(
        from = c.original_bytes,
        to = c.bytes.len(),
        thumb = c.thumb.as_ref().map_or(0, Vec::len),
        from_w = c.original_w,
        from_h = c.original_h,
        w = c.width,
        h = c.height,
        encoding = c.encoding.label(),
        screenshot = c.screenshot,
        ms = c.millis,
        "image compressed"
    );
}

/// An animation (GIF / animated WebP): `Ok(None)` = not animated after all, or not worth
/// keeping animated (too big, too many frames): the caller sends the first frame.
fn compress_animation(bytes: &[u8], fmt: ImageFormat, quality: Quality, progress: &dyn Fn(f32), start: Instant) -> Result<Option<Compressed>, CompressError> {
    let frames = match fmt {
        ImageFormat::Gif => {
            let mut d = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(map_err)?;
            d.set_limits(limits()).map_err(map_err)?;
            check_dims(d.dimensions())?;
            d.into_frames()
        }
        ImageFormat::WebP => {
            let mut d = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(map_err)?;
            d.set_limits(limits()).map_err(map_err)?;
            if !d.has_animation() {
                return Ok(None);
            }
            check_dims(d.dimensions())?;
            d.into_frames()
        }
        _ => return Ok(None),
    };
    let mut list: Vec<(RgbaImage, u32)> = Vec::new();
    let mut decoded = 0u64;
    for f in frames {
        let f = f.map_err(map_err)?;
        let (n, d) = f.delay().numer_denom_ms();
        let delay = n.checked_div(d).map_or(100, |ms| ms.max(20));
        let buf = f.into_buffer();
        decoded += buf.as_raw().len() as u64;
        list.push((buf, delay));
        if list.len() > MAX_FRAMES || decoded > MAX_ANIMATION_DECODED {
            tracing::info!(frames = list.len(), "animation too long: sending its first frame");
            return Ok(None);
        }
    }
    if list.len() < 2 {
        return Ok(None);
    }
    progress(0.3);
    let (ow, oh) = list[0].0.dimensions();
    let (mut w, mut h) = fit(ow, oh, quality.max_side());
    // Keep within what the hub decodes: shrink the canvas of long animations.
    let budget = HUB_ANIMATION_PIXELS / list.len() as u64;
    if u64::from(w) * u64::from(h) > budget {
        let s = (budget as f64 / (f64::from(w) * f64::from(h))).sqrt();
        w = ((f64::from(w) * s) as u32).max(1);
        h = ((f64::from(h) * s) as u32).max(1);
    }
    let resized = (w, h) != (ow, oh);
    if resized {
        for (f, _) in &mut list {
            *f = image::imageops::resize(f, w, h, FilterType::Triangle);
        }
    }
    progress(0.45);
    let mut t = 0i32;
    let mut starts = Vec::with_capacity(list.len());
    for (_, d) in &list {
        starts.push(t);
        t = t.saturating_add(*d as i32);
    }
    let webp_frames: Vec<webp::Frame<'_>> = list.iter().zip(&starts).map(|((f, _), s)| webp::Frame { rgba: f.as_raw(), start_ms: *s }).collect();
    let anim = webp::encode_animation(&webp_frames, w, h, t, ANIM_QUALITY).map(|b| (b, Encoding::AnimatedWebp));
    progress(0.85);
    let gif = if fmt == ImageFormat::Gif && !resized { strip_gif(bytes).map(|b| (b, Encoding::Gif)) } else { None };
    let Some((out, encoding)) = smallest(vec![anim, gif]) else { return Ok(None) };
    if out.len() > MAX_ANIMATED_BYTES {
        tracing::info!(bytes = out.len(), "animation too big after compression: sending its first frame");
        return Ok(None);
    }
    let first = DynamicImage::ImageRgba8(list.swap_remove(0).0);
    let alpha = any_transparent(&first);
    let thumb = thumb_of(&first, alpha);
    let (tw, th) = thumb.as_ref().map_or((w, h), |t| (t.1, t.2));
    let c = Compressed {
        bytes: out,
        encoding,
        width: w,
        height: h,
        animated: true,
        thumb: thumb.map(|t| t.0),
        thumb_w: tw,
        thumb_h: th,
        screenshot: false,
        original_bytes: bytes.len() as u64,
        original_w: ow,
        original_h: oh,
        preview: preview_of(&first),
        millis: start.elapsed().as_millis() as u64,
    };
    progress(1.0);
    log(&c);
    Ok(Some(c))
}

fn check_dims((w, h): (u32, u32)) -> Result<(), CompressError> {
    if w == 0 || h == 0 || w > MAX_INPUT_SIDE || h > MAX_INPUT_SIDE || u64::from(w) * u64::from(h) > MAX_INPUT_PIXELS {
        return Err(CompressError::TooLarge);
    }
    Ok(())
}

/// Copy GIF data sub-blocks from `i` (terminator included) into `out` when given.
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

/// The GIF without comments, plain text and application extensions other than the loop count
/// (XMP lives there): frames, palettes and timing are copied byte for byte.
pub fn strip_gif(b: &[u8]) -> Option<Vec<u8>> {
    if b.len() < 13 || !(b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return None;
    }
    let mut out = b[..13].to_vec();
    let mut i = 13;
    if b[10] & 0x80 != 0 {
        let n = 3 * (1usize << ((b[10] & 7) + 1));
        out.extend(b.get(i..i + n)?);
        i += n;
    }
    loop {
        match *b.get(i)? {
            0x3b => {
                out.push(0x3b);
                return Some(out);
            }
            0x2c => {
                let desc = b.get(i..i + 10)?;
                out.extend(desc);
                i += 10;
                if desc[9] & 0x80 != 0 {
                    let n = 3 * (1usize << ((desc[9] & 7) + 1));
                    out.extend(b.get(i..i + n)?);
                    i += n;
                }
                out.push(*b.get(i)?);
                i = gif_sub_blocks(b, i + 1, Some(&mut out))?;
            }
            0x21 => {
                let label = *b.get(i + 1)?;
                let keep = label == 0xf9 || (label == 0xff && b.get(i + 2..i + 14).is_some_and(|id| id[0] == 11 && (&id[1..] == b"NETSCAPE2.0" || &id[1..] == b"ANIMEXTS1.0")));
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

#[cfg(test)]
mod tests;
