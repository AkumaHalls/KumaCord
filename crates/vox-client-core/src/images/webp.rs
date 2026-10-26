//! A thin safe layer over libwebp (statically linked through `libwebp-sys`): still images, lossy
//! or lossless, and animations. No metadata is ever written (no EXIF / XMP / ICC chunks).
//!
//! libwebp is C, not Rust: there is no mature pure-Rust *lossy* WebP encoder (the `image`
//! crate's encoder is lossless only), and lossy WebP is what makes photos small.

use std::ffi::c_void;

use libwebp_sys as ffi;

/// One still encode.
#[derive(Debug, Clone, Copy)]
pub struct Opts {
    pub lossless: bool,
    /// 0..=100 (lossy: quality; lossless: effort).
    pub quality: f32,
    /// 0 (fast) ..= 6 (small).
    pub method: i32,
    /// Sharper chroma for text and thin coloured lines (lossy only; a little slower).
    pub sharp_yuv: bool,
}

impl Opts {
    pub fn lossy(quality: f32) -> Opts {
        Opts { lossless: false, quality, method: 4, sharp_yuv: false }
    }

    pub fn lossless() -> Opts {
        Opts { lossless: true, quality: 75.0, method: 4, sharp_yuv: false }
    }
}

fn config(o: Opts) -> Option<ffi::WebPConfig> {
    let preset = if o.lossless { ffi::WebPPreset::WEBP_PRESET_DEFAULT } else { ffi::WebPPreset::WEBP_PRESET_PHOTO };
    let mut c = ffi::WebPConfig::new_with_preset(preset, o.quality).ok()?;
    if o.lossless {
        c.lossless = 1;
        c.exact = 0;
    }
    c.method = o.method.clamp(0, 6);
    c.use_sharp_yuv = i32::from(o.sharp_yuv && !o.lossless);
    c.alpha_quality = 100;
    c.thread_level = 1;
    // SAFETY: a plain check of a filled struct.
    (unsafe { ffi::WebPValidateConfig(&c) } != 0).then_some(c)
}

/// A picture that frees itself.
struct Picture(ffi::WebPPicture);

impl Picture {
    /// `pixels` is RGBA (`alpha`) or RGB, tightly packed.
    fn import(pixels: &[u8], w: u32, h: u32, alpha: bool, argb: bool) -> Option<Picture> {
        let bpp = if alpha { 4 } else { 3 };
        if w == 0 || h == 0 || w > 16383 || h > 16383 || pixels.len() != (w * h) as usize * bpp {
            return None;
        }
        let mut p = Picture(ffi::WebPPicture::new().ok()?);
        p.0.use_argb = i32::from(argb);
        p.0.width = w as i32;
        p.0.height = h as i32;
        let stride = (w as usize * bpp) as i32;
        // SAFETY: the buffer holds `h` rows of `stride` bytes (checked above); libwebp copies it.
        let ok = unsafe {
            if alpha {
                ffi::WebPPictureImportRGBA(&mut p.0, pixels.as_ptr(), stride)
            } else {
                ffi::WebPPictureImportRGB(&mut p.0, pixels.as_ptr(), stride)
            }
        };
        (ok != 0).then_some(p)
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: the picture was initialised by `WebPPictureNew`-equivalent init; freeing twice is
        // not possible (only here).
        unsafe { ffi::WebPPictureFree(&mut self.0) };
    }
}

/// Encode a still image. `pixels`: RGBA when `alpha`, else RGB.
pub fn encode(pixels: &[u8], w: u32, h: u32, alpha: bool, o: Opts) -> Option<Vec<u8>> {
    let cfg = config(o)?;
    let mut pic = Picture::import(pixels, w, h, alpha, o.lossless)?;
    // SAFETY: the writer lives on this stack frame for the whole encode and is cleared after; the
    // picture is valid; libwebp only calls `WebPMemoryWrite` with `custom_ptr` = the writer.
    unsafe {
        let mut writer: ffi::WebPMemoryWriter = std::mem::zeroed();
        ffi::WebPMemoryWriterInit(&mut writer);
        pic.0.writer = Some(ffi::WebPMemoryWrite);
        pic.0.custom_ptr = (&mut writer as *mut ffi::WebPMemoryWriter).cast::<c_void>();
        let ok = ffi::WebPEncode(&cfg, &mut pic.0);
        let out = (ok != 0 && !writer.mem.is_null()).then(|| std::slice::from_raw_parts(writer.mem, writer.size).to_vec());
        ffi::WebPMemoryWriterClear(&mut writer);
        out
    }
}

