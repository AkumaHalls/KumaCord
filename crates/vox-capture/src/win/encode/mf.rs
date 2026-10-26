//! Media Foundation hardware H.264 MFT backend (AMD AMF, Intel QSV, NVIDIA): enumerated for the
//! capture adapter's LUID, async event model, D3D11 input via `IMFDXGIDeviceManager` on the
//! capture device, `MFCreateDXGISurfaceBuffer` samples (no CPU readback).
//!
//! Async MFT events (`METransformNeedInput` / `METransformHaveOutput`) are collected by a tiny
//! pump thread that blocks in `IMFMediaEventGenerator::GetEvent` and only bumps counters and
//! signals the sink's event; all `ProcessInput`/`ProcessOutput` calls stay on the capture thread.
//!
//! The converter's output texture is copied (GPU `CopyResource`) into a small ring the MFT owns
//! while encoding, so an MFT that keeps a frame or two in flight never sees it overwritten.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;

use windows::core::Interface;
use windows::Win32::Foundation::{E_NOTIMPL, LUID, S_OK};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::Win32::Foundation::VARIANT_TRUE;

use super::{is_annexb, nal_units, EncParams, HwEncoder};
use crate::win::{os, ComInit, Event};
use crate::{CaptureError, EncoderBackend, Result};

/// In-flight limit (and ring size). Low-latency MFTs output each frame before the next one
/// arrives, so in practice only one frame is ever in the encoder.
const RING: usize = 3;

/// `MFStartup` once per process, never shut down (same reasoning as the pinned MTA: cached
/// MF/COM state must not outlive the platform).
pub(crate) fn mf_startup() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(|e| format!("MFStartup: {e}"))
        })
        .clone()
        .map_err(CaptureError::Unsupported)
}

fn var_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

fn var_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if v {
            VARIANT_TRUE
        } else {
            windows::Win32::Foundation::VARIANT_BOOL(0)
        };
    }
    var
}

/// Bitrate the MFT is created with: the most `set_bitrate` can later ask for.
fn headroom_bits(bps: u32) -> u32 {
    (bps as f64 * super::policy::MAX_RATE_COMPENSATION).min(super::MAX_BITRATE as f64) as u32
}

/// `CODECAPI_AVEncCommonBufferSize` (bits) for `frames` frames at `bps` / `fps`.
fn buffer_bits(bps: u32, fps: u32, frames: f32) -> u32 {
    ((bps as f64 / fps.max(1) as f64) * frames as f64).max(1.0) as u32
}

/// Counters written by the event pump thread.
#[derive(Default)]
struct PumpShared {
    need_input: AtomicU32,
    have_output: AtomicU32,
    /// First failure HRESULT reported through the event queue (0 = none).
    error: AtomicI32,
    stop: AtomicBool,
}

/// COM pointer moved to the pump thread. The MFT is free-threaded and both threads are MTA.
struct SendCom<T>(T);
unsafe impl<T> Send for SendCom<T> {}

pub(crate) struct MfEncoder {
    device: ID3D11Device,
    luid: LUID,
    params: EncParams,
    ev: Arc<Event>,
    inner: Option<Inner>,
}

struct Inner {
    activate: IMFActivate,
    transform: IMFTransform,
    codec: ICodecAPI,
    gen: IMFMediaEventGenerator,
    _devmgr: IMFDXGIDeviceManager,
    in_id: u32,
    out_id: u32,
    provides_samples: bool,
    out_cb: u32,
    pump: Option<mpsc::Receiver<()>>,
    ps: Arc<PumpShared>,
    ring: Vec<ID3D11Texture2D>,
    ring_next: usize,
    /// Sample times (hns) submitted and not yet output, oldest first.
    submitted: std::collections::VecDeque<i64>,
    seq_header: Vec<u8>,
    name: String,
    buf: Vec<u8>,
    frame_dur_hns: i64,
    warned_not_annexb: bool,
}

