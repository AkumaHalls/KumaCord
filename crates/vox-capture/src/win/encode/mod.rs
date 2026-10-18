//! Hardware H.264 encoding: a [`Sink`] for the shared WGC capture loop that feeds the converter's
//! NV12 texture to NVENC or a Media Foundation hardware MFT on the same D3D11 device, and hands
//! Annex-B access units to the caller. No CPU readback, no CPU colour conversion.
//!
//! Latency/CPU rules: at most one frame waits in the encoder (frames arriving while it is busy
//! are dropped before any GPU work), the capture thread never blocks on the GPU or the encoder
//! (completion is an event in the capture loop's `WaitForMultipleObjects`), no per-frame logs.

mod mf;
pub(crate) use mf::mf_startup;
mod nvenc;
mod nvenc_sys;
pub(crate) mod policy;
mod selftest;
pub use selftest::self_test_encoders;
pub(crate) mod tuning;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use policy::{bitrate_change_significant, FpsPolicy, KeyframeGate, RateAdapter};
pub(crate) use tuning::Tuning;
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext, ID3D11Texture2D};

use super::video::{self, Shared, Sink, SinkCtx};
use super::{monotonic_now_us, ComInit, Event};
use crate::{
    CaptureError, DegradationPreference, EncodeConfig, EncodedCallback, EncodedFrame, EncodedStats,
    EncoderBackend, EncoderControl, IdrGate, Result, VideoTarget,
};

pub(crate) const VENDOR_NVIDIA: u32 = 0x10DE;
const MIN_BITRATE: u32 = 100_000;
pub(crate) const MAX_BITRATE: u32 = 80_000_000;
/// Frames smaller than this are not encoded (dropped): below every HW encoder's minimum.
const MIN_ENCODE_W: u32 = 128;
const MIN_ENCODE_H: u32 = 64;
/// An encoder that produces nothing for this long while holding a frame is considered hung.
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
/// Window for the measured-bitrate statistic.
const BITRATE_WINDOW_US: i64 = 1_000_000;

fn clamp_bitrate(bps: u32) -> u32 {
    bps.clamp(MIN_BITRATE, MAX_BITRATE)
}

/// Parameters a backend is (re)initialised with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EncParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Largest size this session will be asked to encode (NVENC reconfigure limit).
    pub max_width: u32,
    pub max_height: u32,
    pub tuning: Tuning,
}

/// One hardware encoder backend. Owned by the capture thread.
pub(crate) trait HwEncoder {
    fn backend(&self) -> EncoderBackend;
    /// Human-readable encoder name for logs.
    fn name(&self) -> String;
    /// Can a frame be submitted right now?
    fn ready(&self) -> bool;
    /// Frames submitted whose output has not been delivered yet.
    fn in_flight(&self) -> usize;
    /// Queue `tex` (NV12, current size). `force_idr` makes it an IDR with SPS/PPS in front.
    fn encode(
        &mut self,
        ctx: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        ts_us: i64,
        force_idr: bool,
    ) -> Result<()>;
    /// New frame size. In-flight frames are discarded; the next frame is an IDR.
    fn resize(&mut self, width: u32, height: u32) -> Result<()>;
    fn set_bitrate(&mut self, bps: u32) -> Result<()>;
    /// Deliver finished access units (Annex-B, SPS/PPS in front of IDRs) with their capture
    /// timestamps. Never blocks.
    fn poll(&mut self, out: &mut dyn FnMut(&[u8], i64)) -> Result<()>;
}

// ---------------------------------------------------------------------------------------------
// Annex-B helpers
// ---------------------------------------------------------------------------------------------

/// NAL unit types in an Annex-B buffer, as (offset of the NAL header byte, nal_unit_type).
pub(crate) fn nal_units(data: &[u8]) -> impl Iterator<Item = (usize, u8)> + '_ {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let hdr = i + 3;
                i = hdr;
                if hdr < data.len() {
                    return Some((hdr, data[hdr] & 0x1f));
                }
                return None;
            }
            i += 1;
        }
        None
    })
}

pub(crate) fn is_annexb(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
}

