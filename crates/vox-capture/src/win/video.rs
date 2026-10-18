//! Windows.Graphics.Capture -> D3D11 video processor (BGRA -> NV12 + downscale) -> fenced
//! staging-ring readback -> callback.
//!
//! Threading: one dedicated thread owns every D3D/WGC object and the immediate context. WGC's
//! free-threaded `FrameArrived` only sets an event; the thread sleeps in
//! `WaitForMultipleObjects` on {stop, target closed, frame arrived, GPU fence, pacing timer}, so
//! an idle or static target costs ~0 CPU.
//!
//! The GPU stages are split: `Converter` produces an NV12 texture and a [`Sink`] consumes it.
//! `Readback` (raw path, `start_video`) copies it to the CPU; the hardware encoder sink
//! (`super::encode`, `start_video_encoded`) hands it to NVENC / an MF transform on the same
//! device (zero-copy). The capture loop, pacing, resize/minimize and teardown logic are shared.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Instant;

use windows::core::{Interface, PCWSTR};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, LUID, RECT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForMultipleObjects,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindow};
use windows_core::BOOL;

use super::adapters::{adapter_by_luid, luid_u64};
use super::enumerate::{adapter_for_monitor, monitors_with_handles};
use super::preview::{Bgra, Main, PreviewAttach, PreviewTap};
use super::{monotonic_now_us, os, wide_to_string, ComInit, Event, OwnedHandle};
use crate::{CaptureError, FrameCallback, Nv12Frame, Result, VideoConfig, VideoStats, VideoTarget};

const POOL_BUFFERS: i32 = 2;
const READBACK_SLOTS: usize = 3;
const MAX_CONSECUTIVE_ERRORS: u32 = 30;
const MIN_CONTENT: i32 = 32;
/// For window targets: how often to double-check the HWND still exists (backup for `Closed`).
const WINDOW_CHECK_MS: u32 = 1000;

/// Counters shared between the capture thread and the public handle.
#[derive(Default)]
pub(crate) struct Shared {
    /// Frames handed to the consumer (raw callback, or encoded access units).
    pub delivered: AtomicU64,
    pub dropped: AtomicU64,
    /// Raw path: GPU convert+readback time. Encoded path: encoder submit-to-output time.
    pub convert_us_sum: AtomicU64,
    pub convert_n: AtomicU64,
    pub running: AtomicBool,
}

impl Shared {
    pub fn new_running() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(true),
            ..Default::default()
        })
    }

    pub fn drop_frames(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
    }
}

/// What a sink factory gets to know about the capture device (built on the capture thread).
pub(crate) struct SinkCtx<'a> {
    pub device: &'a ID3D11Device,
    pub ctx: &'a ID3D11DeviceContext,
    pub adapter_luid: LUID,
    pub adapter_vendor: u32,
    pub adapter_name: &'a str,
    /// Output size expected for the first frame (the encoder is initialised with it).
    pub out_size: (u32, u32),
    pub cfg: VideoConfig,
}

/// Consumer of converted NV12 frames. Lives on the capture thread.
pub(crate) trait Sink {
    /// Signalled when `drain` has work (GPU fence, encoder output). Waited on by the loop.
    fn event(&self) -> HANDLE;
    /// Can a new frame be submitted now? If not, the frame is dropped before any GPU work.
    fn has_free(&self) -> bool;
    /// Queue the NV12 texture `src` (exactly `w`x`h`). `src` is overwritten by the next frame's
    /// conversion, so the sink must be done reading it (or have copied it) by the next submit.
    fn submit(
        &mut self,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        w: u32,
        h: u32,
        ts_us: i64,
    ) -> Result<()>;
    /// Deliver finished work. Must not block on the GPU.
    fn drain(&mut self, ctx: &ID3D11DeviceContext, shared: &Shared) -> Result<()>;
    /// Periodic stats line (called at most every `vox_log::STATS_INTERVAL`).
    fn log_stats(&mut self, shared: &Shared, out_size: (u32, u32));
    /// Short description for the "capture started" log line.
    fn describe(&self) -> String;
    /// Frame-rate cap requested by the sink (degradation policy); applied by the pacer.
    fn fps_cap(&self) -> Option<u32> {
        None
    }
}

pub struct VideoInner {
    stop: Arc<Event>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl VideoInner {
    pub fn stats(&self) -> VideoStats {
        let n = self.shared.convert_n.load(Ordering::Relaxed);
        let sum = self.shared.convert_us_sum.load(Ordering::Relaxed);
        VideoStats {
            frames_delivered: self.shared.delivered.load(Ordering::Relaxed),
            frames_dropped: self.shared.dropped.load(Ordering::Relaxed),
            avg_gpu_convert_us: sum.checked_div(n).unwrap_or(0).min(u32::MAX as u64) as u32,
        }
    }

    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }
}

