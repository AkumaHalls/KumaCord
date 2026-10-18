//! Offline encoder benchmark: CPU-generated BGRA frames -> D3D11 texture -> the capture path's
//! video processor (BGRA -> NV12) -> the production [`EncoderSink`] (rate compensation, keyframe
//! gate, degradation policy) -> the hardware encoder. Frames are fed one at a time (submit, wait
//! for the access unit), so the measured latency is the encoder's own and nothing is dropped.
//!
//! Used by `capture_probe bench`; scored there against the NV12 the encoder actually received.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};
use windows::Win32::System::Threading::{GetCurrentProcess, WaitForSingleObject};
use windows::Win32::System::WindowsProgramming::QueryProcessCycleTime;

use super::encode::tuning::{MfTuning, NvTuning, Tuning};
use super::encode::{self, EncParams, EncShared, EncoderSink};
use super::video::{self, Shared, Sink, SinkCtx};
use super::{os, ComInit, Event};
use crate::{
    CaptureError, DegradationPreference, EncodedFrame, EncoderBackend, EncoderControl, Nv12Frame,
    Result, VideoConfig,
};

/// One benchmark run.
#[derive(Clone, Debug)]
pub struct BenchSpec {
    pub width: u32,
    pub height: u32,
    /// Configured (nominal) encoder frame rate.
    pub fps: u32,
    /// Rate the source delivers frames at (timestamps). Lower than `fps` simulates window
    /// capture, which only produces a frame when the window changes.
    pub input_fps: f64,
    pub frames: u32,
    pub bitrate_bps: u32,
    pub backend: EncoderBackend,
    /// NVENC / MF overrides on top of the v0.2 defaults (see `NvTuning::parse`,
    /// `MfTuning::parse`); `"v01"` selects the v0.1 settings.
    pub nv: String,
    pub mf: String,
    /// Input-rate compensation of the encoder bitrate (v0.2). `false` = v0.1 behaviour.
    pub rate_compensation: bool,
    /// Request a keyframe (like a viewer's PLI) every N source frames; 0 = never.
    pub idr_every: u32,
    /// `EncoderControl::set_bitrate` at these source frame indices (simulated congestion).
    pub bitrate_steps: Vec<(u32, u32)>,
    pub degradation: DegradationPreference,
    /// Honour the degradation policy's fps cap by skipping source frames (like the pacer).
    pub apply_fps_cap: bool,
}

/// Per encoded frame.
#[derive(Clone, Copy, Debug)]
pub struct BenchFrame {
    /// Source frame index (frames skipped by the fps cap are absent).
    pub index: u32,
    pub bytes: u32,
    pub keyframe: bool,
    /// Submit (after the GPU colour conversion was queued) to access unit, µs.
    pub latency_us: u32,
    /// Encoder bitrate / fps cap in force for this frame.
    pub encoder_bps: u32,
    pub fps_cap: u32,
}

#[derive(Clone, Debug)]
pub struct BenchResult {
    pub encoder: String,
    pub backend: EncoderBackend,
    pub frames: Vec<BenchFrame>,
    /// Process CPU (all threads, user + kernel) spent per frame in convert + encode + output,
    /// excluding frame generation, upload and the reference readback.
    pub cpu_us_per_frame: f64,
    pub keyframe_requests_coalesced: u64,
}

struct Collected {
    data: Vec<u8>,
    keyframe: bool,
    at: Option<Instant>,
    count: u64,
}

pub(crate) fn tex2d(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    usage: D3D11_USAGE,
    bind: u32,
    cpu: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut t = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)) }
        .map_err(|e| os("CreateTexture2D(bench)", e))?;
    t.ok_or_else(|| CaptureError::Os("CreateTexture2D(bench) null".into()))
}

/// Block until the GPU finished everything queued so far (fence + event, no spinning).
pub(crate) struct GpuIdle {
    fence: ID3D11Fence,
    ctx4: ID3D11DeviceContext4,
    value: u64,
    ev: Event,
}

