//! macOS: watched streams and cameras drawn by the app itself (M2 of docs/PLANO-macOS.md).
//!
//! On Windows every video plays in a native child window presented by the core's viewer thread
//! (`call.rs`). macOS has no such thing here: the core only keeps the newest decoded frame of each
//! stream ([`vox_client_core::golive::viewer_tex`]) and this module draws it in the app's own
//! OpenGL window, as an egui texture inside the tile's rect. So egui's menus, popups and dialogs
//! simply draw over a video (no cut-outs, no hiding), and a popped-out stream is one more egui
//! viewport (window) showing the same texture.
//!
//! - Only a new frame costs anything: it is taken from the stream's mailbox at the start of a UI
//!   pass and one small shader pass turns it into an RGBA texture of the size it is shown at (the
//!   video's size at most; a smaller tile gets a 4-tap filtered downscale, so it stays smooth
//!   without mipmaps, whose generation costs an extra GPU submission per frame on macOS).
//!   Repaints for any other reason just draw that texture again.
//! - Zero copy for what VideoToolbox decodes: its NV12 picture lives in an IOSurface, and the
//!   shader reads the surface's two planes directly (`CGLTexImageIOSurface2D`, rectangle
//!   textures); the CPU never touches the pixels. The frame is kept until the next one replaces it,
//!   so the decoder cannot reuse the surface while the GPU may still read it. Other buffers (a
//!   software decoder's I420, our own camera's NV12) are uploaded plane by plane.
//! - A stream is uploaded only while it is on screen (its tile was drawn last pass, or it is
//!   popped out) and, for the strip's small tiles, at most `max_fps` times a second. A frame
//!   nobody can see stays in its mailbox and wakes nobody.
//! - If the GL path cannot start (no GL 3 / shader compile error) frames are converted on the CPU
//!   (libyuv) into ordinary egui textures instead.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Rect, Vec2};
use eframe::glow::{self, HasContext};
use vox_client_core::golive::viewer_tex::{NativeNv12, PlaneView, Planes, TexFrame};
use vox_client_core::golive::{TileStyle, Viewer};

