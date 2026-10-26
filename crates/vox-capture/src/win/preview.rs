//! Screen preview tap (v0.3, item 8a): a cheap, low-rate, low-resolution copy of what a running
//! screen capture shows, for the sharer's own tile. Never decodes our encoded stream.
//!
//! It lives inside the capture loop (`super::video::Capture`), on the capture thread and its D3D11
//! device, so it works for the hardware-encoded and the raw (software encoder) sink alike:
//!
//! ```text
//!  converter output (NV12, encode size)        <- already converted for the encoder
//!    -> second video processor: downscale to the tile size (GPU, ~0.1 ms)
//!    -> small staging copy + fence (the same `Readback` ring the raw path and webcam use)
//!    -> callback with a borrowed NV12 frame
//! ```
//!
//! The tap only *reads* the converter's output after the encoder got its frame, at most `fps`
//! times a second (default 5), so the encode path sees no extra latency, no extra waiting and no
//! extra drops. Per tap there is no heap allocation: the processor, the output texture, the input
//! view and the staging ring are all reused (they are rebuilt only when the size changes).
//!
//! A screen that stops changing sends no frames, but the converter's output still holds the last
//! one: a settings change (tile shown again, focus, new size) re-taps it right away, and a frame
//! that arrived too soon after the previous tap is tapped when its time comes (timer), so the
//! preview always ends on the latest picture.
//!
//! If reading the NV12 output ever fails (a driver that cannot take that texture as a processor
//! input), the tap falls back to the BGRA capture frame (no refresh of a static screen then).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::System::Threading::{
    CancelWaitableTimer, CreateWaitableTimerExW, SetWaitableTimer, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
    TIMER_ALL_ACCESS,
};
use windows::core::PCWSTR;

use super::video::{fit, Converter, Readback, Shared, Sink, YuvSpace};
use super::{monotonic_now_us, os, Event, OwnedHandle};
use crate::{CaptureError, FrameCallback, PreviewConfig, Result};

/// Anything closer than this to the next due time counts as due (timer / wake-up granularity).
const SLACK: Duration = Duration::from_millis(2);
/// How long to wait before trying again when the readback ring is full.
const BUSY_RETRY: Duration = Duration::from_millis(8);
/// A preview that stays off this long gives its GPU objects back (the video processor, its
/// output texture, the input view and the staging ring); turning it on builds them again. The
/// wait keeps a menu opening over the tile for a moment from rebuilding everything.
const RELEASE_AFTER: Duration = Duration::from_secs(2);
/// How many preview taps hold GPU objects right now (diagnostics and the autotest).
static HOLDERS: AtomicUsize = AtomicUsize::new(0);

/// Preview taps (of any capture in this process) that currently hold GPU objects.
pub fn gpu_holders() -> usize {
    HOLDERS.load(Ordering::Acquire)
}

/// Consecutive tap failures before the preview gives up for this capture.
const MAX_ERRORS: u32 = 8;

/// The converter's output is BT.709 limited-range NV12 (see `Converter::ensure`).
const NV12_OUT: YuvSpace = YuvSpace { bt709: true, full_range: false };

/// Settings shared between the owner (any thread) and the capture thread.
pub(crate) struct PreviewShared {
    on: AtomicBool,
    fps: AtomicU32,
    max_w: AtomicU32,
    max_h: AtomicU32,
    /// Auto-reset event the capture thread waits on: the settings changed.
    wake: Event,
    delivered: AtomicU64,
    /// The tap holds GPU objects (processor, textures, staging ring) right now.
    resident: AtomicBool,
}

fn clamp_cfg(c: PreviewConfig) -> PreviewConfig {
    PreviewConfig {
        on: c.on,
        fps: c.fps.clamp(1, 30),
        max_width: (c.max_width.clamp(32, 1920)) & !1,
        max_height: (c.max_height.clamp(18, 1080)) & !1,
    }
}

impl PreviewShared {
    fn new() -> Result<Arc<Self>> {
        let d = PreviewConfig::default();
        Ok(Arc::new(Self {
            on: AtomicBool::new(d.on),
            fps: AtomicU32::new(d.fps),
            max_w: AtomicU32::new(d.max_width),
            max_h: AtomicU32::new(d.max_height),
            wake: Event::new(false)?,
            delivered: AtomicU64::new(0),
            resident: AtomicBool::new(false),
        }))
    }