impl MfEncoder {
    pub fn new(device: &ID3D11Device, luid: LUID, p: EncParams, ev: Arc<Event>) -> Result<Self> {
        mf_startup()?;
        let inner = Inner::create(device, luid, p, &ev)?;
        Ok(Self {
            device: device.clone(),
            luid,
            params: p,
            ev,
            inner: Some(inner),
        })
    }

    fn inner(&mut self) -> Result<&mut Inner> {
        self.inner
            .as_mut()
            .ok_or_else(|| CaptureError::Stopped("MF encoder not initialised".into()))
    }
}

/// Whether this adapter has a hardware H.264 encoder MFT, without activating it (see
/// `encode::probe`: activating the NVIDIA MFT costs NVENC for the rest of the process).
pub(super) fn has_hardware_mft(luid: LUID) -> Result<bool> {
    mf_startup()?;
    Ok(!enum_mfts(luid, false)?.is_empty())
}

/// Whether exactly this adapter lists a hardware H.264 encoder MFT (no fallback to other
/// adapters' MFTs: on a hybrid laptop each GPU is its own candidate). Never activates it.
pub(super) fn adapter_has_mft(luid: LUID) -> bool {
    mf_startup().is_ok() && enum_mfts(luid, true).is_ok_and(|v| !v.is_empty())
}

/// Any hardware H.264 encoder MFT on the system, whatever the adapter.
pub(super) fn any_mft() -> bool {
    mf_startup().is_ok() && enum_mfts_all().is_ok_and(|v| !v.is_empty())
}

/// Enumerate hardware H.264 encoder MFTs for this adapter (best first). Without `strict`, a
/// system that lists none for the adapter gets every adapter's (older drivers).
fn enum_mfts(luid: LUID, strict: bool) -> Result<Vec<IMFActivate>> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(|e| os("MFCreateAttributes", e))?;
        let attrs = attrs.ok_or_else(|| CaptureError::Os("MFCreateAttributes null".into()))?;
        let luid64 = (luid.LowPart as u64) | ((luid.HighPart as u32 as u64) << 32);
        attrs
            .SetBlob(
                &MFT_ENUM_ADAPTER_LUID,
                std::slice::from_raw_parts(&luid64 as *const u64 as *const u8, 8),
            )
            .map_err(|e| os("SetBlob(MFT_ENUM_ADAPTER_LUID)", e))?;
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        let r = MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &attrs,
            &mut arr,
            &mut n,
        );
        if r.is_err() || (n == 0 && !strict) {
            // Older systems / drivers: enumerate without the adapter filter.
            if !arr.is_null() {
                CoTaskMemFree(Some(arr as *const _));
            }
            arr = std::ptr::null_mut();
            n = 0;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(&input),
                Some(&output),
                &mut arr,
                &mut n,
            )
            .map_err(|e| os("MFTEnumEx(H.264 HW encoders)", e))?;
        }
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            if let Some(a) = (*arr.add(i)).take() {
                out.push(a);
            }
        }
        if !arr.is_null() {
            CoTaskMemFree(Some(arr as *const _));
        }
        Ok(out)
    }
}

fn enum_mfts_all() -> Result<Vec<IMFActivate>> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut arr, &mut n)
            .map_err(|e| os("MFTEnumEx(H.264 HW encoders)", e))?;
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            if let Some(a) = (*arr.add(i)).take() {
                out.push(a);
            }
        }
        if !arr.is_null() {
            CoTaskMemFree(Some(arr as *const _));
        }
        Ok(out)
    }
}

fn activate_name(a: &IMFActivate) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let mut len = 0u32;
        if a.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len))
            .is_ok()
        {
            return String::from_utf16_lossy(&buf[..len as usize]);
        }
    }
    "unknown H.264 MFT".into()
}

