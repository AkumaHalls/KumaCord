//! Encoder self-test (v0.7): synthetic 720p frames with motion and text -> the capture path's
//! video processor -> the production [`EncoderSink`] -> one candidate's hardware encoder, with a
//! bitrate cut and a requested IDR on the way; then every access unit through the Microsoft
//! software H.264 decoder (the same kind of decoder viewers run: FFmpeg in libwebrtc is just as
//! strict about broken references). Frames are fed one at a time, like the benchmark.
//!
//! What it catches: encoders that do not start, produce undecodable or corrupted pictures, ignore
//! keyframe requests or a lower bitrate (WebRTC's frame dropper then breaks the reference chain
//! for every viewer), or are too slow. About half a second per candidate.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Interface;
use std::mem::ManuallyDrop;

use super::{create_encoder, mf_startup, EncParams, EncShared, EncoderSink, Tuning};
use crate::gpu::{self, EncoderCandidate, SelfTestMeasure, SelfTestResult};
use crate::win::adapters::{adapter_by_luid, luid_u64};
use crate::win::bench::{tex2d, GpuIdle};
use crate::win::video::{self, Shared, Sink, SinkCtx};
use crate::win::{os, ComInit, Event};
use crate::{CaptureError, DegradationPreference, EncodedFrame, EncoderControl, Result, VideoConfig};

const W: u32 = gpu::SELFTEST_WIDTH;
const H: u32 = gpu::SELFTEST_HEIGHT;
const FPS: u32 = gpu::SELFTEST_FPS;
/// Frames whose converter output is read back as the PSNR reference.
fn sampled(i: u32) -> bool {
    i % 10 == 5
}
/// Delta frames from here on count for the bitrate (the encoder had 10 frames to follow the cut).
const RATE_FROM: u32 = gpu::SELFTEST_RATE_CHANGE_AT + 10;

pub fn self_test_encoders(cands: &[EncoderCandidate], on_result: &mut dyn FnMut(&SelfTestResult)) -> Vec<SelfTestResult> {
    let _com = ComInit::mta();
    let mut done: Vec<(EncoderCandidate, SelfTestResult)> = Vec::new();
    for c in cands.iter().filter(|c| c.backend().is_some()) {
        let t0 = Instant::now();
        let nvenc_poisoned = c.kind == gpu::EncoderKind::Nvenc && super::NVIDIA_MFT_USED.load(std::sync::atomic::Ordering::Relaxed);
        let r = match gpu::should_test(c, &done) {
            Err(reason) => SelfTestResult::skipped(&c.id, reason),
            Ok(()) if nvenc_poisoned => SelfTestResult::skipped(&c.id, "o MFT da NVIDIA já foi usado nesta sessão; NVENC fica para o próximo teste"),
            Ok(()) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test_one(c))) {
                Ok(Ok(m)) => SelfTestResult::from_measure(&c.id, &m),
                Ok(Err(e)) => SelfTestResult::failed(&c.id, format!("não iniciou: {e}")),
                Err(_) => SelfTestResult::failed(&c.id, "erro interno no teste"),
            },
        };
        let g = c.gpu.as_ref();
        let verdict = match &r.outcome {
            gpu::TestOutcome::Passed => "pass".to_string(),
            gpu::TestOutcome::Failed { reason } => format!("FAIL ({reason})"),
            gpu::TestOutcome::Skipped { reason } => format!("skipped ({reason})"),
        };
        tracing::info!(
            adapter = %g.map(|g| g.name.as_str()).unwrap_or("-"),
            backend = c.kind.tag(),
            result = %verdict,
            ms_per_frame = format!("{:.1}", r.ms_per_frame),
            kbps = r.kbps,
            target_kbps = r.target_kbps,
            decoded = format!("{}/{}", r.decoded, r.frames),
            psnr_db = format!("{:.1}", r.psnr_db),
            test_ms = t0.elapsed().as_millis() as u64,
            "encoder self-test"
        );
        on_result(&r);
        done.push((c.clone(), r));
    }
    done.into_iter().map(|(_, r)| r).collect()
}

/// One encoded access unit.
struct Au {
    data: Vec<u8>,
    keyframe: bool,
    latency_us: u64,
}

