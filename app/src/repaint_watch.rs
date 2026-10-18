//! The continuous-repaint watch (v1.0, "leveza": nothing repaints without a reason).
//!
//! Counts the frames per two-second window. A window with more than [`BUSY_FPS`] frames a second
//! that the person did not cause (no input in them) is "busy": during the next window egui's own
//! record of who asked for each repaint (`Context::repaint_causes`, file and line of the
//! `request_repaint` call) is collected and, when that window is busy too (a loop, not a burst),
//! the busiest callers go to the log ("continuous repaint", every 10 s at most). So a repaint loop names itself in the log of whoever hits it, and the autotests can
//! check that an idle screen stays idle (`snapshot`).
//!
//! The autotests' own tick (a frame every 100 ms while a role is still doing something) is not a
//! loop: the role declares it ([`set_expected_fps`]) and the threshold rises by that much, and its
//! call site is left out of the names. Video does not count either, because it never asks egui
//! for a frame: watched streams and cameras present in their own native child windows (v1.0 phase
//! 4 measured 1 to 4 backend wake-ups per 2 s while watching a 30 fps stream).
//!
//! Cheap: a few integers per frame; the causes are only collected while a loop is suspected.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use eframe::egui;

/// The length of a counting window.
const WINDOW: Duration = Duration::from_secs(2);
/// More frames a second than this, none of them from input, is a busy window (above the autotests'
/// own 10 a second, below a meter's 15 and the 36 of the loop seen in phase 2).
pub const BUSY_FPS: f32 = 12.0;
/// At most one log line per this (a loop that lasts gets a line every 10 s, so a 20 s measurement
/// sees it).
const REPORT_EVERY: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Watch {
    start: Option<Instant>,
    frames: u32,
    input_frames: u32,
    /// The previous window was busy: who asks for the repaints is being collected.
    collecting: bool,
    causes: HashMap<String, u32>,
    last_report: Option<Instant>,
    busy_windows: u64,
    last_causes: Vec<(String, u32)>,
    /// Frames a second that something declared on purpose (the autotests' tick).
    expected_fps: f32,
}

/// What the watch saw so far (for the autotests).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Busy windows since the app started.
    pub busy_windows: u64,
    /// The callers of the last busy window, most frequent first.
    pub last_causes: Vec<(String, u32)>,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

/// Call once per frame (from `logic`).
pub fn frame(ctx: &egui::Context) {
    let input = ctx.input(|i| !i.events.is_empty() || i.pointer.is_moving() || i.pointer.any_down());
    let collecting = WATCH.lock().map(|w| w.as_ref().is_some_and(|w| w.collecting)).unwrap_or(false);
    // (Outside the lock: egui's own lock is taken here.)
    let causes = if collecting && !input { ctx.repaint_causes() } else { Vec::new() };
    let Ok(mut guard) = WATCH.lock() else { return };
    let w = guard.get_or_insert_with(Watch::default);
    w.tick(Instant::now(), input, causes.iter().map(|c| c.to_string()));
}

/// Frames a second the caller asks for on purpose from now on (the autotests' 100 ms tick: 10;
/// 0 when it stops). They are added to [`BUSY_FPS`].
pub fn set_expected_fps(fps: f32) {
    if let Ok(mut guard) = WATCH.lock() {
        guard.get_or_insert_with(Watch::default).expected_fps = fps.max(0.0);
    }
}

/// The autotests' tick, never named as a cause (it is declared with [`set_expected_fps`]).
fn declared(cause: &str) -> bool {
    cause.contains("autotest")
}

/// What the watch saw so far.
pub fn snapshot() -> Snapshot {
    WATCH.lock().ok().and_then(|g| g.as_ref().map(|w| Snapshot { busy_windows: w.busy_windows, last_causes: w.last_causes.clone() })).unwrap_or_default()
}

