//! Lean D3D11 renderer for the Go Live viewer (Windows only).
//!
//! - Split in three so one viewer thread can drive N video surfaces: [`D3dGpu`] (device,
//!   shaders; one per thread), [`D3dTarget`] (one flip-model swapchain per window, pop-out or
//!   child window inside the app) and [`VideoPlanes`] (one set of textures per stream).
//!   [`D3dRenderer`] bundles one of each for single-window users.
//! - Flip-model swapchain (`FLIP_DISCARD`, 2 buffers) on the viewer's HWND.
//! - Decoded planes are uploaded into dynamic R8 (Y, U, V) or R8 + R8G8 (NV12) textures that are
//!   reused across frames and recreated only when the resolution or layout changes.
//! - A pixel shader converts BT.709 limited-range YUV to RGB. Aspect-correct scaling is done by
//!   the rasterizer (letterboxed viewport + bilinear sampling), so the CPU never touches pixels
//!   apart from one memcpy per plane into the mapped textures.
//! - Nothing spins: the owner calls [`D3dRenderer::upload`] + [`D3dRenderer::draw`] when a frame
//!   arrives and [`D3dRenderer::resize`] + [`D3dRenderer::draw`] when the window changes.
//!
//! HLSL is compiled once per renderer with `D3DCompile` (d3dcompiler_47.dll ships with Windows).

use std::ffi::c_void;

use livekit::webrtc::video_frame::VideoBuffer;
use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{DXGI_STATUS_OCCLUDED, HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_SRV_DIMENSION_TEXTURE2D, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_MWA_NO_ALT_ENTER, DXGI_MWA_NO_WINDOW_CHANGES, DXGI_PRESENT,
    DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

const SHADER: &str = r#"
Texture2D<float>  texY  : register(t0);
Texture2D<float>  texU  : register(t1);
Texture2D<float>  texV  : register(t2);
Texture2D<float2> texUV : register(t1);
SamplerState smp : register(s0);

struct VsOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

// Full-screen triangle from SV_VertexID; the viewport does the letterboxing.
VsOut vs_main(uint id : SV_VertexID) {
    VsOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}

// Same, mirrored horizontally (the local camera preview, like a mirror).
VsOut vs_mirror(uint id : SV_VertexID) {
    VsOut o = vs_main(id);
    o.uv.x = 1.0 - o.uv.x;
    return o;
}

// BT.709, limited (studio) range.
float4 yuv709(float y, float2 c) {
    y = (y - 16.0 / 255.0) * (255.0 / 219.0);
    c = (c - 128.0 / 255.0) * (255.0 / 224.0);
    float3 rgb = float3(y + 1.5748 * c.y,
                        y - 0.1873 * c.x - 0.4681 * c.y,
                        y + 1.8556 * c.x);
    return float4(saturate(rgb), 1.0);
}

float4 ps_i420(VsOut i) : SV_Target {
    return yuv709(texY.Sample(smp, i.uv), float2(texU.Sample(smp, i.uv), texV.Sample(smp, i.uv)));
}

float4 ps_nv12(VsOut i) : SV_Target {
    return yuv709(texY.Sample(smp, i.uv), texUV.Sample(smp, i.uv));
}
"#;

/// Why a renderer call failed; the owner decides between "recreate" and "fall back".
#[derive(Debug)]
pub enum RenderError {
    /// GPU reset / driver update / TDR: drop the renderer and create a new one.
    DeviceLost(windows::core::Error),
    Other(windows::core::Error),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::DeviceLost(e) => write!(f, "device lost: {e}"),
            RenderError::Other(e) => write!(f, "{e}"),
        }
    }
}

