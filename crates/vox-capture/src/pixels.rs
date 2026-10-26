//! Pure picture and sound helpers of the macOS backend: the preview downscale, thumbnail and icon
//! pixel conversions, the synthetic camera's picture, the camera mode choice and the 10 ms audio
//! chunker. No system calls, so they are unit tested on every host.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::{CameraMode, CameraSubtype, AUDIO_CHANNELS, AUDIO_CHUNK_FRAMES, AUDIO_SAMPLE_RATE};

/// An NV12 picture in borrowed planes.
#[derive(Clone, Copy)]
pub(crate) struct Planes<'a> {
    pub width: u32,
    pub height: u32,
    pub y: &'a [u8],
    pub y_stride: usize,
    pub uv: &'a [u8],
    pub uv_stride: usize,
}

/// Scale an NV12 picture to `out_w` x `out_h` (even sizes) into `dst` (tightly packed: Y rows of
/// `out_w` bytes, then the UV rows), averaging 2x2 source samples around each output sample
/// (cheap and smooth enough for a small preview tile). `mirror` flips it left to right.
pub(crate) fn downscale_nv12(src: &Planes<'_>, out_w: u32, out_h: u32, mirror: bool, dst: &mut Vec<u8>) {
    let (sw, sh) = (src.width.max(2) as usize, src.height.max(2) as usize);
    let (ow, oh) = ((out_w.max(2) & !1) as usize, (out_h.max(2) & !1) as usize);
    dst.clear();
    dst.resize(ow * oh * 3 / 2, 0);
    let (dy, duv) = dst.split_at_mut(ow * oh);
    // Fixed point (16.16) steps, sampling at the centre of each output pixel.
    let step_x = ((sw << 16) / ow) as u64;
    let step_y = ((sh << 16) / oh) as u64;
    let col = |ox: usize| -> usize {
        let ox = if mirror { ow - 1 - ox } else { ox };
        ((((ox as u64) * step_x + step_x / 2) >> 16) as usize).min(sw - 1)
    };
    let xs: Vec<usize> = (0..ow).map(col).collect();
    for (oy, row) in dy.chunks_exact_mut(ow).enumerate() {
        let sy = ((((oy as u64) * step_y + step_y / 2) >> 16) as usize).min(sh - 1);
        let sy1 = (sy + 1).min(sh - 1);
        let (r0, r1) = (sy * src.y_stride, sy1 * src.y_stride);
        for (o, &sx) in row.iter_mut().zip(&xs) {
            let sx1 = (sx + 1).min(sw - 1);
            let s = src.y[r0 + sx] as u32 + src.y[r0 + sx1] as u32 + src.y[r1 + sx] as u32 + src.y[r1 + sx1] as u32;
            *o = ((s + 2) / 4) as u8;
        }
    }
    // Chroma: one UV pair per 2x2 output pixels, from the source's UV plane (half size).
    let (cw, ch) = (sw / 2, sh / 2);
    let (ocw, och) = (ow / 2, oh / 2);
    for (oy, row) in duv.chunks_exact_mut(ow).take(och).enumerate() {
        let cy = (((oy * 2 + 1) * sh / oh) / 2).min(ch.saturating_sub(1));
        let r = cy * src.uv_stride;
        for ox in 0..ocw {
            let px = if mirror { ow - 1 - (ox * 2 + 1) } else { ox * 2 + 1 };
            let cx = ((px * sw / ow) / 2).min(cw.saturating_sub(1));
            row[ox * 2] = src.uv[r + cx * 2];
            row[ox * 2 + 1] = src.uv[r + cx * 2 + 1];
        }
    }
}

