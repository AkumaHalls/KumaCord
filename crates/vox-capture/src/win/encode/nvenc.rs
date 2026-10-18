//! NVENC backend: `nvEncodeAPI64.dll` loaded at runtime, D3D11 device input
//! (`NV_ENC_DEVICE_TYPE_DIRECTX`), the converter's NV12 texture registered and mapped directly
//! (zero-copy), async mode with a completion event that the capture loop waits on.
//!
//! Settings: preset/tuning/AQ/VBV from [`NvTuning`] (defaults chosen with `capture_probe bench`,
//! see `docs/quality-bench.md`), no B-frames, no lookahead, infinite GOP with IDR on demand,
//! SPS/PPS repeated on every IDR, BT.709 limited-range VUI.
//! Bitrate changes and resizes (up to the session's max size) use `nvEncReconfigureEncoder`.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::{Arc, OnceLock};

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence,
    ID3D11Texture2D, D3D11_FENCE_FLAG_NONE, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows_core::w;

use super::nvenc_sys::*;
use super::tuning::NvTuning;
use super::{EncParams, HwEncoder};
use crate::win::Event;
use crate::{CaptureError, EncoderBackend, Result};

struct Api {
    fl: NV_ENCODE_API_FUNCTION_LIST,
    /// Driver's max API version as (major << 4) | minor.
    driver_version: u32,
}

// SAFETY: the function list is immutable after creation; function pointers are thread-agnostic.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// The NVIDIA encode runtime loads and is new enough (no session is opened).
pub(super) fn available() -> bool {
    api().is_ok()
}

/// Load the runtime once per process (the DLL is never unloaded).
fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        // System32 only: never pick up a planted DLL from the working directory.
        let lib = LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| format!("nvEncodeAPI64.dll not loadable (no NVIDIA driver?): {e}"))?;
        let max_ver = GetProcAddress(lib, PCSTR(c"NvEncodeAPIGetMaxSupportedVersion".as_ptr().cast()))
            .ok_or("NvEncodeAPIGetMaxSupportedVersion missing")?;
        let create = GetProcAddress(lib, PCSTR(c"NvEncodeAPICreateInstance".as_ptr().cast()))
            .ok_or("NvEncodeAPICreateInstance missing")?;
        let max_ver: PNvEncodeAPIGetMaxSupportedVersion = std::mem::transmute(max_ver);
        let create: PNvEncodeAPICreateInstance = std::mem::transmute(create);
        let mut v = 0u32;
        let st = max_ver(&mut v);
        if st != NV_ENC_SUCCESS {
            return Err(format!("NvEncodeAPIGetMaxSupportedVersion: {}", status_name(st)));
        }
        let need = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
        if v < need {
            return Err(format!(
                "driver supports NVENC API {}.{}, need {}.{} (update the NVIDIA driver)",
                v >> 4,
                v & 0xf,
                NVENCAPI_MAJOR_VERSION,
                NVENCAPI_MINOR_VERSION
            ));
        }
        let mut fl: NV_ENCODE_API_FUNCTION_LIST = zeroed();
        fl.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        let st = create(&mut fl);
        if st != NV_ENC_SUCCESS {
            return Err(format!("NvEncodeAPICreateInstance: {}", status_name(st)));
        }
        Ok(Api {
            fl,
            driver_version: v,
        })
    })
    .as_ref()
    .map_err(|e| CaptureError::Unsupported(format!("NVENC: {e}")))
}

