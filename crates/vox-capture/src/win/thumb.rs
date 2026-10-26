//! One-shot thumbnails (v0.3, item 8c): a single Windows.Graphics.Capture frame of a window or
//! monitor, scaled on the GPU and read back as a small RGBA image. Nothing is kept: no file, no
//! log of the picture, and the capture session is torn down before the call returns.
//!
//! ```text
//!  WGC item -> frame pool (2 buffers) -> StartCapture -> first FrameArrived (timeout)
//!    -> video processor: BGRA -> BGRA at up to 2x the thumbnail size (GPU, optimal quality)
//!    -> staging copy -> Map -> 2x2-ish box filter on the CPU -> RGBA
//! ```
//!
//! The two-step scale (GPU to twice the size, then an exact area average on the CPU) keeps text
//! and thin lines readable when a 4K screen becomes a 240x135 card; the video processor alone
//! aliases a lot at that ratio. If the video processor cannot do the job, the whole frame is read
//! back and averaged on the CPU instead (slower, same picture).
//!
//! One [`GrabberInner`] owns a D3D11 device that is reused for many grabs (creating the device
//! costs more than a grab), so a worker thread makes one and loops. It is bound to its thread.

use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HWND, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindow};
use windows_core::BOOL;

use super::enumerate::{adapter_for_monitor, monitors_with_handles};
use super::video::create_device;
use super::{os, ComInit, Event};
use crate::{CaptureError, Result, RgbaImage, VideoTarget};

/// How long to wait for the first frame of a target before giving up on it.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_millis(2500);

struct Gpu {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    winrt: IDirect3DDevice,
}

pub struct GrabberInner {
    timeout: Duration,
    gpu: Option<Gpu>,
    /// Dropped after `gpu` (declaration order), on the thread that made it.
    _com: ComInit,
}