impl Drop for VideoInner {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn start_video(
    target: VideoTarget,
    cfg: VideoConfig,
    on_frame: FrameCallback,
    preview: Option<PreviewAttach>,
) -> Result<VideoInner> {
    let shared = Shared::new_running();
    let (stop, thread) = spawn_capture(target, cfg, None, shared.clone(), "vox-video", preview, move |sc| {
        Readback::new(sc.device, sc.ctx, on_frame)
    })?;
    Ok(VideoInner {
        stop,
        thread: Some(thread),
        shared,
    })
}

/// Normalise a user config to sane limits.
pub(crate) fn sanitize(cfg: VideoConfig) -> VideoConfig {
    VideoConfig {
        max_fps: cfg.max_fps.clamp(1, 240),
        max_width: cfg.max_width.max(16),
        max_height: cfg.max_height.max(16),
        capture_cursor: cfg.capture_cursor,
    }
}

/// Spawn the capture thread with the sink built by `make_sink` (on that thread, after the D3D
/// device exists). Returns once setup succeeded or failed; on failure the thread is joined.
/// `adapter`: LUID of the adapter for the D3D11 device (the encoder's), `None` = the monitor's.
pub(crate) fn spawn_capture<S, F>(
    target: VideoTarget,
    cfg: VideoConfig,
    adapter: Option<u64>,
    shared: Arc<Shared>,
    thread_name: &str,
    preview: Option<PreviewAttach>,
    make_sink: F,
) -> Result<(Arc<Event>, JoinHandle<()>)>
where
    S: Sink + 'static,
    F: FnOnce(&SinkCtx<'_>) -> Result<S> + Send + 'static,
{
    let cfg = sanitize(cfg);
    let stop = Arc::new(Event::new(true)?);
    let (tx, rx) = mpsc::sync_channel::<Result<()>>(1);

    let thread = {
        let stop = stop.clone();
        let shared = shared.clone();
        std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                let _com = ComInit::mta();
                match Capture::open(target, cfg, adapter, shared.clone(), preview, make_sink) {
                    Ok(mut cap) => {
                        let _ = tx.send(Ok(()));
                        cap.run(&stop);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
                shared.running.store(false, Ordering::Relaxed);
            })
            .map_err(|e| CaptureError::Os(format!("spawn video thread: {e}")))?
    };

    match rx.recv() {
        Ok(Ok(())) => Ok((stop, thread)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(CaptureError::Stopped(
                "video thread exited during setup".into(),
            ))
        }
    }
}

/// Drops frames that arrive faster than `max_fps`, before any GPU work.
pub(crate) struct Pacer {
    interval_us: i64,
    next_due_us: Option<i64>,
}

pub(crate) enum Pace {
    Accept,
    /// Too early; would be due in this many microseconds.
    Early(i64),
}

impl Pacer {
    pub(crate) fn new(fps: u32) -> Self {
        Self {
            interval_us: 1_000_000 / fps as i64,
            next_due_us: None,
        }
    }

    pub(crate) fn set_fps(&mut self, fps: u32) {
        let interval = 1_000_000 / fps.max(1) as i64;
        if interval != self.interval_us {
            // Shift the pending deadline so a lower rate takes effect on the next frame.
            if let Some(due) = self.next_due_us.as_mut() {
                *due += interval - self.interval_us;
            }
            self.interval_us = interval;
        }
    }

    pub(crate) fn check(&mut self, ts_us: i64) -> Pace {
        let tol = self.interval_us / 4; // absorbs vsync / timestamp jitter
        match self.next_due_us {
            Some(due) if ts_us < due - tol => Pace::Early(due - tol - ts_us),
            Some(due) => {
                // Keep a steady cadence; resync after a gap (static content, pause).
                self.next_due_us = Some(if ts_us - due > self.interval_us {
                    ts_us + self.interval_us
                } else {
                    due + self.interval_us
                });
                Pace::Accept
            }
            None => {
                self.next_due_us = Some(ts_us + self.interval_us);
                Pace::Accept
            }
        }
    }
}

/// Fit (w, h) inside (max_w, max_h) keeping aspect, never upscale, even dimensions.
pub(crate) fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let s = (max_w as f64 / w as f64)
        .min(max_h as f64 / h as f64)
        .min(1.0);
    let ow = ((w as f64 * s) as u32) & !1;
    let oh = ((h as f64 * s) as u32) & !1;
    (ow.max(2), oh.max(2))
}

// ---------------------------------------------------------------------------------------------
// GPU convert stage
// ---------------------------------------------------------------------------------------------

struct Processor {
    key: (u32, u32, u32, u32),
    enumerator: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    out_tex: ID3D11Texture2D,
    out_view: ID3D11VideoProcessorOutputView,
    /// Last input view (texture pointer, array slice, view), when `Converter::cache_input` is on.
    in_cache: Option<(usize, u32, ID3D11VideoProcessorInputView)>,
    /// Mirror / rotation last set on the stream (they only change on request).
    xform_applied: Xform,
}

/// Mirror and clockwise rotation of the converted picture (webcam). The GPU video processor does
/// both while it scales, so they cost nothing extra.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct Xform {
    /// Mirror the final picture left to right.
    pub mirror: bool,
    /// 0, 90, 180 or 270 degrees clockwise.
    pub rot: u32,
}

impl Xform {
    /// (flip horizontally, flip vertically) to hand to the video processor so that the result is
    /// "rotate clockwise, then mirror left to right". The processor mirrors the source before it
    /// rotates, so with a quarter turn the flip moves to the other axis.
    pub(crate) fn processor_flips(self) -> (bool, bool) {
        if !self.mirror {
            (false, false)
        } else if self.rot == 90 || self.rot == 270 {
            (false, true)
        } else {
            (true, false)
        }
    }
}

pub(crate) struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    vctx1: Option<ID3D11VideoContext1>,
    proc: Option<Processor>,
    /// Colour space of the input: `None` = desktop RGB (WGC), else a YCbCr space (webcam).
    input_yuv: Option<YuvSpace>,
    /// Keep the input view of the last texture instead of creating one per blit (for a converter
    /// that reads the same texture every time, like the preview tap reading the NV12 output).
    cache_input: bool,
    /// Wanted mirror / rotation (see [`Xform`]).
    xform: Xform,
    /// The GPU's video processor can mirror and rotate (checked when it is created).
    xform_ok: bool,
}

/// YCbCr colour space of a video-processor input (webcam frames).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct YuvSpace {
    pub bt709: bool,
    pub full_range: bool,
}

