//! Monitor / window enumeration and adapter lookup.

use std::collections::HashMap;

use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW, ENUM_CURRENT_SETTINGS,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};
use windows_core::BOOL;

use super::{os, wide_to_string, OwnedHandle};
use crate::{CaptureError, MonitorInfo, Result, WindowInfo};

const MONITORINFOF_PRIMARY: u32 = 1;

unsafe extern "system" fn collect_monitor(
    hmon: HMONITOR,
    _hdc: HDC,
    _rc: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let v = unsafe { &mut *(lparam.0 as *mut Vec<HMONITOR>) };
    v.push(hmon);
    BOOL(1)
}

/// All monitors in enumeration order, paired with their HMONITOR (index == position).
pub(crate) fn monitors_with_handles() -> Result<Vec<(HMONITOR, MonitorInfo)>> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut handles as *mut _ as isize),
        )
    };
    if !ok.as_bool() {
        return Err(CaptureError::Os("EnumDisplayMonitors failed".into()));
    }
    let friendly = friendly_monitor_names();
    let mut out = Vec::with_capacity(handles.len());
    for (index, hmon) in handles.into_iter().enumerate() {
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !unsafe { GetMonitorInfoW(hmon, &mut mi as *mut _ as *mut MONITORINFO) }.as_bool() {
            continue;
        }
        let device = wide_to_string(&mi.szDevice);
        let rc = mi.monitorInfo.rcMonitor;
        // Real mode size (not DPI-virtualized like rcMonitor can be for unaware processes).
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let (width, height) = if unsafe {
            EnumDisplaySettingsW(PCWSTR(mi.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm)
        }
        .as_bool()
            && dm.dmPelsWidth > 0
        {
            (dm.dmPelsWidth, dm.dmPelsHeight)
        } else {
            ((rc.right - rc.left) as u32, (rc.bottom - rc.top) as u32)
        };
        let short = device.trim_start_matches(r"\\.\").to_string();
        let name = match friendly.get(&device) {
            Some(f) if !f.is_empty() => format!("{f} ({short})"),
            _ => short,
        };
        out.push((
            hmon,
            MonitorInfo {
                index,
                name,
                width,
                height,
                primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            },
        ));
    }
    Ok(out)
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    Ok(monitors_with_handles()?
        .into_iter()
        .map(|(_, m)| m)
        .collect())
}

/// GDI device name (`\\.\DISPLAY1`) -> monitor friendly name (`DELL U2720Q`). Best effort.
fn friendly_monitor_names() -> HashMap<String, String> {
    let mut map = HashMap::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
            return map;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return map;
        }
        for p in paths.iter().take(np as usize) {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut tgt.header) != 0 {
                continue;
            }
            map.insert(
                wide_to_string(&src.viewGdiDeviceName),
                wide_to_string(&tgt.monitorFriendlyDeviceName),
            );
        }
    }
    map
}

/// The DXGI adapter that scans out `hmon` (matters with several adapters, e.g. a dGPU plus a
/// virtual display adapter). `None` -> let D3D pick the default hardware adapter.
pub(crate) fn adapter_for_monitor(hmon: HMONITOR) -> Option<IDXGIAdapter> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                if let Ok(desc) = output.GetDesc() {
                    if desc.Monitor == hmon {
                        return adapter.cast().ok();
                    }
                }
                j += 1;
            }
            i += 1;
        }
    }
    None
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let v = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    v.push(hwnd);
    BOOL(1)
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(collect_window), LPARAM(&mut hwnds as *mut _ as isize)) }
        .map_err(|e| os("EnumWindows", e))?;
    let own_pid = unsafe { GetCurrentProcessId() };
    let mut exe_cache: HashMap<u32, String> = HashMap::new();
    let mut out = Vec::new();
    for hwnd in hwnds {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                continue;
            }
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 {
                continue;
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4)
                .is_ok()
                && cloaked != 0
            {
                continue;
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                continue;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == own_pid {
                continue;
            }
            let mut class = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut class);
            if n > 0 && String::from_utf16_lossy(&class[..n as usize]) == "Progman" {
                continue; // the desktop itself; share the monitor instead
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            if title.trim().is_empty() {
                continue;
            }
            let exe_name = exe_cache
                .entry(pid)
                .or_insert_with(|| exe_name_of(pid))
                .clone();
            out.push(WindowInfo {
                hwnd: hwnd.0 as isize,
                title,
                process_id: pid,
                exe_name,
            });
        }
    }
    Ok(out)
}

/// Full path of a process's exe, or `None` if it can't be queried (e.g. elevated).
pub(crate) fn exe_path_of(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let h = OwnedHandle(h);
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size).ok()?;
        Some(String::from_utf16_lossy(&buf[..size as usize]))
    }
}

/// `chrome.exe` for a pid, or empty if the process can't be queried (e.g. elevated).
pub(crate) fn exe_name_of(pid: u32) -> String {
    let Some(path) = exe_path_of(pid) else {
        return String::new();
    };
    path.rsplit(['\\', '/']).next().unwrap_or_default().to_string()
}
