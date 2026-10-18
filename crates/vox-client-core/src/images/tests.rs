//! The compression pipeline on generated pictures (never anybody's real photos).

use super::*;
use image::{ImageEncoder, Rgb, RgbImage, Rgba};

/// A deterministic "random" stream.
pub struct Rng(u32);

impl Rng {
    pub fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0 >> 8
    }

    /// Roughly Gaussian noise of about `amp` levels.
    pub fn noise(&mut self, amp: i32) -> i32 {
        let s: i32 = (0..3).map(|_| (self.next() % 256) as i32 - 128).sum();
        s * amp / 220
    }
}

/// Value noise: random levels on a grid of `cell` pixels, bilinearly interpolated.
struct ValueNoise {
    cell: f32,
    gw: usize,
    grid: Vec<f32>,
}

impl ValueNoise {
    fn new(w: u32, h: u32, cell: u32, rng: &mut Rng) -> Self {
        let gw = (w / cell + 2) as usize;
        let gh = (h / cell + 2) as usize;
        ValueNoise { cell: cell as f32, gw, grid: (0..gw * gh).map(|_| (rng.next() % 1000) as f32 / 500.0 - 1.0).collect() }
    }

    fn at(&self, x: u32, y: u32) -> f32 {
        let (fx, fy) = (x as f32 / self.cell, y as f32 / self.cell);
        let (ix, iy) = (fx as usize, fy as usize);
        let (tx, ty) = (fx - ix as f32, fy - iy as f32);
        let g = |i: usize, j: usize| self.grid[j * self.gw + i];
        let top = g(ix, iy) * (1.0 - tx) + g(ix + 1, iy) * tx;
        let bottom = g(ix, iy + 1) * (1.0 - tx) + g(ix + 1, iy + 1) * tx;
        top * (1.0 - ty) + bottom * ty
    }
}

/// A phone-photo-like picture: colour fields and soft shapes, texture at every scale (like
/// foliage, fabric, gravel) and sensor noise.
pub fn photo_like(w: u32, h: u32, seed: u32) -> RgbImage {
    let mut rng = Rng(seed);
    let blobs: Vec<(f32, f32, f32, [f32; 3])> = (0..12)
        .map(|_| {
            let x = (rng.next() % w) as f32;
            let y = (rng.next() % h) as f32;
            let r = (w.min(h) / 8 + rng.next() % (w.min(h) / 3)) as f32;
            (x, y, r, [(rng.next() % 200) as f32, (rng.next() % 200) as f32, (rng.next() % 200) as f32])
        })
        .collect();
    let scale = (w.max(h) / 1000).max(1);
    let octaves: Vec<(ValueNoise, f32)> =
        [(2, 16.0), (6, 14.0), (20, 12.0), (80, 10.0), (300, 14.0)].iter().map(|&(c, a)| (ValueNoise::new(w, h, c * scale, &mut rng), a)).collect();
    let tint = ValueNoise::new(w, h, 40 * scale, &mut rng);
    RgbImage::from_fn(w, h, |x, y| {
        let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
        let mut c = [60.0 + 120.0 * fx, 80.0 + 90.0 * fy, 150.0 - 80.0 * fx * fy];
        for (bx, by, r, col) in &blobs {
            let d2 = ((x as f32 - bx).powi(2) + (y as f32 - by).powi(2)) / (r * r);
            if d2 < 1.0 {
                let k = (1.0 - d2) * 0.8;
                for i in 0..3 {
                    c[i] = c[i] * (1.0 - k) + col[i] * k;
                }
            }
        }
        let texture: f32 = octaves.iter().map(|(n, a)| n.at(x, y) * a).sum();
        let t = tint.at(x, y) * 12.0;
        let mut px = [0u8; 3];
        for i in 0..3 {
            let shift = if i == 1 { t } else { -t * 0.5 };
            px[i] = (c[i] + texture + shift + rng.noise(7) as f32).clamp(0.0, 255.0) as u8;
        }
        Rgb(px)
    })
}