/// BT.709 YUV -> RGB, video or full range; `u_nv12` picks NV12 (UV in one RG texture) or I420.
const VS: &str = r#"
#if NEW_SHADER_INTERFACE
#define I in
#define O out
#else
#define I attribute
#define O varying
#endif
I vec2 a_pos;
O vec2 v_uv;
void main() {
    v_uv = a_pos;
    gl_Position = vec4(a_pos * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FS: &str = r#"
#ifdef GL_ES
precision mediump float;
#endif
#if NEW_SHADER_INTERFACE
#define I in
#define TEX texture
out vec4 out_color;
#else
#define I varying
#define TEX texture2D
#define out_color gl_FragColor
#endif
uniform sampler2D u_y;
uniform sampler2D u_u;
uniform sampler2D u_v;
uniform float u_nv12;
uniform float u_full;
uniform vec2 u_tap;
I vec2 v_uv;
void main() {
    float y = 0.25 * (TEX(u_y, v_uv + vec2(-u_tap.x, -u_tap.y)).r + TEX(u_y, v_uv + vec2(u_tap.x, -u_tap.y)).r
        + TEX(u_y, v_uv + vec2(-u_tap.x, u_tap.y)).r + TEX(u_y, v_uv + vec2(u_tap.x, u_tap.y)).r);
    vec2 c = u_nv12 > 0.5 ? TEX(u_u, v_uv).rg : vec2(TEX(u_u, v_uv).r, TEX(u_v, v_uv).r);
    if (u_full > 0.5) {
        c = c - 128.0 / 255.0;
    } else {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        c = (c - 128.0 / 255.0) * (255.0 / 224.0);
    }
    vec3 rgb = vec3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);
    out_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
"#;

/// The same for an IOSurface's NV12 planes bound as rectangle textures (pixel coordinates).
const FS_RECT: &str = r#"
out vec4 out_color;
uniform sampler2DRect u_y;
uniform sampler2DRect u_u;
uniform vec2 u_ysize;
uniform vec2 u_csize;
uniform float u_full;
uniform vec2 u_tap;
in vec2 v_uv;
void main() {
    float y = 0.25 * (texture(u_y, (v_uv + vec2(-u_tap.x, -u_tap.y)) * u_ysize).r + texture(u_y, (v_uv + vec2(u_tap.x, -u_tap.y)) * u_ysize).r
        + texture(u_y, (v_uv + vec2(-u_tap.x, u_tap.y)) * u_ysize).r + texture(u_y, (v_uv + vec2(u_tap.x, u_tap.y)) * u_ysize).r);
    vec2 c = texture(u_u, v_uv * u_csize).rg;
    if (u_full > 0.5) {
        c = c - 128.0 / 255.0;
    } else {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        c = (c - 128.0 / 255.0) * (255.0 / 224.0);
    }
    vec3 rgb = vec3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);
    out_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
"#;

/// A linked conversion program and its uniforms.
struct Prog {
    program: glow::Program,
    u_y: Option<glow::UniformLocation>,
    u_u: Option<glow::UniformLocation>,
    u_v: Option<glow::UniformLocation>,
    u_nv12: Option<glow::UniformLocation>,
    u_full: Option<glow::UniformLocation>,
    u_ysize: Option<glow::UniformLocation>,
    u_csize: Option<glow::UniformLocation>,
    u_tap: Option<glow::UniformLocation>,
}

/// The conversion programs and their fixed objects (one per GL context: the app has one).
struct Gpu {
    planes: Prog,
    /// IOSurface (zero-copy) program; `None` when it did not compile.
    surface: Option<Prog>,
    vao: Option<glow::VertexArray>,
    vbo: glow::Buffer,
    a_pos: u32,
    fbo: glow::Framebuffer,
}

/// macOS OpenGL: bind an IOSurface plane to the current rectangle texture.
mod cgl {
    use std::ffi::c_void;

    #[link(name = "OpenGL", kind = "framework")]
    extern "C" {
        pub fn CGLGetCurrentContext() -> *mut c_void;
        #[allow(clippy::too_many_arguments)]
        pub fn CGLTexImageIOSurface2D(ctx: *mut c_void, target: u32, internal_format: u32, width: i32, height: i32, format: u32, ty: u32, io_surface: *mut c_void, plane: u32) -> i32;
    }
}

/// One stream's textures.
enum Tex {
    Gpu {
        /// Y, U (or UV) and V planes, with the size each was allocated at.
        planes: [Option<(glow::Texture, i32, i32)>; 3],
        /// Rectangle textures the IOSurface planes are bound to (Y, UV).
        surfaces: [Option<glow::Texture>; 2],
        out: glow::Texture,
        out_size: (i32, i32),
        id: egui::TextureId,
    },
    Cpu {
        handle: egui::TextureHandle,
        rgba: Vec<u8>,
    },
}

struct Tile {
    tex: Tex,
    /// Size of the picture in the texture (0 until the first frame).
    size: (u32, u32),
    style: TileStyle,
    last_upload: Option<Instant>,
    /// The last zero-copy frame, kept until the next one (its surface stays the decoder's).
    held: Option<TexFrame>,
    /// Physical pixels the picture was last drawn at (the converted texture is made that size).
    shown_px: std::cell::Cell<Option<(f32, f32)>>,
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    uploads: u64,
    upload_us: u64,
    sources: HashMap<&'static str, u64>,
    full_range: u64,
    surface_failures: u64,
    not_drawn: u64,
}

/// The app's macOS video state (lives in `call::VideoUi`).
#[derive(Default)]
pub struct MacVideo {
    gl: Option<Arc<glow::Context>>,
    gpu: Option<Gpu>,
    /// The GL path failed to start: CPU conversion from now on.
    cpu_only: bool,
    tiles: HashMap<String, Tile>,
    /// Streams whose tile was drawn last pass (only those get their frames uploaded), and where
    /// each one was painted this pass (layer, clip).
    pub on_screen: HashSet<String>,
    pub paint_at: HashMap<String, (egui::LayerId, Rect)>,
    /// Size each popped-out window opened with (kept, so the builder never fights a resize).
    pub popout_size: HashMap<String, Vec2>,
    /// Autotest measurements only: draw every stream at most this many times a second (0: off).
    pub measure_cap: u32,
    stats: Stats,
    warned_not_drawn: bool,
}

impl MacVideo {
    /// Start of a UI pass: take the new frames of the streams on screen (or in their own window)
    /// and upload them. `visible_extra`: streams that count as on screen anyway (popped out).
    pub fn upload(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame, viewer: &Viewer, minimized: bool) {
        let keys = viewer.keys();
        self.tiles.retain(|k, t| {
            let keep = keys.contains(k);
            if !keep {
                if let (Tex::Gpu { planes, surfaces, out, .. }, Some(gl)) = (&t.tex, &self.gl) {
                    // SAFETY: our own GL objects on the context they were made on (current here).
                    unsafe {
                        for p in planes.iter().flatten().map(|(p, _, _)| p).chain(surfaces.iter().flatten()) {
                            gl.delete_texture(*p);
                        }
                        gl.delete_texture(*out);
                    }
                }
            }
            keep
        });
        if self.gl.is_none() {
            self.gl = frame.gl().cloned();
        }
        let now = Instant::now();
        for key in keys {
            let Some(st) = viewer.state(&key) else { continue };
            let shown = if st.popped_out { true } else { !minimized && self.on_screen.contains(&key) };
            if !shown || !viewer.has_frame(&key) {
                continue;
            }
            // Small tiles (the strip) are drawn at most `max_fps` times a second; the newest frame
            // is shown once the interval is over.
            let last = self.tiles.get(&key).and_then(|t| t.last_upload);
            let cap = if self.measure_cap > 0 { self.measure_cap } else if st.popped_out { 0 } else { st.max_fps };
            if cap > 0 {
                if let Some(last) = last {
                    let every = Duration::from_secs(1) / cap;
                    let since = now.duration_since(last);
                    if since < every {
                        ctx.request_repaint_after(every - since);
                        continue;
                    }
                }
            }
            let Some(f) = viewer.take_frame(&key) else { continue };
            let style = viewer.draw_info(&key).map(|(s, _, _)| s).unwrap_or_default();
            let t0 = Instant::now();
            let zero_copy = f.native_nv12().is_some_and(|n| self.upload_surface(frame, &key, style, n));
            let ok = zero_copy || f.with_planes(|v| self.upload_one(ctx, frame, &key, style, v)).unwrap_or(false);
            if ok {
                f.presented();
                if let Some(t) = self.tiles.get_mut(&key) {
                    t.last_upload = Some(now);
                    // Keep a zero-copy frame (its surface) until the next one; drop the old one.
                    t.held = zero_copy.then_some(f);
                }
                self.stats.uploads += 1;
                self.stats.upload_us += t0.elapsed().as_micros() as u64;
                crate::repaint_watch::video_frame();
            } else {
                // Never silently: a picture that reached the app and could not be drawn.
                self.stats.not_drawn += 1;
                if !self.warned_not_drawn {
                    self.warned_not_drawn = true;
                    tracing::warn!(key, kind = %f.describe(), cpu_only = self.cpu_only, "mac video: a picture could not be drawn (format or upload failure)");
                }
            }
        }
        self.log_stats(viewer);
    }

    /// Make the GL objects on first use; false: the CPU path (from now on).
    fn ensure_gpu(&mut self) -> bool {
        if !self.cpu_only && self.gpu.is_none() {
            match self.gl.clone().ok_or_else(|| "no GL context".to_string()).and_then(|gl| Gpu::new(&gl)) {
                Ok(g) => {
                    tracing::info!(zero_copy = g.surface.is_some(), "mac video: GPU path (YUV shader)");
                    self.gpu = Some(g);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "mac video: GL path unavailable; converting on the CPU");
                    self.cpu_only = true;
                }
            }
        }
        !self.cpu_only && self.gl.is_some() && self.gpu.is_some()
    }

    /// `key`'s GPU tile (made on first use).
    fn gpu_tile<'a>(tiles: &'a mut HashMap<String, Tile>, gl: &glow::Context, frame: &mut eframe::Frame, key: &str, style: TileStyle) -> Option<&'a mut Tile> {
        let tile = match tiles.entry(key.to_string()) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                // SAFETY: GL calls on the app's context, current during the UI pass.
                let out = match unsafe { new_texture(gl, glow::TEXTURE_2D, false) } {
                    Ok(t) => t,
                    Err(err) => {
                        tracing::warn!(error = %err, "mac video: cannot create a texture");
                        return None;
                    }
                };
                let id = frame.register_native_glow_texture(out);
                e.insert(Tile { tex: Tex::Gpu { planes: [None, None, None], surfaces: [None, None], out, out_size: (0, 0), id }, size: (0, 0), style, last_upload: None, held: None, shown_px: Default::default() })
            }
        };
        tile.style = style;
        matches!(tile.tex, Tex::Gpu { .. }).then_some(tile)
    }

    /// Zero copy: convert a decoded IOSurface straight into the tile's texture.
    fn upload_surface(&mut self, frame: &mut eframe::Frame, key: &str, style: TileStyle, n: NativeNv12) -> bool {
        if !self.ensure_gpu() {
            return false;
        }
        let (Some(gl), Some(gpu)) = (self.gl.clone(), self.gpu.as_ref()) else { return false };
        let Some(prog) = gpu.surface.as_ref() else { return false };
        let Some(tile) = Self::gpu_tile(&mut self.tiles, &gl, frame, key, style) else { return false };
        let target = out_dims((n.width, n.height), tile.shown_px.get());
        let Tex::Gpu { surfaces, out, out_size, .. } = &mut tile.tex else { return false };
        // SAFETY: GL calls on the app's context (current during the UI pass); the surface
        // belongs to a frame the caller keeps alive.
        let res = unsafe { gpu.convert_surface(&gl, prog, n, surfaces, *out, out_size, target) };
        let ok = res.is_ok();
        if ok {
            tile.size = (n.width, n.height);
            *self.stats.sources.entry("iosurface").or_default() += 1;
            if n.full_range {
                self.stats.full_range += 1;
            }
        } else if self.stats.surface_failures == 0 {
            let err = res.err().unwrap_or_default();
            tracing::warn!(error = %err, size = format!("{}x{}", n.width, n.height), "mac video: IOSurface binding failed; uploading planes instead");
            self.stats.surface_failures += 1;
        }
        ok
    }

    fn upload_one(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame, key: &str, style: TileStyle, v: &PlaneView<'_>) -> bool {
        *self.stats.sources.entry(v.source).or_default() += 1;
        if v.full_range {
            self.stats.full_range += 1;
        }
        if self.ensure_gpu() {
            let (Some(gl), Some(gpu)) = (self.gl.clone(), self.gpu.as_ref()) else { return false };
            let Some(tile) = Self::gpu_tile(&mut self.tiles, &gl, frame, key, style) else { return false };
            tile.size = (v.width, v.height);
            let target = out_dims((v.width, v.height), tile.shown_px.get());
            if let Tex::Gpu { planes, out, out_size, .. } = &mut tile.tex {
                // SAFETY: GL calls on the app's context, current during the UI pass.
                return unsafe { gpu.convert_planes(&gl, v, planes, *out, out_size, target) };
            }
            return false;
        }
        // CPU fallback: libyuv to RGBA, then an ordinary egui texture.
        let tile = self.tiles.entry(key.to_string()).or_insert_with(|| Tile {
            tex: Tex::Cpu { handle: ctx.load_texture(format!("video:{key}"), egui::ColorImage::filled([1, 1], Color32::BLACK), egui::TextureOptions::LINEAR), rgba: Vec::new() },
            size: (0, 0),
            style,
            last_upload: None,
            held: None,
            shown_px: Default::default(),
        });
        tile.style = style;
        if let Tex::Cpu { handle, rgba } = &mut tile.tex {
            v.to_rgba(rgba);
            let img = egui::ColorImage::from_rgba_unmultiplied([v.width as usize, v.height as usize], rgba);
            handle.set(img, egui::TextureOptions::LINEAR);
            tile.size = (v.width, v.height);
            return true;
        }
        false
    }

    /// Draw `key`'s newest picture in `rect` (letterboxed, or cropped to fill for a camera; a
    /// mirrored style flips it). Nothing yet: nothing drawn (the tile's black stays).
    pub fn paint(&self, painter: &egui::Painter, key: &str, rect: Rect) {
        let Some(t) = self.tiles.get(key) else { return };
        let (w, h) = t.size;
        if w == 0 || h == 0 || rect.width() < 1.0 || rect.height() < 1.0 {
            return;
        }
        let id = match &t.tex {
            Tex::Gpu { id, .. } => *id,
            Tex::Cpu { handle, .. } => handle.id(),
        };
        let (draw, mut uv) = fit(rect, w as f32 / h as f32, t.style.fill);
        // The picture's size on screen (all of it, also the part a fill crop cuts off).
        let ppp = painter.ctx().pixels_per_point();
        t.shown_px.set(Some((draw.width() * ppp / uv.width().abs().max(1e-3), draw.height() * ppp / uv.height().abs().max(1e-3))));
        if t.style.mirror {
            uv = Rect::from_min_max(Pos2::new(uv.max.x, uv.min.y), Pos2::new(uv.min.x, uv.max.y));
        }
        painter.image(id, draw, uv, Color32::WHITE);
    }

    fn log_stats(&mut self, viewer: &Viewer) {
        let now = Instant::now();
        let since = *self.stats.since.get_or_insert(now);
        let secs = now.duration_since(since).as_secs_f64();
        if secs < vox_log::STATS_INTERVAL.as_secs_f64() {
            return;
        }
        if self.stats.uploads > 0 || self.stats.not_drawn > 0 {
            let replaced: u64 = self.tiles.keys().filter_map(|k| viewer.draw_info(k)).map(|(_, _, r)| r).sum();
            let sizes: Vec<String> = self.tiles.iter().map(|(k, t)| format!("{k}={}x{}", t.size.0, t.size.1)).collect();
            tracing::info!(
                path = if self.cpu_only { "cpu" } else { "gpu" },
                uploaded_fps = format!("{:.1}", self.stats.uploads as f64 / secs),
                upload_us_avg = self.stats.upload_us / self.stats.uploads.max(1),
                replaced_undrawn = replaced,
                sources = ?self.stats.sources,
                full_range = self.stats.full_range,
                not_drawn = self.stats.not_drawn,
                streams = ?sizes,
                "mac video stats"
            );
        }
        self.stats = Stats { since: Some(now), ..Default::default() };
    }
}