fn test_one(c: &EncoderCandidate) -> Result<SelfTestMeasure> {
    let backend = c.backend().ok_or_else(|| CaptureError::Unsupported("processor".into()))?;
    let luid = c.luid().ok_or_else(|| CaptureError::Unsupported("no adapter".into()))?;
    let adapter = adapter_by_luid(luid).ok_or_else(|| CaptureError::NotFound("adapter gone".into()))?;
    let (device, ctx, info) = video::create_device(Some(&adapter))?;
    if luid_u64(info.luid) != luid {
        return Err(CaptureError::Unsupported(format!("no D3D11 device on '{}'", info.name)));
    }
    let mut conv = video::Converter::new(&device, &ctx)?;
    conv.ensure(&device, W, H, W, H)?;
    let bind = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
    let src = tex2d(&device, W, H, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_USAGE_DEFAULT, bind, 0)?;
    let staging = tex2d(&device, W, H, DXGI_FORMAT_NV12, D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32)?;
    let mut idle = GpuIdle::new(&device, &ctx)?;

    let p = EncParams {
        width: W,
        height: H,
        fps: FPS,
        bitrate: gpu::SELFTEST_BPS_START,
        max_width: W,
        max_height: H,
        tuning: Tuning::default(),
    };
    let ev = Arc::new(Event::new(true)?);
    let sc = SinkCtx {
        device: &device,
        ctx: &ctx,
        adapter_luid: info.luid,
        adapter_vendor: info.vendor,
        adapter_name: &info.name,
        out_size: (W, H),
        cfg: VideoConfig { max_fps: FPS, max_width: W, max_height: H, capture_cursor: false },
    };
    let enc = create_encoder(backend, &sc, p, &ev)?;
    if enc.backend() != backend {
        return Err(CaptureError::Unsupported(format!("got {:?} instead", enc.backend())));
    }
    let out: Arc<Mutex<Vec<Au>>> = Arc::new(Mutex::new(Vec::with_capacity(gpu::SELFTEST_FRAMES as usize)));
    let submitted = Arc::new(Mutex::new(Instant::now()));
    let cb = {
        let (out, submitted) = (out.clone(), submitted.clone());
        Box::new(move |f: &EncodedFrame<'_>| {
            let at = submitted.lock().unwrap_or_else(|p| p.into_inner()).elapsed().as_micros() as u64;
            out.lock().unwrap_or_else(|p| p.into_inner()).push(Au { data: f.data.to_vec(), keyframe: f.keyframe, latency_us: at });
        })
    };
    let control = EncoderControl::default();
    let mut sink = EncoderSink::new(enc, ev, control.clone(), cb, Arc::new(EncShared::default()), p, DegradationPreference::Smoothness);
    let shared = Shared::new_running();
    let count = |o: &Arc<Mutex<Vec<Au>>>| o.lock().unwrap_or_else(|p| p.into_inner()).len();
    let wait_for = |sink: &mut EncoderSink, cond: &dyn Fn(&EncoderSink) -> bool| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !cond(sink) {
            if Instant::now() > deadline {
                return Err(CaptureError::Stopped("encoder timed out".into()));
            }
            unsafe { WaitForSingleObject(sink.event(), 50) };
            sink.drain(&ctx, &shared)?;
        }
        Ok(())
    };

    let pitch = W as usize * 4;
    let mut bgra = vec![0u8; pitch * H as usize];
    let mut refs: Vec<(u32, Vec<u8>)> = Vec::new();
    for i in 0..gpu::SELFTEST_FRAMES {
        let ts_us = (i as i64 * 1_000_000 / FPS as i64) + 1_000_000;
        if i == gpu::SELFTEST_RATE_CHANGE_AT {
            control.set_bitrate(gpu::SELFTEST_BPS_LOW);
        }
        if i == gpu::SELFTEST_FORCED_IDR_AT {
            control.request_keyframe();
        }
        paint(i, &mut bgra, pitch);
        unsafe { ctx.UpdateSubresource(&src, 0, None, bgra.as_ptr().cast(), pitch as u32, 0) };
        idle.wait()?;
        let before = count(&out);
        let frame = conv.convert(&src, W, H)?.clone();
        wait_for(&mut sink, &|s| s.has_free())?;
        *submitted.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        sink.submit(&ctx, &frame, W, H, ts_us)?;
        let o = out.clone();
        wait_for(&mut sink, &move |_| count(&o) > before)?;
        if sampled(i) {
            refs.push((i, read_y(&ctx, &staging, &frame)?));
        }
    }
    drop(sink);
    let aus = std::mem::take(&mut *out.lock().unwrap_or_else(|p| p.into_inner()));

    let n = aus.len() as u32;
    let mut m = SelfTestMeasure {
        frames: gpu::SELFTEST_FRAMES,
        access_units: n,
        first_is_idr: aus.first().is_some_and(|a| a.keyframe),
        forced_idr_ok: aus.get(gpu::SELFTEST_FORCED_IDR_AT as usize).is_some_and(|a| a.keyframe),
        target_kbps: gpu::SELFTEST_BPS_LOW / 1000,
        ..Default::default()
    };
    m.ms_per_frame = aus.iter().map(|a| a.latency_us).sum::<u64>() as f32 / n.max(1) as f32 / 1000.0;
    let deltas: Vec<usize> = aus.iter().skip(RATE_FROM as usize).filter(|a| !a.keyframe).map(|a| a.data.len()).collect();
    if !deltas.is_empty() {
        let avg = deltas.iter().sum::<usize>() as f64 / deltas.len() as f64;
        m.kbps = (avg * 8.0 * FPS as f64 / 1000.0) as u32;
    }
    let (decoded, psnr) = decode_all(&aus, &refs);
    m.decoded = decoded;
    m.psnr_db = psnr;
    Ok(m)
}