impl GrabberInner {
    pub fn new() -> Result<Self> {
        Ok(Self { timeout: DEFAULT_TIMEOUT, gpu: None, _com: ComInit::mta() })
    }

    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t.clamp(Duration::from_millis(100), Duration::from_secs(30));
    }

    fn gpu(&mut self) -> Result<&Gpu> {
        if self.gpu.is_none() {
            // The adapter that scans out the primary monitor; WGC copes with other adapters.
            let adapter = monitors_with_handles()
                .ok()
                .and_then(|m| m.into_iter().find(|(_, i)| i.primary).map(|(h, _)| h))
                .and_then(adapter_for_monitor);
            let (device, ctx, _) = create_device(adapter.as_ref())?;
            unsafe {
                if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                    let _ = mt.SetMultithreadProtected(true);
                }
            }
            let winrt: IDirect3DDevice = unsafe {
                let dxgi: IDXGIDevice = device.cast().map_err(|e| os("IDXGIDevice", e))?;
                CreateDirect3D11DeviceFromDXGIDevice(&dxgi).map_err(|e| os("CreateDirect3D11DeviceFromDXGIDevice", e))?
            }
            .cast()
            .map_err(|e| os("IDirect3DDevice", e))?;
            self.gpu = Some(Gpu { device, ctx, winrt });
        }
        Ok(self.gpu.as_ref().expect("just made"))
    }

    pub fn grab(&mut self, target: VideoTarget, max_w: u32, max_h: u32) -> Result<RgbaImage> {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(CaptureError::Unsupported("Windows.Graphics.Capture not supported on this system".into()));
        }
        let (max_w, max_h) = (max_w.clamp(16, 1920), max_h.clamp(16, 1080));
        let timeout = self.timeout;
        let (hmon, hwnd) = match target {
            VideoTarget::Monitor(i) => {
                let (h, _) = monitors_with_handles()?
                    .into_iter()
                    .nth(i)
                    .ok_or_else(|| CaptureError::NotFound(format!("monitor index {i}")))?;
                (h, None)
            }
            VideoTarget::Window(raw) => {
                let hwnd = HWND(raw as *mut _);
                if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                    return Err(CaptureError::NotFound(format!("window 0x{raw:X}")));
                }
                if unsafe { IsIconic(hwnd) }.as_bool() {
                    return Err(CaptureError::Stopped("window is minimized".into()));
                }
                (unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }, Some(hwnd))
            }
        };
        let t_begin = Instant::now();
        let gpu = self.gpu()?;
        let interop = windows_core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| os("IGraphicsCaptureItemInterop", e))?;
        let item: GraphicsCaptureItem = unsafe {
            match hwnd {
                Some(h) => interop.CreateForWindow(h).map_err(|e| os("CreateForWindow", e))?,
                None => interop.CreateForMonitor(hmon).map_err(|e| os("CreateForMonitor", e))?,
            }
        };
        let size = item.Size().map_err(|e| os("GraphicsCaptureItem::Size", e))?;
        if size.Width < 2 || size.Height < 2 {
            return Err(CaptureError::Stopped("capture target has no size".into()));
        }
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &gpu.winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(|e| os("CreateFreeThreaded", e))?;
        let session = pool.CreateCaptureSession(&item).map_err(|e| os("CreateCaptureSession", e))?;
        let ev = Arc::new(Event::new(false)?);
        let token = {
            let ev = ev.clone();
            pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
                ev.set();
                Ok(())
            }))
            .map_err(|e| os("FrameArrived", e))?
        };
        // Declared after the handler so it runs first: stop the session, then the pool.
        let _guard = Teardown { pool: pool.clone(), session: session.clone(), token };
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().map_err(|e| os("StartCapture", e))?;
        let t_started = Instant::now();
        let deadline = Instant::now() + timeout;
        let frame = loop {
            if let Ok(f) = pool.TryGetNextFrame() {
                break f;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(CaptureError::Stopped("no frame within the time limit".into()));
            }
            let r = unsafe { WaitForSingleObject(ev.raw(), left.as_millis().min(u32::MAX as u128 - 1) as u32) };
            if r != WAIT_OBJECT_0 && Instant::now() >= deadline {
                return Err(CaptureError::Stopped("no frame within the time limit".into()));
            }
        };
        let t_frame = Instant::now();
        let content = frame.ContentSize().map_err(|e| os("ContentSize", e))?;
        let tex: ID3D11Texture2D = unsafe {
            let surface = frame.Surface().map_err(|e| os("Frame::Surface", e))?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| os("IDirect3DDxgiInterfaceAccess", e))?;
            access.GetInterface().map_err(|e| os("GetInterface(ID3D11Texture2D)", e))?
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let src_w = (content.Width.max(2) as u32).min(desc.Width);
        let src_h = (content.Height.max(2) as u32).min(desc.Height);
        let (fw, fh) = thumb_fit(src_w, src_h, max_w, max_h);
        let img = scale_to_rgba(gpu, &tex, (desc.Width, desc.Height), (src_w, src_h), (fw, fh));
        let _ = frame.Close();
        tracing::debug!(setup_ms = (t_started - t_begin).as_millis() as u64, wait_ms = (t_frame - t_started).as_millis() as u64, scale_ms = t_frame.elapsed().as_millis() as u64, "thumbnail grabbed");
        img
    }
}

/// Fit (w, h) inside (max_w, max_h) keeping the aspect, never enlarging.
pub(crate) fn thumb_fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let s = (max_w as f64 / w.max(1) as f64).min(max_h as f64 / h.max(1) as f64).min(1.0);
    (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
}

/// Stops the session and the pool (in that order) whichever way `grab` returns.
struct Teardown {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    token: i64,
}