fn classify(e: windows::core::Error) -> RenderError {
    let code = e.code();
    if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
        RenderError::DeviceLost(e)
    } else {
        RenderError::Other(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    I420,
    Nv12,
}

/// Per-resolution plane textures (recreated only on size/layout change). They belong to the
/// [`D3dGpu`] that made them and live per video stream, so a stream keeps its picture when it
/// moves to another window (inline <-> pop-out).
pub struct VideoPlanes {
    layout: Layout,
    width: u32,
    height: u32,
    /// (texture, width, height, bytes per pixel)
    tex: Vec<(ID3D11Texture2D, u32, u32, u32)>,
    srvs: Vec<Option<ID3D11ShaderResourceView>>,
}

impl VideoPlanes {
    /// Size of the video held in the textures.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Bytes held in the textures (for logs).
    pub fn bytes(&self) -> usize {
        self.tex.iter().map(|(_, w, h, bpp)| (*w * *h * *bpp) as usize).sum()
    }

    /// Copy a decoded frame into `slot`'s textures (made or remade as needed). Returns
    /// `Ok(false)` for buffer types we cannot read (the textures keep the previous picture).
    pub fn upload(gpu: &D3dGpu, slot: &mut Option<VideoPlanes>, buf: &dyn VideoBuffer) -> Result<bool, RenderError> {
        let (w, h) = (buf.width(), buf.height());
        if w == 0 || h == 0 {
            return Ok(false);
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        if let Some(i) = buf.as_i420() {
            let p = Self::ensure(gpu, slot, Layout::I420, w, h).map_err(classify)?;
            let (sy, su, sv) = i.strides();
            let (py, pu, pv) = i.data();
            p.write(gpu, 0, py, sy as usize, w as usize, h as usize)?;
            p.write(gpu, 1, pu, su as usize, cw as usize, ch as usize)?;
            p.write(gpu, 2, pv, sv as usize, cw as usize, ch as usize)?;
            Ok(true)
        } else if let Some(n) = buf.as_nv12() {
            let p = Self::ensure(gpu, slot, Layout::Nv12, w, h).map_err(classify)?;
            let (sy, suv) = n.strides();
            let (py, puv) = n.data();
            p.write(gpu, 0, py, sy as usize, w as usize, h as usize)?;
            p.write(gpu, 1, puv, suv as usize, cw as usize * 2, ch as usize)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn ensure<'a>(gpu: &D3dGpu, slot: &'a mut Option<VideoPlanes>, layout: Layout, width: u32, height: u32) -> windows::core::Result<&'a mut VideoPlanes> {
        if slot.as_ref().is_some_and(|p| p.layout == layout && p.width == width && p.height == height) {
            return Ok(slot.as_mut().expect("checked"));
        }
        *slot = None;
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let specs: Vec<(u32, u32, DXGI_FORMAT, u32)> = match layout {
            Layout::I420 => vec![
                (width, height, DXGI_FORMAT_R8_UNORM, 1),
                (cw, ch, DXGI_FORMAT_R8_UNORM, 1),
                (cw, ch, DXGI_FORMAT_R8_UNORM, 1),
            ],
            Layout::Nv12 => vec![(width, height, DXGI_FORMAT_R8_UNORM, 1), (cw, ch, DXGI_FORMAT_R8G8_UNORM, 2)],
        };
        let mut tex = Vec::with_capacity(specs.len());
        let mut srvs = Vec::with_capacity(specs.len());
        for (w, h, format, bpp) in specs {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                MiscFlags: 0,
            };
            let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 } },
            };
            // SAFETY: valid descriptors; out-pointers to locals.
            unsafe {
                let mut t = None;
                gpu.device.CreateTexture2D(&desc, None, Some(&mut t))?;
                let t: ID3D11Texture2D = t.ok_or_else(windows::core::Error::empty)?;
                let mut srv = None;
                gpu.device.CreateShaderResourceView(&t, Some(&srv_desc), Some(&mut srv))?;
                srvs.push(srv);
                tex.push((t, w, h, bpp));
            }
        }
        Ok(slot.insert(VideoPlanes { layout, width, height, tex, srvs }))
    }

    fn write(&self, gpu: &D3dGpu, index: usize, src: &[u8], src_stride: usize, row_bytes: usize, rows: usize) -> Result<(), RenderError> {
        let (tex, _, _, _) = &self.tex[index];
        if rows == 0 || src.len() < src_stride * (rows - 1) + row_bytes {
            return Ok(());
        }
        // SAFETY: dynamic texture mapped with WRITE_DISCARD; we write `rows` rows of `row_bytes`
        // (the texture's width * bpp) at the driver-provided row pitch, then unmap.
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.ctx.Map(tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).map_err(classify)?;
            let pitch = mapped.RowPitch as usize;
            let dst = mapped.pData as *mut u8;
            if pitch == src_stride && pitch == row_bytes {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, row_bytes * rows);
            } else {
                for r in 0..rows {
                    std::ptr::copy_nonoverlapping(src.as_ptr().add(r * src_stride), dst.add(r * pitch), row_bytes);
                }
            }
            gpu.ctx.Unmap(tex, 0);
        }
        Ok(())
    }
}

/// The device, shaders and sampler: one per viewer thread, shared by every video surface.
pub struct D3dGpu {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    vs_mirror: ID3D11VertexShader,
    ps_i420: ID3D11PixelShader,
    ps_nv12: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
}