/// A screenshot-like picture: window chrome, panels and lines of anti-aliased "text".
pub fn screenshot_like(w: u32, h: u32, seed: u32) -> RgbImage {
    let mut rng = Rng(seed);
    let mut img = RgbImage::from_pixel(w, h, Rgb([49, 51, 56]));
    let fill = |img: &mut RgbImage, x0: u32, y0: u32, x1: u32, y1: u32, c: Rgb<u8>| {
        for y in y0..y1.min(img.height()) {
            for x in x0..x1.min(img.width()) {
                img.put_pixel(x, y, c);
            }
        }
    };
    fill(&mut img, 0, 0, w, 32, Rgb([30, 31, 34]));
    fill(&mut img, 0, 32, w / 6, h, Rgb([43, 45, 49]));
    fill(&mut img, w - w / 7, 32, w, h, Rgb([43, 45, 49]));
    // Text: rows of glyphs, each a few strokes with a grey anti-aliased edge.
    let shades = [Rgb([220, 221, 222]), Rgb([150, 152, 156]), Rgb([96, 98, 102])];
    let mut y = 50;
    while y + 14 < h {
        let mut x = w / 6 + 16;
        let line_end = w - w / 7 - 16 - rng.next() % (w / 3);
        let colour = if rng.next().is_multiple_of(9) { Rgb([88, 101, 242]) } else { shades[0] };
        while x + 9 < line_end {
            let gw = 4 + rng.next() % 5;
            if !rng.next().is_multiple_of(6) {
                for _ in 0..3 {
                    let sx = x + rng.next() % gw;
                    let sy = y + rng.next() % 6;
                    let (vw, vh) = if rng.next().is_multiple_of(2) { (1, 4 + rng.next() % 6) } else { (2 + rng.next() % 4, 1) };
                    fill(&mut img, sx, sy, sx + vw, sy + vh, colour);
                    fill(&mut img, sx + vw, sy, sx + vw + 1, sy + vh, shades[2]);
                }
            }
            x += gw + 2;
        }
        y += if rng.next().is_multiple_of(5) { 34 } else { 20 };
    }
    img
}

pub fn png_bytes(img: &RgbImage) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8).unwrap();
    out
}

pub fn jpeg_bytes(img: &RgbImage, q: u8) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q).encode_image(img).unwrap();
    out
}

/// An animated GIF of `frames` frames: a ball moving over a gradient (meme-like).
pub fn gif_bytes(w: u32, h: u32, frames: u32) -> Vec<u8> {
    use image::codecs::gif::{GifEncoder, Repeat};
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new_with_speed(&mut out, 10);
        enc.set_repeat(Repeat::Infinite).unwrap();
        for i in 0..frames {
            let cx = (i * w / frames) as i32;
            let img = image::RgbaImage::from_fn(w, h, |x, y| {
                let d = (x as i32 - cx).pow(2) + (y as i32 - h as i32 / 2).pow(2);
                if d < (h as i32 / 5).pow(2) { Rgba([240, 200, 30, 255]) } else { Rgba([(x * 255 / w) as u8, 60, (y * 255 / h) as u8, 255]) }
            });
            enc.encode_frame(image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(80, 1))).unwrap();
        }
    }
    out
}

/// Splice an APP1/EXIF segment (big-endian TIFF, Orientation = `orientation`, plus a fake GPS
/// string) after the SOI marker.
pub fn with_exif(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    let mut exif = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
    exif.extend(orientation.to_be_bytes());
    exif.extend(b"\0\0\0\0\0\0SECRET-GPS-23.5505S-46.6333W");
    let mut out = jpeg[..2].to_vec();
    out.extend([0xff, 0xe1]);
    out.extend(((exif.len() + 2) as u16).to_be_bytes());
    out.extend(exif);
    out.extend(&jpeg[2..]);
    out
}

fn has(b: &[u8], needle: &[u8]) -> bool {
    b.windows(needle.len()).any(|w| w == needle)
}

fn no_progress(_: f32) {}

fn decode_out(c: &Compressed) -> DynamicImage {
    image::load_from_memory(&c.bytes).unwrap()
}