// ---------------------------------------------------------------- the app side

enum PopAction {
    Close,
    Fullscreen(bool),
}

impl crate::app::App {
    /// Start of a UI pass: upload the new frames of the videos on screen.
    pub(crate) fn mac_video_upload(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let Ok(v) = Viewer::get() else { return };
        let minimized = self.notify.hidden || crate::app::window_minimized(ctx);
        self.video.mac.upload(ctx, frame, &v, minimized);
    }

    /// After everything is drawn (`sync_video`): paint each video where its tile asked for it,
    /// on the tile's own layer (so egui's popups stay above it), and show the popped-out ones in
    /// their own windows.
    pub(crate) fn paint_videos(&mut self, ctx: &egui::Context, keys: &[String]) {
        let Ok(v) = Viewer::get() else { return };
        let ppp = ctx.pixels_per_point();
        let mac = &mut self.video.mac;
        let paint_at = std::mem::take(&mut mac.paint_at);
        let mut on_screen = HashSet::new();
        for key in keys {
            let Some(rect) = self.video.want.get(key) else { continue };
            let Some((layer, clip)) = paint_at.get(key) else { continue };
            mac.paint(&ctx.layer_painter(*layer).with_clip_rect(*clip), key, *rect);
            on_screen.insert(key.clone());
        }
        // Where each video was drawn (physical pixels), like the placed child windows on Windows:
        // for logs and the autotests.
        let placed = &mut self.video.placed;
        placed.retain(|k, _| on_screen.contains(k));
        for key in &on_screen {
            if let Some(rect) = self.video.want.get(key) {
                placed.insert(key.clone(), crate::call::to_px(*rect, ppp));
            }
        }
        // A frame that came while its tile was off screen woke nobody: show it now.
        if on_screen.iter().any(|k| !mac.on_screen.contains(k) && v.has_frame(k)) {
            ctx.request_repaint();
        }
        mac.on_screen = on_screen;
        self.popout_windows(ctx, &v);
    }

