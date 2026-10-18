//! The sharer's own preview (v0.3, item 8a): while we are live, our stream tile in the call view
//! shows a small, low-rate picture of what we share, so we can check that the right window is on.
//!
//! The picture comes from the capture itself (`vox_capture::ScreenPreview`: a GPU downscale of the
//! frame the encoder just got, a small readback, at most a few frames a second); our own encoded
//! stream is never decoded and our own screen-share audio is never subscribed (we are not a
//! remote participant of our own room). Frames go to a local viewer tile ([`SELF_SHARE_KEY`]) that
//! the app places in the call view.
//!
//! A source switch ("Trocar tela") runs a second capture next to the first for a moment; only the
//! capture that owns the stream shows its frames (`active` generation), the retired one is turned
//! off.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use vox_capture::{FrameCallback, PreviewConfig, ScreenPreview};

use super::viewer::ViewerStream;
use crate::stats::StreamCounters;

/// Viewer key of our own Go Live preview tile.
pub const SELF_SHARE_KEY: &str = "live:self";

struct State {
    sink: ViewerStream,
    counters: Arc<StreamCounters>,
    cfg: Mutex<PreviewConfig>,
    /// Generation of the capture whose frames are shown.
    active: AtomicU64,
    next: AtomicU64,
}

/// Where a share's preview frames go and how the preview is set (shared by the share's captures).
#[derive(Clone)]
pub struct SharePreview(Arc<State>);

impl SharePreview {
    /// `sink` is the tile [`SELF_SHARE_KEY`]; `counters` count the frames shown.
    pub fn new(sink: ViewerStream, counters: Arc<StreamCounters>, cfg: PreviewConfig) -> Self {
        Self(Arc::new(State { sink, counters, cfg: Mutex::new(cfg), active: AtomicU64::new(0), next: AtomicU64::new(1) }))
    }

    pub fn config(&self) -> PreviewConfig {
        *self.0.cfg.lock()
    }

    pub fn counters(&self) -> Arc<StreamCounters> {
        self.0.counters.clone()
    }

    pub(crate) fn set_config(&self, cfg: PreviewConfig) {
        *self.0.cfg.lock() = cfg;
    }

    /// The tap of one new capture: off until [`CaptureTap::activate`].
    pub(crate) fn attach(&self) -> Option<CaptureTap> {
        let sp = match ScreenPreview::new() {
            Ok(sp) => sp,
            Err(e) => {
                tracing::warn!(error = %e, "screen preview unavailable");
                return None;
            }
        };
        let gen = self.0.next.fetch_add(1, Ordering::Relaxed);
        Some(CaptureTap { sp, gen, owner: self.clone() })
    }
}

/// One capture's preview tap.
pub(crate) struct CaptureTap {
    pub(crate) sp: ScreenPreview,
    gen: u64,
    owner: SharePreview,
}

impl CaptureTap {
    /// The callback for `vox_capture::start_video*_with_preview`.
    pub(crate) fn callback(&self) -> FrameCallback {
        let (st, gen) = (self.owner.0.clone(), self.gen);
        Box::new(move |f| {
            // A retired capture may still deliver a frame or two while it shuts down.
            if st.active.load(Ordering::Acquire) == gen {
                st.counters.frame(f.width, f.height);
                st.sink.submit_nv12(f);
            }
        })
    }

    /// This capture now owns the stream: show its frames, with the share's current settings.
    pub(crate) fn activate(&self) {
        self.owner.0.active.store(self.gen, Ordering::Release);
        self.sp.set(self.owner.config());
    }

    /// This capture was retired: no more frames, no more GPU work.
    pub(crate) fn deactivate(&self) {
        let mut c = self.sp.config();
        c.on = false;
        self.sp.set(c);
    }

    pub(crate) fn apply(&self, cfg: PreviewConfig) {
        if self.owner.0.active.load(Ordering::Acquire) == self.gen {
            self.sp.set(cfg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn frame_of(cb: &mut FrameCallback) {
        let (y, uv) = (vec![0u8; 16 * 16], vec![128u8; 16 * 8]);
        cb(&vox_capture::Nv12Frame { width: 16, height: 16, timestamp_us: 0, y: &y, y_stride: 16, uv: &uv, uv_stride: 16 });
    }

    #[test]
    fn only_the_active_capture_shows_frames() {
        let counters = Arc::new(StreamCounters::default());
        let p = SharePreview::new(ViewerStream::null(), counters.clone(), PreviewConfig::default());
        let (old, new) = (p.attach().unwrap(), p.attach().unwrap());
        let (mut cb_old, mut cb_new) = (old.callback(), new.callback());
        // Nobody owns the stream yet.
        frame_of(&mut cb_old);
        assert_eq!(counters.frames.load(Ordering::Relaxed), 0);
        old.activate();
        frame_of(&mut cb_old);
        frame_of(&mut cb_new);
        assert_eq!(counters.frames.load(Ordering::Relaxed), 1);
        // A source switch hands the stream over; the retired capture is quiet and switched off.
        p.set_config(PreviewConfig { on: true, fps: 15, max_width: 960, max_height: 540 });
        new.activate();
        old.deactivate();
        frame_of(&mut cb_old);
        frame_of(&mut cb_new);
        assert_eq!(counters.frames.load(Ordering::Relaxed), 2);
        assert!(!old.sp.config().on);
        assert_eq!(new.sp.config(), p.config());
        // Settings changes reach only the capture that owns the stream.
        let cfg = PreviewConfig { on: false, ..p.config() };
        old.apply(cfg);
        assert!(!old.sp.config().on);
        new.apply(cfg);
        assert_eq!(new.sp.config(), cfg);
    }
}