impl D3dGpu {
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: plain D3D11 object creation with valid descriptors and out-pointers.
        unsafe {
            let levels: [D3D_FEATURE_LEVEL; 4] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                // Single-threaded use, and no driver worker threads: measured -3 MB private and
                // ~60 fewer handles on NVIDIA, with no CPU cost for one small draw per frame.
                D3D11_CREATE_DEVICE_SINGLETHREADED | D3D11_CREATE_DEVICE_PREVENT_INTERNAL_THREADING_OPTIMIZATIONS,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )?;
            let device: ID3D11Device = device.ok_or_else(windows::core::Error::empty)?;
            let ctx: ID3D11DeviceContext = ctx.ok_or_else(windows::core::Error::empty)?;

            let vs_code = compile("vs_main", "vs_4_0")?;
            let mut vs = None;
            device.CreateVertexShader(blob_bytes(&vs_code), None, Some(&mut vs))?;
            let mut vs_mirror = None;
            device.CreateVertexShader(blob_bytes(&compile("vs_mirror", "vs_4_0")?), None, Some(&mut vs_mirror))?;
            let mut ps_i420 = None;
            device.CreatePixelShader(blob_bytes(&compile("ps_i420", "ps_4_0")?), None, Some(&mut ps_i420))?;
            let mut ps_nv12 = None;
            device.CreatePixelShader(blob_bytes(&compile("ps_nv12", "ps_4_0")?), None, Some(&mut ps_nv12))?;

            let sdesc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sdesc, Some(&mut sampler))?;
            Ok(D3dGpu {
                device,
                ctx,
                vs: vs.ok_or_else(windows::core::Error::empty)?,
                vs_mirror: vs_mirror.ok_or_else(windows::core::Error::empty)?,
                ps_i420: ps_i420.ok_or_else(windows::core::Error::empty)?,
                ps_nv12: ps_nv12.ok_or_else(windows::core::Error::empty)?,
                sampler: sampler.ok_or_else(windows::core::Error::empty)?,
            })
        }
    }

    /// Check for a removed device without presenting (e.g. after a failed Map).
    pub fn device_lost(&self) -> bool {
        // SAFETY: simple query on our device.
        unsafe { self.device.GetDeviceRemovedReason().is_err() }
    }

    /// After dropping a [`D3dTarget`]: unbind everything and flush, so DXGI really destroys the
    /// old swapchain and the same window can take a new one right away.
    pub fn flush_released(&self) {
        // SAFETY: plain state reset on our own immediate context.
        unsafe {
            self.ctx.ClearState();
            self.ctx.Flush();
        }
    }

    /// Draw `planes` letterboxed into `target`'s back buffer (black when there is no picture yet)
    /// and present. Returns `Ok(false)` while the window is occluded (nothing shown).
    pub fn draw(&self, target: &mut D3dTarget, planes: Option<&VideoPlanes>) -> Result<bool, RenderError> {
        self.draw_styled(target, planes, DrawStyle::default())
    }

    /// [`D3dGpu::draw`] with a [`DrawStyle`] (camera tiles: crop-to-fill, mirrored self-view).
    pub fn draw_styled(&self, target: &mut D3dTarget, planes: Option<&VideoPlanes>, style: DrawStyle) -> Result<bool, RenderError> {
        let Some(rtv) = target.rtv.clone() else { return Ok(false) };
        let (bw, bh) = target.size;
        // SAFETY: all bound objects are owned by this renderer and outlive the draw call.
        unsafe {
            let ctx = &self.ctx;
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            if let Some(p) = planes {
                let (x, y, w, h) = if style.fill { fill_viewport(p.width, p.height, bw, bh) } else { fit_viewport(p.width, p.height, bw, bh) };
                ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: x, TopLeftY: y, Width: w, Height: h, MinDepth: 0.0, MaxDepth: 1.0 }]));
                ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.IASetInputLayout(None);
                ctx.VSSetShader(if style.mirror { &self.vs_mirror } else { &self.vs }, None);
                match p.layout {
                    Layout::I420 => ctx.PSSetShader(&self.ps_i420, None),
                    Layout::Nv12 => ctx.PSSetShader(&self.ps_nv12, None),
                }
                ctx.PSSetShaderResources(0, Some(&p.srvs));
                ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
                ctx.Draw(3, 0);
            }
            // VSync-paced; the viewer's single-slot mailbox drops frames we can't show.
            let hr = target.swapchain.Present(1, DXGI_PRESENT(0));
            if hr == DXGI_STATUS_OCCLUDED {
                target.occluded = true;
                return Ok(false);
            }
            hr.ok().map_err(classify)?;
            target.occluded = false;
        }
        Ok(true)
    }
}