    /// Each popped-out watched stream in its own window (an egui viewport). Closing it brings
    /// the stream back into the app; a double-click or F11 toggles fullscreen, Esc leaves it.
    fn popout_windows(&mut self, ctx: &egui::Context, v: &Viewer) {
        let watched: Vec<String> = self.voice.watching.iter().filter(|k| self.is_watching(k)).cloned().collect();
        let mut open = HashSet::new();
        for key in watched {
            let Some(st) = v.state(&key).filter(|s| s.popped_out) else { continue };
            open.insert(key.clone());
            let title = v.draw_info(&key).map(|(_, t, _)| t).filter(|t| !t.is_empty()).unwrap_or_else(|| crate::brand::APP_NAME.to_string());
            let size = *self.video.mac.popout_size.entry(key.clone()).or_insert_with(|| {
                let (w, h) = if st.video.0 > 0 { st.video } else { (1280, 720) };
                let s = (1280.0 / w as f32).min(720.0 / h as f32).min(1.0);
                Vec2::new(w as f32 * s, h as f32 * s)
            });
            let id = egui::ViewportId::from_hash_of(("stream_popout", &key));
            let builder = egui::ViewportBuilder::default().with_title(title).with_inner_size(size).with_min_inner_size([320.0, 180.0]);
            let mac = &self.video.mac;
            let action = ctx.show_viewport_immediate(id, builder, |ui, _class| {
                let vctx = ui.ctx().clone();
                let rect = vctx.content_rect();
                let painter = ui.painter().clone();
                painter.rect_filled(rect, 0.0, Color32::BLACK);
                mac.paint(&painter, &key, rect);
                let resp = ui.interact(rect, egui::Id::new(("stream_popout_area", &key)), egui::Sense::click());
                let fullscreen = vctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                let (esc, f11, close) = vctx.input(|i| (i.key_pressed(egui::Key::Escape), i.key_pressed(egui::Key::F11), i.viewport().close_requested()));
                if close {
                    return Some(PopAction::Close);
                }
                if resp.double_clicked() || f11 || (esc && fullscreen) {
                    let on = !fullscreen && !esc;
                    vctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
                    return Some(PopAction::Fullscreen(on));
                }
                None
            });
            match action {
                Some(PopAction::Close) => {
                    tracing::info!(key, "stream window closed: back inside the app");
                    v.pop_out(&key, false);
                    open.remove(&key);
                }
                Some(PopAction::Fullscreen(on)) => {
                    tracing::info!(key, on, "stream window fullscreen");
                    v.set_fullscreen(&key, on);
                }
                None => {}
            }
        }
        self.video.mac.popout_size.retain(|k, _| open.contains(k));
    }
}