/// BGRA rows (with a stride) to tightly packed, opaque RGBA.
pub(crate) fn bgra_to_rgba(src: &[u8], stride: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 4);
    for r in 0..h {
        for px in src[r * stride..r * stride + w * 4].as_chunks::<4>().0 {
            out.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    out
}

/// Premultiplied RGBA (what a CoreGraphics bitmap holds) to straight alpha.
pub(crate) fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// The synthetic camera's picture: a shaded background, a "head" moving side to side, a sweeping
/// bar and a dark box in the top-left corner that blinks (15 frames on, 15 off). Writes `w` x `h`
/// NV12 into the planes.
pub(crate) fn render_test_card(index: u64, w: usize, h: usize, y: &mut [u8], y_stride: usize, uv: &mut [u8], uv_stride: usize) {
    let head_r = (h / 5).max(4);
    let travel = w.saturating_sub(head_r * 4).max(1);
    let phase = (index % 120) as usize;
    let swing = if phase < 60 { phase } else { 120 - phase };
    let head_x = head_r * 2 + travel * swing / 60;
    let head_y = h / 2;
    let bar_y = (index as usize * 4) % h.max(1);
    let corner = index % 30 < 15;
    let box_ = (w.min(h) / 8).max(8);
    for r in 0..h {
        let row = &mut y[r * y_stride..r * y_stride + w];
        let base = 70 + (r * 80 / h.max(1)) as u8;
        row.fill(base);
        let dy = r.abs_diff(head_y);
        if dy < head_r {
            // Half-width of the circle on this row (integer square root).
            let half = isqrt(head_r * head_r - dy * dy);
            let (a, b) = (head_x.saturating_sub(half), (head_x + half).min(w));
            row[a..b].fill(190);
        }
        if r.abs_diff(bar_y) < 3 {
            row.fill(235);
        }
        if corner && r < box_ {
            row[..box_.min(w)].fill(16);
        }
    }
    for r in 0..h / 2 {
        let row = &mut uv[r * uv_stride..r * uv_stride + w];
        for (i, px) in row.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            // A warm tint on the left, cool on the right.
            let t = (i * 2 * 40 / w.max(1)) as u8;
            px[0] = 108 + t;
            px[1] = 148 - t;
        }
    }
}