/// One frame of an animation: the full canvas (RGBA) and when it starts.
pub struct Frame<'a> {
    pub rgba: &'a [u8],
    pub start_ms: i32,
}

/// Encode an animation (frames are full RGBA canvases of `w`x`h`). Each frame may be lossy or
/// lossless, whichever is smaller (`allow_mixed`); `end_ms` is when the last frame ends.
pub fn encode_animation(frames: &[Frame<'_>], w: u32, h: u32, end_ms: i32, quality: f32) -> Option<Vec<u8>> {
    let cfg = config(Opts { lossless: false, quality, method: 4, sharp_yuv: false })?;
    // SAFETY: every pointer handed to libwebp is valid for the call; the encoder is deleted on
    // every path (guard), pictures free themselves, and the assembled data is copied then freed.
    unsafe {
        let mut opts: ffi::WebPAnimEncoderOptions = std::mem::zeroed();
        if ffi::WebPAnimEncoderOptionsInitInternal(&mut opts, ffi::WEBP_MUX_ABI_VERSION as i32) == 0 {
            return None;
        }
        opts.anim_params.loop_count = 0;
        opts.allow_mixed = 1;
        struct Enc(*mut ffi::WebPAnimEncoder);
        impl Drop for Enc {
            fn drop(&mut self) {
                // SAFETY: created by `WebPAnimEncoderNewInternal`, deleted once.
                unsafe { ffi::WebPAnimEncoderDelete(self.0) };
            }
        }
        let enc = ffi::WebPAnimEncoderNewInternal(w as i32, h as i32, &opts, ffi::WEBP_MUX_ABI_VERSION as i32);
        if enc.is_null() {
            return None;
        }
        let enc = Enc(enc);
        for f in frames {
            let mut pic = Picture::import(f.rgba, w, h, true, true)?;
            if ffi::WebPAnimEncoderAdd(enc.0, &mut pic.0, f.start_ms, &cfg) == 0 {
                return None;
            }
        }
        if ffi::WebPAnimEncoderAdd(enc.0, std::ptr::null_mut(), end_ms, std::ptr::null()) == 0 {
            return None;
        }
        let mut data = ffi::WebPData::default();
        if ffi::WebPAnimEncoderAssemble(enc.0, &mut data) == 0 || data.bytes.is_null() {
            return None;
        }
        let out = std::slice::from_raw_parts(data.bytes, data.size).to_vec();
        ffi::WebPDataClear(&mut data);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stills_and_animations_decode_back() {
        let (w, h) = (64u32, 40u32);
        let rgb: Vec<u8> = (0..w * h).flat_map(|i| [(i % 256) as u8, (i / 7 % 256) as u8, 30]).collect();
        let lossy = encode(&rgb, w, h, false, Opts::lossy(80.0)).unwrap();
        let back = image::load_from_memory(&lossy).unwrap();
        assert_eq!((back.width(), back.height()), (w, h));
        let rgba: Vec<u8> = (0..w * h).flat_map(|i| [200, 10, (i % 256) as u8, if i % 2 == 0 { 0 } else { 255 }]).collect();
        let lossless = encode(&rgba, w, h, true, Opts::lossless()).unwrap();
        let back = image::load_from_memory(&lossless).unwrap().to_rgba8();
        // Exact wherever something is visible (the colour under fully transparent pixels is free).
        for (got, want) in back.as_raw().chunks(4).zip(rgba.chunks(4)) {
            assert_eq!(got[3], want[3]);
            if want[3] > 0 {
                assert_eq!(got, want, "lossless is exact");
            }
        }
        assert!(encode(&rgb, w, h + 1, false, Opts::lossy(80.0)).is_none(), "wrong buffer size");
        let frames: Vec<Vec<u8>> = (0..3).map(|k| (0..w * h).flat_map(|_| [k * 80, 50, 50, 255]).collect()).collect();
        let list: Vec<Frame<'_>> = frames.iter().enumerate().map(|(i, f)| Frame { rgba: f, start_ms: i as i32 * 100 }).collect();
        let anim = encode_animation(&list, w, h, 300, 75.0).unwrap();
        let d = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&anim)).unwrap();
        assert!(d.has_animation());
    }
}