/// The converted texture's size: the size the picture was last shown at (whole picture, physical
/// pixels), never larger than the video, keeping its aspect; the video's size before it was shown.
fn out_dims((vw, vh): (u32, u32), shown: Option<(f32, f32)>) -> (i32, i32) {
    let scale = shown.map_or(1.0, |(sw, sh)| (sw / vw as f32).max(sh / vh as f32).min(1.0));
    let w = ((vw as f32 * scale).round() as i32).max(2);
    let h = ((vh as f32 * scale).round() as i32).max(2);
    (w, h)
}

/// Where a `aspect` picture goes in `rect`, and which part of it (UV): letterboxed, or cropped
/// to cover the whole rect (`fill`).
fn fit(rect: Rect, aspect: f32, fill: bool) -> (Rect, Rect) {
    let full = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    let ra = rect.width() / rect.height();
    if fill {
        let uv = if aspect > ra {
            let u = ra / aspect;
            Rect::from_min_max(Pos2::new((1.0 - u) / 2.0, 0.0), Pos2::new((1.0 + u) / 2.0, 1.0))
        } else {
            let v = aspect / ra;
            Rect::from_min_max(Pos2::new(0.0, (1.0 - v) / 2.0), Pos2::new(1.0, (1.0 + v) / 2.0))
        };
        return (rect, uv);
    }
    let size = if aspect > ra { Vec2::new(rect.width(), rect.width() / aspect) } else { Vec2::new(rect.height() * aspect, rect.height()) };
    (Rect::from_center_size(rect.center(), size), full)
}