    fn set_resident(&self, on: bool) {
        if self.resident.swap(on, Ordering::AcqRel) != on {
            if on {
                HOLDERS.fetch_add(1, Ordering::AcqRel);
            } else {
                HOLDERS.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    fn config(&self) -> PreviewConfig {
        PreviewConfig {
            on: self.on.load(Ordering::Relaxed),
            fps: self.fps.load(Ordering::Relaxed),
            max_width: self.max_w.load(Ordering::Relaxed),
            max_height: self.max_h.load(Ordering::Relaxed),
        }
    }
}

/// Owner-side handle (see `crate::ScreenPreview`).
#[derive(Clone)]
pub struct PreviewHandle(pub(crate) Arc<PreviewShared>);

impl PreviewHandle {
    pub fn new() -> Result<Self> {
        PreviewShared::new().map(Self)
    }

    pub fn set(&self, cfg: PreviewConfig) {
        let cfg = clamp_cfg(cfg);
        if self.0.config() == cfg {
            return;
        }
        self.0.fps.store(cfg.fps, Ordering::Relaxed);
        self.0.max_w.store(cfg.max_width, Ordering::Relaxed);
        self.0.max_h.store(cfg.max_height, Ordering::Relaxed);
        self.0.on.store(cfg.on, Ordering::Release);
        self.0.wake.set();
    }

    pub fn config(&self) -> PreviewConfig {
        self.0.config()
    }

    pub fn frames(&self) -> u64 {
        self.0.delivered.load(Ordering::Relaxed)
    }

    /// Does the tap hold GPU objects (a second video processor, textures) right now?
    pub fn holds_gpu(&self) -> bool {
        self.0.resident.load(Ordering::Acquire)
    }
}

/// What `start_video*` gets: the settings handle and where the frames go.
pub(crate) struct PreviewAttach {
    pub sh: Arc<PreviewShared>,
    pub cb: FrameCallback,
}

impl PreviewAttach {
    pub(crate) fn new(handle: &PreviewHandle, cb: FrameCallback) -> Self {
        Self { sh: handle.0.clone(), cb }
    }
}

/// The converter's NV12 output (encode size).
pub(crate) struct Main<'a> {
    pub tex: &'a ID3D11Texture2D,
    pub w: u32,
    pub h: u32,
}

/// The captured BGRA frame (fallback source).
pub(crate) struct Bgra<'a> {
    pub tex: &'a ID3D11Texture2D,
    pub tex_w: u32,
    pub tex_h: u32,
    /// Content size inside the texture.
    pub w: u32,
    pub h: u32,
}

pub(crate) struct PreviewTap {
    sh: Arc<PreviewShared>,
    conv: Converter,
    rb: Readback,
    rb_stats: Shared,
    timer: OwnedHandle,
    /// Settings seen by the last service pass.
    cfg: PreviewConfig,
    /// The converter output changed since the last tap.
    dirty: bool,
    last_tap: Option<Instant>,
    /// Tapping the BGRA capture frame because the NV12 output could not be read.
    from_bgra: bool,
    errors: u32,
    dead: bool,
    taps: u64,
    last_stats: (Instant, u64),
    /// Since when the preview has been off (and still holds GPU objects).
    off_since: Option<Instant>,
}

impl Drop for PreviewTap {
    fn drop(&mut self) {
        self.sh.set_resident(false);
    }
}

impl PreviewTap {
    pub(crate) fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext, a: PreviewAttach) -> Result<Self> {
        let PreviewAttach { sh, mut cb } = a;
        let user_sh = sh.clone();
        // Frames still in the ring when the preview is switched off are not delivered.
        let wrapped: FrameCallback = Box::new(move |f| {
            if user_sh.on.load(Ordering::Acquire) {
                user_sh.delivered.fetch_add(1, Ordering::Relaxed);
                cb(f);
            }
        });
        let mut conv = Converter::new(device, ctx)?;
        conv.set_cache_input(true);
        conv.set_input_yuv(NV12_OUT);
        let timer = unsafe {
            CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
                .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
                .map_err(|e| os("CreateWaitableTimerExW(preview)", e))?
        };
        Ok(Self {
            cfg: sh.config(),
            sh,
            conv,
            rb: Readback::new(device, ctx, wrapped)?,
            rb_stats: Shared::default(),
            timer: OwnedHandle(timer),
            dirty: false,
            last_tap: None,
            from_bgra: false,
            errors: 0,
            dead: false,
            taps: 0,
            last_stats: (Instant::now(), 0),
            off_since: None,
        })
    }