/// Does this access unit contain an IDR slice?
pub(crate) fn has_idr(data: &[u8]) -> bool {
    nal_units(data).any(|(_, t)| t == 5)
}

// ---------------------------------------------------------------------------------------------
// Sink
// ---------------------------------------------------------------------------------------------

/// Encoder-specific counters shared with the public handle.
#[derive(Default)]
pub(crate) struct EncShared {
    keyframes: AtomicU64,
    bytes: AtomicU64,
    /// Measured output bitrate over the last window.
    bitrate_bps: AtomicU32,
    bitrate_at_us: AtomicI64,
    backend: AtomicU8,
    target_bps: AtomicU32,
    encoder_bps: AtomicU32,
    fps_cap: AtomicU32,
    coalesced: AtomicU64,
}

fn backend_to_u8(b: EncoderBackend) -> u8 {
    match b {
        EncoderBackend::Auto => 0,
        EncoderBackend::Nvenc => 1,
        EncoderBackend::MediaFoundation => 2,
    }
}

fn backend_from_u8(v: u8) -> EncoderBackend {
    match v {
        1 => EncoderBackend::Nvenc,
        2 => EncoderBackend::MediaFoundation,
        _ => EncoderBackend::Auto,
    }
}

struct Pending {
    ts_us: i64,
    width: u32,
    height: u32,
    submitted: Instant,
}

pub(crate) struct EncoderSink {
    enc: Box<dyn HwEncoder>,
    /// Manual-reset event shared with the backend (NVENC completion / MF event pump). Stable for
    /// the sink's lifetime, also across encoder re-creation.
    ev: Arc<Event>,
    control: EncoderControl,
    cb: EncodedCallback,
    es: Arc<EncShared>,
    size: (u32, u32),
    /// Network target (config, then `EncoderControl::set_bitrate`).
    target: u32,
    /// Bitrate currently configured on the encoder (`rate`-compensated target).
    bitrate: u32,
    rate: RateAdapter,
    /// Input-rate compensation on (v0.2); off reproduces v0.1 (benchmark comparisons).
    compensate: bool,
    policy: FpsPolicy,
    kf: KeyframeGate,
    pending: VecDeque<Pending>,
    /// Holds delta output after a frame the encoder may have half-taken (an encode error) or
    /// in-flight frames thrown away, until the next IDR (see [`IdrGate`]).
    gate: IdrGate,
    /// Frames skipped in `submit` (too small), folded into `Shared::dropped` by `drain`.
    skipped: u64,
    win: BitrateWindow,
    last_stats: (Instant, u64, u64, u64, u64),
}

struct BitrateWindow {
    start_us: i64,
    bytes: u64,
}

impl BitrateWindow {
    fn account(&mut self, es: &EncShared, len: usize, now_us: i64) {
        es.bytes.fetch_add(len as u64, Ordering::Relaxed);
        self.bytes += len as u64;
        let dt = now_us - self.start_us;
        if dt >= BITRATE_WINDOW_US {
            let bps = (self.bytes as f64 * 8.0 * 1e6 / dt as f64) as u32;
            es.bitrate_bps.store(bps, Ordering::Relaxed);
            es.bitrate_at_us.store(now_us, Ordering::Relaxed);
            self.start_us = now_us;
            self.bytes = 0;
        }
    }
}

impl Sink for EncoderSink {
    fn event(&self) -> HANDLE {
        self.ev.raw()
    }

    fn has_free(&self) -> bool {
        self.enc.ready()
    }

