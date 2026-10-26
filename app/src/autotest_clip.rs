//! Airspace autotest (a child module of `autotest`, part of `cam-b`, v0.8.1): an egui overlay over
//! part of a remote camera must be cut out of the video's child window instead of hiding it.
//!
//! With A's camera focused (a big tile), B:
//! 1. opens the soundboard panel (anchored bottom-right; a status message of a few lines makes it
//!    as tall as one holding sounds, so it is over the bottom-right part of the tile): the video
//!    is "clipped", its child window stays visible with a window region, a point under the panel
//!    is outside the region and a point of the video away from it is inside, and frames keep
//!    being presented (the presented count grows while covered);
//! 2. closes it and opens A's right-click menu in the middle of the tile: the same checks;
//! 3. closes the menu: the region is gone (no region, window visible) and the video still plays.
//!
//! `VOX_AUTOTEST_SHOT_DIR=<dir>`: also saves what the app window looks like at each step
//! (`clip-*.png`: the window as Windows renders it, and the screen when the app is in front), for
//! a look at the hole with the popup in it.

use std::time::Instant;

use super::Auto;
use crate::app::App;
use crate::call::clip::{self, Clip, Px};
use crate::context_menu::{Lead, Origin};

#[derive(Default)]
pub struct State {
    phase: u8,
    at: Option<Instant>,
    presented: u64,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST B airspace: {what}");
}

fn presented(key: &str) -> u64 {
    Auto::tile_state(key).map(|s| s.presented).unwrap_or(0)
}

impl Auto {
    fn clip_waited(&self, secs: f32) -> bool {
        self.clip.at.is_some_and(|t| t.elapsed().as_secs_f32() >= secs)
    }

    fn clip_next(&mut self, phase: u8) {
        self.clip.phase = phase;
        self.clip.at = Some(Instant::now());
    }

    /// Checks that `key`'s video is drawn around an overlay (`what`): clipped, its window shown
    /// with a region that leaves out the cut and keeps the rest. Returns the cuts.
    fn check_clipped(&self, app: &App, key: &str, what: &str) -> Result<Vec<Px>, String> {
        let clip = app.video.clips.get(key).cloned();
        let placed = app.video.placed.get(key).copied();
        let window = window_state(app, key);
        log(&format!("{what}: clip={clip:?} placed={placed:?} window(shown, region box)={window:?}"));
        let Some(Clip::Cut(cuts)) = clip else {
            return Err(format!("{what}: the camera video is not clipped around the overlay (clip={clip:?})"));
        };
        let Some((_, _, w, h)) = placed else { return Err(format!("{what}: the camera video is not placed")) };
        // (macOS: no video window; egui draws the overlay over the video it drew itself.)
        if cfg!(windows) && !window.is_some_and(|(shown, region)| shown && region.is_some()) {
            return Err(format!("{what}: the video window is not shown with a region: {window:?}"));
        }
        // A point under the overlay is cut out; a point of what is left still shows.
        let c = cuts[0];
        let under = (c.0 + c.2 / 2, c.1 + c.3 / 2);
        let rest = clip::subtract((0, 0, w, h), &cuts);
        let Some(r) = rest.iter().max_by_key(|r| r.2 as i64 * r.3 as i64) else {
            return Err(format!("{what}: nothing of the video is left: {cuts:?}"));
        };
        let away = (r.0 + r.2 / 2, r.1 + r.3 / 2);
        let (hit_under, hit_away) = (window_hit(app, key, under), window_hit(app, key, away));
        log(&format!("{what}: region holds {under:?} (under the overlay): {hit_under:?}, {away:?} (the video): {hit_away:?}"));
        if cfg!(windows) && (hit_under != Some(false) || hit_away != Some(true)) {
            return Err(format!("{what}: the window region does not match the cut ({hit_under:?} / {hit_away:?})"));
        }
        Ok(cuts)
    }

