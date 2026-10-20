//! `mac-view` (v1.1, macOS phase M2): watching a stream drawn by the app itself
//! (`video_gl.rs`). Logs in, joins the first voice room, watches the first person who is live
//! (any client: the release machines use `vox-cli share h264file ...`), then checks that frames
//! keep being drawn inside the app, in a pinned card, in the stream fullscreen, in its own window
//! (pop-out) and back. With `--watch-secs N` it first just watches for N seconds (a CPU
//! measurement window, nothing else ticking). `--mode static`: the sharer shows a window that
//! does not change; only the first picture is checked.

use super::{Auto, Screen};
use crate::app::App;
use crate::call::TileKey;

/// `VOX_AUTOTEST_SHOT_DIR`: ask egui for a picture of the app's own window (only its own
/// framebuffer, nothing else on the screen); [`save_shots`] writes it as `<dir>/<name>.png`.
fn shot(app: &App, name: &str) {
    if std::env::var_os("VOX_AUTOTEST_SHOT_DIR").is_some() {
        app.repaint_ctx().send_viewport_cmd(eframe::egui::ViewportCommand::Screenshot(eframe::egui::UserData::new(name.to_string())));
    }
}

/// Write the window pictures egui handed back this pass.
fn save_shots(app: &App) {
    let Some(dir) = std::env::var_os("VOX_AUTOTEST_SHOT_DIR") else { return };
    let shots: Vec<(String, std::sync::Arc<eframe::egui::ColorImage>)> = app.repaint_ctx().input(|i| {
        i.raw
            .events
            .iter()
            .filter_map(|e| match e {
                eframe::egui::Event::Screenshot { user_data, image, .. } => Some((user_data.data.as_ref()?.downcast_ref::<String>()?.clone(), image.clone())),
                _ => None,
            })
            .collect()
    });
    for (name, img) in shots {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
        let res = image::RgbaImage::from_raw(img.size[0] as u32, img.size[1] as u32, rgba).map(|i| i.save(&path));
        tracing::info!(target: "autotest", "AUTOTEST mac-view: window picture {} ({:?})", path.display(), res.map(|r| r.is_ok()));
    }
}

/// Frames drawn for the watched stream so far (the viewer counts what the app uploaded).
fn presented(key: &str) -> u64 {
    vox_client_core::golive::Viewer::get().ok().and_then(|v| v.state(key)).map_or(0, |s| s.presented)
}