/// Call an NVENC function pointer, mapping a missing pointer to an error.
macro_rules! nv {
    ($api:expr, $f:ident ( $($arg:expr),* $(,)? )) => {
        match $api.fl.$f {
            Some(f) => f($($arg),*),
            None => -1,
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    /// Blit queued; waiting for the fence before submitting to NVENC.
    WaitGpu { ts_us: i64, force_idr: bool, value: u64 },
    /// Submitted to NVENC; waiting for the completion event.
    Encoding,
}

struct Registered {
    tex: ID3D11Texture2D,
    handle: *mut c_void,
}

pub(crate) struct NvEncoder {
    api: &'static Api,
    enc: *mut c_void,
    device: ID3D11Device,
    params: EncParams,
    cfg: Box<NV_ENC_CONFIG>,
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    bitstream: *mut c_void,
    reg: Option<Registered>,
    mapped: *mut c_void,
    stage: Stage,
    /// GPU fence used to wait (without spinning) for the converter's blit before NVENC maps
    /// the texture; the NVIDIA driver otherwise busy-waits inside nvEncMapInputResource.
    fence: Option<(ID3D11Fence, ID3D11DeviceContext4)>,
    fence_value: u64,
    ev: Arc<Event>,
    event_registered: bool,
    frame_idx: u32,
}

impl NvEncoder {
    pub fn new(device: &ID3D11Device, p: EncParams, ev: Arc<Event>) -> Result<Self> {
        let api = api()?;
        let mut me = Self {
            api,
            enc: null_mut(),
            device: device.clone(),
            params: p,
            cfg: Box::new(zeroed()),
            init: Box::new(zeroed()),
            bitstream: null_mut(),
            reg: None,
            mapped: null_mut(),
            stage: Stage::Idle,
            fence: create_fence(device),
            fence_value: 0,
            ev,
            event_registered: false,
            frame_idx: 0,
        };
        // Counted from here to `drop` (which also runs when `open` fails).
        super::NVENC_LIVE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // On error `me` is dropped, which releases whatever was created so far.
        me.open()?;
        Ok(me)
    }

    fn err(&self, what: &str, st: NVENCSTATUS) -> CaptureError {
        let detail = if self.enc.is_null() {
            String::new()
        } else {
            unsafe {
                match self.api.fl.nvEncGetLastErrorString {
                    Some(f) => {
                        let p = f(self.enc);
                        if p.is_null() {
                            String::new()
                        } else {
                            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                        }
                    }
                    None => String::new(),
                }
            }
        };
        CaptureError::Os(format!("NVENC {what}: {} {detail}", status_name(st)))
    }

    fn check(&self, what: &str, st: NVENCSTATUS) -> Result<()> {
        if st == NV_ENC_SUCCESS {
            Ok(())
        } else {
            Err(self.err(what, st))
        }
    }

    fn caps(&self, cap: u32) -> i32 {
        let mut cp: NV_ENC_CAPS_PARAM = zeroed();
        cp.version = NV_ENC_CAPS_PARAM_VER;
        cp.capsToQuery = cap;
        let mut v = 0i32;
        let st = unsafe {
            nv!(self.api, nvEncGetEncodeCaps(self.enc, NV_ENC_CODEC_H264_GUID, &mut cp, &mut v))
        };
        if st == NV_ENC_SUCCESS {
            v
        } else {
            0
        }
    }

    fn open(&mut self) -> Result<()> {
        let p = self.params;
        let api = self.api;
        unsafe {
            let mut op: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = zeroed();
            op.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
            op.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
            op.device = self.device.as_raw();
            op.apiVersion = NVENCAPI_VERSION;
            let mut enc = null_mut();
            let st = nv!(api, nvEncOpenEncodeSessionEx(&mut op, &mut enc));
            if st != NV_ENC_SUCCESS {
                return Err(CaptureError::Unsupported(format!(
                    "NVENC open session: {}",
                    status_name(st)
                )));
            }
            self.enc = enc;

            // H.264 support.
            // The driver rejects an array size larger than the count, so ask for the count first.
            let mut count = 0u32;
            let st = nv!(api, nvEncGetEncodeGUIDCount(enc, &mut count));
            self.check("GetEncodeGUIDCount", st)?;
            let mut guids = [GUID::zeroed(); 16];
            let mut n = 0u32;
            let st = nv!(
                api,
                nvEncGetEncodeGUIDs(enc, guids.as_mut_ptr(), count.min(16), &mut n)
            );
            self.check("GetEncodeGUIDs", st)?;
            if !guids[..(n as usize).min(16)].contains(&NV_ENC_CODEC_H264_GUID) {
                return Err(CaptureError::Unsupported("NVENC: no H.264 on this GPU".into()));
            }
            if self.caps(NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT) == 0 {
                return Err(CaptureError::Unsupported("NVENC: async mode unsupported".into()));
            }
            let (cw, ch) = (
                self.caps(NV_ENC_CAPS_WIDTH_MAX).max(0) as u32,
                self.caps(NV_ENC_CAPS_HEIGHT_MAX).max(0) as u32,
            );
            if cw > 0 && (p.width > cw || p.height > ch) {
                return Err(CaptureError::Unsupported(format!(
                    "NVENC H.264: {}x{} exceeds the GPU limit {cw}x{ch}",
                    p.width, p.height
                )));
            }
            let max_w = p.max_width.max(p.width).min(if cw > 0 { cw } else { 4096 }) & !1;
            let max_h = p.max_height.max(p.height).min(if ch > 0 { ch } else { 4096 }) & !1;
            self.params.max_width = max_w;
            self.params.max_height = max_h;

            // Preset + low-latency tuning info, then our overrides.
            let t = p.tuning.nv;
            let pguid = preset_guid(t.preset);
            let tuning_info = if t.ull {
                NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY
            } else {
                NV_ENC_TUNING_INFO_LOW_LATENCY
            };
            let mut preset: Box<NV_ENC_PRESET_CONFIG> = Box::new(zeroed());
            preset.version = NV_ENC_PRESET_CONFIG_VER;
            preset.presetCfg.version = NV_ENC_CONFIG_VER;
            let st = nv!(
                api,
                nvEncGetEncodePresetConfigEx(
                    enc,
                    NV_ENC_CODEC_H264_GUID,
                    pguid,
                    tuning_info,
                    &mut *preset
                )
            );
            self.check("GetEncodePresetConfigEx", st)?;
            *self.cfg = preset.presetCfg;
            let cfg = &mut *self.cfg;
            cfg.version = NV_ENC_CONFIG_VER;
            cfg.profileGUID = NV_ENC_H264_PROFILE_MAIN_GUID;
            cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
            cfg.frameIntervalP = 1; // IPPP, no B-frames
            let rc = &mut cfg.rcParams;
            rc.version = NV_ENC_RC_PARAMS_VER;
            tracing::debug!(
                preset = t.preset,
                ull = t.ull,
                preset_multipass = rc.multiPass,
                preset_rc_flags = format!("0x{:x}", rc.flags),
                preset_key_scale = rc.lowDelayKeyFrameScale,
                "NVENC preset defaults"
            );
            apply_tuning(rc, &t);
            set_rate(rc, p.bitrate, p.fps, &t);
            let h = &mut cfg.encodeCodecConfig.h264Config;
            h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
            h.flags |= H264_REPEAT_SPSPPS;
            h.flags &= !(H264_OUTPUT_AUD | H264_ENABLE_INTRA_REFRESH | H264_ENABLE_LTR);
            let vui = &mut h.h264VUIParameters;
            vui.videoSignalTypePresentFlag = 1;
            vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            vui.videoFullRangeFlag = 0;
            vui.colourDescriptionPresentFlag = 1;
            vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            vui.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
            vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;

            let init = &mut *self.init;
            init.version = NV_ENC_INITIALIZE_PARAMS_VER;
            init.encodeGUID = NV_ENC_CODEC_H264_GUID;
            init.presetGUID = pguid;
            init.encodeWidth = p.width;
            init.encodeHeight = p.height;
            init.darWidth = p.width;
            init.darHeight = p.height;
            init.frameRateNum = p.fps;
            init.frameRateDen = 1;
            init.enableEncodeAsync = 1;
            init.enablePTD = 1;
            init.encodeConfig = &mut *self.cfg;
            init.maxEncodeWidth = max_w;
            init.maxEncodeHeight = max_h;
            init.tuningInfo = tuning_info;
            let st = nv!(api, nvEncInitializeEncoder(enc, init));
            self.check("InitializeEncoder", st)?;

            let mut evp: NV_ENC_EVENT_PARAMS = zeroed();
            evp.version = NV_ENC_EVENT_PARAMS_VER;
            evp.completionEvent = self.ev.raw().0;
            let st = nv!(api, nvEncRegisterAsyncEvent(enc, &mut evp));
            self.check("RegisterAsyncEvent", st)?;
            self.event_registered = true;

            let mut bs: NV_ENC_CREATE_BITSTREAM_BUFFER = zeroed();
            bs.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            let st = nv!(api, nvEncCreateBitstreamBuffer(enc, &mut bs));
            self.check("CreateBitstreamBuffer", st)?;
            self.bitstream = bs.bitstreamBuffer;
        }
        Ok(())
    }

    fn reconfigure(&mut self, flags: u32) -> Result<()> {
        let mut rp: Box<NV_ENC_RECONFIGURE_PARAMS> = Box::new(zeroed());
        rp.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        self.init.encodeConfig = &mut *self.cfg;
        rp.reInitEncodeParams = *self.init;
        rp.flags = flags;
        let st = unsafe { nv!(self.api, nvEncReconfigureEncoder(self.enc, &mut *rp)) };
        self.check("ReconfigureEncoder", st)
    }

    fn unregister(&mut self) {
        if let Some(r) = self.reg.take() {
            unsafe {
                let _ = nv!(self.api, nvEncUnregisterResource(self.enc, r.handle));
            }
        }
    }

    fn unmap(&mut self) {
        if !self.mapped.is_null() {
            unsafe {
                let _ = nv!(self.api, nvEncUnmapInputResource(self.enc, self.mapped));
            }
            self.mapped = null_mut();
        }
    }

    /// Wait (bounded) for an in-flight frame and discard its output.
    fn discard_in_flight(&mut self) {
        let stage = std::mem::replace(&mut self.stage, Stage::Idle);
        if stage == Stage::Encoding {
            unsafe {
                if WaitForSingleObject(self.ev.raw(), 500) == WAIT_OBJECT_0 {
                    let mut lk: NV_ENC_LOCK_BITSTREAM = zeroed();
                    lk.version = NV_ENC_LOCK_BITSTREAM_VER;
                    lk.outputBitstream = self.bitstream;
                    if nv!(self.api, nvEncLockBitstream(self.enc, &mut lk)) == NV_ENC_SUCCESS {
                        let _ = nv!(self.api, nvEncUnlockBitstream(self.enc, self.bitstream));
                    }
                }
            }
            self.ev.reset();
        }
        self.unmap();
    }

    /// Release everything (idempotent; used by Drop and before a full re-create).
    fn teardown(&mut self) {
        if self.enc.is_null() {
            return;
        }
        self.discard_in_flight();
        self.unregister();
        unsafe {
            if !self.bitstream.is_null() {
                let _ = nv!(self.api, nvEncDestroyBitstreamBuffer(self.enc, self.bitstream));
                self.bitstream = null_mut();
            }
            if self.event_registered {
                let mut evp: NV_ENC_EVENT_PARAMS = zeroed();
                evp.version = NV_ENC_EVENT_PARAMS_VER;
                evp.completionEvent = self.ev.raw().0;
                let _ = nv!(self.api, nvEncUnregisterAsyncEvent(self.enc, &mut evp));
                self.event_registered = false;
            }
            let _ = nv!(self.api, nvEncDestroyEncoder(self.enc));
        }
        self.enc = null_mut();
    }

    /// Map the registered texture and hand it to NVENC (async; completion sets `ev`).
    fn submit(&mut self, ts_us: i64, force_idr: bool) -> Result<()> {
        let api = self.api;
        unsafe {
            let mut mp: NV_ENC_MAP_INPUT_RESOURCE = zeroed();
            mp.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
            mp.registeredResource = self.reg.as_ref().map(|r| r.handle).unwrap_or(null_mut());
            let st = nv!(api, nvEncMapInputResource(self.enc, &mut mp));
            self.check("MapInputResource", st)?;
            self.mapped = mp.mappedResource;

            let mut pp: Box<NV_ENC_PIC_PARAMS> = Box::new(zeroed());
            pp.version = NV_ENC_PIC_PARAMS_VER;
            pp.inputWidth = self.params.width;
            pp.inputHeight = self.params.height;
            pp.inputPitch = self.params.width;
            pp.encodePicFlags = if force_idr {
                NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS
            } else {
                0
            };
            pp.frameIdx = self.frame_idx;
            pp.inputTimeStamp = ts_us as u64;
            pp.inputDuration = 1_000_000 / self.params.fps.max(1) as u64;
            pp.inputBuffer = self.mapped;
            pp.outputBitstream = self.bitstream;
            pp.completionEvent = self.ev.raw().0;
            pp.bufferFmt = mp.mappedBufferFmt;
            pp.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
            self.ev.reset();
            let st = nv!(api, nvEncEncodePicture(self.enc, &mut *pp));
            if st != NV_ENC_SUCCESS && st != NV_ENC_ERR_NEED_MORE_INPUT {
                self.unmap();
                return Err(CaptureError::Stopped(format!(
                    "{}",
                    self.err("EncodePicture", st)
                )));
            }
        }
        self.frame_idx = self.frame_idx.wrapping_add(1);
        self.stage = Stage::Encoding;
        Ok(())
    }

    fn recreate(&mut self, p: EncParams) -> Result<()> {
        self.teardown();
        let fresh = Self::new(&self.device, p, self.ev.clone())?;
        *self = fresh;
        Ok(())
    }
}

fn create_fence(device: &ID3D11Device) -> Option<(ID3D11Fence, ID3D11DeviceContext4)> {
    unsafe {
        let dev5: ID3D11Device5 = device.cast().ok()?;
        let ctx = device.GetImmediateContext().ok()?;
        let ctx4: ID3D11DeviceContext4 = ctx.cast().ok()?;
        let mut fence: Option<ID3D11Fence> = None;
        dev5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence).ok()?;
        Some((fence?, ctx4))
    }
}

/// Rate-control mode, AQ, multipass, QP cap. Lookahead stays off (it adds frames of latency).
fn apply_tuning(rc: &mut NV_ENC_RC_PARAMS, t: &NvTuning) {
    rc.rateControlMode = if t.vbr { NV_ENC_PARAMS_RC_VBR } else { NV_ENC_PARAMS_RC_CBR };
    rc.flags &= !(RC_ENABLE_LOOKAHEAD
        | RC_ENABLE_AQ
        | RC_ENABLE_TEMPORAL_AQ
        | RC_AQ_STRENGTH_MASK
        | RC_ENABLE_MAX_QP
        | RC_ENABLE_MIN_QP);
    rc.lookaheadDepth = 0;
    if let Some(s) = t.aq {
        rc.flags |= RC_ENABLE_AQ | ((s.min(15) as u32) << RC_AQ_STRENGTH_SHIFT);
    }
    if t.temporal_aq {
        rc.flags |= RC_ENABLE_TEMPORAL_AQ;
    }
    if let Some(m) = t.multipass {
        rc.multiPass = m.min(2) as u32;
    }
    if t.key_scale > 0 {
        rc.lowDelayKeyFrameScale = t.key_scale;
    }
    if t.max_qp > 0 {
        rc.flags |= RC_ENABLE_MAX_QP;
        let q = t.max_qp as u32;
        rc.maxQP = NV_ENC_QP {
            qpInterP: q,
            qpInterB: q,
            qpIntra: q,
        };
    }
}

fn set_rate(rc: &mut NV_ENC_RC_PARAMS, bitrate: u32, fps: u32, t: &NvTuning) {
    rc.averageBitRate = bitrate;
    rc.maxBitRate = bitrate;
    // ~1 frame of VBV: every frame fits its budget, so no frame waits in the network queue.
    let vbv = ((bitrate as f64 / fps.max(1) as f64) * t.vbv_frames as f64).max(1.0) as u32;
    rc.vbvBufferSize = vbv;
    rc.vbvInitialDelay = vbv;
}

impl HwEncoder for NvEncoder {
    fn backend(&self) -> EncoderBackend {
        EncoderBackend::Nvenc
    }

    fn name(&self) -> String {
        format!(
            "NVENC H.264 Main {} (API {}.{}, driver max {}.{})",
            self.params.tuning.nv,
            NVENCAPI_MAJOR_VERSION,
            NVENCAPI_MINOR_VERSION,
            self.api.driver_version >> 4,
            self.api.driver_version & 0xf
        )
    }

    fn ready(&self) -> bool {
        self.stage == Stage::Idle
    }

    fn in_flight(&self) -> usize {
        (self.stage != Stage::Idle) as usize
    }

    fn encode(
        &mut self,
        _ctx: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        ts_us: i64,
        force_idr: bool,
    ) -> Result<()> {
        if self.stage != Stage::Idle {
            return Err(CaptureError::Os("NVENC busy".into()));
        }
        let api = self.api;
        unsafe {
            if self.reg.as_ref().map(|r| r.tex.as_raw()) != Some(tex.as_raw()) {
                self.unregister();
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                let mut rr: NV_ENC_REGISTER_RESOURCE = zeroed();
                rr.version = NV_ENC_REGISTER_RESOURCE_VER;
                rr.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
                rr.width = desc.Width;
                rr.height = desc.Height;
                rr.pitch = 0;
                rr.resourceToRegister = tex.as_raw();
                rr.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
                rr.bufferUsage = NV_ENC_INPUT_IMAGE;
                let st = nv!(api, nvEncRegisterResource(self.enc, &mut rr));
                self.check("RegisterResource", st)?;
                self.reg = Some(Registered {
                    tex: tex.clone(),
                    handle: rr.registeredResource,
                });
            }
            if let Some((fence, ctx4)) = &self.fence {
                // Wait for the blit on the GPU via the loop's event instead of inside the driver.
                self.fence_value += 1;
                let value = self.fence_value;
                self.ev.reset();
                ctx4.Signal(fence, value)
                    .map_err(|e| crate::win::os("ID3D11DeviceContext4::Signal", e))?;
                ctx4.Flush();
                fence
                    .SetEventOnCompletion(value, self.ev.raw())
                    .map_err(|e| crate::win::os("ID3D11Fence::SetEventOnCompletion", e))?;
                self.stage = Stage::WaitGpu {
                    ts_us,
                    force_idr,
                    value,
                };
                return Ok(());
            }
        }
        self.submit(ts_us, force_idr)
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.discard_in_flight();
        self.unregister(); // the converter's output texture is recreated at the new size
        let mut p = self.params;
        p.width = width;
        p.height = height;
        if width <= self.params.max_width && height <= self.params.max_height {
            self.init.encodeWidth = width;
            self.init.encodeHeight = height;
            self.init.darWidth = width;
            self.init.darHeight = height;
            match self.reconfigure(RECONF_RESET_ENCODER | RECONF_FORCE_IDR) {
                Ok(()) => {
                    self.params = p;
                    return Ok(());
                }
                Err(e) => tracing::warn!(error = %e, "NVENC reconfigure for resize failed; re-creating"),
            }
        }
        p.max_width = p.max_width.max(width);
        p.max_height = p.max_height.max(height);
        self.recreate(p)
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        set_rate(&mut self.cfg.rcParams, bps, self.params.fps, &self.params.tuning.nv);
        self.reconfigure(0)?;
        self.params.bitrate = bps;
        Ok(())
    }

    fn poll(&mut self, out: &mut dyn FnMut(&[u8], i64)) -> Result<()> {
        match self.stage {
            Stage::Idle => {
                self.ev.reset(); // stale signal: keep the manual-reset event from spinning the loop
                Ok(())
            }
            Stage::WaitGpu {
                ts_us,
                force_idr,
                value,
            } => {
                let done = self
                    .fence
                    .as_ref()
                    .is_none_or(|(f, _)| unsafe { f.GetCompletedValue() } >= value);
                if done {
                    self.stage = Stage::Idle;
                    self.submit(ts_us, force_idr)?;
                }
                Ok(())
            }
            Stage::Encoding => {
                if unsafe { WaitForSingleObject(self.ev.raw(), 0) } != WAIT_OBJECT_0 {
                    return Ok(());
                }
                self.ev.reset();
                let api = self.api;
                let res = unsafe {
                    let mut lk: NV_ENC_LOCK_BITSTREAM = zeroed();
                    lk.version = NV_ENC_LOCK_BITSTREAM_VER;
                    lk.outputBitstream = self.bitstream;
                    let st = nv!(api, nvEncLockBitstream(self.enc, &mut lk));
                    if st == NV_ENC_SUCCESS {
                        let data = std::slice::from_raw_parts(
                            lk.bitstreamBufferPtr as *const u8,
                            lk.bitstreamSizeInBytes as usize,
                        );
                        out(data, lk.outputTimeStamp as i64);
                        let _ = nv!(api, nvEncUnlockBitstream(self.enc, self.bitstream));
                        Ok(())
                    } else {
                        Err(CaptureError::Stopped(format!("{}", self.err("LockBitstream", st))))
                    }
                };
                self.stage = Stage::Idle;
                self.unmap();
                res
            }
        }
    }
}

impl Drop for NvEncoder {
    fn drop(&mut self) {
        self.teardown();
        super::NVENC_LIVE.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}
