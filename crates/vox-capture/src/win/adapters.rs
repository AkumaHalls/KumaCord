//! DXGI adapter enumeration for the automatic encoder choice (v0.7): vendor, integrated or
//! dedicated, LUID, driver version, and the adapter that drives the primary monitor.

use windows::core::Interface;
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};

use super::{enumerate, wide_to_string};
use crate::gpu::{self, EncoderCandidate, GpuInfo, VENDOR_MICROSOFT};

pub(crate) fn luid_u64(l: LUID) -> u64 {
    (l.LowPart as u64) | ((l.HighPart as u32 as u64) << 32)
}

pub(crate) fn luid_from_u64(v: u64) -> LUID {
    LUID { LowPart: v as u32, HighPart: (v >> 32) as u32 as i32 }
}

fn hardware_adapters() -> Vec<IDXGIAdapter1> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return out };
        let mut i = 0;
        while let Ok(a) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(d) = a.GetDesc1() else { continue };
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || d.VendorId == VENDOR_MICROSOFT {
                continue;
            }
            out.push(a);
        }
    }
    out
}

/// LUID of the adapter that scans out the primary monitor.
pub(crate) fn primary_luid() -> Option<u64> {
    let hmon = enumerate::monitors_with_handles().ok().and_then(|m| {
        let primary = m.iter().position(|(_, i)| i.primary).unwrap_or(0);
        m.into_iter().nth(primary).map(|(h, _)| h)
    })?;
    let a = enumerate::adapter_for_monitor(hmon)?;
    unsafe { a.GetDesc() }.ok().map(|d| luid_u64(d.AdapterLuid))
}

pub fn list_gpus() -> Vec<GpuInfo> {
    let primary = primary_luid();
    let mut out: Vec<GpuInfo> = hardware_adapters()
        .into_iter()
        .filter_map(|a| {
            let d = unsafe { a.GetDesc1() }.ok()?;
            let name = wide_to_string(&d.Description);
            let dedicated_mb = d.DedicatedVideoMemory as u64 / (1024 * 1024);
            // The user-mode driver version (the "CheckInterfaceSupport(IDXGIDevice)" idiom).
            let driver = unsafe { a.CheckInterfaceSupport(&IDXGIDevice::IID) }
                .map(|v| gpu::driver_version_string(v as u64))
                .unwrap_or_default();
            let luid = luid_u64(d.AdapterLuid);
            Some(GpuInfo {
                integrated: gpu::is_integrated(d.VendorId, &name, dedicated_mb),
                name,
                vendor_id: d.VendorId,
                device_id: d.DeviceId,
                subsys_id: d.SubSysId,
                revision: d.Revision,
                luid,
                dedicated_mb,
                driver,
                primary: primary == Some(luid),
                instance: 0,
            })
        })
        .collect();
    gpu::dedup(&mut out);
    gpu::number_instances(&mut out);
    out
}

/// The adapter with this LUID, if it still exists.
pub(crate) fn adapter_by_luid(luid: u64) -> Option<IDXGIAdapter> {
    hardware_adapters().into_iter().find_map(|a| {
        let d = unsafe { a.GetDesc1() }.ok()?;
        (luid_u64(d.AdapterLuid) == luid).then(|| a.cast().ok()).flatten()
    })
}

pub fn encoder_candidates(gpus: &[GpuInfo]) -> Vec<EncoderCandidate> {
    let _com = super::ComInit::mta();
    let nvenc = super::encode::nvenc_available();
    let listed: Vec<(u64, bool)> = gpus.iter().map(|g| (g.luid, super::encode::mft_listed(g.luid))).collect();
    let mut cands = gpu::build_candidates(gpus, nvenc, &|g| listed.iter().any(|&(l, ok)| l == g.luid && ok));
    // Drivers that register their MFT without an adapter LUID: the per-adapter enumeration finds
    // nothing, the global one does. v0.6 used that MFT on the primary adapter; keep doing so.
    if !listed.iter().any(|&(_, ok)| ok) && super::encode::mft_listed_any() {
        if let Some(g) = gpus.iter().find(|g| g.primary).or(gpus.first()) {
            let c = EncoderCandidate::hardware(crate::EncoderKind::MediaFoundation, g);
            // Before the processor, which stays last.
            let n = cands.len() - 1;
            cands.insert(n, c);
            cands[..=n].sort_by_key(|c| c.tier());
        }
    }
    cands
}