impl GpuIdle {
    pub(crate) fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext) -> Result<Self> {
        let dev5: ID3D11Device5 = device.cast().map_err(|e| os("ID3D11Device5", e))?;
        let ctx4: ID3D11DeviceContext4 = ctx.cast().map_err(|e| os("ID3D11DeviceContext4", e))?;
        let mut fence: Option<ID3D11Fence> = None;
        unsafe { dev5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence) }
            .map_err(|e| os("CreateFence", e))?;
        Ok(Self {
            fence: fence.ok_or_else(|| CaptureError::Os("fence null".into()))?,
            ctx4,
            value: 0,
            ev: Event::new(false)?,
        })
    }

    pub(crate) fn wait(&mut self) -> Result<()> {
        self.value += 1;
        unsafe {
            self.ctx4
                .Signal(&self.fence, self.value)
                .map_err(|e| os("Signal", e))?;
            self.ctx4.Flush();
            self.fence
                .SetEventOnCompletion(self.value, self.ev.raw())
                .map_err(|e| os("SetEventOnCompletion", e))?;
            if WaitForSingleObject(self.ev.raw(), 5000) != WAIT_OBJECT_0 {
                return Err(CaptureError::Stopped("GPU did not go idle".into()));
            }
        }
        Ok(())
    }
}

fn process_cycles() -> u64 {
    let mut c = 0u64;
    unsafe {
        let _ = QueryProcessCycleTime(GetCurrentProcess(), &mut c);
    }
    c
}

fn tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