#[test]
fn photos_are_resized_and_shrunk_without_metadata() {
    let src = jpeg_bytes(&photo_like(3000, 2000, 1), 92);
    let dirty = with_exif(&src, 1);
    let c = compress(&dirty, Quality::Normal, &no_progress).unwrap();
    assert_eq!((c.width, c.height), (1920, 1280));
    assert!(!c.screenshot, "a photo is not a screenshot");
    assert!(matches!(c.encoding, Encoding::WebpLossy | Encoding::Jpeg), "{:?}", c.encoding);
    assert!(c.bytes.len() * 4 < dirty.len(), "{} -> {}", dirty.len(), c.bytes.len());
    assert!(!has(&c.bytes, b"SECRET") && !has(&c.bytes, b"Exif"), "metadata gone");
    let back = decode_out(&c);
    assert_eq!((back.width(), back.height()), (1920, 1280));
    // Thumbnail and preview.
    let t = image::load_from_memory(c.thumb.as_ref().unwrap()).unwrap();
    assert_eq!((t.width(), t.height(), c.thumb_w, c.thumb_h), (400, 267, 400, 267));
    assert!(c.preview.w <= PREVIEW_BOX && c.preview.h <= PREVIEW_BOX);
    // The higher qualities allow more pixels; nothing is enlarged.
    let hi = compress(&src, Quality::High, &no_progress).unwrap();
    assert_eq!((hi.width, hi.height), (2560, 1707));
    let orig = compress(&src, Quality::Original, &no_progress).unwrap();
    assert_eq!((orig.width, orig.height), (3000, 2000));
    let small = compress(&jpeg_bytes(&photo_like(320, 200, 2), 90), Quality::Normal, &no_progress).unwrap();
    assert_eq!((small.width, small.height), (320, 200));
    assert!(small.thumb.is_none(), "already inside the thumbnail box");
    assert_eq!((small.thumb_w, small.thumb_h), (320, 200));
}

#[test]
fn screenshots_stay_crisp() {
    let shot = screenshot_like(1920, 1080, 3);
    let png = png_bytes(&shot);
    let c = compress(&png, Quality::Normal, &no_progress).unwrap();
    assert!(c.screenshot);
    assert_eq!((c.width, c.height), (1920, 1080));
    assert!(matches!(c.encoding, Encoding::WebpLossless | Encoding::WebpLossy), "{:?}", c.encoding);
    assert!(c.bytes.len() < png.len(), "{} -> {}", png.len(), c.bytes.len());
    // Text pixels survive: lossless is exact, high-quality lossy stays very close.
    let back = decode_out(&c).to_rgb8();
    let mut worst = 0i32;
    for (a, b) in back.pixels().zip(shot.pixels()).step_by(7) {
        for i in 0..3 {
            worst = worst.max((i32::from(a[i]) - i32::from(b[i])).abs());
        }
    }
    if c.encoding == Encoding::WebpLossless {
        assert_eq!(worst, 0);
    } else {
        assert!(worst < 90, "{worst}");
    }
    assert!(!looks_like_screenshot(photo_like(400, 300, 4).as_raw(), 400, 300, 3));
}

#[test]
fn transparency_is_kept() {
    let img = image::RgbaImage::from_fn(900, 600, |x, y| if (x / 60 + y / 60) % 2 == 0 { Rgba([220, 30, 30, 255]) } else { Rgba([0, 0, 0, 0]) });
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png).write_image(img.as_raw(), 900, 600, image::ExtendedColorType::Rgba8).unwrap();
    let c = compress(&png, Quality::Normal, &no_progress).unwrap();
    assert_eq!(c.mime(), "image/webp", "JPEG has no alpha");
    let back = decode_out(&c).to_rgba8();
    assert_eq!(back.get_pixel(90, 10)[3], 0, "transparent stays transparent");
    assert_eq!(back.get_pixel(10, 10)[3], 255);
    assert!(c.bytes.len() < png.len());
    let t = image::load_from_memory(c.thumb.as_ref().unwrap()).unwrap().to_rgba8();
    assert!(t.pixels().any(|p| p[3] < 255), "the thumbnail keeps the transparency");
}