impl Drop for Teardown {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// GPU path first, whole-frame CPU path if the video processor refuses.
fn scale_to_rgba(
    gpu: &Gpu,
    tex: &ID3D11Texture2D,
    tex_size: (u32, u32),
    src: (u32, u32),
    fin: (u32, u32),
) -> Result<RgbaImage> {
    // Twice the final size when the source has the pixels for it.
    let mid = if src.0 >= fin.0 * 2 && src.1 >= fin.1 * 2 { (fin.0 * 2, fin.1 * 2) } else { fin };
    match gpu_scale(gpu, tex, tex_size, src, mid) {
        Ok(bgra) => Ok(box_downscale_bgra(&bgra, mid.0 as usize * 4, mid, fin)),
        Err(e) => {
            tracing::debug!(error = %e, "thumbnail: video processor failed; scaling on the CPU");
            let (bgra, stride) = read_texture(gpu, tex)?;
            Ok(box_downscale_bgra(&bgra, stride, src, fin))
        }
    }
}

/// BGRA texture -> BGRA `out` size on the GPU -> tightly packed bytes.
fn gpu_scale(gpu: &Gpu, tex: &ID3D11Texture2D, tex_size: (u32, u32), src: (u32, u32), out: (u32, u32)) -> Result<Vec<u8>> {
    let vdev: ID3D11VideoDevice = gpu.device.cast().map_err(|e| CaptureError::Unsupported(format!("no ID3D11VideoDevice: {e}")))?;
    let vctx: ID3D11VideoContext = gpu.ctx.cast().map_err(|e| os("ID3D11VideoContext", e))?;
    let out_tex = unsafe {
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 30, Denominator: 1 },
            InputWidth: tex_size.0,
            InputHeight: tex_size.1,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 30, Denominator: 1 },
            OutputWidth: out.0,
            OutputHeight: out.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        let enumerator = vdev.CreateVideoProcessorEnumerator(&desc).map_err(|e| os("CreateVideoProcessorEnumerator", e))?;
        let support = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM).unwrap_or(0);
        if support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
            return Err(CaptureError::Unsupported("video processor cannot output BGRA".into()));
        }
        let vp = vdev.CreateVideoProcessor(&enumerator, 0).map_err(|e| os("CreateVideoProcessor", e))?;
        let out_desc = D3D11_TEXTURE2D_DESC {
            Width: out.0,
            Height: out.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut out_tex = None;
        gpu.device.CreateTexture2D(&out_desc, None, Some(&mut out_tex)).map_err(|e| os("CreateTexture2D(thumb out)", e))?;
        let out_tex = out_tex.ok_or_else(|| CaptureError::Os("CreateTexture2D returned null".into()))?;
        let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut out_view = None;
        vdev.CreateVideoProcessorOutputView(&out_tex, &enumerator, &ov_desc, Some(&mut out_view))
            .map_err(|e| os("CreateVideoProcessorOutputView", e))?;
        let out_view = out_view.ok_or_else(|| CaptureError::Os("output view null".into()))?;
        let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut in_view = None;
        vdev.CreateVideoProcessorInputView(tex, &enumerator, &iv_desc, Some(&mut in_view))
            .map_err(|e| os("CreateVideoProcessorInputView", e))?;

        vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
        vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
        let src_rect = RECT { left: 0, top: 0, right: src.0 as i32, bottom: src.1 as i32 };
        let out_rect = RECT { left: 0, top: 0, right: out.0 as i32, bottom: out.1 as i32 };
        vctx.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&src_rect));
        vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&out_rect));
        vctx.VideoProcessorSetOutputTargetRect(&vp, true, Some(&out_rect));
        if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
            // RGB in, RGB out: no colour conversion, just the scale.
            v1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
        }
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            pInputSurface: std::mem::ManuallyDrop::new(in_view),
            ..Default::default()
        };
        let r = vctx.VideoProcessorBlt(&vp, &out_view, 0, std::slice::from_ref(&stream));
        std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
        r.map_err(|e| os("VideoProcessorBlt", e))?;
        out_tex
    };
    let (bytes, stride) = read_texture(gpu, &out_tex)?;
    if stride == out.0 as usize * 4 {
        return Ok(bytes);
    }
    // read_texture packs rows to the texture's own width, so this is only a safety net.
    Err(CaptureError::Os("unexpected thumbnail stride".into()))
}