/// One window's flip-model swapchain (a pop-out window, or a child window inside the app).
pub struct D3dTarget {
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    /// Back-buffer size.
    size: (u32, u32),
    occluded: bool,
}

impl D3dTarget {
    /// Swapchain for `hwnd` (client size `width x height`). The window may belong to another
    /// thread (the app's child window): DXGI is told not to watch its message queue.
    pub fn new(gpu: &D3dGpu, hwnd: isize, width: u32, height: u32) -> windows::core::Result<Self> {
        let (width, height) = (width.max(1), height.max(1));
        // SAFETY: DXGI object creation with a valid descriptor; `hwnd` is a live window kept
        // alive by the caller for as long as this target exists.
        unsafe {
            let dxgi_device: IDXGIDevice = gpu.device.cast()?;
            let adapter = dxgi_device.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: 0,
            };
            let hwnd = HWND(hwnd as *mut c_void);
            let swapchain = factory.CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER | DXGI_MWA_NO_WINDOW_CHANGES);
            let mut t = D3dTarget { swapchain, rtv: None, size: (width, height), occluded: false };
            t.create_rtv(gpu)?;
            Ok(t)
        }
    }

    fn create_rtv(&mut self, gpu: &D3dGpu) -> windows::core::Result<()> {
        // SAFETY: buffer 0 of our own swapchain; RTV desc None = whole resource.
        unsafe {
            let back: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
            let mut rtv = None;
            gpu.device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
            self.rtv = rtv;
        }
        Ok(())
    }

    /// Resize the swapchain buffers to the new client size (no-op if unchanged or zero).
    pub fn resize(&mut self, gpu: &D3dGpu, width: u32, height: u32) -> Result<(), RenderError> {
        if width == 0 || height == 0 || (width, height) == self.size {
            return Ok(());
        }
        self.rtv = None;
        // SAFETY: every reference to the back buffer (the RTV, and the OM binding) is released
        // before ResizeBuffers, as DXGI requires.
        unsafe {
            gpu.ctx.OMSetRenderTargets(None, None);
            gpu.ctx.Flush();
            self.swapchain
                .ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))
                .map_err(classify)?;
        }
        self.size = (width, height);
        self.create_rtv(gpu).map_err(classify)
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn occluded(&self) -> bool {
        self.occluded
    }
}

/// One device + one window + one stream's textures (the winit viewer and the probes).
pub struct D3dRenderer {
    gpu: D3dGpu,
    target: D3dTarget,
    planes: Option<VideoPlanes>,
}

impl D3dRenderer {
    /// Create the device, swapchain and shaders for `hwnd` (client size `width x height`).
    pub fn new(hwnd: isize, width: u32, height: u32) -> windows::core::Result<Self> {
        let gpu = D3dGpu::new()?;
        let target = D3dTarget::new(&gpu, hwnd, width, height)?;
        Ok(D3dRenderer { gpu, target, planes: None })
    }

    /// Resize the swapchain buffers to the new client size (no-op if unchanged or zero).
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), RenderError> {
        self.target.resize(&self.gpu, width, height)
    }

    /// Size of the video currently held in the plane textures.
    pub fn video_size(&self) -> Option<(u32, u32)> {
        self.planes.as_ref().map(VideoPlanes::size)
    }

    /// Copy a decoded frame into the plane textures. Returns `Ok(false)` for buffer types we
    /// cannot read (the textures keep the previous picture).
    pub fn upload(&mut self, buf: &dyn VideoBuffer) -> Result<bool, RenderError> {
        VideoPlanes::upload(&self.gpu, &mut self.planes, buf)
    }

    /// Draw the current textures letterboxed into the back buffer and present.
    /// Returns `Ok(false)` while the window is occluded (nothing shown).
    pub fn draw(&mut self) -> Result<bool, RenderError> {
        self.gpu.draw(&mut self.target, self.planes.as_ref())
    }

    pub fn occluded(&self) -> bool {
        self.target.occluded()
    }

    /// Check for a removed device without presenting (e.g. after a failed Map).
    pub fn device_lost(&self) -> bool {
        self.gpu.device_lost()
    }

    /// Bytes held in plane textures (for logs).
    pub fn texture_bytes(&self) -> usize {
        self.planes.as_ref().map(VideoPlanes::bytes).unwrap_or(0)
    }
}

/// How a stream is drawn into its window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrawStyle {
    /// Mirror horizontally (the local camera preview).
    pub mirror: bool,
    /// Crop to fill the window (camera tiles, like Discord) instead of letterboxing.
    pub fill: bool,
}