impl Watch {
    fn tick(&mut self, now: Instant, input: bool, causes: impl Iterator<Item = String>) {
        for c in causes.filter(|c| !declared(c)) {
            *self.causes.entry(c).or_default() += 1;
        }
        let start = *self.start.get_or_insert(now);
        self.frames += 1;
        if input {
            self.input_frames += 1;
        }
        let elapsed = now.duration_since(start);
        if elapsed < WINDOW {
            return;
        }
        let idle = self.frames.saturating_sub(self.input_frames);
        let busy = idle as f32 / elapsed.as_secs_f32() > BUSY_FPS + self.expected_fps;
        // A loop is two busy windows in a row: the first one starts collecting, the second one names
        // the callers (a burst, like a menu opening, is never reported).
        if busy && self.collecting && !self.causes.is_empty() {
            let mut top: Vec<(String, u32)> = self.causes.drain().collect();
            top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            top.truncate(6);
            if self.last_report.is_none_or(|t| now.duration_since(t) >= REPORT_EVERY) {
                self.last_report = Some(now);
                tracing::warn!(frames = self.frames, without_input = idle, secs = format!("{:.1}", elapsed.as_secs_f32()), causes = ?top, "continuous repaint");
            }
            self.last_causes = top;
        }
        self.causes.clear();
        if busy {
            self.busy_windows += 1;
        }
        self.collecting = busy;
        self.start = Some(now);
        self.frames = 0;
        self.input_frames = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(w: &mut Watch, t0: Instant, fps: u32, secs: u32, input: bool, cause: &str) -> Instant {
        let mut t = t0;
        let step = Duration::from_secs(1) / fps;
        for _ in 0..fps * secs {
            t += step;
            w.tick(t, input, std::iter::once(cause.to_string()).filter(|c| !c.is_empty()));
        }
        t
    }

    #[test]
    fn an_idle_screen_is_quiet() {
        let mut w = Watch::default();
        let t = run(&mut w, Instant::now(), 2, 10, false, "");
        // A frame now and then (a clock, a status line): never busy.
        assert_eq!(w.busy_windows, 0);
        assert!(!w.collecting);
        let _ = t;
    }

    #[test]
    fn input_is_not_a_loop() {
        let mut w = Watch::default();
        run(&mut w, Instant::now(), 60, 6, true, "egui/scroll.rs:1");
        assert_eq!(w.busy_windows, 0, "moving the mouse repaints, and that is fine");
    }

    #[test]
    fn a_burst_is_not_a_loop() {
        let mut w = Watch::default();
        let t = run(&mut w, Instant::now(), 30, 2, false, "app/src/menu.rs:1 ");
        run(&mut w, t, 1, 10, false, "app/src/menu.rs:1 ");
        assert!(w.last_causes.is_empty(), "one busy window, then quiet: nothing named");
    }

    #[test]
    fn a_declared_tick_is_not_a_loop() {
        // The autotests' 10 a second plus a few backend wake-ups (watching a stream): quiet.
        let mut w = Watch { expected_fps: 10.0, ..Default::default() };
        run(&mut w, Instant::now(), 14, 8, false, "app/src/autotest.rs:1429 ");
        assert_eq!(w.busy_windows, 0);
        assert!(w.last_causes.is_empty());
        // A real loop on top of the tick is still caught, and named without the tick.
        let mut w = Watch { expected_fps: 10.0, ..Default::default() };
        let t = Instant::now();
        let step = Duration::from_secs(1) / 40;
        for i in 0..40 * 7 {
            let cause = if i % 4 == 0 { "app/src/autotest.rs:1429 " } else { "app/src/x.rs:10 " };
            w.tick(t + step * (i + 1), false, std::iter::once(cause.to_string()));
        }
        assert!(w.busy_windows >= 2);
        assert_eq!(w.last_causes.first().map(|c| c.0.as_str()), Some("app/src/x.rs:10 "));
        assert!(w.last_causes.iter().all(|c| !c.0.contains("autotest")));
    }

    #[test]
    fn a_repaint_loop_is_counted_and_named() {
        let mut w = Watch::default();
        run(&mut w, Instant::now(), 36, 7, false, "app/src/x.rs:10 ");
        assert!(w.busy_windows >= 2);
        assert_eq!(w.last_causes.first().map(|c| c.0.as_str()), Some("app/src/x.rs:10 "));
        assert!(w.last_causes[0].1 > 30);
    }
}