#[test]
fn animated_gifs_stay_animated_and_small() {
    let gif = gif_bytes(480, 270, 30);
    let c = compress(&gif, Quality::Normal, &no_progress).unwrap();
    assert!(c.animated);
    assert!(matches!(c.encoding, Encoding::AnimatedWebp | Encoding::Gif));
    assert!(c.bytes.len() <= gif.len(), "{} -> {}", gif.len(), c.bytes.len());
    assert_eq!((c.width, c.height), (480, 270));
    let t = image::load_from_memory(c.thumb.as_ref().unwrap()).unwrap();
    assert_eq!((t.width(), t.height()), (400, 225));
    // A single-frame GIF is a still.
    let still = compress(&gif_bytes(64, 64, 1), Quality::Normal, &no_progress).unwrap();
    assert!(!still.animated);
    // Junk extensions disappear when the GIF itself is kept.
    let flags = gif[10];
    let at = 13 + if flags & 0x80 != 0 { 3 * (1usize << ((flags & 7) + 1)) } else { 0 };
    let mut dirty = gif[..at].to_vec();
    dirty.extend([0x21, 0xfe, 6]);
    dirty.extend(b"SECRET");
    dirty.push(0);
    dirty.extend(&gif[at..]);
    let clean = strip_gif(&dirty).unwrap();
    assert!(!has(&clean, b"SECRET"));
    assert_eq!(clean.len(), gif.len());
}

#[test]
fn huge_pictures_are_brought_down() {
    // 8000x8000 (64 MP): a cheap pattern, JPEG so the test stays fast.
    let img = RgbImage::from_fn(8000, 8000, |x, y| Rgb([(x / 32 % 256) as u8, (y / 32 % 256) as u8, ((x + y) / 64 % 256) as u8]));
    let jpeg = jpeg_bytes(&img, 80);
    drop(img);
    let c = compress(&jpeg, Quality::Normal, &no_progress).unwrap();
    assert_eq!((c.width, c.height), (1920, 1920));
    assert!(c.bytes.len() < ATTACHMENT_MAX_BYTES);
    let o = compress(&jpeg, Quality::Original, &no_progress).unwrap();
    assert_eq!((o.width, o.height), (4096, 4096));
    assert!(o.bytes.len() <= ATTACHMENT_MAX_BYTES);
}

#[test]
fn exif_orientation_is_applied() {
    // Left half red, right half blue, marked "rotate 90 degrees clockwise" (6).
    let src = RgbImage::from_fn(800, 400, |x, _| if x < 400 { Rgb([230, 20, 20]) } else { Rgb([20, 20, 230]) });
    let c = compress(&with_exif(&jpeg_bytes(&src, 92), 6), Quality::Normal, &no_progress).unwrap();
    assert_eq!((c.width, c.height), (400, 800));
    let back = decode_out(&c).to_rgb8();
    let (top, bottom) = (back.get_pixel(200, 50), back.get_pixel(200, 750));
    assert!(top[0] > 180 && top[2] < 80, "top {top:?}");
    assert!(bottom[2] > 180 && bottom[0] < 80, "bottom {bottom:?}");
    assert!(!has(&c.bytes, b"SECRET"));
}

/// A PNG whose header claims `w`x`h`.
fn png_claiming(w: u32, h: u32) -> Vec<u8> {
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
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let ihdr = [w.to_be_bytes().as_slice(), h.to_be_bytes().as_slice(), &[8, 2, 0, 0, 0]].concat();
    for (kind, data) in [(b"IHDR", ihdr), (b"IDAT", vec![0x78, 0x9c, 1, 0, 0, 0xff, 0xff]), (b"IEND", vec![])] {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(&data);
        out.extend(&body);
        out.extend(crc32(&body).to_be_bytes());
    }
    out
}

#[test]
fn bombs_and_junk_are_refused_before_decoding() {
    assert_eq!(compress(&png_claiming(100_000, 100_000), Quality::Normal, &no_progress).unwrap_err(), CompressError::TooLarge);
    assert_eq!(compress(&png_claiming(15_000, 15_000), Quality::Normal, &no_progress).unwrap_err(), CompressError::TooLarge, "225 MP");
    assert_eq!(compress(b"%PDF-1.7 not an image", Quality::Normal, &no_progress).unwrap_err(), CompressError::NotImage);
    let mut cut = jpeg_bytes(&photo_like(300, 200, 9), 90);
    cut.truncate(100);
    assert!(compress(&cut, Quality::Normal, &no_progress).is_err());
    assert_eq!(compress(&vec![0u8; MAX_INPUT_BYTES + 1], Quality::Normal, &no_progress).unwrap_err(), CompressError::FileTooBig);
}