    /// Handles for the capture loop's wait: readback done, settings changed, pacing timer.
    pub(crate) fn handles(&self) -> [HANDLE; 3] {
        [Sink::event(&self.rb), self.sh.wake.raw(), self.timer.0]
    }

    pub(crate) fn on_readback(&mut self, ctx: &ID3D11DeviceContext) {
        self.drain(ctx);
        self.maybe_release(ctx);
    }

    /// Off for [`RELEASE_AFTER`] and nothing in flight: give the GPU objects back. Otherwise arm
    /// the timer for the moment it is due.
    fn maybe_release(&mut self, ctx: &ID3D11DeviceContext) {
        if self.cfg.on || !self.sh.resident.load(Ordering::Relaxed) {
            self.off_since = None;
            return;
        }
        let since = *self.off_since.get_or_insert_with(Instant::now);
        let waited = since.elapsed();
        if waited < RELEASE_AFTER {
            self.arm_timer(RELEASE_AFTER - waited);
            return;
        }
        if !self.rb.release() {
            // A copy is still in flight: the readback event comes back here.
            return;
        }
        self.conv.release();
        // Destroyed objects are only freed once the context lets go of them.
        unsafe { ctx.Flush() };
        self.sh.set_resident(false);
        self.off_since = None;
        self.dirty = false;
        tracing::info!("screen preview off: GPU objects released");
    }

    /// The settings changed (or the owner asked for a refresh).
    pub(crate) fn on_wake(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, main: Option<Main<'_>>) {
        let new = self.sh.config();
        let old = std::mem::replace(&mut self.cfg, new);
        if new.on && (!old.on || (new.max_width, new.max_height) != (old.max_width, old.max_height)) {
            // Shown again or resized: show the current picture now, whatever the pace.
            self.last_tap = None;
            self.dirty = true;
        } else if new.on && new.fps != old.fps {
            self.dirty = true;
        }
        self.service(device, ctx, main, None);
    }

    pub(crate) fn on_timer(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, main: Option<Main<'_>>) {
        self.service(device, ctx, main, None);
    }

    /// A new picture was converted for the encoder.
    pub(crate) fn on_frame(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, main: Main<'_>, bgra: Bgra<'_>) {
        self.dirty = true;
        self.service(device, ctx, Some(main), Some(bgra));
    }

    fn drain(&mut self, ctx: &ID3D11DeviceContext) {
        if let Err(e) = Sink::drain(&mut self.rb, ctx, &self.rb_stats) {
            self.fail("preview readback", &e);
        }
    }

    fn fail(&mut self, what: &str, e: &CaptureError) {
        self.errors += 1;
        if self.errors == 1 || self.errors == MAX_ERRORS {
            tracing::warn!(error = %e, count = self.errors, "{what} failed");
        }
        if self.errors >= MAX_ERRORS {
            tracing::warn!("screen preview switched off for this capture after repeated failures");
            self.dead = true;
            self.cancel_timer();
        }
    }

    fn arm_timer(&self, after: Duration) {
        let due: i64 = -((after.as_micros() as i64).max(500) * 10); // relative, 100 ns units
        // SAFETY: a valid timer handle owned by `self`.
        let _ = unsafe { SetWaitableTimer(self.timer.0, &due, 0, None, None, false) };
    }

    fn cancel_timer(&self) {
        // SAFETY: a valid timer handle owned by `self`.
        let _ = unsafe { CancelWaitableTimer(self.timer.0) };
    }