impl Auto {
    pub(super) fn run_mac_view(&mut self, app: &mut App) -> Option<i32> {
        let key = app.voice.watching.first().cloned();
        let shown = key.as_deref().map_or(0, presented);
        save_shots(app);
        match self.step {
            0 => self.connect(app),
            1 => {
                if app.screen == Screen::Main && app.ready && app.hub_up {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "logged in; joining voice");
                } else if self.waited(30) {
                    return self.fail("could not log in");
                }
            }
            2 => {
                let me = app.my_id();
                let live = app.members_here().iter().find(|m| m.streaming && Some(m.user.id) != me).map(|m| m.user.id);
                if let Some(id) = live.filter(|_| self.waited(2)) {
                    app.watch_focused(&id.to_string());
                    self.goto(3, "someone is live; watching (focused)");
                } else if self.waited(90) {
                    return self.fail("nobody went live");
                }
            }
            // `--mode static`: the sharer's window does not change (no new frames at all): the
            // first picture must still be drawn, and soon (the sharer answers our keyframe
            // request with its last frame).
            3 if self.args.mode.as_deref() == Some("static") => {
                if shown >= 1 {
                    let st = key.as_deref().and_then(|k| vox_client_core::golive::Viewer::get().ok()?.state(k));
                    tracing::info!(target: "autotest", "AUTOTEST mac-view: static stream: first picture drawn {:.1} s after watching ({shown} drawn), video {:?}", self.step_at.elapsed().as_secs_f32(), st.map(|s| s.video));
                    shot(app, "mac-view-static");
                    app.leave_voice();
                    self.goto(9, "static stream drawn; leaving");
                } else if self.waited(20) {
                    return self.fail("static stream: no picture drawn in 20 s (black tile)");
                }
            }
            3 => {
                if shown >= 30 {
                    self.frames_first = shown;
                    let st = key.as_deref().and_then(|k| vox_client_core::golive::Viewer::get().ok()?.state(k));
                    tracing::info!(target: "autotest", "AUTOTEST mac-view: frames drawn inside the app: {shown}, video {:?}", st.map(|s| s.video));
                    // `--mode decode-only`: the window is redrawn once a second during the measurement
                    // (the stream is still received and decoded at full rate): the decode cost alone.
                    #[cfg(target_os = "macos")]
                    if self.args.mode.as_deref() == Some("decode-only") {
                        app.video.mac.measure_cap = 1;
                    }
                    self.goto(4, "frames drawn; measurement window");
                } else if self.waited(25) {
                    return self.fail(&format!("no frames drawn ({shown})"));
                }
            }
            4 => {
                if self.waited(self.args.watch_secs) {
                    let secs = self.step_at.elapsed().as_secs_f64().max(1.0);
                    tracing::info!(target: "autotest", "AUTOTEST mac-view: watched {:.0} s, drawn {:.1} fps", secs, (shown - self.frames_first) as f64 / secs);
                    #[cfg(target_os = "macos")]
                    {
                        app.video.mac.measure_cap = 0;
                    }
                    shot(app, "mac-view-inline");
                    self.frames_first = shown;
                    let id = key.as_deref().and_then(|k| k.parse().ok());
                    if let Some(id) = id.filter(|id| !app.is_pinned(TileKey::Stream(*id))) {
                        app.toggle_pin(TileKey::Stream(id));
                    }
                    self.goto(5, "pinned");
                }
            }
            5 if self.waited(3) => {
                if shown < self.frames_first + 20 {
                    return self.fail("no frames while pinned");
                }
                self.frames_first = shown;
                let ctx = app.repaint_ctx();
                if let Some(k) = &key {
                    app.fullscreen_stream(&ctx, k);
                }
                self.goto(6, "stream fullscreen");
            }
            6 if self.waited(2) && !self.waited(3) && self.quiet_frames == 0 => {
                // (One picture of the stream fullscreen, a second before moving on.)
                shot(app, "mac-view-fullscreen");
                self.quiet_frames = 1;
            }
            6 if self.waited(3) => {
                self.quiet_frames = 0;
                if !app.call.fullscreen || shown < self.frames_first + 20 {
                    return self.fail(&format!("fullscreen {} with {} new frames", app.call.fullscreen, shown - self.frames_first));
                }
                self.frames_first = shown;
                let ctx = app.repaint_ctx();
                app.set_fullscreen(&ctx, false);
                if let Some(k) = &key {
                    app.pop_out_stream(k, true);
                }
                self.goto(7, "fullscreen left; popped out into its own window");
            }
            7 if self.waited(4) => {
                let popped = key.as_deref().is_some_and(|k| app.stream_popped(k));
                if !popped || shown < self.frames_first + 20 {
                    return self.fail(&format!("pop-out: popped {popped}, {} new frames", shown - self.frames_first));
                }
                self.frames_first = shown;
                if let Some(k) = &key {
                    app.pop_out_stream(k, false);
                }
                self.goto(8, "back inside the app");
            }
            8 if self.waited(3) => {
                let popped = key.as_deref().is_some_and(|k| app.stream_popped(k));
                if popped || shown < self.frames_first + 20 {
                    return self.fail(&format!("back inside: popped {popped}, {} new frames", shown - self.frames_first));
                }
                tracing::info!(target: "autotest", "AUTOTEST mac-view: inline, pinned, fullscreen and pop-out all drew frames ({shown} in total)");
                app.leave_voice();
                self.goto(9, "leaving");
            }
            9 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }
}
