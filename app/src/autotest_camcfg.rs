//! Webcam configuration autotest (a child module of `autotest`, part of the `cam-a` / `cam-b`
//! run with the synthetic camera, `VOX_FAKE_CAMERA=1`; B also needs `VOX_CAM_PROBE=1`).
//!
//! The synthetic picture has a dark box in its top-left corner. B samples the corners of the
//! frames it decodes from A's camera (`golive::camera::camera_probe`), so it can tell the
//! picture's orientation. B mutes itself when it is ready; A then goes through the settings the
//! way the Settings window does (`App::edit_camera_profile`), a few seconds each:
//!
//! | step | A changes                         | B must see (landscape L / portrait P, dark corner) |
//! |------|-----------------------------------|-----------------------------------------------------|
//! | 0    | nothing (mirror only in my view)  | L-TL: others get the un-mirrored picture             |
//! | 1    | "Espelhar para os outros"         | L-TR                                                 |
//! | 2    | mirror off, rotate 90             | P-TR (the frame size changed to portrait)            |
//! | 3    | rotate 180                        | L-BR                                                 |
//! | 4    | rotate 0, zoom 2x                 | L-none (the box is cropped away; same frame size)    |
//! | 5    | zoom 1x                           | L-TL again                                           |
//!
//! B passes when it saw those states in that order (other states in between are the changes
//! in flight). (That the local preview is mirrored on the GPU while the others get the plain
//! picture is checked by `vox-capture`'s `synthetic_camera_mirror_rotate_zoom_on_the_gpu`.)

use std::time::{Duration, Instant};

use super::Auto;
use crate::app::App;

/// How long A keeps each setting: the probe's window is 1.5 s, plus the encode / network delay.
const PHASE: Duration = Duration::from_secs(8);
/// B gives up on the whole sequence after this.
const B_LIMIT: Duration = Duration::from_secs(120);

const EXPECTED: [&str; 6] = ["L-TL", "L-TR", "P-TR", "L-BR", "L-none", "L-TL"];

#[derive(Default)]
pub struct State {
    /// A: the phase being shown and since when.
    a_phase: Option<(usize, Instant)>,
    /// B: when it asked A to start, and the states it has seen, collapsed.
    b_started: Option<Instant>,
    b_seen: Vec<String>,
    b_sizes: Vec<(u32, u32)>,
}

pub fn fake_camera() -> bool {
    std::env::var("VOX_FAKE_CAMERA").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn apply(app: &mut App, phase: usize) {
    app.edit_camera_profile(|p| match phase {
        1 => p.mirror_others = true,
        2 => {
            p.mirror_others = false;
            p.rotation = 90;
        }
        3 => p.rotation = 180,
        4 => {
            p.rotation = 0;
            p.zoom_pct = 200;
        }
        5 => p.zoom_pct = 100,
        _ => {}
    });
}

impl Auto {
    /// A: while holding the camera, change its settings once B is ready.
    pub(super) fn camcfg_a(&mut self, app: &mut App) {
        let me = app.my_id();
        let b_ready = app.members_here().iter().any(|m| Some(m.user.id) != me && m.muted && !m.deafened);
        match self.camcfg.a_phase {
            None if b_ready => {
                tracing::info!(target: "autotest", "AUTOTEST A camera config: B is ready; phase 0 (default picture)");
                self.camcfg.a_phase = Some((0, Instant::now()));
            }
            Some((i, at)) if at.elapsed() >= PHASE && i < EXPECTED.len() - 1 => {
                apply(app, i + 1);
                let p = app.camera_profile();
                tracing::info!(
                    target: "autotest",
                    "AUTOTEST A camera config: phase {} (mirror others={} rotation={} zoom={}%, preview mirror={})",
                    i + 1, p.mirror_others, p.rotation, p.zoom_pct, p.mirror_preview
                );
                self.camcfg.a_phase = Some((i + 1, Instant::now()));
            }
            _ => {}
        }
    }

    /// B: watch A's picture. Starts by muting itself (A's cue). `Some(Ok)` when the sequence was
    /// seen, `Some(Err)` on timeout.
    pub(super) fn camcfg_b(&mut self, app: &mut App) -> Option<Result<(), String>> {
        let _ = app;
        let started = *self.camcfg.b_started.get_or_insert_with(Instant::now);
        let id = self.cam_of.clone()?;
        if let Some(p) = vox_client_core::golive::camera::camera_probe(&id) {
            if p.settled() {
                let label = p.label();
                if self.camcfg.b_seen.last() != Some(&label) {
                    tracing::info!(
                        target: "autotest",
                        "AUTOTEST B camera config: A's picture is now {label} ({}x{}, dark corners {:?}) at +{:.0}s",
                        p.width, p.height, p.dark, started.elapsed().as_secs_f32()
                    );
                    self.camcfg.b_seen.push(label);
                    self.camcfg.b_sizes.push((p.width, p.height));
                }
            }
        }
        // The expected states, in order (anything else in between is a change in flight).
        let mut next = 0;
        for s in &self.camcfg.b_seen {
            if next < EXPECTED.len() && s == EXPECTED[next] {
                next += 1;
            }
        }
        if next == EXPECTED.len() {
            let sizes: Vec<String> = self.camcfg.b_sizes.iter().map(|(w, h)| format!("{w}x{h}")).collect();
            tracing::info!(target: "autotest", "AUTOTEST B camera config: saw {:?} (sizes {sizes:?})", self.camcfg.b_seen);
            return Some(Ok(()));
        }
        if started.elapsed() > B_LIMIT {
            return Some(Err(format!(
                "A's picture never went through {:?}; B saw {:?} (matched {next} of {})",
                EXPECTED,
                self.camcfg.b_seen,
                EXPECTED.len()
            )));
        }
        None
    }
}