    /// Tap now if the preview is on, something changed and the pace allows; else arrange to be
    /// called again when it does.
    fn service(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, main: Option<Main<'_>>, bgra: Option<Bgra<'_>>) {
        if self.dead {
            return;
        }
        // Without a fence (old drivers) this is also where finished copies are picked up.
        self.drain(ctx);
        if !self.cfg.on {
            self.dirty = false;
            self.cancel_timer();
            self.maybe_release(ctx);
            return;
        }
        self.off_since = None;
        if !self.dirty {
            return;
        }
        let interval = Duration::from_micros(1_000_000 / self.cfg.fps.max(1) as u64);
        let due_in = match self.last_tap {
            None => Duration::ZERO,
            Some(t) => interval.saturating_sub(t.elapsed()),
        };
        if due_in > SLACK {
            self.arm_timer(due_in);
            return;
        }
        if !Sink::has_free(&self.rb) {
            self.arm_timer(BUSY_RETRY);
            return;
        }
        let ts = monotonic_now_us();
        self.sh.set_resident(true);
        let res = if self.from_bgra {
            match bgra {
                Some(b) => self.tap(device, ctx, b.tex, (b.tex_w, b.tex_h), (b.w, b.h), None, ts),
                None => return, // only a captured frame can be tapped in this mode
            }
        } else {
            match main {
                Some(m) => self.tap(device, ctx, m.tex, (m.w, m.h), (m.w, m.h), Some(NV12_OUT), ts),
                None => return, // nothing converted yet
            }
        };
        match res {
            Ok(()) => {
                self.errors = 0;
                self.dirty = false;
                self.last_tap = Some(Instant::now());
                self.taps += 1;
            }
            Err(e) if !self.from_bgra => {
                // Most likely this GPU cannot read the NV12 output as a processor input.
                tracing::warn!(error = %e, "preview: tapping the NV12 output failed; using the captured frame instead");
                self.from_bgra = true;
                self.conv.set_input_rgb();
                self.fail("preview tap", &e);
            }
            Err(e) => self.fail("preview tap", &e),
        }
    }

    /// Downscale `src` to the preview size and queue its readback.
    #[allow(clippy::too_many_arguments)]
    fn tap(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        tex_size: (u32, u32),
        content: (u32, u32),
        yuv: Option<YuvSpace>,
        ts_us: i64,
    ) -> Result<()> {
        if content.0 < 2 || content.1 < 2 {
            return Ok(());
        }
        let (pw, ph) = fit(content.0, content.1, self.cfg.max_width, self.cfg.max_height);
        match yuv {
            Some(y) => self.conv.set_input_yuv(y),
            None => self.conv.set_input_rgb(),
        }
        self.conv.ensure(device, tex_size.0, tex_size.1, pw, ph)?;
        let out = self.conv.convert_slice(src, 0, content.0, content.1)?.clone();
        Sink::submit(&mut self.rb, ctx, &out, pw, ph, ts_us)
    }

    pub(crate) fn log_stats(&mut self) {
        let (t0, n0) = self.last_stats;
        let n = self.taps;
        self.last_stats = (Instant::now(), n);
        if !self.cfg.on {
            return;
        }
        let secs = t0.elapsed().as_secs_f64().max(0.001);
        tracing::info!(
            fps = format!("{:.1}", (n - n0) as f64 / secs),
            target_fps = self.cfg.fps,
            max = format!("{}x{}", self.cfg.max_width, self.cfg.max_height),
            from_bgra = self.from_bgra,
            "screen preview stats"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_clamped_and_shared() {
        let c = clamp_cfg(PreviewConfig { on: true, fps: 500, max_width: 99_999, max_height: 3 });
        assert_eq!((c.fps, c.max_width, c.max_height), (30, 1920, 18));
        let c = clamp_cfg(PreviewConfig { on: true, fps: 0, max_width: 641, max_height: 361 });
        assert_eq!((c.fps, c.max_width, c.max_height), (1, 640, 360));

        let h = PreviewHandle::new().unwrap();
        assert_eq!(h.config(), PreviewConfig::default());
        assert!(!h.config().on);
        let want = PreviewConfig { on: true, fps: 15, max_width: 960, max_height: 540 };
        h.set(want);
        assert_eq!(h.clone().config(), want);
        assert_eq!(h.frames(), 0);
    }
}