/// The Y plane of the converter's NV12 output (`W` bytes per row).
fn read_y(ctx: &ID3D11DeviceContext, staging: &ID3D11Texture2D, frame: &ID3D11Texture2D) -> Result<Vec<u8>> {
    unsafe {
        ctx.CopyResource(staging, frame);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).map_err(|e| os("Map(self-test)", e))?;
        let pitch = m.RowPitch as usize;
        let base = m.pData as *const u8;
        let mut y = vec![0u8; (W * H) as usize];
        for r in 0..H as usize {
            std::ptr::copy_nonoverlapping(base.add(r * pitch), y.as_mut_ptr().add(r * W as usize), W as usize);
        }
        ctx.Unmap(staging, 0);
        Ok(y)
    }
}

/// Synthetic screen content, deterministic: a page of pseudo-text scrolling up on the left, a
/// panning noise with a bouncing box on the right (like a browser next to a game).
fn paint(i: u32, buf: &mut [u8], pitch: usize) {
    let (w, h) = (W as usize, H as usize);
    let scroll = i as usize * 3;
    let (bx, by) = (w / 2 + (i as usize * 13) % (w / 2 - 160), (i as usize * 7) % (h - 120));
    for y in 0..h {
        let row = &mut buf[y * pitch..y * pitch + w * 4];
        let ty = y + scroll;
        let (line, ly) = (ty / 20, ty % 20);
        for x in 0..w {
            let px: [u8; 4] = if x < w / 2 {
                // 9x20 cells; a "glyph" is a few strokes picked by a hash, every 6th cell a space.
                let (col, lx) = (x / 9, x % 9);
                let hsh = (line as u32).wrapping_mul(7919) ^ (col as u32).wrapping_mul(104_729);
                let stroke = (lx == 1 && hsh & 1 != 0)
                    || (lx == 5 && hsh & 2 != 0)
                    || (ly == 4 && hsh & 4 != 0)
                    || (ly == 9 && hsh & 8 != 0)
                    || (ly == 14 && hsh & 16 != 0);
                let ink = (4..15).contains(&ly) && (1..6).contains(&lx) && col % 6 != 5 && stroke;
                if ink {
                    [30, 30, 30, 255]
                } else {
                    [245, 245, 245, 255]
                }
            } else if (bx..bx + 160).contains(&x) && (by..by + 120).contains(&y) {
                [40, 90, 230, 255]
            } else {
                // Panning gradient with faint 4x4 noise: keeps the rate control busy, so an encoder that
                // ignores the target shows it.
                let xs = x + i as usize * 4;
                let n = ((xs / 4) as u32).wrapping_mul(2_654_435_761) ^ ((y / 4) as u32).wrapping_mul(40_503);
                let v = (n.rotate_left(13).wrapping_mul(2_246_822_519) >> 27) as u8; // 0..31
                let g = (xs / 5) as u8;
                [g.wrapping_add(v), ((y / 3) as u8).wrapping_add(v), 160 - v, 255]
            };
            row[x * 4..x * 4 + 4].copy_from_slice(&px);
        }
    }
}

