//! Deterministic, hard-to-encode synthetic scenes for `capture_probe bench` (BGRA, CPU).
//!
//! - `pointcloud`: ~450k coloured points of a rotating 3D cloud (the v0.1 field case: dense
//!   high-frequency dots moving in different directions with parallax).
//! - `text`: fast scroll + sideways pan over a page of pseudo-text (thin high-contrast glyphs,
//!   syntax-coloured words), like flinging through code or a web page.
//! - `game`: a perspective textured floor with a turning, moving camera, scrolling clouds,
//!   moving sprites and a static HUD.
//!
//! Every frame is a pure function of (scene, size, time): the same input for every encoder.

#![allow(dead_code)]

use std::f64::consts::PI;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    PointCloud,
    Text,
    Game,
}

impl Scene {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pointcloud" | "points" => Self::PointCloud,
            "text" => Self::Text,
            "game" => Self::Game,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::PointCloud => "pointcloud",
            Self::Text => "text",
            Self::Game => "game",
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn hash2(x: i64, y: i64) -> u32 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= h >> 29;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    (h ^ (h >> 32)) as u32
}

#[inline]
fn px(r: u8, g: u8, b: u8) -> u32 {
    0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

pub struct SceneGen {
    scene: Scene,
    w: usize,
    h: usize,
    points: Vec<[f32; 6]>,  // x y z r g b
    page: Vec<u32>,         // text page
    page_w: usize,
    page_h: usize,
    tex: Vec<u32>,          // game floor texture 1024x1024
    accum: Vec<[u16; 3]>,   // point cloud additive buffer
}

const TEX: usize = 1024;

impl SceneGen {
    pub fn new(scene: Scene, w: u32, h: u32) -> Self {
        let (w, h) = (w as usize, h as usize);
        let mut g = Self {
            scene,
            w,
            h,
            points: Vec::new(),
            page: Vec::new(),
            page_w: 0,
            page_h: 0,
            tex: Vec::new(),
            accum: Vec::new(),
        };
        match scene {
            Scene::PointCloud => g.init_points(),
            Scene::Text => g.init_page(),
            Scene::Game => g.init_texture(),
        }
        g
    }

    /// Paint the frame at time `t` (seconds) into `bgra` (`stride` bytes per row).
    pub fn render(&mut self, t: f64, bgra: &mut [u8], stride: usize) {
        match self.scene {
            Scene::PointCloud => self.render_points(t, bgra, stride),
            Scene::Text => self.render_text(t, bgra, stride),
            Scene::Game => self.render_game(t, bgra, stride),
        }
    }

    // ----- point cloud -----------------------------------------------------------------------

    fn init_points(&mut self) {
        let mut r = Rng(0x5EED_0001);
        let n = 450_000;
        self.points.reserve(n);
        for i in 0..n {
            let (x, y, z);
            match i % 3 {
                0 => {
                    // spiral galaxy arms
                    let arm = (r.next() % 4) as f64;
                    let d = r.f().powf(0.6) * 1.6;
                    let a = arm * PI / 2.0 + d * 3.2 + (r.f() - 0.5) * 0.5;
                    x = a.cos() * d;
                    z = a.sin() * d;
                    y = (r.f() - 0.5) * 0.15 * (1.7 - d);
                }
                1 => {
                    // noisy sphere shell
                    let u = r.f() * 2.0 - 1.0;
                    let th = r.f() * 2.0 * PI;
                    let rad = 0.9 + (r.f() - 0.5) * 0.1;
                    let s = (1.0 - u * u).sqrt();
                    x = s * th.cos() * rad;
                    y = u * rad;
                    z = s * th.sin() * rad;
                }
                _ => {
                    // terrain-like sheet with ridges
                    x = (r.f() - 0.5) * 3.2;
                    z = (r.f() - 0.5) * 3.2;
                    y = -0.9 + 0.25 * (x * 3.0).sin() * (z * 2.3).cos() + (r.f() - 0.5) * 0.03;
                }
            }
            let hue = (x.atan2(z) / (2.0 * PI) + 0.5 + y) * 6.0;
            let (cr, cg, cb) = hue_rgb(hue);
            let k = 0.55 + 0.45 * r.f();
            self.points.push([x as f32, y as f32, z as f32, (cr * k) as f32, (cg * k) as f32, (cb * k) as f32]);
        }
        self.accum = vec![[0; 3]; self.w * self.h];
    }

    fn render_points(&mut self, t: f64, bgra: &mut [u8], stride: usize) {
        let (w, h) = (self.w, self.h);
        for a in self.accum.iter_mut() {
            *a = [0; 3];
        }
        let yaw = t * 1.6; // ~92 deg/s
        let pitch = 0.35 + 0.25 * (t * 0.9).sin();
        let dist = 2.3 + 0.6 * (t * 0.7).sin();
        let (sy, cy) = yaw.sin_cos();
        let (sp, cp) = pitch.sin_cos();
        let f = 1.0 * h as f64;
        let (cx, cyy) = (w as f64 / 2.0, h as f64 / 2.0);
        for p in &self.points {
            let (x, y, z) = (p[0] as f64, p[1] as f64, p[2] as f64);
            let x1 = x * cy - z * sy;
            let z1 = x * sy + z * cy;
            let y2 = y * cp - z1 * sp;
            let z2 = y * sp + z1 * cp + dist;
            if z2 < 0.3 {
                continue;
            }
            let u = cx + x1 * f / z2;
            let v = cyy - y2 * f / z2;
            if u < 0.0 || v < 0.0 || u >= (w - 1) as f64 || v >= (h - 1) as f64 {
                continue;
            }
            let bright = (2.6 / z2).min(1.4);
            let add = [
                (p[0] as f64 * 160.0 * bright) as u16,
                (p[1] as f64 * 160.0 * bright) as u16,
                (p[2] as f64 * 160.0 * bright) as u16,
            ];
            let (ui, vi) = (u as usize, v as usize);
            let size = if z2 < 2.2 { 2 } else { 1 };
            for dy in 0..size {
                for dx in 0..size {
                    let a = &mut self.accum[(vi + dy) * w + ui + dx];
                    a[0] = a[0].saturating_add(add[0]);
                    a[1] = a[1].saturating_add(add[1]);
                    a[2] = a[2].saturating_add(add[2]);
                }
            }
        }
        let accum = &self.accum;
        par_rows(bgra, stride, h, |y, row| {
            let bg = (8.0 + 18.0 * y as f64 / h as f64) as u16;
            for x in 0..w {
                let a = accum[y * w + x];
                let r = (a[0] + bg / 2).min(255) as u8;
                let g = (a[1] + bg / 2).min(255) as u8;
                let b = (a[2] + bg).min(255) as u8;
                row[x * 4..x * 4 + 4].copy_from_slice(&[b, g, r, 255]);
            }
        });
    }

    // ----- text -------------------------------------------------------------------------------

    fn init_page(&mut self) {
        // Cells 8x16 with 6x11 pseudo-glyphs made of strokes.
        let (cw, ch) = (8usize, 16usize);
        let cols = 260usize;
        let rows = 420usize;
        self.page_w = cols * cw;
        self.page_h = rows * ch;
        let bg = px(30, 31, 36);
        self.page = vec![bg; self.page_w * self.page_h];
        let palette = [
            px(212, 212, 212),
            px(86, 156, 214),
            px(206, 145, 120),
            px(78, 201, 176),
            px(197, 134, 192),
            px(220, 220, 170),
            px(106, 153, 85),
        ];
        let mut r = Rng(0x7E47_0002);
        for row in 0..rows {
            let indent = (r.next() % 12) as usize * 2;
            let len = if r.next().is_multiple_of(9) { 0 } else { 20 + (r.next() % 140) as usize };
            let mut col = indent;
            let mut color = palette[0];
            let mut word_left = 0usize;
            // line numbers gutter
            for (i, dch) in format!("{:>5}", row + 1).bytes().enumerate() {
                if dch != b' ' {
                    self.glyph(i, row, cw, ch, dch as u64 * 7919, px(110, 118, 129));
                }
            }
            while col < (indent + len).min(cols - 8) {
                if word_left == 0 {
                    word_left = 1 + (r.next() % 9) as usize;
                    color = palette[(r.next() % palette.len() as u64) as usize];
                    col += 1; // space
                    continue;
                }
                self.glyph(col + 7, row, cw, ch, r.next(), color);
                col += 1;
                word_left -= 1;
            }
        }
    }

    fn glyph(&mut self, col: usize, row: usize, cw: usize, ch: usize, seed: u64, color: u32) {
        let (x0, y0) = (col * cw + 1, row * ch + 3);
        let mut s = Rng(seed);
        let strokes = 2 + s.next() % 3;
        for _ in 0..strokes {
            let kind = s.next() % 4;
            let a = (s.next() % 6) as usize;
            let b = (s.next() % 11) as usize;
            for k in 0..11usize {
                let (x, y) = match kind {
                    0 => (a, k),                    // vertical
                    1 => (k.min(5), b),             // horizontal
                    2 => (k * 5 / 10, k),           // diagonal
                    _ => (5 - (k * 5 / 10), k),     // anti-diagonal
                };
                if kind == 1 && k > 5 {
                    break;
                }
                let (px_, py) = (x0 + x, y0 + y);
                if px_ < self.page_w && py < self.page_h {
                    self.page[py * self.page_w + px_] = color;
                }
            }
        }
    }

    fn render_text(&mut self, t: f64, bgra: &mut [u8], stride: usize) {
        let (w, h) = (self.w, self.h);
        // Fling-scroll ~1400 px/s with easing bursts, plus a sideways pan.
        let scroll = 1400.0 * t + 300.0 * (t * 2.1).sin();
        let pan = 200.0 + 180.0 * (t * 1.3).sin();
        let max_y = (self.page_h - h) as f64;
        let oy = (scroll % (2.0 * max_y)) as i64;
        let oy = if oy as f64 > max_y { (2.0 * max_y) as i64 - oy } else { oy } as usize;
        let ox = (pan.max(0.0) as usize).min(self.page_w.saturating_sub(w));
        let page = &self.page;
        let pw = self.page_w;
        par_rows(bgra, stride, h, |y, row| {
            let src = &page[(oy + y) * pw + ox..];
            for x in 0..w {
                let c = src[x];
                row[x * 4..x * 4 + 4].copy_from_slice(&[c as u8, (c >> 8) as u8, (c >> 16) as u8, 255]);
            }
        });
    }

    // ----- game ---------------------------------------------------------------------------------

    fn init_texture(&mut self) {
        self.tex = vec![0; TEX * TEX];
        for y in 0..TEX {
            for x in 0..TEX {
                let h1 = hash2(x as i64 / 4, y as i64 / 4) & 0xff;
                let h2 = hash2(x as i64, y as i64) & 0x3f;
                let tile = ((x / 128) + (y / 128)) % 2 == 0;
                let path = ((x as i64 - 512).abs() < 40) || ((y as i64 - 300).abs() < 30);
                let (r, g, b) = if path {
                    (150 + h2 as i32 / 2, 120 + h2 as i32 / 2, 80 + h1 as i32 / 8)
                } else if tile {
                    (40 + h1 as i32 / 6, 110 + h1 as i32 / 3 + h2 as i32 / 2, 35 + h2 as i32 / 3)
                } else {
                    (60 + h1 as i32 / 5, 135 + h1 as i32 / 4, 45 + h2 as i32 / 2)
                };
                self.tex[y * TEX + x] = px(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8);
            }
        }
    }

    fn render_game(&mut self, t: f64, bgra: &mut [u8], stride: usize) {
        let (w, h) = (self.w, self.h);
        let yaw = t * 0.9 + 0.4 * (t * 0.5).sin(); // turning ~50-75 deg/s
        let (cam_x, cam_y) = (t * 6.0 * yaw.cos(), t * 6.0 * yaw.sin() + 3.0 * t);
        let horizon = h as f64 * 0.42;
        let cam_h = 1.6;
        let fov = 0.9;
        let (sy, cy) = yaw.sin_cos();
        let tex = &self.tex;
        let sprites: Vec<(f64, f64, f64, u32)> = (0..48)
            .map(|i| {
                let a = i as f64 * 0.7 + t * (0.6 + (i % 5) as f64 * 0.25);
                let x = (w as f64) * (0.5 + 0.45 * (a * 1.3).sin());
                let y = horizon + (h as f64 - horizon) * (0.15 + 0.8 * ((a * 0.7 + i as f64).cos() * 0.5 + 0.5));
                let r = 6.0 + (i % 7) as f64 * 3.0;
                let c = px((i * 53 % 255) as u8, (i * 97 % 255) as u8, (i * 31 % 255) as u8);
                (x, y, r, c)
            })
            .collect();
        par_rows(bgra, stride, h, |y, row| {
            let yf = y as f64;
            for x in 0..w {
                let mut c;
                if yf < horizon {
                    // sky with drifting clouds
                    let k = yf / horizon;
                    let base = (70.0 + 100.0 * k, 120.0 + 90.0 * k, 200.0 + 40.0 * k);
                    let u = (x as f64 / w as f64 - 0.5) * 2.0 + yaw * 1.5 + t * 0.05;
                    let n = value_noise(u * 6.0, k * 4.0 + t * 0.1);
                    let cl = ((n - 0.55) * 4.0).clamp(0.0, 1.0);
                    c = px(
                        (base.0 + (255.0 - base.0) * cl) as u8,
                        (base.1 + (255.0 - base.1) * cl) as u8,
                        (base.2 + (255.0 - base.2) * cl) as u8,
                    );
                } else {
                    let dy = (yf - horizon + 0.5) / h as f64;
                    let d = cam_h * 0.5 / dy;
                    let sx = (x as f64 / w as f64 - 0.5) * 2.0 * fov;
                    let fx = cy - sx * sy;
                    let fz = sy + sx * cy;
                    let wx = cam_x + fx * d;
                    let wz = cam_y + fz * d;
                    let tx = ((wx * 64.0) as i64).rem_euclid(TEX as i64) as usize;
                    let tz = ((wz * 64.0) as i64).rem_euclid(TEX as i64) as usize;
                    let tc = tex[tz * TEX + tx];
                    let fog = (d / 60.0).min(1.0);
                    let mix = |a: u32, b: f64| ((a as f64) * (1.0 - fog) + b * fog) as u32;
                    c = 0xFF00_0000
                        | (mix((tc >> 16) & 0xff, 175.0) << 16)
                        | (mix((tc >> 8) & 0xff, 200.0) << 8)
                        | mix(tc & 0xff, 230.0);
                }
                for &(sx, sy_, r, sc) in &sprites {
                    let (ddx, ddy) = (x as f64 - sx, yf - sy_);
                    if ddy.abs() < r && ddx * ddx + ddy * ddy < r * r {
                        c = sc;
                    }
                }
                // HUD: static minimap frame + health bar + pseudo-text strip
                if (y > h - 70 && y < h - 40 && x > 40 && x < 440)
                    && (x < 40 + ((400.0 * 0.7) as usize) || y == h - 69 || y == h - 41)
                {
                    c = px(200, 40, 40);
                }
                if x > w - 260 && x < w - 20 && y > 20 && y < 260 && (x == w - 259 || x == w - 21 || y == 21 || y == 259) {
                    c = px(255, 255, 255);
                }
                if y > 24 && y < 36 && x > 30 && x < 520 && hash2(x as i64 / 3, 1).is_multiple_of(3) && (x / 9) % 7 != 0 {
                    c = px(255, 255, 255);
                }
                row[x * 4..x * 4 + 4].copy_from_slice(&[c as u8, (c >> 8) as u8, (c >> 16) as u8, 255]);
            }
        });
    }
}

fn value_noise(x: f64, y: f64) -> f64 {
    let (xi, yi) = (x.floor() as i64, y.floor() as i64);
    let (xf, yf) = (x - xi as f64, y - yi as f64);
    let s = |v: f64| v * v * (3.0 - 2.0 * v);
    let g = |a: i64, b: i64| (hash2(a, b) & 0xffff) as f64 / 65535.0;
    let (u, v) = (s(xf), s(yf));
    let a = g(xi, yi) * (1.0 - u) + g(xi + 1, yi) * u;
    let b = g(xi, yi + 1) * (1.0 - u) + g(xi + 1, yi + 1) * u;
    a * (1.0 - v) + b * v
}

fn hue_rgb(h: f64) -> (f64, f64, f64) {
    let h = h.rem_euclid(6.0);
    let x = 1.0 - ((h % 2.0) - 1.0).abs();
    match h as u32 {
        0 => (1.0, x, 0.2),
        1 => (x, 1.0, 0.2),
        2 => (0.2, 1.0, x),
        3 => (0.2, x, 1.0),
        4 => (x, 0.2, 1.0),
        _ => (1.0, 0.2, x),
    }
}

/// Run `f(y, row)` over row bands on all cores.
fn par_rows(buf: &mut [u8], stride: usize, h: usize, f: impl Fn(usize, &mut [u8]) + Sync) {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    let band = h.div_ceil(n);
    std::thread::scope(|s| {
        for (bi, chunk) in buf[..stride * h].chunks_mut(stride * band).enumerate() {
            let f = &f;
            s.spawn(move || {
                for (ri, row) in chunk.chunks_mut(stride).enumerate() {
                    f(bi * band + ri, row);
                }
            });
        }
    });
}