impl Converter {
    pub(crate) fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext) -> Result<Self> {
        let vdev: ID3D11VideoDevice = device
            .cast()
            .map_err(|e| CaptureError::Unsupported(format!("no ID3D11VideoDevice: {e}")))?;
        let vctx: ID3D11VideoContext = ctx.cast().map_err(|e| os("ID3D11VideoContext", e))?;
        let vctx1 = vctx.cast::<ID3D11VideoContext1>().ok();
        Ok(Self {
            vdev,
            vctx,
            vctx1,
            proc: None,
            input_yuv: None,
            cache_input: false,
            xform: Xform::default(),
            xform_ok: true,
        })
    }

    /// Mirror / rotate the converted picture from the next blit on (webcam). Ignored, with
    /// [`Converter::transform_supported`] false, on GPUs whose video processor cannot do it.
    pub(crate) fn set_transform(&mut self, xform: Xform) {
        self.xform = xform;
    }

    pub(crate) fn transform_supported(&self) -> bool {
        self.xform_ok
    }

    /// Cache the input view of the last source texture (see the field).
    pub(crate) fn set_cache_input(&mut self, on: bool) {
        self.cache_input = on;
    }

    /// Drop the video processor, its NV12 output texture and the cached input view (they are
    /// rebuilt by the next `ensure`).
    pub(crate) fn release(&mut self) {
        self.proc = None;
    }

    /// Treat the input as desktop RGB again (undo [`Converter::set_input_yuv`]).
    pub(crate) fn set_input_rgb(&mut self) {
        if self.input_yuv.is_some() {
            self.input_yuv = None;
            self.proc = None;
        }
    }

    /// The NV12 texture the last [`Converter::convert`] wrote (valid until the next convert or a
    /// size change).
    pub(crate) fn output(&self) -> Option<&ID3D11Texture2D> {
        self.proc.as_ref().map(|p| &p.out_tex)
    }

    /// Treat the input as YCbCr in `space` (webcam) instead of desktop RGB. A change rebuilds the
    /// processor on the next `ensure`.
    pub(crate) fn set_input_yuv(&mut self, space: YuvSpace) {
        if self.input_yuv != Some(space) {
            self.input_yuv = Some(space);
            self.proc = None;
        }
    }

    pub(crate) fn ensure(
        &mut self,
        device: &ID3D11Device,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
    ) -> Result<()> {
        let key = (in_w, in_h, out_w, out_h);
        if self.proc.as_ref().is_some_and(|p| p.key == key) {
            return Ok(());
        }
        self.proc = None;
        unsafe {
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = self
                .vdev
                .CreateVideoProcessorEnumerator(&desc)
                .map_err(|e| os("CreateVideoProcessorEnumerator", e))?;
            let support = enumerator
                .CheckVideoProcessorFormat(DXGI_FORMAT_NV12)
                .unwrap_or(0);
            if support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
                return Err(CaptureError::Unsupported(
                    "GPU video processor cannot output NV12".into(),
                ));
            }
            let vp = self
                .vdev
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|e| os("CreateVideoProcessor", e))?;

            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: out_w,
                Height: out_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut out_tex = None;
            device
                .CreateTexture2D(&tex_desc, None, Some(&mut out_tex))
                .map_err(|e| os("CreateTexture2D(NV12 out)", e))?;
            let out_tex =
                out_tex.ok_or_else(|| CaptureError::Os("CreateTexture2D returned null".into()))?;

            let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut out_view = None;
            self.vdev
                .CreateVideoProcessorOutputView(
                    &out_tex,
                    &enumerator,
                    &ov_desc,
                    Some(&mut out_view),
                )
                .map_err(|e| os("CreateVideoProcessorOutputView", e))?;
            let out_view = out_view.ok_or_else(|| CaptureError::Os("output view null".into()))?;

            // Static per-processor state.
            self.vctx.VideoProcessorSetStreamFrameFormat(
                &vp,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            self.vctx
                .VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            let out_rect = RECT {
                left: 0,
                top: 0,
                right: out_w as i32,
                bottom: out_h as i32,
            };
            self.vctx
                .VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&out_rect));
            self.vctx
                .VideoProcessorSetOutputTargetRect(&vp, true, Some(&out_rect));
            if let Some(v1) = &self.vctx1 {
                // Desktop is full-range sRGB-ish; webcams are BT.601/709 YCbCr (JPEG: full range);
                // encoders expect BT.709 limited range.
                let input = match self.input_yuv {
                    None => DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    Some(YuvSpace { bt709: false, full_range: false }) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
                    Some(YuvSpace { bt709: false, full_range: true }) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
                    Some(YuvSpace { bt709: true, full_range: false }) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                    Some(YuvSpace { bt709: true, full_range: true }) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
                };
                v1.VideoProcessorSetStreamColorSpace1(&vp, 0, input);
                v1.VideoProcessorSetOutputColorSpace1(
                    &vp,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
            } else {
                // Bits: YCbCr_Matrix (2: 0 = BT.601, 1 = BT.709), Nominal_Range (4-5: 1 = 16-235,
                // 2 = 0-255). RGB input: all zero (full range).
                let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: match self.input_yuv {
                        None => 0,
                        Some(y) => ((y.bt709 as u32) << 2) | (if y.full_range { 2 } else { 1 } << 4),
                    },
                };
                                                                                // YCbCr_Matrix=1 (BT.709) | Nominal_Range=1 (16-235)
                let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 2) | (1 << 4),
                };
                self.vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &input);
                self.vctx.VideoProcessorSetOutputColorSpace(&vp, &output);
            }
            tracing::debug!(in_w, in_h, out_w, out_h, "video processor (re)created");
            let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
            if enumerator.GetVideoProcessorCaps(&mut caps).is_ok() {
                let need = (D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_MIRROR.0 | D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION.0) as u32;
                let ok = caps.FeatureCaps & need == need && self.vctx1.is_some();
                if ok != self.xform_ok {
                    tracing::warn!(feature_caps = caps.FeatureCaps, "GPU video processor cannot mirror / rotate: the camera picture is left as it is");
                }
                self.xform_ok = ok;
            }
            self.proc = Some(Processor {
                key,
                enumerator,
                vp,
                out_tex,
                out_view,
                in_cache: None,
                xform_applied: Xform::default(),
            });
        }
        Ok(())
    }

    /// Queue BGRA `tex` (content in its top-left `src_w`x`src_h`) -> NV12 output texture.
    pub(crate) fn convert(
        &mut self,
        tex: &ID3D11Texture2D,
        src_w: u32,
        src_h: u32,
    ) -> Result<&ID3D11Texture2D> {
        self.convert_slice(tex, 0, src_w, src_h)
    }

    /// [`Converter::convert`] for one slice of a texture array (decoder output pools).
    pub(crate) fn convert_slice(
        &mut self,
        tex: &ID3D11Texture2D,
        array_slice: u32,
        src_w: u32,
        src_h: u32,
    ) -> Result<&ID3D11Texture2D> {
        self.convert_slice_rect(tex, array_slice, (0, 0, src_w, src_h))
    }

    /// [`Converter::convert_slice`] for a part of the texture (left, top, right, bottom): the
    /// video processor's source rectangle, so a crop costs nothing.
    pub(crate) fn convert_slice_rect(
        &mut self,
        tex: &ID3D11Texture2D,
        array_slice: u32,
        src: (u32, u32, u32, u32),
    ) -> Result<&ID3D11Texture2D> {
        let cache = self.cache_input;
        let p = self
            .proc
            .as_mut()
            .ok_or_else(|| CaptureError::Os("processor not ready".into()))?;
        unsafe {
            let tex_id = tex.as_raw() as usize;
            let cached = match &p.in_cache {
                Some((t, s, v)) if cache && *t == tex_id && *s == array_slice => Some(v.clone()),
                _ => None,
            };
            let in_view = match cached {
                Some(v) => Some(v),
                None => {
                    let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                        FourCC: 0,
                        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPIV {
                                MipSlice: 0,
                                ArraySlice: array_slice,
                            },
                        },
                    };
                    let mut v = None;
                    self.vdev
                        .CreateVideoProcessorInputView(tex, &p.enumerator, &iv_desc, Some(&mut v))
                        .map_err(|e| os("CreateVideoProcessorInputView", e))?;
                    if cache {
                        // The view keeps the texture alive, so the pointer cannot be reused.
                        p.in_cache = v.clone().map(|v| (tex_id, array_slice, v));
                    }
                    v
                }
            };
            let src_rect = RECT {
                left: src.0 as i32,
                top: src.1 as i32,
                right: src.2 as i32,
                bottom: src.3 as i32,
            };
            self.vctx
                .VideoProcessorSetStreamSourceRect(&p.vp, 0, true, Some(&src_rect));
            if self.xform_ok && p.xform_applied != self.xform {
                let rotation = match self.xform.rot {
                    90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
                    180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
                    270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
                    _ => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
                };
                self.vctx
                    .VideoProcessorSetStreamRotation(&p.vp, 0, self.xform.rot != 0, rotation);
                if let Some(v1) = &self.vctx1 {
                    let (fh, fv) = self.xform.processor_flips();
                    v1.VideoProcessorSetStreamMirror(&p.vp, 0, self.xform.mirror, fh, fv);
                }
                p.xform_applied = self.xform;
            }
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL(1),
                pInputSurface: std::mem::ManuallyDrop::new(in_view),
                ..Default::default()
            };
            let r =
                self.vctx
                    .VideoProcessorBlt(&p.vp, &p.out_view, 0, std::slice::from_ref(&stream));
            std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
            r.map_err(|e| os("VideoProcessorBlt", e))?;
        }
        Ok(&p.out_tex)
    }
}

