//! Cheap moving NV12 test pattern, used as a stand-in capture source (`share test`).
//!
//! Cost is a handful of row memcpys/memsets per frame: a precomputed luma ramp is copied with a
//! per-frame horizontal offset, a bright bar sweeps vertically and a box moves diagonally. The
//! motion keeps the encoder honest (static frames would encode to almost nothing).

/// Precomputed rows reused across frames.
pub struct TestPattern {
    width: u32,
    height: u32,
    /// Luma ramp of length 2*width so any offset window of `width` bytes is contiguous.
    ramp: Vec<u8>,
}

impl TestPattern {
    pub fn new(width: u32, height: u32) -> Self {
        assert!(width >= 16 && height >= 16 && width.is_multiple_of(2) && height.is_multiple_of(2));
        let w = width as usize;
        let ramp = (0..2 * w).map(|x| (16 + (x * 219 / w) % 220) as u8).collect();
        Self { width, height, ramp }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Render frame `index` into NV12 planes.
    pub fn render(&self, index: u64, y: &mut [u8], y_stride: usize, uv: &mut [u8], uv_stride: usize) {
        let (w, h) = (self.width as usize, self.height as usize);
        assert!(y_stride >= w && uv_stride >= w);
        assert!(y.len() >= y_stride * (h - 1) + w && uv.len() >= uv_stride * (h / 2 - 1) + w);

        // Luma: scrolling ramp.
        let shift = (index as usize * 4) % w;
        for row in 0..h {
            let dst = &mut y[row * y_stride..row * y_stride + w];
            dst.copy_from_slice(&self.ramp[shift..shift + w]);
        }
        // Sweeping bright bar (8 rows).
        let bar = (index as usize * 3) % (h - 8);
        for row in bar..bar + 8 {
            y[row * y_stride..row * y_stride + w].fill(235);
        }
        // Diagonally moving dark box (1/8 of the frame).
        let (bw, bh) = ((w / 8) & !1, (h / 8) & !1);
        let bx = ((index as usize * 5) % (w - bw)) & !1;
        let by = ((index as usize * 3) % (h - bh)) & !1;
        for row in by..by + bh {
            y[row * y_stride + bx..row * y_stride + bx + bw].fill(16);
        }

        // Chroma: horizontal colour bands whose hue slowly cycles.
        let ch = h / 2;
        let phase = (index % 256) as u8;
        for row in 0..ch {
            let band = (row * 8 / ch) as u8;
            let u = 64u8.wrapping_add(band.wrapping_mul(24)).wrapping_add(phase);
            let v = 192u8.wrapping_sub(band.wrapping_mul(20)).wrapping_add(phase / 2);
            let dst = &mut uv[row * uv_stride..row * uv_stride + w];
            for px in dst.as_chunks_mut::<2>().0 {
                px[0] = u;
                px[1] = v;
            }
        }
        // Box is neutral grey in chroma so it reads as a dark square.
        for row in by / 2..(by + bh) / 2 {
            uv[row * uv_stride + bx..row * uv_stride + bx + bw].fill(128);
        }
    }
}

/// Parse `WxH@fps` (any part may be omitted: `1280x720`, `@60`, `1920x1080@60`).
pub fn parse_geometry(spec: &str, default: (u32, u32, u32)) -> Option<(u32, u32, u32)> {
    let (mut w, mut h, mut fps) = default;
    let (dims, rate) = match spec.split_once('@') {
        Some((d, r)) => (d, Some(r)),
        None => (spec, None),
    };
    if !dims.is_empty() {
        let (ws, hs) = dims.split_once(['x', 'X'])?;
        w = ws.trim().parse().ok()?;
        h = hs.trim().parse().ok()?;
    }
    if let Some(r) = rate {
        fps = r.trim().parse().ok()?;
    }
    if w < 16 || h < 16 || w > 7680 || h > 4320 || fps == 0 || fps > 240 {
        return None;
    }
    // NV12 needs even dimensions.
    Some((w & !1, h & !1, fps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_change_and_respect_strides() {
        let (w, h) = (64usize, 48usize);
        let (ys, uvs) = (w + 16, w + 8);
        let p = TestPattern::new(w as u32, h as u32);
        let mut y0 = vec![0xAAu8; ys * h];
        let mut uv0 = vec![0xAAu8; uvs * h / 2];
        let mut y1 = y0.clone();
        let mut uv1 = uv0.clone();
        p.render(0, &mut y0, ys, &mut uv0, uvs);
        p.render(1, &mut y1, ys, &mut uv1, uvs);
        assert_ne!(y0, y1, "pattern must move between frames");
        // Padding bytes beyond the visible width are untouched.
        for row in 0..h {
            assert!(y0[row * ys + w..(row + 1) * ys].iter().all(|&b| b == 0xAA));
        }
        for row in 0..h / 2 {
            assert!(uv0[row * uvs + w..(row + 1) * uvs].iter().all(|&b| b == 0xAA));
        }
        // Luma stays in video range.
        for row in 0..h {
            assert!(y0[row * ys..row * ys + w].iter().all(|&b| (16..=235).contains(&b)));
        }
    }

    #[test]
    fn deterministic_per_index() {
        let p = TestPattern::new(32, 32);
        let mut a = (vec![0u8; 32 * 32], vec![0u8; 32 * 16]);
        let mut b = (vec![1u8; 32 * 32], vec![1u8; 32 * 16]);
        p.render(42, &mut a.0, 32, &mut a.1, 32);
        p.render(42, &mut b.0, 32, &mut b.1, 32);
        assert_eq!(a, b);
    }

    #[test]
    fn geometry_parsing() {
        let d = (1280, 720, 30);
        assert_eq!(parse_geometry("1920x1080@60", d), Some((1920, 1080, 60)));
        assert_eq!(parse_geometry("640x480", d), Some((640, 480, 30)));
        assert_eq!(parse_geometry("@15", d), Some((1280, 720, 15)));
        assert_eq!(parse_geometry("", d), Some(d));
        assert_eq!(parse_geometry("641x481", d), Some((640, 480, 30)));
        assert_eq!(parse_geometry("axb", d), None);
        assert_eq!(parse_geometry("1280x720@0", d), None);
    }
}