impl Inner {
    fn create(device: &ID3D11Device, luid: LUID, p: EncParams, ev: &Arc<Event>) -> Result<Self> {
        let candidates = enum_mfts(luid, false)?;
        if candidates.is_empty() {
            return Err(CaptureError::Unsupported(
                "no hardware H.264 encoder MFT for this adapter".into(),
            ));
        }
        let mut last_err = None;
        for a in candidates {
            let name = activate_name(&a);
            match Self::try_activate(device, a, &name, p, ev) {
                Ok(inner) => return Ok(inner),
                Err(e) => {
                    tracing::debug!(mft = %name, error = %e, "H.264 MFT rejected");
                    last_err = Some(e);
                }
            }
        }
        Err(CaptureError::Unsupported(format!(
            "no usable hardware H.264 MFT: {}",
            last_err.map(|e| e.to_string()).unwrap_or_default()
        )))
    }

    fn try_activate(
        device: &ID3D11Device,
        activate: IMFActivate,
        name: &str,
        p: EncParams,
        ev: &Arc<Event>,
    ) -> Result<Self> {
        let r = Self::setup(device, &activate, name, p, ev);
        if r.is_err() {
            unsafe {
                let _ = activate.ShutdownObject();
            }
        }
        r
    }

    fn setup(
        device: &ID3D11Device,
        activate: &IMFActivate,
        name: &str,
        p: EncParams,
        ev: &Arc<Event>,
    ) -> Result<Self> {
        unsafe {
            let transform: IMFTransform = activate
                .ActivateObject()
                .map_err(|e| os("IMFActivate::ActivateObject", e))?;
            let attrs = transform
                .GetAttributes()
                .map_err(|e| os("IMFTransform::GetAttributes", e))?;
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err(CaptureError::Unsupported(format!("{name}: not an async MFT")));
            }
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(|e| os("MF_TRANSFORM_ASYNC_UNLOCK", e))?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return Err(CaptureError::Unsupported(format!("{name}: not D3D11 aware")));
            }