/// Y PSNR between the reference and a decoded plane, over the text page (the left half; the noise
/// on the right is meant to be lossy), every other pixel and row.
fn psnr(reference: &[u8], dec: *const u8, pitch: usize) -> f32 {
    let mut se = 0u64;
    let mut n = 0u64;
    for y in (0..H as usize).step_by(2) {
        for x in (0..W as usize / 2).step_by(2) {
            let a = reference[y * W as usize + x] as i64;
            let b = unsafe { *dec.add(y * pitch + x) } as i64;
            se += ((a - b) * (a - b)) as u64;
            n += 1;
        }
    }
    let mse = se as f64 / n.max(1) as f64;
    if mse < 1e-9 {
        return 99.0;
    }
    (10.0 * (255.0 * 255.0 / mse).log10()) as f32
}

/// Decode every access unit with the Microsoft software H.264 decoder. Returns the number of
/// pictures that came out and the worst PSNR over the sampled frames (0 when none could be
/// compared). Any decoder error stops it: the count then stays below the input.
fn decode_all(aus: &[Au], refs: &[(u32, Vec<u8>)]) -> (u32, f32) {
    let mut d = match Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "self-test: H.264 decoder unavailable; only the encoder is checked");
            return (aus.len() as u32, 0.0);
        }
    };
    let mut worst = f32::MAX;
    let mut on_pic = |idx: u32, data: *const u8, pitch: usize| {
        if let Some((_, r)) = refs.iter().find(|(i, _)| *i == idx) {
            worst = worst.min(psnr(r, data, pitch));
        }
    };
    let res = (|| -> Result<()> {
        for (k, au) in aus.iter().enumerate() {
            d.feed(k as u32, &au.data, &mut on_pic)?;
        }
        d.drain(&mut on_pic)
    })();
    if let Err(e) = res {
        tracing::debug!(error = %e, decoded = d.decoded, "self-test: decode error");
    }
    (d.decoded, if worst == f32::MAX { 0.0 } else { worst })
}

const FRAME_HNS: i64 = 10_000_000 / FPS as i64;

struct Decoder {
    t: IMFTransform,
    out_cb: u32,
    provides: bool,
    sample: Option<IMFSample>,
    decoded: u32,
}