// ---------------------------------------------------------------------------------------------
// Readback stage: ring of NV12 staging textures, completion signalled by an ID3D11Fence event so
// `Map` never waits on the GPU and the thread never polls.
// ---------------------------------------------------------------------------------------------

struct Slot {
    tex: Option<ID3D11Texture2D>,
    w: u32,
    h: u32,
    fence_value: u64,
    ts_us: i64,
    submitted: Instant,
}

pub(crate) struct Readback {
    device: ID3D11Device,
    slots: Vec<Slot>,
    pending: VecDeque<usize>,
    fence: Option<(ID3D11Fence, ID3D11DeviceContext4)>,
    fence_value: u64,
    fence_ev: Arc<Event>,
    cb: FrameCallback,
    last_stats: (Instant, u64, u64),
}

impl Sink for Readback {
    fn event(&self) -> HANDLE {
        self.fence_ev.raw()
    }
    fn has_free(&self) -> bool {
        self.pending.len() < self.slots.len()
    }
    fn submit(
        &mut self,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        w: u32,
        h: u32,
        ts_us: i64,
    ) -> Result<()> {
        Readback::submit(self, ctx, src, w, h, ts_us)
    }
    fn drain(&mut self, ctx: &ID3D11DeviceContext, shared: &Shared) -> Result<()> {
        Readback::drain(self, ctx, shared)
    }
    fn log_stats(&mut self, s: &Shared, out_size: (u32, u32)) {
        let (t0, d0, x0) = self.last_stats;
        let d = s.delivered.load(Ordering::Relaxed);
        let x = s.dropped.load(Ordering::Relaxed);
        let secs = t0.elapsed().as_secs_f64().max(0.001);
        let n = s.convert_n.load(Ordering::Relaxed).max(1);
        tracing::info!(
            fps = format!("{:.1}", (d - d0) as f64 / secs),
            dropped = x - x0,
            out = format!("{}x{}", out_size.0, out_size.1),
            avg_convert_us = s.convert_us_sum.load(Ordering::Relaxed) / n,
            "video capture stats"
        );
        self.last_stats = (Instant::now(), d, x);
    }
    fn describe(&self) -> String {
        format!("raw NV12 readback (gpu_fence={})", self.fence.is_some())
    }
}