            let mut token = 0u32;
            let mut mgr: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut mgr)
                .map_err(|e| os("MFCreateDXGIDeviceManager", e))?;
            let mgr = mgr.ok_or_else(|| CaptureError::Os("device manager null".into()))?;
            mgr.ResetDevice(device, token)
                .map_err(|e| os("IMFDXGIDeviceManager::ResetDevice", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)
                .map_err(|e| os("MFT_MESSAGE_SET_D3D_MANAGER", e))?;

            let (mut in_id, mut out_id) = ([0u32], [0u32]);
            match transform.GetStreamIDs(&mut in_id, &mut out_id) {
                Ok(()) => {}
                Err(e) if e.code() == E_NOTIMPL => {}
                Err(e) => return Err(os("GetStreamIDs", e)),
            }
            let (in_id, out_id) = (in_id[0], out_id[0]);

            let codec: ICodecAPI = transform
                .cast()
                .map_err(|e| os("ICodecAPI", e))?;
            // Rate control and GOP structure must be set before the media types.
            let set = |api: &windows_core::GUID, v: VARIANT, what: &str| {
                if let Err(e) = codec.SetValue(api, &v) {
                    tracing::debug!(mft = name, setting = what, hr = format!("0x{:08X}", e.code().0 as u32), "codec setting rejected");
                    false
                } else {
                    true
                }
            };
            let t = p.tuning.mf;
            set(&CODECAPI_AVLowLatencyMode, var_bool(true), "AVLowLatencyMode");
            let mut rc_ok = set(
                &CODECAPI_AVEncCommonRateControlMode,
                var_u32(t.rc),
                "RateControlMode",
            );
            if !rc_ok && t.rc != eAVEncCommonRateControlMode_CBR.0 as u32 {
                tracing::warn!(mft = name, rc = t.rc, "rate-control mode rejected; using CBR");
                rc_ok = set(
                    &CODECAPI_AVEncCommonRateControlMode,
                    var_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
                    "RateControlMode=CBR",
                );
            }
            // The NVIDIA MFT never raises the mean bitrate above the value it was started with
            // (measured: 3 -> 8 Mbps mid-stream stays at ~3.4). Start with headroom for the
            // input-rate compensation, then set the real target.
            let headroom = if t.headroom { headroom_bits(p.bitrate) } else { p.bitrate };
            set(&CODECAPI_AVEncCommonMaxBitRate, var_u32(headroom), "MaxBitRate(headroom)");
            set(&CODECAPI_AVEncCommonMeanBitRate, var_u32(headroom), "MeanBitRate(headroom)");
            if t.rc != 0 {
                set(
                    &CODECAPI_AVEncCommonMaxBitRate,
                    var_u32((p.bitrate as f32 * t.peak_factor) as u32),
                    "MaxBitRate",
                );
            }
            set(&CODECAPI_AVEncMPVDefaultBPictureCount, var_u32(0), "BPictureCount=0");
            // Long GOP; keyframes come from CODECAPI_AVEncVideoForceKeyFrame on demand.
            if !set(&CODECAPI_AVEncMPVGOPSize, var_u32(0xFFFF_FFFF), "GOPSize=inf") {
                set(&CODECAPI_AVEncMPVGOPSize, var_u32(p.fps * 3600), "GOPSize=1h");
            }
            set(
                &CODECAPI_AVEncCommonBufferSize,
                var_u32(buffer_bits(headroom, p.fps, t.vbv_frames)),
                "BufferSize(headroom)",
            );
            if let Some(q) = t.quality_vs_speed {
                set(&CODECAPI_AVEncCommonQualityVsSpeed, var_u32(q), "QualityVsSpeed");
            }
            if let Some(q) = t.max_qp {
                set(&CODECAPI_AVEncVideoMaxQP, var_u32(q), "MaxQP");
            }
            if let Some(q) = t.min_qp {
                set(&CODECAPI_AVEncVideoMinQP, var_u32(q), "MinQP");
            }
            if !rc_ok {
                return Err(CaptureError::Unsupported(format!("{name}: CBR not supported")));
            }

            let out_t = MFCreateMediaType().map_err(|e| os("MFCreateMediaType", e))?;
            out_t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok();
            out_t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).ok();
            out_t.SetUINT32(&MF_MT_AVG_BITRATE, headroom).ok();
            out_t
                .SetUINT64(&MF_MT_FRAME_SIZE, ((p.width as u64) << 32) | p.height as u64)
                .ok();
            out_t
                .SetUINT64(&MF_MT_FRAME_RATE, ((p.fps as u64) << 32) | 1)
                .ok();
            out_t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).ok();
            out_t
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .ok();
            out_t
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)
                .ok();
            out_t
                .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
                .ok();
            out_t
                .SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
                .ok();
            transform
                .SetOutputType(out_id, &out_t, 0)
                .map_err(|e| os("SetOutputType(H.264)", e))?;

            // Input: the MFT's own NV12 type with our size/rate.
            let mut in_t = None;
            for i in 0.. {
                let Ok(t) = transform.GetInputAvailableType(in_id, i) else {
                    break;
                };
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    in_t = Some(t);
                    break;
                }
            }
            let in_t = match in_t {
                Some(t) => t,
                None => {
                    let t = MFCreateMediaType().map_err(|e| os("MFCreateMediaType", e))?;
                    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok();
                    t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok();
                    t
                }
            };
            in_t
                .SetUINT64(&MF_MT_FRAME_SIZE, ((p.width as u64) << 32) | p.height as u64)
                .ok();
            in_t.SetUINT64(&MF_MT_FRAME_RATE, ((p.fps as u64) << 32) | 1).ok();
            in_t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).ok();
            in_t
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .ok();
            in_t
                .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
                .ok();
            in_t
                .SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
                .ok();
            transform
                .SetInputType(in_id, &in_t, 0)
                .map_err(|e| os("SetInputType(NV12)", e))?;

            let info = transform
                .GetOutputStreamInfo(out_id)
                .map_err(|e| os("GetOutputStreamInfo", e))?;
            let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

            let gen: IMFMediaEventGenerator = transform
                .cast()
                .map_err(|e| os("IMFMediaEventGenerator", e))?;

            // Ring of NV12 input textures owned by this encoder.
            let mut ring = Vec::with_capacity(RING);
            for _ in 0..RING {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: p.width,
                    Height: p.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .map_err(|e| os("CreateTexture2D(MF input ring)", e))?;
                ring.push(t.ok_or_else(|| CaptureError::Os("ring texture null".into()))?);
            }

            let mut me = Inner {
                activate: activate.clone(),
                transform,
                codec,
                gen,
                _devmgr: mgr,
                in_id,
                out_id,
                provides_samples,
                out_cb: info.cbSize,
                pump: None,
                ps: Arc::new(PumpShared::default()),
                ring,
                ring_next: 0,
                submitted: std::collections::VecDeque::with_capacity(RING),
                seq_header: Vec::new(),
                name: name.to_string(),
                buf: Vec::with_capacity(256 * 1024),
                frame_dur_hns: 10_000_000 / p.fps.max(1) as i64,
                warned_not_annexb: false,
            };
            me.refresh_seq_header();
            me.start_pump(ev.clone())?;
            me.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| os("NOTIFY_BEGIN_STREAMING", e))?;
            me.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| os("NOTIFY_START_OF_STREAM", e))?;
            // Now the real target (see `headroom_bits`).
            let _ = me.set_value(
                &CODECAPI_AVEncCommonBufferSize,
                var_u32(buffer_bits(p.bitrate, p.fps, t.vbv_frames)),
                "BufferSize",
            );
            me.set_value(&CODECAPI_AVEncCommonMeanBitRate, var_u32(p.bitrate), "MeanBitRate")?;
            Ok(me)
        }
    }

    fn start_pump(&mut self, ev: Arc<Event>) -> Result<()> {
        let gen = SendCom(self.gen.clone());
        let ps = self.ps.clone();
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("vox-mf-events".into())
            .spawn(move || {
                let _com = ComInit::mta();
                let gen = gen;
                loop {
                    let r = unsafe { gen.0.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) };
                    if ps.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match r {
                        Ok(e) => {
                            let ty = unsafe { e.GetType() }.unwrap_or(0) as i32;
                            let status = unsafe { e.GetStatus() }.unwrap_or(S_OK);
                            if status.is_err() {
                                let _ = ps.error.compare_exchange(
                                    0,
                                    status.0,
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                );
                            } else if ty == METransformNeedInput.0 {
                                ps.need_input.fetch_add(1, Ordering::Relaxed);
                            } else if ty == METransformHaveOutput.0 {
                                ps.have_output.fetch_add(1, Ordering::Relaxed);
                            } else if ty == MEError.0 {
                                let _ = ps.error.compare_exchange(
                                    0,
                                    MF_E_SHUTDOWN.0,
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                );
                            }
                            ev.set();
                        }
                        Err(e) => {
                            if e.code() != MF_E_SHUTDOWN {
                                let _ = ps.error.compare_exchange(
                                    0,
                                    e.code().0,
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                );
                                ev.set();
                            }
                            break;
                        }
                    }
                }
                drop(gen);
                let _ = tx.send(());
            })
            .map_err(|e| CaptureError::Os(format!("spawn MF event thread: {e}")))?;
        self.pump = Some(rx);
        Ok(())
    }

    fn refresh_seq_header(&mut self) {
        unsafe {
            if let Ok(t) = self.transform.GetOutputCurrentType(self.out_id) {
                if let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                    let mut v = vec![0u8; n as usize];
                    if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut v, None).is_ok() {
                        self.seq_header = v;
                    }
                }
            }
        }
    }

    fn set_value(&self, api: &windows_core::GUID, v: VARIANT, what: &str) -> Result<()> {
        unsafe { self.codec.SetValue(api, &v) }.map_err(|e| os(what, e))
    }

    /// One `ProcessOutput`. Returns the output sample time (hns) and fills `self.buf` with an
    /// Annex-B access unit (SPS/PPS inserted in front of IDRs when the MFT omits them).
    fn process_output(&mut self) -> Result<Option<i64>> {
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample().map_err(|e| os("MFCreateSample", e))?;
                let b = MFCreateMemoryBuffer(self.out_cb.max(1024 * 1024))
                    .map_err(|e| os("MFCreateMemoryBuffer", e))?;
                s.AddBuffer(&b).map_err(|e| os("AddBuffer", e))?;
                Some(s)
            };
            let mut db = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.out_id,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.transform.ProcessOutput(0, &mut db, &mut status);
            let sample = ManuallyDrop::take(&mut db[0].pSample);
            drop(ManuallyDrop::take(&mut db[0].pEvents));
            if let Err(e) = r {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    // New output type (e.g. the MFT finalised its SPS): accept it.
                    let t = self
                        .transform
                        .GetOutputAvailableType(self.out_id, 0)
                        .map_err(|e| os("GetOutputAvailableType", e))?;
                    self.transform
                        .SetOutputType(self.out_id, &t, 0)
                        .map_err(|e| os("SetOutputType(stream change)", e))?;
                    self.refresh_seq_header();
                    return Ok(None);
                }
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(None);
                }
                return Err(CaptureError::Stopped(format!(
                    "MF encoder '{}' ProcessOutput failed: hr=0x{:08X} {}",
                    self.name,
                    e.code().0 as u32,
                    e.message()
                )));
            }
            let Some(sample) = sample else {
                return Ok(None);
            };
            let ts = sample.GetSampleTime().unwrap_or(0);
            let mb = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| os("ConvertToContiguousBuffer", e))?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            mb.Lock(&mut ptr, None, Some(&mut len))
                .map_err(|e| os("IMFMediaBuffer::Lock", e))?;
            let data = std::slice::from_raw_parts(ptr as *const u8, len as usize);
            self.buf.clear();
            if !is_annexb(data) && !self.warned_not_annexb {
                self.warned_not_annexb = true;
                tracing::error!(mft = %self.name, "MFT output is not Annex-B");
            }
            let mut has_sps = false;
            let mut idr = false;
            let mut first_non_aud = None;
            for (off, t) in nal_units(data) {
                match t {
                    7 => has_sps = true,
                    5 => idr = true,
                    _ => {}
                }
                if t != 9 && first_non_aud.is_none() {
                    // Start code begins 3 or 4 bytes before the NAL header.
                    let sc = if off >= 4 && data[off - 4] == 0 { off - 4 } else { off - 3 };
                    first_non_aud = Some(sc);
                }
            }
            if idr && !has_sps && !self.seq_header.is_empty() {
                let at = first_non_aud.unwrap_or(0);
                self.buf.extend_from_slice(&data[..at]);
                self.buf.extend_from_slice(&self.seq_header);
                self.buf.extend_from_slice(&data[at..]);
            } else {
                self.buf.extend_from_slice(data);
            }
            let _ = mb.Unlock();
            Ok(Some(ts))
        }
    }

    fn shutdown(&mut self) {
        // Wake and stop the pump: a queued dummy event unblocks GetEvent.
        self.ps.stop.store(true, Ordering::Relaxed);
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.gen.QueueEvent(
                MEUnknown.0 as u32,
                &windows_core::GUID::zeroed(),
                S_OK,
                std::ptr::null(),
            );
        }
        if let Some(rx) = self.pump.take() {
            if rx.recv_timeout(Duration::from_secs(2)).is_err() {
                // Last resort: shutting the MFT down fails a blocked GetEvent with MF_E_SHUTDOWN.
                if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                    let _ = unsafe { s.Shutdown() };
                }
                if rx.recv_timeout(Duration::from_secs(2)).is_err() {
                    tracing::warn!(mft = %self.name, "MF event thread did not exit; detached");
                }
            }
        }
        unsafe {
            if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl HwEncoder for MfEncoder {
    fn backend(&self) -> EncoderBackend {
        EncoderBackend::MediaFoundation
    }

    fn name(&self) -> String {
        self.inner
            .as_ref()
            .map(|i| format!("{} [{}]", i.name, self.params.tuning.mf))
            .unwrap_or_default()
    }

    fn ready(&self) -> bool {
        self.inner.as_ref().is_some_and(|i| {
            i.ps.need_input.load(Ordering::Relaxed) > 0 && i.submitted.len() < RING
        })
    }

    fn in_flight(&self) -> usize {
        self.inner.as_ref().map_or(0, |i| i.submitted.len())
    }

    fn encode(
        &mut self,
        ctx: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        ts_us: i64,
        force_idr: bool,
    ) -> Result<()> {
        let i = self.inner()?;
        if i.ps.need_input.load(Ordering::Relaxed) == 0 {
            return Err(CaptureError::Os("MFT not accepting input".into()));
        }
        if force_idr {
            i.set_value(&CODECAPI_AVEncVideoForceKeyFrame, var_u32(1), "ForceKeyFrame")?;
        }
        unsafe {
            let slot = &i.ring[i.ring_next];
            ctx.CopyResource(slot, tex);
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, slot, 0, false)
                .map_err(|e| os("MFCreateDXGISurfaceBuffer", e))?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                if let Ok(n) = b2.GetContiguousLength() {
                    let _ = buffer.SetCurrentLength(n);
                }
            }
            let sample = MFCreateSample().map_err(|e| os("MFCreateSample", e))?;
            sample.AddBuffer(&buffer).map_err(|e| os("AddBuffer", e))?;
            sample
                .SetSampleTime(ts_us * 10)
                .map_err(|e| os("SetSampleTime", e))?;
            let _ = sample.SetSampleDuration(i.frame_dur_hns);
            i.ps.need_input.fetch_sub(1, Ordering::Relaxed);
            i.transform
                .ProcessInput(i.in_id, &sample, 0)
                .map_err(|e| os("ProcessInput", e))?;
        }
        i.ring_next = (i.ring_next + 1) % RING;
        i.submitted.push_back(ts_us * 10);
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        // MF encoders don't reliably support mid-stream format changes: re-create the MFT.
        // The old one is shut down first (some vendors limit concurrent sessions).
        self.inner = None;
        self.params.width = width;
        self.params.height = height;
        self.inner = Some(Inner::create(&self.device, self.luid, self.params, &self.ev)?);
        Ok(())
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        let fps = self.params.fps;
        let t = self.params.tuning.mf;
        let i = self.inner()?;
        if t.rc != 0 || !t.headroom {
            // Raise the cap first when going up (some MFTs reject mean > max). With headroom the
            // CBR cap is the 3x start value already.
            let peak = if t.rc != 0 { t.peak_factor } else { 1.0 };
            let _ = i.set_value(&CODECAPI_AVEncCommonMaxBitRate, var_u32((bps as f32 * peak) as u32), "MaxBitRate");
        }
        // Buffer first: a larger mean than the current buffer allows is ignored on the way up.
        let _ = i.set_value(
            &CODECAPI_AVEncCommonBufferSize,
            var_u32(buffer_bits(bps, fps, t.vbv_frames)),
            "BufferSize",
        );
        i.set_value(&CODECAPI_AVEncCommonMeanBitRate, var_u32(bps), "MeanBitRate")?;
        self.params.bitrate = bps;
        Ok(())
    }

    fn poll(&mut self, out: &mut dyn FnMut(&[u8], i64)) -> Result<()> {
        self.ev.reset();
        let i = self.inner()?;
        let err = i.ps.error.load(Ordering::Relaxed);
        if err != 0 {
            return Err(CaptureError::Stopped(format!(
                "MF encoder '{}' failed: hr=0x{:08X}",
                i.name, err as u32
            )));
        }
        while i.ps.have_output.load(Ordering::Relaxed) > 0 {
            i.ps.have_output.fetch_sub(1, Ordering::Relaxed);
            if let Some(ts_hns) = i.process_output()? {
                // Everything submitted up to this frame is done (skipped frames included).
                while i.submitted.front().is_some_and(|&t| t <= ts_hns) {
                    i.submitted.pop_front();
                }
                out(&i.buf, ts_hns / 10);
            }
        }
        Ok(())
    }
}