/// A `target` texture with linear filtering (and mipmaps when `mips`), clamped at the edges.
/// Leaves it bound on the active unit.
unsafe fn new_texture(gl: &glow::Context, target: u32, mips: bool) -> Result<glow::Texture, String> {
    // SAFETY: plain GL object creation and parameters on the current context.
    unsafe {
        let t = gl.create_texture()?;
        gl.bind_texture(target, Some(t));
        let min = if mips { glow::LINEAR_MIPMAP_LINEAR } else { glow::LINEAR };
        gl.tex_parameter_i32(target, glow::TEXTURE_MIN_FILTER, min as i32);
        gl.tex_parameter_i32(target, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(target, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(target, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
        Ok(t)
    }
}

/// Compile and link `fs` with the shared vertex shader.
unsafe fn link(gl: &glow::Context, version: &str, fs: &str) -> Result<Prog, String> {
    // SAFETY: GL object creation on the current context; every object is checked.
    unsafe {
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (kind, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, fs)] {
            let s = gl.create_shader(kind)?;
            gl.shader_source(s, &format!("{version}#define NEW_SHADER_INTERFACE 1\n{src}"));
            gl.compile_shader(s);
            if !gl.get_shader_compile_status(s) {
                let log = gl.get_shader_info_log(s);
                gl.delete_shader(s);
                gl.delete_program(program);
                return Err(format!("shader: {log}"));
            }
            gl.attach_shader(program, s);
            shaders.push(s);
        }
        // The vertex attribute at 0 in both programs (they share the VAO).
        gl.bind_attrib_location(program, 0, "a_pos");
        gl.link_program(program);
        let linked = gl.get_program_link_status(program);
        for s in shaders {
            gl.detach_shader(program, s);
            gl.delete_shader(s);
        }
        if !linked {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(format!("link: {log}"));
        }
        let u = |name: &str| gl.get_uniform_location(program, name);
        Ok(Prog { u_y: u("u_y"), u_u: u("u_u"), u_v: u("u_v"), u_nv12: u("u_nv12"), u_full: u("u_full"), u_ysize: u("u_ysize"), u_csize: u("u_csize"), u_tap: u("u_tap"), program })
    }
}

impl Gpu {
    fn new(gl: &glow::Context) -> Result<Gpu, String> {
        let version = egui_glow_version(gl);
        if !version.1 {
            return Err(format!("GLSL {} has no RG textures / new interface", version.0.trim()));
        }
        // SAFETY: GL object creation on the current context; every object is checked.
        unsafe {
            let planes = link(gl, version.0, FS)?;
            let surface = match link(gl, version.0, FS_RECT) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "mac video: no zero-copy program");
                    None
                }
            };
            let a_pos = 0;
            let vao = gl.create_vertex_array().ok();
            let vbo = gl.create_buffer()?;
            gl.bind_vertex_array(vao);
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            // One triangle covering the target; uv = position, so (0,0) samples the first row.
            let tri: [f32; 6] = [0.0, 0.0, 2.0, 0.0, 0.0, 2.0];
            let bytes: Vec<u8> = tri.iter().flat_map(|f| f.to_ne_bytes()).collect();
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(a_pos);
            gl.vertex_attrib_pointer_f32(a_pos, 2, glow::FLOAT, false, 8, 0);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            let fbo = gl.create_framebuffer()?;
            Ok(Gpu { planes, surface, vao, vbo, a_pos, fbo })
        }
    }

    /// Upload `v`'s planes and convert them into `out`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn convert_planes(&self, gl: &glow::Context, v: &PlaneView<'_>, planes: &mut [Option<(glow::Texture, i32, i32)>; 3], out: glow::Texture, out_size: &mut (i32, i32), target: (i32, i32)) -> bool {
        let (w, h) = (v.width as i32, v.height as i32);
        let (cw, ch) = ((w + 1) / 2, (h + 1) / 2);
        // SAFETY: GL calls on the current context with our own objects; uploads read
        // `rows x stride` bytes, which the plane slices hold (checked in `upload_plane`).
        unsafe {
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            let nv12 = matches!(v.planes, Planes::Nv12 { .. });
            let ok = match v.planes {
                Planes::I420 { y, u, v: pv, strides: (sy, su, sv) } => {
                    upload_plane(gl, &mut planes[0], 0, glow::R8, glow::RED, y, sy as i32, 1, w, h)
                        && upload_plane(gl, &mut planes[1], 1, glow::R8, glow::RED, u, su as i32, 1, cw, ch)
                        && upload_plane(gl, &mut planes[2], 2, glow::R8, glow::RED, pv, sv as i32, 1, cw, ch)
                }
                Planes::Nv12 { y, uv, strides: (sy, suv) } => {
                    upload_plane(gl, &mut planes[0], 0, glow::R8, glow::RED, y, sy as i32, 1, w, h) && upload_plane(gl, &mut planes[1], 1, glow::RG8, glow::RG, uv, suv as i32, 2, cw, ch)
                }
            };
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            gl.active_texture(glow::TEXTURE0);
            if !ok {
                return false;
            }
            let p = &self.planes;
            gl.use_program(Some(p.program));
            gl.uniform_1_f32(p.u_nv12.as_ref(), if nv12 { 1.0 } else { 0.0 });
            gl.uniform_1_f32(p.u_full.as_ref(), if v.full_range { 1.0 } else { 0.0 });
            let units: Vec<(u32, Option<glow::Texture>)> = planes.iter().map(|p| (glow::TEXTURE_2D, p.map(|(t, _, _)| t))).collect();
            self.draw(gl, p, &units, out, out_size, (w, h), target)
        }
    }

    /// Zero copy: bind the IOSurface's planes (Y: R8, UV: RG8) to rectangle textures and convert
    /// them into `out`. False when the binding fails (the caller uploads planes instead).
    #[allow(clippy::too_many_arguments)]
    unsafe fn convert_surface(&self, gl: &glow::Context, p: &Prog, n: NativeNv12, surfaces: &mut [Option<glow::Texture>; 2], out: glow::Texture, out_size: &mut (i32, i32), target: (i32, i32)) -> Result<(), String> {
        let (w, h) = (n.width as i32, n.height as i32);
        let (cw, ch) = ((w + 1) / 2, (h + 1) / 2);
        // SAFETY: GL / CGL calls on the current context with our own textures; the IOSurface is
        // alive (the caller holds its frame).
        unsafe {
            let ctx = cgl::CGLGetCurrentContext();
            if ctx.is_null() {
                return Err("no current CGL context".into());
            }
            for (i, (internal, format, pw, ph)) in [(glow::R8, glow::RED, w, h), (glow::RG8, glow::RG, cw, ch)].into_iter().enumerate() {
                gl.active_texture(glow::TEXTURE0 + i as u32);
                let t = match surfaces[i] {
                    Some(t) => {
                        gl.bind_texture(glow::TEXTURE_RECTANGLE, Some(t));
                        t
                    }
                    None => match new_texture(gl, glow::TEXTURE_RECTANGLE, false) {
                        Ok(t) => {
                            surfaces[i] = Some(t);
                            t
                        }
                        Err(e) => return Err(format!("texture: {e}")),
                    },
                };
                let _ = t;
                let err = cgl::CGLTexImageIOSurface2D(ctx, glow::TEXTURE_RECTANGLE, internal, pw, ph, format, glow::UNSIGNED_BYTE, n.io_surface, i as u32);
                if err != 0 {
                    gl.bind_texture(glow::TEXTURE_RECTANGLE, None);
                    gl.active_texture(glow::TEXTURE0);
                    return Err(format!("CGLTexImageIOSurface2D plane {i} ({pw}x{ph}): CGL error {err}"));
                }
            }
            gl.use_program(Some(p.program));
            gl.uniform_2_f32(p.u_ysize.as_ref(), w as f32, h as f32);
            gl.uniform_2_f32(p.u_csize.as_ref(), cw as f32, ch as f32);
            gl.uniform_1_f32(p.u_full.as_ref(), if n.full_range { 1.0 } else { 0.0 });
            let units = [(glow::TEXTURE_RECTANGLE, surfaces[0]), (glow::TEXTURE_RECTANGLE, surfaces[1])];
            let ok = self.draw(gl, p, &units, out, out_size, (w, h), target);
            for i in 0..2 {
                gl.active_texture(glow::TEXTURE0 + i);
                gl.bind_texture(glow::TEXTURE_RECTANGLE, None);
            }
            gl.active_texture(glow::TEXTURE0);
            if ok {
                Ok(())
            } else {
                Err("framebuffer incomplete".into())
            }
        }
    }

    /// Run program `p` (already in use, its own uniforms set) over `units` (a `src`-sized
    /// picture) into `out`, resized to `(w, h)`; a smaller `out` gets the 4-tap filter. Leaves the
    /// GL state egui relies on as egui sets it again before painting (it binds its own program,
    /// VAO, textures, blending, scissor and viewport); restores the framebuffer and the viewport.
    #[allow(clippy::too_many_arguments)]
    unsafe fn draw(&self, gl: &glow::Context, p: &Prog, units: &[(u32, Option<glow::Texture>)], out: glow::Texture, out_size: &mut (i32, i32), src: (i32, i32), (w, h): (i32, i32)) -> bool {
        // SAFETY: GL calls on the current context with our own objects.
        unsafe {
            let prev_fbo = gl.get_parameter_framebuffer(glow::FRAMEBUFFER_BINDING);
            let mut vp = [0i32; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut vp);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(out));
            if *out_size != (w, h) {
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w, h, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                *out_size = (w, h);
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(out), 0);
            let complete = gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
            if complete {
                gl.viewport(0, 0, w, h);
                gl.disable(glow::SCISSOR_TEST);
                gl.disable(glow::BLEND);
                gl.uniform_1_i32(p.u_y.as_ref(), 0);
                gl.uniform_1_i32(p.u_u.as_ref(), 1);
                gl.uniform_1_i32(p.u_v.as_ref(), 2);
                // Downscaling by more than ~1.3x: four taps a quarter of an output pixel apart.
                let tap = if src.0 as f32 > w as f32 * 1.3 { (0.25 / w as f32, 0.25 / h as f32) } else { (0.0, 0.0) };
                gl.uniform_2_f32(p.u_tap.as_ref(), tap.0, tap.1);
                for (i, (target, t)) in units.iter().enumerate() {
                    gl.active_texture(glow::TEXTURE0 + i as u32);
                    gl.bind_texture(*target, *t);
                }
                gl.bind_vertex_array(self.vao);
                if self.vao.is_none() {
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
                    gl.enable_vertex_attrib_array(self.a_pos);
                    gl.vertex_attrib_pointer_f32(self.a_pos, 2, glow::FLOAT, false, 8, 0);
                }
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                gl.bind_vertex_array(None);
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, prev_fbo);
            gl.viewport(vp[0], vp[1], vp[2], vp[3]);
            complete
        }
    }
}