impl Decoder {
    fn new() -> Result<Self> {
        mf_startup()?;
        unsafe {
            let t: IMFTransform = CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| os("CoCreateInstance(H.264 decoder)", e))?;
            if let Ok(a) = t.GetAttributes() {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let it = MFCreateMediaType().map_err(|e| os("MFCreateMediaType", e))?;
            it.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok();
            it.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).ok();
            it.SetUINT64(&MF_MT_FRAME_SIZE, ((W as u64) << 32) | H as u64).ok();
            it.SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1).ok();
            it.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).ok();
            t.SetInputType(0, &it, 0).map_err(|e| os("decoder SetInputType", e))?;
            let mut me = Self { t, out_cb: 0, provides: false, sample: None, decoded: 0 };
            me.set_output()?;
            me.t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ok();
            me.t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ok();
            Ok(me)
        }
    }

    fn set_output(&mut self) -> Result<()> {
        unsafe {
            let mut chosen = None;
            for i in 0.. {
                let Ok(t) = self.t.GetOutputAvailableType(0, i) else { break };
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    chosen = Some(t);
                    break;
                }
            }
            let t = chosen.ok_or_else(|| CaptureError::Unsupported("decoder has no NV12 output".into()))?;
            self.t.SetOutputType(0, &t, 0).map_err(|e| os("decoder SetOutputType", e))?;
            let info = self.t.GetOutputStreamInfo(0).map_err(|e| os("decoder GetOutputStreamInfo", e))?;
            self.provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32 != 0;
            self.out_cb = info.cbSize.max(W * H * 3 / 2);
            self.sample = None;
            Ok(())
        }
    }

    fn feed(&mut self, k: u32, data: &[u8], on_pic: &mut dyn FnMut(u32, *const u8, usize)) -> Result<()> {
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32).map_err(|e| os("MFCreateMemoryBuffer", e))?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None).map_err(|e| os("Lock", e))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            let _ = buf.Unlock();
            buf.SetCurrentLength(data.len() as u32).map_err(|e| os("SetCurrentLength", e))?;
            let s = MFCreateSample().map_err(|e| os("MFCreateSample", e))?;
            s.AddBuffer(&buf).map_err(|e| os("AddBuffer", e))?;
            s.SetSampleTime(k as i64 * FRAME_HNS).ok();
            s.SetSampleDuration(FRAME_HNS).ok();
            if let Err(e) = self.t.ProcessInput(0, &s, 0) {
                if e.code() != MF_E_NOTACCEPTING {
                    return Err(os("decoder ProcessInput", e));
                }
                self.pull(on_pic)?;
                self.t.ProcessInput(0, &s, 0).map_err(|e| os("decoder ProcessInput (retry)", e))?;
            }
            self.pull(on_pic)
        }
    }

    fn drain(&mut self, on_pic: &mut dyn FnMut(u32, *const u8, usize)) -> Result<()> {
        unsafe { self.t.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.map_err(|e| os("decoder drain", e))?;
        self.pull(on_pic)
    }

    /// Take every picture the decoder has ready.
    fn pull(&mut self, on_pic: &mut dyn FnMut(u32, *const u8, usize)) -> Result<()> {
        loop {
            unsafe {
                if !self.provides && self.sample.is_none() {
                    let s = MFCreateSample().map_err(|e| os("MFCreateSample", e))?;
                    let b = MFCreateMemoryBuffer(self.out_cb).map_err(|e| os("MFCreateMemoryBuffer", e))?;
                    s.AddBuffer(&b).map_err(|e| os("AddBuffer", e))?;
                    self.sample = Some(s);
                } else if let Some(b) = self.sample.as_ref().and_then(|s| s.GetBufferByIndex(0).ok()) {
                    // Reused for the next picture (the decoder fails on a full buffer).
                    let _ = b.SetCurrentLength(0);
                }
                let mut db = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(if self.provides { None } else { self.sample.clone() }),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let r = self.t.ProcessOutput(0, &mut db, &mut status);
                let sample = ManuallyDrop::take(&mut db[0].pSample);
                drop(ManuallyDrop::take(&mut db[0].pEvents));
                match r {
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.set_output()?;
                        continue;
                    }
                    Err(e) => return Err(os("decoder ProcessOutput", e)),
                    Ok(()) => {}
                }
                let Some(sample) = sample else { continue };
                self.decoded += 1;
                let idx = ((sample.GetSampleTime().unwrap_or(0) + FRAME_HNS / 2) / FRAME_HNS) as u32;
                let buf = sample.GetBufferByIndex(0).map_err(|e| os("GetBufferByIndex", e))?;
                if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                    let (mut p, mut pitch) = (std::ptr::null_mut(), 0i32);
                    if b2.Lock2D(&mut p, &mut pitch).is_ok() {
                        if pitch > 0 {
                            on_pic(idx, p, pitch as usize);
                        }
                        let _ = b2.Unlock2D();
                        continue;
                    }
                }
                let mut p = std::ptr::null_mut();
                let mut len = 0u32;
                if buf.Lock(&mut p, None, Some(&mut len)).is_ok() {
                    if len as usize >= (W * H) as usize {
                        on_pic(idx, p, W as usize);
                    }
                    let _ = buf.Unlock();
                }
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.t.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every encoder of this machine through the real self-test (skips quietly without a GPU).
    #[test]
    fn self_test_this_machine() {
        let gpus = crate::list_gpus();
        let cands = crate::encoder_candidates(&gpus);
        eprintln!("gpus: {gpus:#?}\ncandidates: {:?}", cands.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
        let t0 = Instant::now();
        let results = self_test_encoders(&cands, &mut |r| eprintln!("{r:?}"));
        eprintln!("self-test took {} ms", t0.elapsed().as_millis());
        // A machine with NVENC must pass it (the reference host has an RTX 50-series GPU).
        for (c, r) in cands.iter().filter(|c| c.backend().is_some()).zip(&results) {
            if c.kind == gpu::EncoderKind::Nvenc {
                assert!(r.passed(), "{r:?}");
            }
        }
        // The MFTs on their own (here after NVENC, which is the order that keeps NVENC working).
        let mf: Vec<EncoderCandidate> = cands.iter().filter(|c| c.kind == gpu::EncoderKind::MediaFoundation).cloned().collect();
        for r in self_test_encoders(&mf, &mut |r| eprintln!("{r:?}")) {
            assert!(r.passed(), "{r:?}");
        }
    }

    #[test]
    fn synthetic_frames_move() {
        let pitch = W as usize * 4;
        let mut a = vec![0u8; pitch * H as usize];
        let mut b = a.clone();
        paint(0, &mut a, pitch);
        paint(1, &mut b, pitch);
        let differ = a.iter().zip(&b).filter(|(x, y)| x != y).count();
        assert!(differ > a.len() / 4, "{differ}");
        // The page has dark ink on it.
        assert!(a.chunks(pitch).take(40).any(|r| r[..pitch / 2].iter().any(|&v| v < 64)));
    }
}