fn isqrt(v: usize) -> usize {
    let mut x = (v as f64).sqrt() as usize;
    while x * x > v {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= v {
        x += 1;
    }
    x
}

/// The native mode to open for a wanted size and rate: an exact `pick` when the camera offers it;
/// else the smallest mode at least as big as wanted that reaches `fps` (uncompressed first at equal
/// size), else the biggest that reaches it, else the fastest of the biggest.
pub(crate) fn choose_mode(modes: &[CameraMode], want_w: u32, want_h: u32, fps: u32, pick: Option<CameraMode>) -> Option<CameraMode> {
    if let Some(p) = pick {
        if let Some(m) = modes.iter().find(|m| m.matches(&p)) {
            return Some(*m);
        }
    }
    let rank = |s: CameraSubtype| match s {
        CameraSubtype::Nv12 => 0,
        CameraSubtype::Yuy2 => 1,
        CameraSubtype::Mjpg => 2,
    };
    let area = |m: &CameraMode| m.width as u64 * m.height as u64;
    let fast: Vec<&CameraMode> = modes.iter().filter(|m| m.fps_milli + 500 >= fps * 1000).collect();
    let big_enough = fast.iter().filter(|m| m.width >= want_w && m.height >= want_h).min_by_key(|m| (area(m), rank(m.subtype), std::cmp::Reverse(m.fps_milli)));
    if let Some(m) = big_enough {
        return Some(**m);
    }
    if let Some(m) = fast.iter().max_by_key(|m| (area(m), std::cmp::Reverse(rank(m.subtype)), m.fps_milli)) {
        return Some(**m);
    }
    modes.iter().max_by_key(|m| (area(m), m.fps_milli)).copied()
}

/// Turns whatever the system delivers (any rate, mono or stereo, float or 16-bit, interleaved or
/// one buffer per channel) into the crate's audio contract: 48 kHz stereo i16 in 10 ms chunks.
/// Linear resampling when the rate differs (ScreenCaptureKit is asked for 48 kHz, so normally
/// not). Timestamps: the first sample of each chunk, on the capture clock.
pub(crate) struct AudioChunker {
    buf: Vec<i16>,
    /// Capture time of `buf[0]`.
    buf_ts: Option<i64>,
    /// Resampler: position of the next output frame in input frames (relative to `prev`).
    pos: f64,
    prev: [f32; 2],
    have_prev: bool,
    in_rate: u32,
}

impl Default for AudioChunker {
    fn default() -> Self {
        Self { buf: Vec::with_capacity(AUDIO_CHUNK_FRAMES * 4), buf_ts: None, pos: 0.0, prev: [0.0; 2], have_prev: false, in_rate: AUDIO_SAMPLE_RATE }
    }
}

fn to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

impl AudioChunker {
    /// `frames` input frames read through `sample(frame, channel)` (channel 0 or 1; mono sources
    /// give the same value twice), captured at `ts_us`, at `rate` Hz. Calls `emit` for every full
    /// 10 ms chunk.
    pub(crate) fn push(&mut self, frames: usize, rate: u32, ts_us: i64, sample: impl Fn(usize, usize) -> f32, emit: &mut dyn FnMut(&[i16], i64)) {
        if frames == 0 || rate == 0 {
            return;
        }
        if rate != self.in_rate {
            self.in_rate = rate;
            self.pos = 0.0;
            self.have_prev = false;
        }
        let ch = AUDIO_CHANNELS as usize;
        let start_len = self.buf.len() / ch;
        if rate == AUDIO_SAMPLE_RATE {
            for f in 0..frames {
                self.buf.push(to_i16(sample(f, 0)));
                self.buf.push(to_i16(sample(f, 1)));
            }
        } else {
            // Linear interpolation between consecutive input frames.
            let step = rate as f64 / AUDIO_SAMPLE_RATE as f64;
            let get = |f: isize, c: usize, prev: &[f32; 2]| if f < 0 { prev[c] } else { sample(f as usize, c) };
            if !self.have_prev {
                self.prev = [sample(0, 0), sample(0, 1)];
                self.have_prev = true;
                self.pos = 1.0;
            }
            // `pos` counts from the previous call's last frame (index -1 here).
            while self.pos <= frames as f64 {
                let i = self.pos.floor();
                let t = (self.pos - i) as f32;
                let i = i as isize - 1;
                for c in 0..2 {
                    let a = get(i, c, &self.prev);
                    let b = if (i + 1) < frames as isize { get(i + 1, c, &self.prev) } else { a };
                    self.buf.push(to_i16(a + (b - a) * t));
                }
                self.pos += step;
            }
            self.pos -= frames as f64;
            self.prev = [sample(frames - 1, 0), sample(frames - 1, 1)];
        }
        if self.buf_ts.is_none() {
            self.buf_ts = Some(ts_us - start_len as i64 * 1_000_000 / AUDIO_SAMPLE_RATE as i64);
        }
        let chunk = AUDIO_CHUNK_FRAMES * ch;
        let mut off = 0;
        while self.buf.len() - off >= chunk {
            let ts = self.buf_ts.unwrap_or(ts_us) + (off / ch) as i64 * 1_000_000 / AUDIO_SAMPLE_RATE as i64;
            emit(&self.buf[off..off + chunk], ts);
            off += chunk;
        }
        if off > 0 {
            self.buf.drain(..off);
            // The leftover starts `off` frames after the old start.
            self.buf_ts = self.buf_ts.map(|t| t + (off / ch) as i64 * 1_000_000 / AUDIO_SAMPLE_RATE as i64);
        }
        if self.buf.is_empty() {
            self.buf_ts = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(w: usize, h: usize, index: u64) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 3 / 2];
        let (y, uv) = v.split_at_mut(w * h);
        render_test_card(index, w, h, y, w, uv, w);
        v
    }

    fn planes(v: &[u8], w: u32, h: u32) -> Planes<'_> {
        let (y, uv) = v.split_at((w * h) as usize);
        Planes { width: w, height: h, y, y_stride: w as usize, uv, uv_stride: w as usize }
    }

    #[test]
    fn downscale_keeps_the_picture_and_mirrors() {
        // The blinking box (dark, top-left) is on in frame 0.
        let src = card(320, 180, 0);
        let p = planes(&src, 320, 180);
        let mut out = Vec::new();
        downscale_nv12(&p, 160, 90, false, &mut out);
        assert_eq!(out.len(), 160 * 90 * 3 / 2);
        let at = |o: &[u8], x: usize, y: usize| o[y * 160 + x];
        assert!(at(&out, 2, 2) < 40, "dark box top-left: {}", at(&out, 2, 2));
        assert!(at(&out, 157, 2) > 60, "nothing top-right");
        downscale_nv12(&p, 160, 90, true, &mut out);
        assert!(at(&out, 157, 2) < 40, "mirrored: the box is top-right");
        assert!(at(&out, 2, 2) > 60);
        // Chroma comes along (tint differs left / right).
        let uv = &out[160 * 90..];
        assert_ne!(uv[0], uv[158]);
        // Odd or tiny sizes never panic.
        downscale_nv12(&p, 3, 1, false, &mut out);
        assert_eq!(out.len(), 2 * 2 * 3 / 2);
        // Upscaling works too (a small window's preview).
        downscale_nv12(&planes(&card(64, 36, 0), 64, 36), 128, 72, false, &mut out);
        assert_eq!(out.len(), 128 * 72 * 3 / 2);
    }

    #[test]
    fn test_card_blinks() {
        let on = card(64, 64, 0);
        let off = card(64, 64, 15);
        assert!(on[0] < 40 && off[0] > 60);
        assert_eq!(card(64, 64, 30)[0], on[0], "period of 30 frames");
    }

    #[test]
    fn pixel_conversions() {
        // Two BGRA pixels on a row with padding.
        let src = [1, 2, 3, 9, 10, 20, 30, 9, 0, 0, 0, 0];
        assert_eq!(bgra_to_rgba(&src, 12, 2, 1), vec![3, 2, 1, 255, 30, 20, 10, 255]);
        let mut px = vec![64, 32, 0, 128, 10, 10, 10, 0, 200, 100, 50, 255];
        unpremultiply(&mut px);
        assert_eq!(px, vec![128, 64, 0, 128, 10, 10, 10, 0, 200, 100, 50, 255]);
    }

    fn m(width: u32, height: u32, fps: u32, subtype: CameraSubtype) -> CameraMode {
        CameraMode { width, height, fps_milli: fps * 1000, subtype }
    }

    #[test]
    fn mode_choice() {
        use CameraSubtype::*;
        // A FaceTime camera: 720p and 480p at 30.
        let ft = [m(1280, 720, 30, Nv12), m(640, 480, 30, Nv12)];
        assert_eq!(choose_mode(&ft, 1280, 720, 30, None), Some(ft[0]));
        assert_eq!(choose_mode(&ft, 640, 360, 30, None), Some(ft[1]));
        assert_eq!(choose_mode(&ft, 1920, 1080, 30, None), Some(ft[0]), "biggest when none is big enough");
        // 60 fps wanted: only modes that reach it.
        let phone = [m(1920, 1080, 30, Nv12), m(1920, 1080, 60, Nv12), m(1280, 720, 60, Nv12)];
        assert_eq!(choose_mode(&phone, 1280, 720, 60, None), Some(phone[2]));
        assert_eq!(choose_mode(&phone, 1280, 720, 30, None), Some(phone[2]));
        // Nothing fast enough: the fastest of the biggest.
        assert_eq!(choose_mode(&[m(640, 480, 15, Yuy2)], 1280, 720, 30, None), Some(m(640, 480, 15, Yuy2)));
        // An exact pick wins; one the camera lacks is ignored.
        assert_eq!(choose_mode(&phone, 1280, 720, 60, Some(m(1920, 1080, 30, Nv12))), Some(phone[0]));
        assert_eq!(choose_mode(&phone, 1280, 720, 60, Some(m(800, 600, 30, Nv12))), Some(phone[2]));
        // Uncompressed before MJPEG at equal size.
        assert_eq!(choose_mode(&[m(1280, 720, 30, Mjpg), m(1280, 720, 30, Nv12)], 1280, 720, 30, None).unwrap().subtype, Nv12);
        assert_eq!(choose_mode(&[], 1280, 720, 30, None), None);
    }

    #[test]
    fn audio_chunks_are_10_ms_of_48k_stereo() {
        let mut c = AudioChunker::default();
        let mut got: Vec<(usize, i64, i16, i16)> = Vec::new();
        // 1024-frame buffers (what ScreenCaptureKit delivers), left = +0.5, right = -0.5.
        for i in 0..3 {
            let ts = 1_000_000 + i * 1024 * 1_000_000 / 48_000;
            c.push(1024, 48_000, ts, |_, ch| if ch == 0 { 0.5 } else { -0.5 }, &mut |s, t| got.push((s.len(), t, s[0], s[1])));
        }
        // 3072 frames = 6 chunks of 480 (2880), 192 left over.
        assert_eq!(got.len(), 6);
        assert!(got.iter().all(|g| g.0 == 960 && g.2 == 16384 && g.3 == -16384));
        // Chunk timestamps advance by 10 ms from the first sample.
        let ts: Vec<i64> = got.iter().map(|g| g.1).collect();
        assert_eq!(ts[0], 1_000_000);
        for w in ts.windows(2) {
            assert_eq!(w[1] - w[0], 10_000);
        }
    }

    #[test]
    fn audio_resamples_other_rates() {
        let mut c = AudioChunker::default();
        let mut frames = 0;
        // One second of 44.1 kHz becomes one second of 48 kHz (100 chunks, give or take one).
        for i in 0..100 {
            c.push(441, 44_100, i * 10_000, |f, _| ((i as usize * 441 + f) % 100) as f32 / 100.0, &mut |s, _| frames += s.len() / 2);
        }
        assert!((47_520..=48_000).contains(&frames), "{frames}");
        // Mono-as-stereo and clipping.
        let mut c = AudioChunker::default();
        let mut first = None;
        c.push(480, 48_000, 0, |_, _| 3.0, &mut |s, _| first = Some((s[0], s[1])));
        assert_eq!(first, Some((32767, 32767)));
    }
}