/// Upload one plane into texture unit `unit` (made / resized as needed). `bpp`: bytes per texel.
#[allow(clippy::too_many_arguments)]
unsafe fn upload_plane(gl: &glow::Context, slot: &mut Option<(glow::Texture, i32, i32)>, unit: u32, internal: u32, format: u32, data: &[u8], stride: i32, bpp: i32, w: i32, h: i32) -> bool {
    if w <= 0 || h <= 0 || stride < w * bpp || data.len() < (stride as usize) * (h as usize - 1) + (w * bpp) as usize {
        return false;
    }
    // SAFETY: GL calls on the current context; the slice covers `h` rows of `stride` bytes.
    unsafe {
        gl.active_texture(glow::TEXTURE0 + unit);
        let tex = match *slot {
            Some((t, tw, th)) if (tw, th) == (w, h) => {
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
                t
            }
            other => {
                if let Some((t, _, _)) = other {
                    gl.delete_texture(t);
                }
                let Ok(t) = new_texture(gl, glow::TEXTURE_2D, false) else { return false };
                gl.tex_image_2d(glow::TEXTURE_2D, 0, internal as i32, w, h, 0, format, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                *slot = Some((t, w, h));
                t
            }
        };
        let _ = tex;
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, stride / bpp);
        gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, w, h, format, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(data)));
        true
    }
}