/// Crop-to-fill viewport: the video covers the whole `bw x bh` back buffer, centred, the
/// overflow clipped by the rasterizer. Returns (x, y, w, h); x / y are <= 0.
pub fn fill_viewport(fw: u32, fh: u32, bw: u32, bh: u32) -> (f32, f32, f32, f32) {
    if fw == 0 || fh == 0 || bw == 0 || bh == 0 {
        return (0.0, 0.0, bw as f32, bh as f32);
    }
    let s = (bw as f64 / fw as f64).max(bh as f64 / fh as f64);
    let w = (fw as f64 * s).round().max(bw as f64);
    let h = (fh as f64 * s).round().max(bh as f64);
    (((bw as f64 - w) / 2.0).floor() as f32, ((bh as f64 - h) / 2.0).floor() as f32, w as f32, h as f32)
}

/// Letterboxed viewport for a `fw x fh` video inside a `bw x bh` back buffer (whole pixels,
/// centred). Returns (x, y, w, h).
pub fn fit_viewport(fw: u32, fh: u32, bw: u32, bh: u32) -> (f32, f32, f32, f32) {
    if fw == 0 || fh == 0 || bw == 0 || bh == 0 {
        return (0.0, 0.0, bw as f32, bh as f32);
    }
    let s = (bw as f64 / fw as f64).min(bh as f64 / fh as f64);
    let w = ((fw as f64 * s).round() as u32).clamp(1, bw);
    let h = ((fh as f64 * s).round() as u32).clamp(1, bh);
    let x = (bw - w) / 2;
    let y = (bh - h) / 2;
    (x as f32, y as f32, w as f32, h as f32)
}

fn compile(entry: &str, target: &str) -> windows::core::Result<ID3DBlob> {
    let entry = std::ffi::CString::new(entry).expect("no NUL");
    let target = std::ffi::CString::new(target).expect("no NUL");
    let mut code = None;
    let mut errors = None;
    // SAFETY: source pointer/length describe `SHADER`; entry/target are NUL-terminated.
    let res = unsafe {
        D3DCompile(
            SHADER.as_ptr() as *const c_void,
            SHADER.len(),
            PCSTR(c"vox_viewer.hlsl".as_ptr() as *const u8),
            None,
            None,
            PCSTR(entry.as_ptr() as *const u8),
            PCSTR(target.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = res {
        if let Some(err) = errors {
            let msg = String::from_utf8_lossy(blob_bytes(&err)).into_owned();
            tracing::warn!(entry = ?entry, error = %msg.trim(), "viewer shader compile failed");
        }
        return Err(e);
    }
    code.ok_or_else(windows::core::Error::empty)
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob owns `GetBufferSize()` bytes at `GetBufferPointer()` for its lifetime,
    // which the returned slice borrows.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_letterbox() {
        assert_eq!(fit_viewport(1920, 1080, 1920, 1080), (0.0, 0.0, 1920.0, 1080.0));
        assert_eq!(fit_viewport(1920, 1080, 1600, 900), (0.0, 0.0, 1600.0, 900.0));
        // Pillarbox: 4:3 into 16:9.
        assert_eq!(fit_viewport(1440, 1080, 1920, 1080), (240.0, 0.0, 1440.0, 1080.0));
        // Letterbox: 16:9 into a tall window.
        let (x, y, w, h) = fit_viewport(1920, 1080, 800, 800);
        assert_eq!((x, w), (0.0, 800.0));
        assert_eq!(h, 450.0);
        assert_eq!(y, 175.0);
    }

    #[test]
    fn viewport_fill_covers_and_centres() {
        assert_eq!(fill_viewport(1280, 720, 1280, 720), (0.0, 0.0, 1280.0, 720.0));
        // 16:9 video in a 4:3 tile: full height, sides cropped.
        let (x, y, w, h) = fill_viewport(1280, 720, 400, 300);
        assert_eq!((y, h), (0.0, 300.0));
        assert!(w > 400.0 && x < 0.0 && (x + w / 2.0 - 200.0).abs() <= 1.0);
        // 4:3 video in a 16:9 tile: full width, top/bottom cropped.
        let (x, y, w, h) = fill_viewport(640, 480, 1600, 900);
        assert_eq!((x, w), (0.0, 1600.0));
        assert!(h >= 900.0 && y <= 0.0);
    }

    #[test]
    fn shaders_compile() {
        for (e, t) in [("vs_main", "vs_4_0"), ("vs_mirror", "vs_4_0"), ("ps_i420", "ps_4_0"), ("ps_nv12", "ps_4_0")] {
            let blob = compile(e, t).unwrap_or_else(|err| panic!("{e}: {err}"));
            assert!(!blob_bytes(&blob).is_empty());
        }
    }
}