/// Copy a BGRA texture to the CPU. Returns the bytes (rows tightly packed at `width * 4`) and
/// that stride.
fn read_texture(gpu: &Gpu, tex: &ID3D11Texture2D) -> Result<(Vec<u8>, usize)> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    let (w, h) = (desc.Width as usize, desc.Height as usize);
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: desc.Width,
        Height: desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    unsafe { gpu.device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }.map_err(|e| os("CreateTexture2D(thumb staging)", e))?;
    let staging = staging.ok_or_else(|| CaptureError::Os("CreateTexture2D returned null".into()))?;
    let mut out = vec![0u8; w * h * 4];
    unsafe {
        gpu.ctx.CopyResource(&staging, tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        // Blocks until the GPU is done with the copy (a few ms at most for a thumbnail).
        gpu.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).map_err(|e| os("Map(thumb staging)", e))?;
        let base = m.pData as *const u8;
        for y in 0..h {
            std::ptr::copy_nonoverlapping(base.add(y * m.RowPitch as usize), out.as_mut_ptr().add(y * w * 4), w * 4);
        }
        gpu.ctx.Unmap(&staging, 0);
    }
    Ok((out, w * 4))
}

/// Area-average the top-left `src` pixels of a BGRA image (rows `stride` bytes apart) down to
/// `dst`, as RGBA with a fully opaque alpha. Equal sizes only swizzle. Never upscales: callers
/// pass `dst <= src`; a larger `dst` falls back to nearest-neighbour.
pub(crate) fn box_downscale_bgra(bgra: &[u8], stride: usize, src: (u32, u32), dst: (u32, u32)) -> RgbaImage {
    let (sw, sh) = (src.0 as usize, src.1 as usize);
    let (dw, dh) = (dst.0 as usize, dst.1 as usize);
    let mut data = vec![255u8; dw * dh * 4];
    for dy in 0..dh {
        let y0 = dy * sh / dh;
        let y1 = ((dy + 1) * sh / dh).max(y0 + 1).min(sh);
        for dx in 0..dw {
            let x0 = dx * sw / dw;
            let x1 = ((dx + 1) * sw / dw).max(x0 + 1).min(sw);
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for y in y0..y1 {
                let row = &bgra[y * stride..];
                for x in x0..x1 {
                    let p = &row[x * 4..x * 4 + 4];
                    b += p[0] as u32;
                    g += p[1] as u32;
                    r += p[2] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (dy * dw + dx) * 4;
            data[o] = (r / n) as u8;
            data[o + 1] = (g / n) as u8;
            data[o + 2] = (b / n) as u8;
        }
    }
    RgbaImage { width: dst.0, height: dst.1, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_filter_averages_and_swizzles() {
        // 4x2 BGRA: left half pure blue-ish (b=200), right half red (r=100).
        let mut px = Vec::new();
        for _y in 0..2 {
            for x in 0..4 {
                if x < 2 { px.extend_from_slice(&[200, 0, 0, 0]) } else { px.extend_from_slice(&[0, 0, 100, 0]) }
            }
        }
        let same = box_downscale_bgra(&px, 16, (4, 2), (4, 2));
        assert_eq!(&same.data[..4], &[0, 0, 200, 255], "BGRA -> RGBA, opaque");
        let half = box_downscale_bgra(&px, 16, (4, 2), (2, 1));
        assert_eq!(half.data, vec![0, 0, 200, 255, 100, 0, 0, 255]);
        let mixed = box_downscale_bgra(&px, 16, (4, 2), (1, 1));
        assert_eq!(&mixed.data[..], &[50, 0, 100, 255]);
    }

    #[test]
    fn fit_keeps_aspect_and_never_enlarges() {
        assert_eq!(thumb_fit(1920, 1080, 320, 180), (320, 180));
        assert_eq!(thumb_fit(960, 600, 240, 135), (216, 135));
        assert_eq!(thumb_fit(1080, 1920, 320, 180), (101, 180));
        assert_eq!(thumb_fit(100, 50, 320, 180), (100, 50));
        assert_eq!(thumb_fit(4000, 10, 320, 180), (320, 1));
    }

    #[test]
    fn box_filter_respects_stride_and_crop() {
        // 3 px wide content inside rows of 4 px (stride 16): the 4th column is padding.
        let mut px = vec![0u8; 16 * 2];
        for y in 0..2 {
            for x in 0..3 {
                px[y * 16 + x * 4 + 2] = 90; // red
            }
            px[y * 16 + 12 + 2] = 250; // padding, must be ignored
        }
        let img = box_downscale_bgra(&px, 16, (3, 2), (3, 2));
        assert!(img.data.chunks(4).all(|p| p == [90, 0, 0, 255]));
    }
}
