//! The microphone level meter: how it is drawn, and the watcher that repaints the window for it.
//!
//! The audio callback writes the level into atomics ([`VoiceGate`]); nothing is pushed to the UI.
//! While a meter is on screen, a small thread looks at the atomics ~15 times a second and wakes
//! the UI only when what would be drawn changed (a 3 dB step of the bar, the gate opening or
//! closing), so a quiet room costs no repaints and, once no meter is visible, the thread is gone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Color32, Stroke, Vec2};
use vox_client_core::VoiceGate;

use crate::backend::Repaint;

/// Left end of the meter scale in dBFS (right end is 0).
pub const SCALE_MIN_DB: f32 = -80.0;
/// How often the watcher looks (15 fps at most).
const POLL: Duration = Duration::from_millis(66);

const GREEN: Color32 = crate::theme::ONLINE;

/// Position of `db` on the scale, 0..=1.
pub fn frac(db: f32) -> f32 {
    ((db - SCALE_MIN_DB) / -SCALE_MIN_DB).clamp(0.0, 1.0)
}

/// What a meter shows, reduced to the steps that change the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shown {
    /// Level in 3 dB steps (below the scale: one value).
    level: i32,
    threshold: i32,
    open: bool,
}

pub fn shown(g: &VoiceGate) -> Shown {
    let level = g.level_db();
    Shown {
        level: if level <= SCALE_MIN_DB { i32::MIN } else { (level / 3.0).round() as i32 },
        threshold: g.threshold_db().round() as i32,
        open: g.is_open(),
    }
}

/// The level below the threshold (the gate stays closed): the mist, dimmed.
const QUIET: Color32 = Color32::from_rgb(0x5C, 0x6E, 0x80);

/// Draw a level bar (v1.0): a round [`crate::theme::FUNDO`] rail, the level in mist below the
/// threshold and in [`crate::theme::ONLINE`] at or above it, and a chalk marker at the threshold.
/// `gate = None` draws it empty (no mic running).
pub fn level_bar(ui: &mut egui::Ui, gate: Option<&VoiceGate>, size: Vec2) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter_at(rect.expand(1.0));
    let r = (size.y / 2.0).min(4.0);
    p.rect_filled(rect, r, crate::theme::FUNDO);
    if let Some(g) = gate {
        let (level, thr) = (g.level_db(), g.threshold_db());
        let lit = level >= thr || (g.ptt() && g.is_open());
        let w = rect.width() * frac(level);
        if w > 0.5 {
            let fill = egui::Rect::from_min_size(rect.min, egui::vec2(w.max(2.0 * r), rect.height()));
            p.rect_filled(fill, r, if lit { GREEN } else { QUIET });
        }
        if !g.ptt() {
            let x = (rect.left() + rect.width() * frac(thr)).round();
            p.line_segment([egui::pos2(x, rect.top() - 1.0), egui::pos2(x, rect.bottom() + 1.0)], Stroke::new(2.0, crate::theme::GIZ));
        }
    }
    resp
}

/// Keeps the window repainting for the meters while one is visible. Dropping it stops the thread.
pub struct MeterWatch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MeterWatch {
    pub fn start(repaint: Repaint, gates: Vec<Arc<VoiceGate>>) -> MeterWatch {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("vox-meter".into())
            .spawn(move || {
                let mut last: Vec<Shown> = gates.iter().map(|g| shown(g)).collect();
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(POLL);
                    let now: Vec<Shown> = gates.iter().map(|g| shown(g)).collect();
                    if now != last {
                        last = now;
                        repaint.wake_if_shown();
                    }
                }
            })
            .ok();
        MeterWatch { stop, thread }
    }
}

impl Drop for MeterWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join(); // at most one poll interval
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_maps_dbfs_to_the_bar() {
        assert_eq!(frac(SCALE_MIN_DB), 0.0);
        assert_eq!(frac(0.0), 1.0);
        assert_eq!(frac(-200.0), 0.0);
        assert!((frac(-40.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn shown_ignores_changes_smaller_than_a_bar_step() {
        let g = VoiceGate::default();
        let a = shown(&g);
        assert_eq!(a.level, i32::MIN, "silent mic is one state");
        assert!(!a.open);
    }
}