#[test]
fn progress_goes_up_to_one() {
    let seen = std::sync::Mutex::new(Vec::new());
    compress(&jpeg_bytes(&photo_like(900, 600, 5), 90), Quality::Normal, &|p| seen.lock().unwrap().push(p)).unwrap();
    let seen = seen.into_inner().unwrap();
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
    assert_eq!(seen.last().copied(), Some(1.0));
}

#[test]
fn clipboard_pictures_go_through_the_same_pipeline() {
    let shot = DynamicImage::ImageRgb8(screenshot_like(1280, 720, 6));
    let c = compress_image(shot, 1280 * 720 * 4, Quality::Normal, &no_progress).unwrap();
    assert!(c.screenshot && c.bytes.len() < 1280 * 720);
}

fn kb(n: u64) -> String {
    if n >= 1024 * 1024 { format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)) } else { format!("{} KB", n.div_ceil(1024)) }
}

/// The report's size table (`cargo test -p vox-client-core --lib size_table -- --nocapture`):
/// typical inputs, what they weigh before and after.
#[test]
fn size_table() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("foto de celular 4032x3024 (JPEG q92)", jpeg_bytes(&photo_like(4032, 3024, 11), 92)),
        ("print 1920x1080 (PNG)", png_bytes(&screenshot_like(1920, 1080, 12))),
        ("print 4K 3840x2160 (PNG)", png_bytes(&screenshot_like(3840, 2160, 13))),
        ("meme GIF 480x270, 40 quadros", gif_bytes(480, 270, 40)),
    ];
    eprintln!("| entrada | antes | depois | saída | miniatura | tempo |");
    eprintln!("|---|---|---|---|---|---|");
    for (name, bytes) in cases {
        let c = compress(&bytes, Quality::Normal, &no_progress).unwrap();
        eprintln!(
            "| {name} | {} | {} | {}x{} {} | {} | {} ms |",
            kb(bytes.len() as u64),
            kb(c.bytes.len() as u64),
            c.width,
            c.height,
            c.encoding.label(),
            kb(c.thumb.as_ref().map_or(0, |t| t.len()) as u64),
            c.millis
        );
        assert!(c.bytes.len() < bytes.len(), "{name}");
        assert!(c.bytes.len() <= ATTACHMENT_MAX_BYTES, "{name}");
    }
}

/// What the app sends passes the hub's own checks unchanged (the hub only drops the JPEG's JFIF
/// header), thumbnails included.
#[test]
fn the_hub_accepts_what_the_app_makes() {
    let alpha = image::RgbaImage::from_fn(700, 500, |x, _| Rgba([10, 200, 90, if x < 350 { 0 } else { 255 }]));
    let mut alpha_png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut alpha_png).write_image(alpha.as_raw(), 700, 500, image::ExtendedColorType::Rgba8).unwrap();
    let inputs = [
        jpeg_bytes(&photo_like(2400, 1600, 21), 90),
        png_bytes(&screenshot_like(1920, 1080, 22)),
        alpha_png,
        gif_bytes(320, 180, 12),
        jpeg_bytes(&photo_like(300, 300, 23), 90),
    ];
    for (i, input) in inputs.iter().enumerate() {
        let c = compress(input, Quality::Normal, &no_progress).unwrap();
        let mut body = c.bytes.clone();
        let thumb_len = c.thumb.as_ref().map_or(0, Vec::len);
        if let Some(t) = &c.thumb {
            body.extend(t);
        }
        let up = voxhub_server::attachments::check_upload(&body, thumb_len).unwrap_or_else(|e| panic!("case {i}: {e:?}"));
        assert_eq!((up.main.width, up.main.height, up.main.animated), (c.width, c.height, c.animated), "case {i}");
        assert_eq!(up.main.kind.mime(), c.mime(), "case {i}");
        assert!(up.main.bytes.len() <= c.bytes.len() && up.main.bytes.len() + 64 >= c.bytes.len(), "case {i}: not re-encoded");
        if c.thumb.is_some() {
            let t = up.thumb.as_ref().unwrap();
            assert_eq!((t.width, t.height), (c.thumb_w, c.thumb_h), "case {i}");
        }
    }
}