    fn submit(
        &mut self,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        w: u32,
        h: u32,
        ts_us: i64,
    ) -> Result<()> {
        let mut force = false;
        if w < MIN_ENCODE_W || h < MIN_ENCODE_H {
            // A tiny window: skip quietly (counted as dropped in `drain`), keep running.
            self.skipped += 1;
            return Ok(());
        }
        if (w, h) != self.size {
            let lost = self.pending.len() as u64;
            self.pending.clear();
            if lost > 0 {
                self.gate.arm();
            }
            self.enc.resize(w, h).map_err(|e| {
                CaptureError::Stopped(format!("encoder reinit at {w}x{h} failed: {e}"))
            })?;
            tracing::info!(
                from = format!("{}x{}", self.size.0, self.size.1),
                to = format!("{w}x{h}"),
                backend = ?self.enc.backend(),
                discarded = lost,
                "encoder resized; next frame is IDR"
            );
            self.size = (w, h);
            force = true;
        }
        if let Some(bps) = self.control.take_bitrate_change() {
            self.target = clamp_bitrate(bps);
            self.es.target_bps.store(self.target, Ordering::Relaxed);
        }
        // fps-first degradation (the capture loop's pacer applies the cap to later frames).
        let cap0 = self.policy.cap();
        let cap = self.policy.update(self.target, w, h, ts_us);
        if cap != cap0 {
            tracing::info!(
                from = cap0,
                to = cap,
                target_kbps = self.target / 1000,
                size = format!("{w}x{h}"),
                "encoder frame-rate cap changed (degradation policy)"
            );
            self.es.fps_cap.store(cap, Ordering::Relaxed);
        }
        self.rate.on_frame(ts_us);
        self.rate.limit_fps(cap);
        let want = if self.compensate {
            clamp_bitrate(self.rate.encoder_bitrate(self.target))
        } else {
            self.target
        };
        if want != self.bitrate && bitrate_change_significant(self.bitrate, want) {
            match self.enc.set_bitrate(want) {
                Ok(()) => {
                    tracing::debug!(from = self.bitrate, to = want, target = self.target, "encoder bitrate changed");
                    self.bitrate = want;
                    self.es.encoder_bps.store(want, Ordering::Relaxed);
                }
                Err(e) => tracing::warn!(error = %e, bps = want, "encoder bitrate change failed"),
            }
        }
        if self.control.take_keyframe_request() {
            self.kf.request();
        }
        let force = self.kf.take(ts_us, force);
        self.es.coalesced.store(self.kf.coalesced, Ordering::Relaxed);
        self.pending.push_back(Pending {
            ts_us,
            width: w,
            height: h,
            submitted: Instant::now(),
        });
        if let Err(e) = self.enc.encode(ctx, src, ts_us, force) {
            self.pending.pop_back();
            // The encoder may have taken part of it (an async MFT): restart the chain cleanly.
            // Also keeps a failed frame's keyframe request.
            self.kf.request();
            self.gate.arm();
            return Err(e);
        }
        Ok(())
    }

    fn fps_cap(&self) -> Option<u32> {
        Some(self.policy.cap())
    }

    fn drain(&mut self, _ctx: &ID3D11DeviceContext, shared: &Shared) -> Result<()> {
        shared.drop_frames(std::mem::take(&mut self.skipped));
        if let Some(p) = self.pending.front() {
            let waited = p.submitted.elapsed();
            if waited > STALL_TIMEOUT && self.enc.in_flight() > 0 {
                return Err(CaptureError::Stopped(format!(
                    "{:?} encoder stalled: no output for {} ms",
                    self.enc.backend(),
                    waited.as_millis()
                )));
            }
        }
        // Disjoint field borrows: the backend buffer is handed straight to the callback.
        let pending = &mut self.pending;
        let cb = &mut self.cb;
        let es = &self.es;
        let size = self.size;
        let win = &mut self.win;
        let kf = &mut self.kf;
        let gate = &mut self.gate;
        self.enc.poll(&mut |data, ts| {
            // Match the output with its submitted frame (in order; an encoder may skip frames).
            let mut meta = None;
            while let Some(p) = pending.pop_front() {
                if p.ts_us == ts {
                    meta = Some(p);
                    break;
                }
                if p.ts_us > ts {
                    pending.push_front(p);
                    break;
                }
                shared.drop_frames(1);
            }
            let (w, h, enc_us) = match &meta {
                Some(p) => (p.width, p.height, p.submitted.elapsed().as_micros() as u64),
                None => (size.0, size.1, 0),
            };
            let keyframe = has_idr(data);
            if !gate.pass(keyframe) {
                shared.drop_frames(1);
                if gate.withheld.is_multiple_of(30) {
                    kf.request(); // the IDR asked for was lost or refused: ask again
                }
                return;
            }
            if keyframe {
                es.keyframes.fetch_add(1, Ordering::Relaxed);
                kf.saw_idr(ts);
            }
            cb(&EncodedFrame {
                data,
                keyframe,
                width: w,
                height: h,
                timestamp_us: ts,
            });
            shared.delivered.fetch_add(1, Ordering::Relaxed);
            if enc_us > 0 {
                shared.convert_us_sum.fetch_add(enc_us, Ordering::Relaxed);
                shared.convert_n.fetch_add(1, Ordering::Relaxed);
            }
            win.account(es, data.len(), monotonic_now_us());
        })
    }