/// egui_glow's GLSL version line, and whether it is GLSL 1.40+ / ES 3 (what the shader needs).
fn egui_glow_version(gl: &glow::Context) -> (&'static str, bool) {
    let v = eframe::egui_glow::ShaderVersion::get(gl);
    (v.version_declaration(), v.is_new_shader_interface())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_texture_follows_the_shown_size_up_to_the_video() {
        assert_eq!(out_dims((1920, 1080), None), (1920, 1080));
        assert_eq!(out_dims((1920, 1080), Some((960.0, 540.0))), (960, 540));
        assert_eq!(out_dims((1920, 1080), Some((3840.0, 2160.0))), (1920, 1080), "never upscaled");
        // A letterboxed tile: the larger ratio wins (no axis below its shown size).
        assert_eq!(out_dims((1920, 1080), Some((640.0, 400.0))), (711, 400));
        assert_eq!(out_dims((640, 360), Some((0.5, 0.5))), (2, 2));
    }

    #[test]
    fn letterbox_and_fill() {
        let r = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(400.0, 300.0));
        // 16:9 in 4:3: full width, bars above and below.
        let (d, uv) = fit(r, 16.0 / 9.0, false);
        assert!((d.width() - 400.0).abs() < 0.01 && (d.height() - 225.0).abs() < 0.01);
        assert_eq!(d.center(), r.center());
        assert_eq!(uv, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)));
        // Fill: the whole rect, the sides of the picture cut off.
        let (d, uv) = fit(r, 16.0 / 9.0, true);
        assert_eq!(d, r);
        assert!((uv.width() - 0.75).abs() < 0.001 && (uv.center().x - 0.5).abs() < 0.001 && uv.height() == 1.0);
        // A tall picture in a wide rect: pillarbox / crop top and bottom.
        let (d, _) = fit(r, 0.5, false);
        assert!((d.height() - 300.0).abs() < 0.01 && (d.width() - 150.0).abs() < 0.01);
        let (_, uv) = fit(r, 0.5, true);
        assert!(uv.width() == 1.0 && (uv.height() - 0.375).abs() < 0.001);
    }
}