    /// Frames presented since the last baseline, and the rate; `Err` when none were.
    fn clip_frames(&mut self, key: &str, what: &str) -> Result<(), String> {
        let secs = self.clip.at.map(|t| t.elapsed().as_secs_f32()).unwrap_or(1.0);
        let d = presented(key).saturating_sub(self.clip.presented);
        log(&format!("presented {what}: +{d} in {secs:.1}s = {:.1} fps", d as f32 / secs));
        if d == 0 {
            return Err(format!("the camera video stopped presenting frames {what}"));
        }
        Ok(())
    }

    /// The airspace part of `cam-b` (see the module doc). `Some(Ok)` when done.
    pub(super) fn clip_b(&mut self, app: &mut App, key: &str) -> Option<Result<(), String>> {
        let step = match self.clip.phase {
            0 => {
                // With no sounds the panel is short and stays under the focused tile; a few lines
                // of status message make it as tall as one with a couple of rows of sounds.
                let lines: Vec<String> = (1..=9).map(|i| format!("linha de teste {i}")).collect();
                app.sb.message = Some((true, lines.join("\n")));
                app.sb.open = true;
                log("opened the soundboard panel over the focused camera");
                self.clip_next(1);
                Ok(())
            }
            1 if self.clip_waited(2.0) => self.check_clipped(app, key, "soundboard open").and_then(|cuts| {
                // More than the padding: the panel itself is over the video.
                let pad = (clip::PAD * app.repaint_ctx().pixels_per_point()).ceil() as i32;
                if !cuts.iter().any(|c| c.2 > 4 * pad && c.3 > 4 * pad) {
                    return Err(format!("the soundboard is not really over the camera video (cuts {cuts:?}); the layout changed?"));
                }
                shot(app, "soundboard");
                self.clip.presented = presented(key);
                self.clip_next(2);
                Ok(())
            }),
            2 if self.clip_waited(3.0) => self.clip_frames(key, "while the soundboard is over it").and_then(|()| {
                self.check_clipped(app, key, "soundboard still open")?;
                app.sb.open = false;
                app.sb.message = None;
                // A's menu, opened where a right-click in the middle of the tile would.
                let ppp = app.repaint_ctx().pixels_per_point();
                let (x, y, w, h) = app.video.placed.get(key).copied().ok_or("the camera video is not placed")?;
                let user = self.cam_of.as_deref().and_then(|i| i.parse::<i64>().ok()).and_then(|id| app.known_user(id));
                let user = user.ok_or("A is not a known user")?;
                let pos = eframe::egui::pos2((x + w / 2) as f32 / ppp, (y + h / 2) as f32 / ppp);
                let ctx = app.repaint_ctx();
                app.open_person_menu(&ctx, &user, Origin::Tile, Lead::Camera, pos);
                log(&format!("closed the soundboard; opened A's menu at {pos:?}"));
                self.clip_next(3);
                Ok(())
            }),
            3 if self.clip_waited(2.0) => self.check_clipped(app, key, "menu open").map(|_| {
                shot(app, "menu");
                self.clip.presented = presented(key);
                self.clip_next(4);
            }),
            4 if self.clip_waited(2.0) => self.clip_frames(key, "while the menu is over it").map(|()| {
                app.cmenu.open = None;
                eframe::egui::Popup::close_all(&app.repaint_ctx());
                log("closed the menu");
                self.clip.presented = presented(key);
                self.clip_next(5);
            }),
            5 if self.clip_waited(2.0) => {
                let clip = app.video.clips.get(key).cloned();
                let window = window_state(app, key);
                log(&format!("overlay closed: clip={clip:?} window(shown, region box)={window:?}"));
                if clip.is_some() {
                    return Some(Err(format!("the video is still clipped after the overlay closed: {clip:?}")));
                }
                if cfg!(windows) && window != Some((true, None)) {
                    return Some(Err(format!("the video window kept a region (or is hidden) after the overlay closed: {window:?}")));
                }
                if let Err(e) = self.clip_frames(key, "after the overlay closed") {
                    return Some(Err(e));
                }
                shot(app, "closed");
                return Some(Ok(()));
            }
            _ => Ok(()),
        };
        step.err().map(Err)
    }
}

