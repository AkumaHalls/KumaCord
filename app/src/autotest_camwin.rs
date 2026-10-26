//! "Configurar câmera" autotest (v0.8.1, role `camwin`, a child module of `autotest`): one
//! process with the synthetic camera (`VOX_FAKE_CAMERA=1`), logged in to a hub (no voice needed).
//! With `VOX_FAKE_CAMERA_LOWLIGHT=1` the synthetic camera also plays a dim room: while its
//! exposure is automatic and its auto-exposure priority allows it, it delivers a third of its
//! frame rate.
//!
//! 1. Open the window: a local preview starts (the call camera is off), the controls and the
//!    formats arrive.
//! 2. Brightness to 200 the way the slider does it: the running preview reports 200, the profile
//!    holds it and survives a settings.json round trip, the dirty flag is flushed.
//! 3. (dim room) The low-light state shows; "Priorizar fluidez" sets the priority control to
//!    "keep the frame rate", the rate comes back and the state clears; the profile remembers it.
//! 4. "Restaurar padrão": brightness and the priority back to the camera's defaults, profile
//!    empty.
//! 5. Close the window: the preview stops (the camera is released).

use std::time::Instant;

use vox_capture::CameraControlId;

use super::Auto;
use crate::app::{App, CameraPreviewUi};
use crate::camera_cfg::SavedControl;

#[derive(Default)]
pub struct State {
    since: Option<Instant>,
    fps_low: f32,
}