    fn log_stats(&mut self, s: &Shared, out_size: (u32, u32)) {
        let (t0, d0, x0, b0, k0) = self.last_stats;
        let d = s.delivered.load(Ordering::Relaxed);
        let x = s.dropped.load(Ordering::Relaxed);
        let b = self.es.bytes.load(Ordering::Relaxed);
        let k = self.es.keyframes.load(Ordering::Relaxed);
        let secs = t0.elapsed().as_secs_f64().max(0.001);
        let n = s.convert_n.load(Ordering::Relaxed).max(1);
        tracing::info!(
            backend = ?self.enc.backend(),
            fps = format!("{:.1}", (d - d0) as f64 / secs),
            dropped = x - x0,
            out = format!("{}x{}", out_size.0, out_size.1),
            kbps = format!("{:.0}", (b - b0) as f64 * 8.0 / secs / 1000.0),
            target_kbps = self.target / 1000,
            encoder_kbps = self.bitrate / 1000,
            fps_cap = self.policy.cap(),
            keyframes = k - k0,
            keyframe_requests_coalesced = self.kf.coalesced,
            avg_encode_us = s.convert_us_sum.load(Ordering::Relaxed) / n,
            "video encode stats"
        );
        self.last_stats = (Instant::now(), d, x, b, k);
    }

    fn describe(&self) -> String {
        format!(
            "{:?} '{}' {}x{} @ {} kbps, degradation {:?}",
            self.enc.backend(),
            self.enc.name(),
            self.size.0,
            self.size.1,
            self.bitrate / 1000,
            self.policy.pref()
        )
    }
}

impl EncoderSink {
    pub(crate) fn new(
        enc: Box<dyn HwEncoder>,
        ev: Arc<Event>,
        control: EncoderControl,
        cb: EncodedCallback,
        es: Arc<EncShared>,
        p: EncParams,
        degradation: DegradationPreference,
    ) -> Self {
        es.backend.store(backend_to_u8(enc.backend()), Ordering::Relaxed);
        es.target_bps.store(p.bitrate, Ordering::Relaxed);
        es.encoder_bps.store(p.bitrate, Ordering::Relaxed);
        es.fps_cap.store(p.fps, Ordering::Relaxed);
        Self {
            enc,
            ev,
            control,
            cb,
            es,
            size: (p.width, p.height),
            target: p.bitrate,
            bitrate: p.bitrate,
            rate: RateAdapter::new(p.fps),
            compensate: true,
            policy: FpsPolicy::new(degradation, p.fps),
            kf: KeyframeGate::default(),
            pending: VecDeque::new(),
            gate: IdrGate::default(),
            skipped: 0,
            win: BitrateWindow {
                start_us: monotonic_now_us(),
                bytes: 0,
            },
            last_stats: (Instant::now(), 0, 0, 0, 0),
        }
    }

    pub(crate) fn name(&self) -> String {
        self.enc.name()
    }

    pub(crate) fn encoder_bitrate(&self) -> u32 {
        self.bitrate
    }

    pub(crate) fn set_rate_compensation(&mut self, on: bool) {
        self.compensate = on;
    }

    pub(crate) fn keyframe_requests_coalesced(&self) -> u64 {
        self.kf.coalesced
    }
}

/// The NVIDIA H.264 MFT was activated in this process: from then on NVENC sessions fail on the
/// affected drivers (see `gpu::should_test`), so a self-test of NVENC would only report that.
pub(crate) static NVIDIA_MFT_USED: AtomicBool = AtomicBool::new(false);