impl Readback {
    pub(crate) fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext, cb: FrameCallback) -> Result<Self> {
        let fence = (|| -> windows_core::Result<(ID3D11Fence, ID3D11DeviceContext4)> {
            let dev5: ID3D11Device5 = device.cast()?;
            let ctx4: ID3D11DeviceContext4 = ctx.cast()?;
            let mut fence: Option<ID3D11Fence> = None;
            unsafe { dev5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)? };
            Ok((
                fence.ok_or_else(|| {
                    windows_core::Error::from(windows::Win32::Foundation::E_POINTER)
                })?,
                ctx4,
            ))
        })();
        let fence = match fence {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::warn!(
                    hr = format!("0x{:08X}", e.code().0 as u32),
                    "ID3D11Fence unavailable; readback will block on Map"
                );
                None
            }
        };
        let slots = (0..READBACK_SLOTS)
            .map(|_| Slot {
                tex: None,
                w: 0,
                h: 0,
                fence_value: 0,
                ts_us: 0,
                submitted: Instant::now(),
            })
            .collect();
        Ok(Self {
            device: device.clone(),
            slots,
            pending: VecDeque::new(),
            fence,
            fence_value: 0,
            fence_ev: Arc::new(Event::new(false)?),
            cb,
            last_stats: (Instant::now(), 0, 0),
        })
    }

    fn submit(
        &mut self,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        w: u32,
        h: u32,
        ts_us: i64,
    ) -> Result<()> {
        let idx = (0..self.slots.len())
            .find(|i| !self.pending.contains(i))
            .ok_or_else(|| CaptureError::Os("no free slot".into()))?;
        let slot = &mut self.slots[idx];
        if slot.tex.is_none() || slot.w != w || slot.h != h {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut t = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t)) }
                .map_err(|e| os("CreateTexture2D(staging)", e))?;
            slot.tex = t;
            slot.w = w;
            slot.h = h;
        }
        unsafe {
            ctx.CopyResource(slot.tex.as_ref().unwrap(), src);
            if let Some((fence, ctx4)) = &self.fence {
                self.fence_value += 1;
                ctx4.Signal(fence, self.fence_value)
                    .map_err(|e| os("ID3D11DeviceContext4::Signal", e))?;
                slot.fence_value = self.fence_value;
            }
            ctx.Flush();
        }
        slot.ts_us = ts_us;
        slot.submitted = Instant::now();
        self.pending.push_back(idx);
        if self.pending.len() == 1 {
            self.arm_front()?;
        }
        Ok(())
    }

    /// Drop every staging texture (they are created again by the next `submit`). Only when
    /// nothing is in flight; returns whether it did.
    pub(crate) fn release(&mut self) -> bool {
        if !self.pending.is_empty() {
            return false;
        }
        for s in &mut self.slots {
            s.tex = None;
            s.w = 0;
            s.h = 0;
        }
        true
    }

    fn arm_front(&self) -> Result<()> {
        if let (Some((fence, _)), Some(&idx)) = (&self.fence, self.pending.front()) {
            unsafe { fence.SetEventOnCompletion(self.slots[idx].fence_value, self.fence_ev.raw()) }
                .map_err(|e| os("ID3D11Fence::SetEventOnCompletion", e))?;
        }
        Ok(())
    }

    /// Deliver every completed readback, oldest first. Never waits on the GPU when a fence exists.
    fn drain(&mut self, ctx: &ID3D11DeviceContext, shared: &Shared) -> Result<()> {
        while let Some(&idx) = self.pending.front() {
            let slot = &self.slots[idx];
            if let Some((fence, _)) = &self.fence {
                if unsafe { fence.GetCompletedValue() } < slot.fence_value {
                    return self.arm_front();
                }
            }
            let convert_us = slot.submitted.elapsed().as_micros() as u64;
            let tex = slot.tex.as_ref().unwrap();
            let (w, h) = (slot.w, slot.h);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            let mapped = unsafe { ctx.Map(tex, 0, D3D11_MAP_READ, 0, Some(&mut m)) };
            self.pending.pop_front();
            mapped.map_err(|e| os("Map(staging)", e))?;
            let pitch = m.RowPitch as usize;
            // SAFETY: an NV12 staging map is RowPitch*H bytes of Y followed by RowPitch*H/2 of UV.
            unsafe {
                let base = m.pData as *const u8;
                let frame = Nv12Frame {
                    width: w,
                    height: h,
                    timestamp_us: slot.ts_us,
                    y: std::slice::from_raw_parts(base, pitch * h as usize),
                    y_stride: pitch as u32,
                    uv: std::slice::from_raw_parts(
                        base.add(pitch * h as usize),
                        pitch * (h as usize / 2),
                    ),
                    uv_stride: pitch as u32,
                };
                (self.cb)(&frame);
                ctx.Unmap(tex, 0);
            }
            shared.delivered.fetch_add(1, Ordering::Relaxed);
            shared
                .convert_us_sum
                .fetch_add(convert_us, Ordering::Relaxed);
            shared.convert_n.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Capture session
// ---------------------------------------------------------------------------------------------

struct Capture<S: Sink> {
    target: VideoTarget,
    hwnd: Option<HWND>,
    cfg: VideoConfig,
    shared: Arc<Shared>,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_ev: Arc<Event>,
    closed_ev: Arc<Event>,
    timer: OwnedHandle,
    frame_token: i64,
    closed_token: i64,
    pool_size: SizeInt32,
    conv: Converter,
    /// Always `Some` until `Drop`, which tears the sink down first (before the device).
    sink: Option<S>,
    pacer: Pacer,
    deferred: Option<Direct3D11CaptureFrame>,
    out_size: (u32, u32),
    consecutive_errors: u32,
    stats_gate: vox_log::RateLimiter,
    /// Optional low-rate preview tap (item 8a), fed from the converter's output.
    preview: Option<PreviewTap>,
}

impl<S: Sink> Capture<S> {
    fn sink(&mut self) -> &mut S {
        self.sink.as_mut().expect("sink present until drop")
    }

    fn open<F>(
        target: VideoTarget,
        cfg: VideoConfig,
        adapter_req: Option<u64>,
        shared: Arc<Shared>,
        preview: Option<PreviewAttach>,
        make_sink: F,
    ) -> Result<Self>
    where
        F: FnOnce(&SinkCtx<'_>) -> Result<S>,
    {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(CaptureError::Unsupported(
                "Windows.Graphics.Capture not supported on this system".into(),
            ));
        }

        // Resolve target -> HMONITOR (for adapter choice) and optional HWND.
        let (hmon, hwnd, label) = match target {
            VideoTarget::Monitor(i) => {
                let mons = monitors_with_handles()?;
                let (h, info) = mons
                    .into_iter()
                    .nth(i)
                    .ok_or_else(|| CaptureError::NotFound(format!("monitor index {i}")))?;
                (
                    h,
                    None,
                    format!("monitor {i} '{}' {}x{}", info.name, info.width, info.height),
                )
            }
            VideoTarget::Window(raw) => {
                let hwnd = HWND(raw as *mut _);
                if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                    return Err(CaptureError::NotFound(format!("window 0x{raw:X}")));
                }
                (
                    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) },
                    Some(hwnd),
                    format!("window 0x{raw:X}"),
                )
            }
        };

        // The device goes where the encoder is. WGC accepts a device on another adapter than the
        // one scanning out the monitor and copies each frame across (hybrid laptops: panel on
        // the iGPU, encoder on the dGPU).
        let mon_adapter = adapter_for_monitor(hmon);
        let mon_luid = mon_adapter.as_ref().and_then(|a| unsafe { a.GetDesc() }.ok()).map(|d| luid_u64(d.AdapterLuid));
        let pick = crate::gpu::capture_adapter(adapter_req, mon_luid, &|l| adapter_by_luid(l).is_some());
        let adapter = if pick.cross { pick.luid.and_then(adapter_by_luid) } else { mon_adapter };
        let (device, ctx, adapter_info) = create_device(adapter.as_ref())?;
        if pick.cross && pick.luid != Some(luid_u64(adapter_info.luid)) {
            return Err(CaptureError::Unsupported(format!(
                "no D3D11 device on the encoder's adapter (got '{}')",
                adapter_info.name
            )));
        }
        let adapter_name = adapter_info.name.clone();
        unsafe {
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        let winrt_device: IDirect3DDevice = unsafe {
            let dxgi: IDXGIDevice = device.cast().map_err(|e| os("IDXGIDevice", e))?;
            CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
                .map_err(|e| os("CreateDirect3D11DeviceFromDXGIDevice", e))?
        }
        .cast()
        .map_err(|e| os("IDirect3DDevice", e))?;

        let interop = windows_core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| os("IGraphicsCaptureItemInterop", e))?;
        let item: GraphicsCaptureItem = unsafe {
            match hwnd {
                Some(h) => interop
                    .CreateForWindow(h)
                    .map_err(|e| os("CreateForWindow", e))?,
                None => interop
                    .CreateForMonitor(hmon)
                    .map_err(|e| os("CreateForMonitor", e))?,
            }
        };
        let pool_size = item
            .Size()
            .map_err(|e| os("GraphicsCaptureItem::Size", e))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS,
            pool_size,
        )
        .map_err(|e| os("CreateFreeThreaded", e))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|e| os("CreateCaptureSession", e))?;

        let frame_ev = Arc::new(Event::new(false)?);
        let closed_ev = Arc::new(Event::new(true)?);
        let frame_token = {
            let ev = frame_ev.clone();
            pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
                ev.set();
                Ok(())
            }))
            .map_err(|e| os("FrameArrived", e))?
        };
        let closed_token = {
            let ev = closed_ev.clone();
            item.Closed(&TypedEventHandler::new(move |_, _| {
                ev.set();
                Ok(())
            }))
            .map_err(|e| os("Closed", e))?
        };

        if let Err(e) = session.SetIsCursorCaptureEnabled(cfg.capture_cursor) {
            tracing::warn!(
                hr = format!("0x{:08X}", e.code().0 as u32),
                "IsCursorCaptureEnabled not settable"
            );
        }
        let border_off = session.SetIsBorderRequired(false).is_ok();
        // Slightly below the target interval so vsync jitter never halves the rate; the Pacer
        // enforces the exact cap.
        let min_interval = TimeSpan {
            Duration: (10_000_000 / cfg.max_fps as i64) * 3 / 4,
        };
        let min_interval_ok = session.SetMinUpdateInterval(min_interval).is_ok();

        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
            .map_err(|e| os("CreateWaitableTimerExW", e))?
        };

        let conv = Converter::new(&device, &ctx)?;
        let (ow, oh) = fit(
            pool_size.Width.max(2) as u32,
            pool_size.Height.max(2) as u32,
            cfg.max_width,
            cfg.max_height,
        );
        // Built before StartCapture so a failing encoder never starts WGC (and errors out of
        // start_video_encoded synchronously).
        let sink = make_sink(&SinkCtx {
            device: &device,
            ctx: &ctx,
            adapter_luid: adapter_info.luid,
            adapter_vendor: adapter_info.vendor,
            adapter_name: &adapter_name,
            out_size: (ow, oh),
            cfg,
        })?;

        // A preview that cannot be set up never stops the share.
        let preview = preview.and_then(|p| match PreviewTap::new(&device, &ctx, p) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(error = %e, "screen preview unavailable");
                None
            }
        });

        session.StartCapture().map_err(|e| os("StartCapture", e))?;
        tracing::info!(
            target = %label,
            adapter = %adapter_name,
            cross_adapter = pick.cross,
            src = format!("{}x{}", pool_size.Width, pool_size.Height),
            out = format!("{ow}x{oh}"),
            max_fps = cfg.max_fps,
            cursor = cfg.capture_cursor,
            border_off,
            min_update_interval = min_interval_ok,
            sink = %sink.describe(),
            preview = preview.is_some(),
            "video capture started"
        );

        Ok(Self {
            target,
            hwnd,
            cfg,
            shared,
            device,
            ctx,
            winrt_device,
            item,
            pool,
            session,
            frame_ev,
            closed_ev,
            timer: OwnedHandle(timer),
            frame_token,
            closed_token,
            pool_size,
            conv,
            sink: Some(sink),
            pacer: Pacer::new(cfg.max_fps),
            deferred: None,
            out_size: (0, 0),
            consecutive_errors: 0,
            stats_gate: vox_log::RateLimiter::new(),
            preview,
        })
    }

    fn run(&mut self, stop: &Event) {
        let mut handles = vec![
            stop.raw(),
            self.closed_ev.raw(),
            self.frame_ev.raw(),
            self.sink().event(),
            self.timer.0,
        ];
        // 5: preview readback done, 6: preview settings changed, 7: preview pacing timer.
        if let Some(p) = &self.preview {
            handles.extend(p.handles());
        }
        let timeout = if self.hwnd.is_some() {
            WINDOW_CHECK_MS
        } else {
            INFINITE
        };
        let _ = self.stats_gate.ready(vox_log::STATS_INTERVAL); // arm: first stats line after one interval
        let reason: String = loop {
            let r = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
            let res = match r.0.wrapping_sub(WAIT_OBJECT_0.0) {
                0 => break "stopped by owner".into(),
                1 => break "capture target closed".into(),
                2 => self.on_frames(),
                3 => self.drain_sink(),
                4 => self.on_timer(),
                5..=7 if self.preview.is_some() => {
                    self.preview_event(r.0.wrapping_sub(WAIT_OBJECT_0.0));
                    Ok(())
                }
                _ if r == WAIT_TIMEOUT => {
                    if let Some(h) = self.hwnd {
                        if !unsafe { IsWindow(Some(h)) }.as_bool() {
                            break "target window no longer exists".into();
                        }
                    }
                    Ok(())
                }
                _ => {
                    break format!(
                        "WaitForMultipleObjects failed: {:?}",
                        windows_core::Error::from_thread()
                    )
                }
            };
            if let Err(e) = res {
                if let Some(fatal) = self.handle_error(e) {
                    break fatal;
                }
            }
            self.maybe_log_stats();
        };
        let s = self.shared.as_ref();
        tracing::info!(
            reason = %reason,
            target = ?self.target,
            delivered = s.delivered.load(Ordering::Relaxed),
            dropped = s.dropped.load(Ordering::Relaxed),
            "video capture stopped"
        );
    }

    /// A preview handle fired (index into `PreviewTap::handles`, offset by 5). Preview trouble
    /// never reaches the capture loop's error handling.
    fn preview_event(&mut self, idx: u32) {
        let Some(p) = self.preview.as_mut() else { return };
        let main = self.conv.output().map(|t| Main { tex: t, w: self.out_size.0, h: self.out_size.1 });
        match idx {
            5 => p.on_readback(&self.ctx),
            6 => p.on_wake(&self.device, &self.ctx, main),
            _ => p.on_timer(&self.device, &self.ctx, main),
        }
    }

    fn drain_sink(&mut self) -> Result<()> {
        let sink = self.sink.as_mut().expect("sink present until drop");
        sink.drain(&self.ctx, &self.shared)
    }

    fn handle_error(&mut self, e: CaptureError) -> Option<String> {
        let removed = unsafe { self.device.GetDeviceRemovedReason() };
        if let Err(r) = removed {
            if r.code() == DXGI_ERROR_DEVICE_REMOVED
                || r.code() == DXGI_ERROR_DEVICE_RESET
                || r.code().is_err()
            {
                return Some(format!(
                    "GPU device lost (hr=0x{:08X}): {e}",
                    r.code().0 as u32
                ));
            }
        }
        // Sinks report unrecoverable conditions (encoder failure) as `Stopped`.
        if let CaptureError::Stopped(m) = &e {
            return Some(m.clone());
        }
        self.shared.drop_frames(1);
        self.consecutive_errors += 1;
        if self.consecutive_errors == 1 || self.consecutive_errors.is_multiple_of(10) {
            tracing::warn!(error = %e, count = self.consecutive_errors, "video frame failed");
        }
        (self.consecutive_errors >= MAX_CONSECUTIVE_ERRORS)
            .then(|| format!("too many consecutive errors, last: {e}"))
    }

    fn on_frames(&mut self) -> Result<()> {
        let mut latest: Option<Direct3D11CaptureFrame> = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            if latest.replace(f).is_some() {
                self.shared.drop_frames(1);
            }
        }
        let Some(frame) = latest else { return Ok(()) };
        if self.deferred.take().is_some() {
            self.shared.drop_frames(1); // superseded by a newer frame
        }
        self.consider(frame, false)
    }

    fn on_timer(&mut self) -> Result<()> {
        match self.deferred.take() {
            Some(f) => self.consider(f, true),
            None => Ok(()),
        }
    }

    fn consider(&mut self, frame: Direct3D11CaptureFrame, from_timer: bool) -> Result<()> {
        let content = frame.ContentSize().map_err(|e| os("ContentSize", e))?;
        // Minimized window: pause (WGC mostly stops sending frames anyway). No GPU work, no spin.
        if let Some(h) = self.hwnd {
            if unsafe { IsIconic(h) }.as_bool() {
                return Ok(());
            }
        }
        // Degenerate sizes: the first frame after restoring a minimized window still carries the
        // minimized caption size (~160x28); skipping it avoids two needless pool recreations.
        if content.Width < MIN_CONTENT || content.Height < MIN_CONTENT {
            return Ok(());
        }
        let resized = content != self.pool_size;
        let ts_us = frame
            .SystemRelativeTime()
            .map_err(|e| os("SystemRelativeTime", e))?
            .Duration
            / 10;

        if let Some(cap) = self.sink.as_ref().and_then(|s| s.fps_cap()) {
            self.pacer.set_fps(cap.clamp(1, self.cfg.max_fps));
        }
        if !resized {
            let pace_ts = if from_timer {
                monotonic_now_us()
            } else {
                ts_us
            };
            if let Pace::Early(wait_us) = self.pacer.check(pace_ts) {
                // Keep the newest frame and deliver it when due, so the last change on a
                // now-static screen is never lost. Superseded if another frame arrives first.
                self.deferred = Some(frame);
                let due: i64 = -(wait_us.max(500) * 10); // relative, 100 ns units
                unsafe { SetWaitableTimer(self.timer.0, &due, 0, None, None, false) }
                    .map_err(|e| os("SetWaitableTimer", e))?;
                return Ok(());
            }
        }

        let processed = if self.sink().has_free() {
            self.process(&frame, content, ts_us)
        } else {
            self.shared.drop_frames(1);
            Ok(())
        };
        let _ = frame.Close();
        drop(frame);

        // Recreate the pool even if this frame failed, or every later frame would fail too.
        if resized {
            self.pool
                .Recreate(
                    &self.winrt_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    POOL_BUFFERS,
                    content,
                )
                .map_err(|e| os("FramePool::Recreate", e))?;
            tracing::info!(
                from = format!("{}x{}", self.pool_size.Width, self.pool_size.Height),
                to = format!("{}x{}", content.Width, content.Height),
                "capture target resized"
            );
            self.pool_size = content;
        }
        processed?;
        // Deliver anything already finished. Without a fence this is synchronous (Map waits).
        self.drain_sink()
    }

    fn process(
        &mut self,
        frame: &Direct3D11CaptureFrame,
        content: SizeInt32,
        ts_us: i64,
    ) -> Result<()> {
        let tex: ID3D11Texture2D = unsafe {
            let surface = frame.Surface().map_err(|e| os("Frame::Surface", e))?;
            let access: IDirect3DDxgiInterfaceAccess = surface
                .cast()
                .map_err(|e| os("IDirect3DDxgiInterfaceAccess", e))?;
            access
                .GetInterface()
                .map_err(|e| os("GetInterface(ID3D11Texture2D)", e))?
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let src_w = (content.Width as u32).min(desc.Width);
        let src_h = (content.Height as u32).min(desc.Height);
        let (ow, oh) = fit(src_w, src_h, self.cfg.max_width, self.cfg.max_height);
        if (ow, oh) != self.out_size {
            if self.out_size != (0, 0) {
                tracing::info!(w = ow, h = oh, "video output size changed");
            }
            self.out_size = (ow, oh);
        }
        self.conv
            .ensure(&self.device, desc.Width, desc.Height, ow, oh)?;
        let out = self.conv.convert(&tex, src_w, src_h)?.clone();
        let sink = self.sink.as_mut().expect("sink present until drop");
        sink.submit(&self.ctx, &out, ow, oh, ts_us)?;
        self.consecutive_errors = 0;
        if let Some(p) = self.preview.as_mut() {
            // After the encoder got its frame, and never able to fail it.
            let bgra = Bgra { tex: &tex, tex_w: desc.Width, tex_h: desc.Height, w: src_w, h: src_h };
            p.on_frame(&self.device, &self.ctx, Main { tex: &out, w: ow, h: oh }, bgra);
        }
        Ok(())
    }

    fn maybe_log_stats(&mut self) {
        if !self.stats_gate.ready(vox_log::STATS_INTERVAL) {
            return;
        }
        let out = self.out_size;
        let sink = self.sink.as_mut().expect("sink present until drop");
        sink.log_stats(&self.shared, out);
        if let Some(p) = self.preview.as_mut() {
            p.log_stats();
        }
    }
}