fn lowlight_sim() -> bool {
    std::env::var("VOX_FAKE_CAMERA_LOWLIGHT").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn live_value(app: &App, id: CameraControlId) -> Option<(i32, bool)> {
    app.settings_ui.cam.live.as_ref()?.controls.iter().find(|c| c.id == id).map(|c| (c.value, c.auto))
}

fn saved(app: &App, id: CameraControlId) -> Option<SavedControl> {
    app.camera_profile().controls.get(id.key()).copied()
}

impl Auto {
    pub(super) fn run_camwin(&mut self, app: &mut App) -> Option<i32> {
        if !super::camcfg::fake_camera() && self.step == 0 {
            return self.fail("needs VOX_FAKE_CAMERA=1");
        }
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    app.open_camera_window();
                    self.goto(2, "opened Configurar câmera");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if let CameraPreviewUi::Failed(e) = &app.settings_ui.camera_preview {
                    return self.fail(&format!("the preview failed: {e}"));
                }
                let live = app.settings_ui.cam.live.as_ref().is_some_and(|l| !l.from_call && l.controls.len() >= 8);
                if app.camera_window_preview() && live && !app.settings_ui.cam.formats.is_empty() {
                    let l = app.settings_ui.cam.live.as_ref().expect("checked");
                    tracing::info!(
                        target: "autotest",
                        "AUTOTEST camwin: preview on, {} controls, {} formats, groups {:?}",
                        l.controls.len(),
                        app.settings_ui.cam.formats.len(),
                        crate::camera_cfg::group_controls(&l.controls).iter().map(|(g, v)| format!("{}:{}", g.title(), v.len())).collect::<Vec<_>>()
                    );
                    // What the "Brilho" slider does when dragged to 200.
                    app.edit_camera_profile(|p| p.set_control(CameraControlId::Brightness, 200, false));
                    self.goto(3, "brightness -> 200");
                } else if self.waited(20) {
                    return self.fail(&format!(
                        "window not ready: preview={:?} live={} formats={}",
                        app.settings_ui.camera_preview,
                        app.settings_ui.cam.live.is_some(),
                        app.settings_ui.cam.formats.len()
                    ));
                }
            }
            3 => {
                if live_value(app, CameraControlId::Brightness) == Some((200, false)) {
                    if saved(app, CameraControlId::Brightness) != Some(SavedControl { value: 200, auto: false }) {
                        return self.fail("the profile does not hold brightness 200");
                    }
                    let json = serde_json::to_string(&app.settings).expect("settings serialize");
                    let back: crate::settings::Settings = serde_json::from_str(&json).expect("settings load");
                    if back.camera_profiles != app.settings.camera_profiles {
                        return self.fail("the camera profiles do not survive a settings.json round trip");
                    }
                    app.flush_camera_settings(false);
                    if app.settings_ui.cam.dirty {
                        return self.fail("settings.json was not written after the change");
                    }
                    tracing::info!(target: "autotest", "AUTOTEST camwin: brightness 200 applied to the preview and saved");
                    if lowlight_sim() {
                        self.goto(4, "waiting for the dim-room state");
                    } else {
                        tracing::info!(target: "autotest", "AUTOTEST camwin: VOX_FAKE_CAMERA_LOWLIGHT not set; skipping Priorizar fluidez");
                        app.reset_camera_controls();
                        self.goto(6, "Restaurar padrão");
                    }
                } else if self.waited(5) {
                    return self.fail(&format!("brightness not applied: live {:?}", live_value(app, CameraControlId::Brightness)));
                }
            }
            4 => {
                let l = app.settings_ui.cam.live.clone();
                if l.as_ref().is_some_and(|l| l.low_light) {
                    let l = l.expect("checked");
                    self.camwin.fps_low = l.fps;
                    tracing::info!(target: "autotest", "AUTOTEST camwin: low light at {:.1} fps (of {:.0}); pressing Priorizar fluidez", l.fps, l.requested);
                    if crate::camera_cfg::smooth_active(&l.controls, &app.camera_profile()) {
                        return self.fail("Priorizar fluidez already on before pressing it");
                    }
                    app.set_camera_smooth(true);
                    self.camwin.since = Some(Instant::now());
                    self.goto(5, "Priorizar fluidez on");
                } else if self.waited(25) {
                    return self.fail(&format!("no low-light state: live fps {:?}", l.map(|l| l.fps)));
                }
            }
            5 => {
                let Some(l) = app.settings_ui.cam.live.clone() else {
                    return if self.waited(15) { self.fail("the camera stopped reporting") } else { None };
                };
                let priority = live_value(app, CameraControlId::ExposurePriority);
                if priority == Some((0, false)) && !l.low_light && l.fps >= 25.0 {
                    let p = app.camera_profile();
                    if !p.smooth || saved(app, CameraControlId::ExposurePriority) != Some(SavedControl { value: 0, auto: false }) {
                        return self.fail("the profile does not remember Priorizar fluidez");
                    }
                    if !crate::camera_cfg::smooth_active(&l.controls, &p) {
                        return self.fail("Priorizar fluidez does not show as on");
                    }
                    tracing::info!(
                        target: "autotest",
                        "AUTOTEST camwin: Priorizar fluidez brought the camera from {:.1} to {:.1} fps in {:.1} s",
                        self.camwin.fps_low,
                        l.fps,
                        self.camwin.since.map_or(0.0, |t| t.elapsed().as_secs_f32())
                    );
                    app.reset_camera_controls();
                    self.goto(6, "Restaurar padrão");
                } else if self.waited(15) {
                    return self.fail(&format!("frame rate not back: priority {priority:?} fps {:.1} low_light {}", l.fps, l.low_light));
                }
            }
            6 => {
                let b = live_value(app, CameraControlId::Brightness);
                let pr = live_value(app, CameraControlId::ExposurePriority);
                if b == Some((128, false)) && pr == Some((1, false)) {
                    let p = app.camera_profile();
                    if !p.controls.is_empty() || p.smooth {
                        return self.fail("the profile still has adjustments after Restaurar padrão");
                    }
                    tracing::info!(target: "autotest", "AUTOTEST camwin: Restaurar padrão put the camera back");
                    app.close_camera_window();
                    self.goto(7, "closed the window");
                } else if self.waited(5) {
                    return self.fail(&format!("not reset: brightness {b:?} priority {pr:?}"));
                }
            }
            7 => {
                if app.settings_ui.cam.live.is_none() && app.settings_ui.camera_preview == CameraPreviewUi::Off && self.waited(1) {
                    tracing::info!(target: "autotest", "AUTOTEST camwin: preview stopped with the window");
                    return self.pass();
                } else if self.waited(8) {
                    return self.fail("the preview kept running after the window closed");
                }
            }
            _ => {}
        }
        None
    }
}