/// Run one benchmark. `gen(index, bgra, stride)` paints source frame `index` (time
/// `index / input_fps`); `on_source` receives the NV12 the encoder was given (only frames that
/// were encoded); `on_output` receives each access unit (Annex-B).
pub fn run(
    spec: &BenchSpec,
    gen: &mut dyn FnMut(u32, &mut [u8], usize),
    mut on_source: Option<&mut dyn FnMut(&Nv12Frame<'_>)>,
    on_output: &mut dyn FnMut(&[u8]),
) -> Result<BenchResult> {
    let _com = ComInit::mta();
    let (w, h) = (spec.width & !1, spec.height & !1);
    let tuning = Tuning {
        nv: NvTuning::DEFAULT
            .parse(&spec.nv)
            .map_err(CaptureError::Unsupported)?,
        mf: MfTuning::DEFAULT
            .parse(&spec.mf)
            .map_err(CaptureError::Unsupported)?,
    };
    let hmon = super::enumerate::monitors_with_handles().ok().and_then(|m| {
        let primary = m.iter().position(|(_, i)| i.primary).unwrap_or(0);
        m.into_iter().nth(primary).map(|(h, _)| h)
    });
    let adapter = hmon.and_then(super::enumerate::adapter_for_monitor);
    let (device, ctx, info) = video::create_device(adapter.as_ref())?;
    let mut conv = video::Converter::new(&device, &ctx)?;
    conv.ensure(&device, w, h, w, h)?;
    let src = tex2d(
        &device,
        w,
        h,
        DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_USAGE_DEFAULT,
        (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        0,
    )?;
    let staging = tex2d(
        &device,
        w,
        h,
        DXGI_FORMAT_NV12,
        D3D11_USAGE_STAGING,
        0,
        D3D11_CPU_ACCESS_READ.0 as u32,
    )?;
    let mut idle = GpuIdle::new(&device, &ctx)?;

    let p = EncParams {
        width: w,
        height: h,
        fps: spec.fps,
        bitrate: spec.bitrate_bps,
        max_width: w,
        max_height: h,
        tuning,
    };
    let ev = Arc::new(Event::new(true)?);
    let sc = SinkCtx {
        device: &device,
        ctx: &ctx,
        adapter_luid: info.luid,
        adapter_vendor: info.vendor,
        adapter_name: &info.name,
        out_size: (w, h),
        cfg: VideoConfig {
            max_fps: spec.fps,
            max_width: w,
            max_height: h,
            capture_cursor: false,
        },
    };
    let enc = encode::create_encoder(spec.backend, &sc, p, &ev)?;
    let backend = enc.backend();
    let collected = Arc::new(Mutex::new(Collected {
        data: Vec::with_capacity(1 << 20),
        keyframe: false,
        at: None,
        count: 0,
    }));
    let cb = {
        let c = collected.clone();
        Box::new(move |f: &EncodedFrame<'_>| {
            let mut c = c.lock().unwrap();
            c.data.clear();
            c.data.extend_from_slice(f.data);
            c.keyframe = f.keyframe;
            c.at = Some(Instant::now());
            c.count += 1;
        })
    };
    let control = EncoderControl::default();
    let es = Arc::new(EncShared::default());
    // Without the pacer emulation an fps cap would only skew the rate compensation.
    let degradation = if spec.apply_fps_cap { spec.degradation } else { DegradationPreference::Smoothness };
    let mut sink = EncoderSink::new(enc, ev, control.clone(), cb, es, p, degradation);
    sink.set_rate_compensation(spec.rate_compensation);
    let encoder = sink.name();
    let shared = Shared::new_running();

    let pitch = w as usize * 4;
    let mut bgra = vec![0u8; pitch * h as usize];
    let mut frames = Vec::with_capacity(spec.frames as usize);
    let mut cycles = 0u64;
    let (tsc0, t0) = (tsc(), Instant::now());
    let mut next_due_us: Option<i64> = None;
    let wait_for = |sink: &mut EncoderSink, cond: &dyn Fn(&EncoderSink) -> bool| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !cond(sink) {
            if Instant::now() > deadline {
                return Err(CaptureError::Stopped("benchmark: encoder timed out".into()));
            }
            unsafe { WaitForSingleObject(sink.event(), 50) };
            sink.drain(&ctx, &shared)?;
        }
        Ok(())
    };

    for i in 0..spec.frames {
        let ts_us = (i as f64 * 1e6 / spec.input_fps) as i64 + 1_000_000;
        for &(at, bps) in &spec.bitrate_steps {
            if at == i {
                control.set_bitrate(bps);
            }
        }
        if spec.idr_every > 0 && i > 0 && i % spec.idr_every == 0 {
            control.request_keyframe();
        }
        // Pacer emulation (capture loop): honour the policy's fps cap.
        if spec.apply_fps_cap {
            let cap = sink.fps_cap().unwrap_or(spec.fps).clamp(1, spec.fps);
            let interval = 1_000_000 / cap as i64;
            if let Some(due) = next_due_us {
                if ts_us < due - interval / 4 {
                    continue;
                }
                next_due_us = Some(if ts_us - due > interval { ts_us + interval } else { due + interval });
            } else {
                next_due_us = Some(ts_us + interval);
            }
        }

        gen(i, &mut bgra, pitch);
        unsafe {
            ctx.UpdateSubresource(&src, 0, None, bgra.as_ptr().cast(), pitch as u32, 0);
        }
        idle.wait()?;

        let before = collected.lock().unwrap().count;
        let c0 = process_cycles();
        let out = conv.convert(&src, w, h)?.clone();
        wait_for(&mut sink, &|s| s.has_free())?;
        let t_submit = Instant::now();
        sink.submit(&ctx, &out, w, h, ts_us)?;
        let coll = collected.clone();
        wait_for(&mut sink, &move |_| coll.lock().unwrap().count > before)?;
        cycles += process_cycles() - c0;

        {
            let c = collected.lock().unwrap();
            on_output(&c.data);
            frames.push(BenchFrame {
                index: i,
                bytes: c.data.len() as u32,
                keyframe: c.keyframe,
                latency_us: c.at.map_or(0, |a| a.duration_since(t_submit).as_micros() as u32),
                encoder_bps: sink.encoder_bitrate(),
                fps_cap: sink.fps_cap().unwrap_or(spec.fps),
            });
        }

        if let Some(cb) = on_source.as_mut() {
            unsafe {
                ctx.CopyResource(&staging, &out);
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))
                    .map_err(|e| os("Map(bench staging)", e))?;
                let pitch = m.RowPitch as usize;
                let base = m.pData as *const u8;
                let f = Nv12Frame {
                    width: w,
                    height: h,
                    timestamp_us: ts_us,
                    y: std::slice::from_raw_parts(base, pitch * h as usize),
                    y_stride: pitch as u32,
                    uv: std::slice::from_raw_parts(base.add(pitch * h as usize), pitch * (h as usize / 2)),
                    uv_stride: pitch as u32,
                };
                cb(&f);
                ctx.Unmap(&staging, 0);
            }
        }
    }
    let secs = t0.elapsed().as_secs_f64().max(1e-6);
    let tsc_hz = (tsc() - tsc0) as f64 / secs;
    let n = frames.len().max(1) as f64;
    let cpu_us_per_frame = if tsc_hz > 0.0 {
        cycles as f64 / tsc_hz * 1e6 / n
    } else {
        0.0
    };
    let coalesced = sink.keyframe_requests_coalesced();
    drop(sink);
    Ok(BenchResult {
        encoder,
        backend,
        frames,
        cpu_us_per_frame,
        keyframe_requests_coalesced: coalesced,
    })
}