/// NVENC encoders alive in this process (sessions open or opening).
pub(crate) static NVENC_LIVE: AtomicUsize = AtomicUsize::new(0);

/// The NVIDIA H.264 MFT must not be activated while NVENC works in this process (it would break
/// every later NVENC session, see `gpu::should_test`): refused while an NVENC encoder is alive
/// (a camera on NVENC while the share would fall back to the MFT, say). The plan moves on.
fn nvidia_mft_allowed() -> Result<()> {
    match NVENC_LIVE.load(Ordering::Relaxed) {
        0 => Ok(()),
        n => Err(CaptureError::Unsupported(format!(
            "NVIDIA MFT not activated: {n} NVENC encoder(s) running in this process"
        ))),
    }
}

/// The NVIDIA encode library loads (no session opened).
pub(crate) fn nvenc_available() -> bool {
    nvenc::available()
}

/// This adapter lists a hardware H.264 encoder MFT of its own (not activated).
pub(crate) fn mft_listed(luid: u64) -> bool {
    mf::adapter_has_mft(super::adapters::luid_from_u64(luid))
}

/// Some adapter lists a hardware H.264 encoder MFT (not activated).
pub(crate) fn mft_listed_any() -> bool {
    mf::any_mft()
}

/// Create a backend on the capture device. `Auto` tries NVENC on NVIDIA first, then MF.
pub(crate) fn create_encoder(
    backend: EncoderBackend,
    sc: &SinkCtx<'_>,
    p: EncParams,
    ev: &Arc<Event>,
) -> Result<Box<dyn HwEncoder>> {
    let nv = sc.adapter_vendor == VENDOR_NVIDIA;
    let mut p = p;
    // The start-high-then-lower trick (`MfTuning::headroom`) is measured on the NVIDIA MFT only,
    // which never rises above its start rate. An MFT that instead never *lowers* it would send
    // 3x the target for good (and WebRTC drops most of it): AMD / Intel start at the target.
    if !nv {
        p.tuning.mf.headroom = false;
    }
    match backend {
        EncoderBackend::Nvenc => {
            if !nv {
                return Err(CaptureError::Unsupported(format!(
                    "NVENC requested but the capture adapter is '{}' (vendor 0x{:04X})",
                    sc.adapter_name, sc.adapter_vendor
                )));
            }
            Ok(Box::new(nvenc::NvEncoder::new(sc.device, p, ev.clone())?))
        }
        EncoderBackend::MediaFoundation => {
            if nv {
                nvidia_mft_allowed()?;
            }
            let e = mf::MfEncoder::new(sc.device, sc.adapter_luid, p, ev.clone())?;
            if nv {
                NVIDIA_MFT_USED.store(true, Ordering::Relaxed);
            }
            Ok(Box::new(e))
        }
        EncoderBackend::Auto => {
            if nv && auto_prefers_nvenc() {
                match nvenc::NvEncoder::new(sc.device, p, ev.clone()) {
                    Ok(e) => return Ok(Box::new(e)),
                    Err(e) => tracing::warn!(error = %e, "NVENC unavailable; trying Media Foundation"),
                }
            }
            if nv && auto_prefers_nvenc() {
                nvidia_mft_allowed()?;
            }
            match mf::MfEncoder::new(sc.device, sc.adapter_luid, p, ev.clone()) {
                Ok(e) => {
                    if nv {
                        NVIDIA_MFT_USED.store(true, Ordering::Relaxed);
                    }
                    Ok(Box::new(e))
                }
                Err(e) if nv && !auto_prefers_nvenc() => {
                    tracing::warn!(error = %e, "MF encoder unavailable; trying NVENC");
                    Ok(Box::new(nvenc::NvEncoder::new(sc.device, p, ev.clone())?))
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// `Auto` on NVIDIA: NVENC (v0.2). With the v0.2 tuning (P4, LOW_LATENCY) it matched or beat
/// the NVIDIA H.264 MFT at equal bitrate on every synthetic scene (text 1080p60 @ 12 Mbps: worst
/// frame SSIM 0.976 vs 0.901), with ~1 ms less encode latency and less process CPU, and it
/// follows bitrate changes natively (the MFT never rises above its start bitrate, see
/// `MfTuning::headroom`). `docs/quality-bench.md` has the numbers. MF stays the fallback and
/// the backend for AMD / Intel.
fn auto_prefers_nvenc() -> bool {
    true
}

// ---------------------------------------------------------------------------------------------
// Public entry points (wired from lib.rs)
// ---------------------------------------------------------------------------------------------

pub struct EncodedInner {
    stop: Arc<Event>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    es: Arc<EncShared>,
    control: EncoderControl,
}

impl EncodedInner {
    pub fn control(&self) -> EncoderControl {
        self.control.clone()
    }

    pub fn backend(&self) -> EncoderBackend {
        backend_from_u8(self.es.backend.load(Ordering::Relaxed))
    }

    pub fn stats(&self) -> EncodedStats {
        encoded_stats(&self.shared, &self.es)
    }

    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }
}

/// Public statistics of an encoding sink (screen capture or webcam).
pub(crate) fn encoded_stats(s: &Shared, es: &EncShared) -> EncodedStats {
    let n = s.convert_n.load(Ordering::Relaxed);
    let sum = s.convert_us_sum.load(Ordering::Relaxed);
    // A static screen produces no frames: report 0 rather than a stale value.
    let fresh = monotonic_now_us() - es.bitrate_at_us.load(Ordering::Relaxed)
        < 3 * BITRATE_WINDOW_US;
    EncodedStats {
        frames_encoded: s.delivered.load(Ordering::Relaxed),
        frames_dropped: s.dropped.load(Ordering::Relaxed),
        keyframes: es.keyframes.load(Ordering::Relaxed),
        avg_encode_us: sum.checked_div(n).unwrap_or(0).min(u32::MAX as u64) as u32,
        bitrate_bps: if fresh {
            es.bitrate_bps.load(Ordering::Relaxed)
        } else {
            0
        },
        target_bitrate_bps: es.target_bps.load(Ordering::Relaxed),
        encoder_bitrate_bps: es.encoder_bps.load(Ordering::Relaxed),
        fps_cap: es.fps_cap.load(Ordering::Relaxed),
        keyframe_requests_coalesced: es.coalesced.load(Ordering::Relaxed),
    }
}

impl Drop for EncodedInner {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn start_video_encoded(
    target: VideoTarget,
    cfg: EncodeConfig,
    on_frame: EncodedCallback,
    preview: Option<super::preview::PreviewAttach>,
) -> Result<EncodedInner> {
    let vcfg = video::sanitize(cfg.video);
    let shared = Shared::new_running();
    let es = Arc::new(EncShared::default());
    let control = EncoderControl::default();
    let backend = cfg.backend;
    let degradation = cfg.degradation;
    let bitrate = clamp_bitrate(cfg.bitrate_bps);

    let (stop, thread) = {
        let es = es.clone();
        let control = control.clone();
        video::spawn_capture(target, vcfg, cfg.adapter_luid, shared.clone(), "vox-video-enc", preview, move |sc| {
            let ev = Arc::new(Event::new(true)?);
            // A minimized window reports its caption size (~160x28): start at a placeholder size;
            // the first real frame resizes the encoder (with an IDR) anyway.
            let (w0, h0) = if sc.out_size.0 < MIN_ENCODE_W || sc.out_size.1 < MIN_ENCODE_H {
                video::fit(1280, 720, sc.cfg.max_width, sc.cfg.max_height)
            } else {
                sc.out_size
            };
            let p = EncParams {
                width: w0,
                height: h0,
                fps: sc.cfg.max_fps,
                bitrate,
                max_width: sc.cfg.max_width & !1,
                max_height: sc.cfg.max_height & !1,
                tuning: Tuning::default(),
            };
            let t0 = Instant::now();
            let enc = create_encoder(backend, sc, p, &ev)?;
            tracing::info!(
                requested = ?backend,
                chosen = ?enc.backend(),
                encoder = %enc.name(),
                adapter = %sc.adapter_name,
                size = format!("{}x{}", p.width, p.height),
                fps = p.fps,
                kbps = bitrate / 1000,
                init_ms = t0.elapsed().as_millis() as u64,
                degradation = ?degradation,
                "hardware encoder ready"
            );
            Ok(EncoderSink::new(enc, ev, control, on_frame, es, p, degradation))
        })?
    };
    Ok(EncodedInner {
        stop,
        thread: Some(thread),
        shared,
        es,
        control,
    })
}

/// Probe once (on a dedicated MTA thread, so the caller's COM apartment is untouched) which
/// backends work on the adapter that drives the primary monitor.
pub fn available_encoders() -> Vec<EncoderBackend> {
    static CACHE: OnceLock<Vec<EncoderBackend>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            std::thread::Builder::new()
                .name("vox-enc-probe".into())
                .spawn(|| {
                    let _com = ComInit::mta();
                    probe()
                })
                .ok()
                .and_then(|t| t.join().ok())
                .unwrap_or_default()
        })
        .clone()
}

fn probe() -> Vec<EncoderBackend> {
    let t0 = Instant::now();
    let hmon = super::enumerate::monitors_with_handles()
        .ok()
        .and_then(|m| {
            let primary = m.iter().position(|(_, i)| i.primary).unwrap_or(0);
            m.into_iter().nth(primary).map(|(h, _)| h)
        });
    let adapter = hmon.and_then(super::enumerate::adapter_for_monitor);
    let (device, _ctx, info) = match video::create_device(adapter.as_ref()) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "encoder probe: no D3D11 device");
            return Vec::new();
        }
    };
    let p = EncParams {
        width: 640,
        height: 360,
        fps: 30,
        bitrate: 1_000_000,
        max_width: 640,
        max_height: 360,
        tuning: Tuning::default(),
    };
    let ev = match Event::new(true) {
        Ok(e) => Arc::new(e),
        Err(_) => return Vec::new(),
    };
    let nv_ok = info.vendor == VENDOR_NVIDIA
        && match nvenc::NvEncoder::new(&device, p, ev.clone()) {
            Ok(_) => true,
            Err(e) => {
                tracing::info!(error = %e, "encoder probe: NVENC not usable");
                false
            }
        };
    // With NVENC working, only list the MFT, never activate it: once the NVIDIA H.264 MFT has
    // been instantiated in a process, every later nvEncOpenEncodeSessionEx there fails with
    // NV_ENC_ERR_INVALID_VERSION (seen with driver 610.88, NVENC API 13.1; capture_probe:
    // "encoders" then "camera --backend nvenc" in one process). Probing MF this way used to
    // send every Go Live and camera to MF on such drivers.
    let mf_ok = if nv_ok {
        match mf::has_hardware_mft(info.luid) {
            Ok(found) => found,
            Err(e) => {
                tracing::info!(error = %e, "encoder probe: Media Foundation HW H.264 not listed");
                false
            }
        }
    } else {
        match mf::MfEncoder::new(&device, info.luid, p, ev.clone()) {
            Ok(_) => true,
            Err(e) => {
                tracing::info!(error = %e, "encoder probe: Media Foundation HW H.264 not usable");
                false
            }
        }
    };
    let mut out = Vec::new();
    if auto_prefers_nvenc() {
        if nv_ok {
            out.push(EncoderBackend::Nvenc);
        }
        if mf_ok {
            out.push(EncoderBackend::MediaFoundation);
        }
    } else {
        if mf_ok {
            out.push(EncoderBackend::MediaFoundation);
        }
        if nv_ok {
            out.push(EncoderBackend::Nvenc);
        }
    }
    tracing::info!(
        adapter = %info.name,
        vendor = format!("0x{:04X}", info.vendor),
        encoders = ?out,
        probe_ms = t0.elapsed().as_millis() as u64,
        "hardware encoders probed"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_scan() {
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0, 0, 1,
        ];
        let types: Vec<u8> = nal_units(&au).map(|(_, t)| t).collect();
        assert_eq!(types, vec![7, 8, 5]);
        assert!(has_idr(&au));
        assert!(is_annexb(&au));
        assert!(!has_idr(&[0, 0, 1, 0x41, 0]));
    }
}