impl<S: Sink> Drop for Capture<S> {
    fn drop(&mut self) {
        self.deferred = None;
        let _ = self.pool.RemoveFrameArrived(self.frame_token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
        // Encoder / readback first: it holds GPU resources and may still reference our textures.
        self.sink = None;
        self.preview = None;
        let _ = self.winrt_device.Close();
        unsafe {
            self.ctx.ClearState();
            self.ctx.Flush();
        }
        tracing::debug!(
            frame_ev_refs = Arc::strong_count(&self.frame_ev),
            closed_ev_refs = Arc::strong_count(&self.closed_ev),
            "capture teardown"
        );
    }
}

pub(crate) struct AdapterInfo {
    pub name: String,
    pub luid: LUID,
    /// PCI vendor id (0x10DE NVIDIA, 0x1002 AMD, 0x8086 Intel).
    pub vendor: u32,
}

pub(crate) fn create_device(
    adapter: Option<&IDXGIAdapter>,
) -> Result<(ID3D11Device, ID3D11DeviceContext, AdapterInfo)> {
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    let levels: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let attempt =
        |a: Option<&IDXGIAdapter>| -> windows_core::Result<(ID3D11Device, ID3D11DeviceContext)> {
            let mut dev = None;
            let mut ctx = None;
            let driver = if a.is_some() {
                D3D_DRIVER_TYPE_UNKNOWN
            } else {
                D3D_DRIVER_TYPE_HARDWARE
            };
            unsafe {
                D3D11CreateDevice(
                    a,
                    driver,
                    HMODULE::default(),
                    flags,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut dev),
                    None,
                    Some(&mut ctx),
                )?
            };
            Ok((dev.unwrap(), ctx.unwrap()))
        };
    let (dev, ctx) = match attempt(adapter) {
        Ok(v) => v,
        Err(e) if adapter.is_some() => {
            tracing::warn!(
                hr = format!("0x{:08X}", e.code().0 as u32),
                "D3D11 device on monitor's adapter failed; using default"
            );
            attempt(None).map_err(|e| os("D3D11CreateDevice", e))?
        }
        Err(e) => return Err(os("D3D11CreateDevice", e)),
    };
    let info = (|| -> windows_core::Result<AdapterInfo> {
        let dxgi: IDXGIDevice = dev.cast()?;
        let a = unsafe { dxgi.GetAdapter()? };
        let d = unsafe { a.GetDesc()? };
        Ok(AdapterInfo {
            name: wide_to_string(&d.Description),
            luid: d.AdapterLuid,
            vendor: d.VendorId,
        })
    })()
    .unwrap_or(AdapterInfo {
        name: String::new(),
        luid: LUID::default(),
        vendor: 0,
    });
    Ok((dev, ctx, info))
}