#[cfg(windows)]
fn window_state(app: &App, key: &str) -> Option<(bool, Option<Px>)> {
    app.video_window_state(key)
}

#[cfg(not(windows))]
fn window_state(_app: &App, _key: &str) -> Option<(bool, Option<Px>)> {
    None
}

#[cfg(windows)]
fn window_hit(app: &App, key: &str, (x, y): (i32, i32)) -> Option<bool> {
    app.video_window_hit(key, x, y)
}

#[cfg(not(windows))]
fn window_hit(_app: &App, _key: &str, _at: (i32, i32)) -> Option<bool> {
    None
}

/// `VOX_AUTOTEST_SHOT_DIR`: save the app window (as Windows renders it) and, when the app is in
/// front, the screen under it.
fn shot(app: &App, label: &str) {
    let Some(dir) = std::env::var_os("VOX_AUTOTEST_SHOT_DIR") else { return };
    let Some(hwnd) = app.video.parent else { return };
    #[cfg(windows)]
    for (screen, kind) in [(false, "window"), (true, "screen")] {
        let path = std::path::Path::new(&dir).join(format!("clip-{label}-{kind}.png"));
        match capture::client(hwnd, screen) {
            Some(img) => match img.save(&path) {
                Ok(()) => log(&format!("saved {}", path.display())),
                Err(e) => log(&format!("could not save {}: {e}", path.display())),
            },
            None => log(&format!("no {kind} capture (the app is not in front, or the capture failed)")),
        }
    }
    #[cfg(not(windows))]
    let _ = (dir, hwnd, label);
}

#[cfg(windows)]
pub(super) mod capture {
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, HDC, SRCCOPY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetForegroundWindow};

    #[link(name = "user32")]
    unsafe extern "system" {
        fn PrintWindow(hwnd: HWND, hdc: HDC, flags: u32) -> i32;
    }
    const PW_CLIENTONLY: u32 = 1;
    const PW_RENDERFULLCONTENT: u32 = 2;

    /// The client area of `hwnd`: as Windows renders the window (`screen == false`), or as the
    /// screen shows it (only when the window is in front, so nothing else is captured).
    pub fn client(hwnd: isize, screen: bool) -> Option<image::RgbaImage> {
        let hwnd = hwnd as HWND;
        // SAFETY: GDI objects created, used and released here, on this thread; valid
        // out-pointers and a buffer sized for the bitmap.
        unsafe {
            if screen && GetForegroundWindow() != hwnd {
                return None;
            }
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            if GetClientRect(hwnd, &mut r) == 0 || r.right <= 0 || r.bottom <= 0 {
                return None;
            }
            let (w, h) = (r.right, r.bottom);
            let src = if screen { GetDC(null_mut()) } else { GetDC(hwnd) };
            let mem = CreateCompatibleDC(src);
            let bmp = CreateCompatibleBitmap(src, w, h);
            let old = SelectObject(mem, bmp);
            let ok = if screen {
                let mut p = POINT { x: 0, y: 0 };
                ClientToScreen(hwnd, &mut p);
                BitBlt(mem, 0, 0, w, h, src, p.x, p.y, SRCCOPY) != 0
            } else {
                PrintWindow(hwnd, mem, PW_CLIENTONLY | PW_RENDERFULLCONTENT) != 0
            };
            SelectObject(mem, old);
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB;
            let mut buf = vec![0u8; w as usize * h as usize * 4];
            let rows = if ok { GetDIBits(mem, bmp, 0, h as u32, buf.as_mut_ptr().cast(), &mut bi, DIB_RGB_COLORS) } else { 0 };
            DeleteObject(bmp);
            DeleteDC(mem);
            ReleaseDC(if screen { null_mut() } else { hwnd }, src);
            if rows == 0 {
                return None;
            }
            for px in buf.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
                px[3] = 255;
            }
            image::RgbaImage::from_raw(w as u32, h as u32, buf)
        }
    }
}
