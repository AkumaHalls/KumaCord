//! Application icons for the Go Live picker (item 8c): what a window shows when it has no
//! picture to give (minimized, or a capture that failed). The icon is read from the window
//! (`WM_GETICON`, then the window class), then from the program's exe, and drawn into RGBA with
//! its alpha recovered by drawing it twice (over black and over white). Memory only.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetObjectW, SelectObject, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, DrawIconEx, GetClassLongPtrW, GetIconInfo, GetWindowThreadProcessId, IsIconic, SendMessageTimeoutW, DI_NORMAL, GCLP_HICON,
    GCLP_HICONSM, HICON, ICONINFO, ICON_BIG, ICON_SMALL2, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_GETICON,
};

use super::enumerate::exe_path_of;
use crate::{CaptureError, Result, RgbaImage};

pub fn is_window_minimized(hwnd: isize) -> bool {
    unsafe { IsIconic(HWND(hwnd as *mut _)) }.as_bool()
}

/// An icon that must be freed with `DestroyIcon` (made by `SHGetFileInfoW`), or one owned by the
/// window / class (never freed by us).
struct Icon {
    h: HICON,
    owned: bool,
}

impl Drop for Icon {
    fn drop(&mut self) {
        if self.owned {
            let _ = unsafe { DestroyIcon(self.h) };
        }
    }
}

/// The icon of the window `hwnd`, drawn at its natural size but at most `max_size` pixels wide
/// and tall.
pub fn window_icon(hwnd: isize, max_size: u32) -> Result<RgbaImage> {
    let h = HWND(hwnd as *mut _);
    let icon = find_icon(h).ok_or_else(|| CaptureError::NotFound("no icon for this window".into()))?;
    render_icon(icon.h, max_size.clamp(16, 256))
}

fn find_icon(hwnd: HWND) -> Option<Icon> {
    // Asking a hung window for its icon would hang us (WM_GETICON is a message to that window's
    // thread), so the message is sent with an abort-if-hung timeout.
    let ask = |kind: usize| -> Option<Icon> {
        let mut res = 0usize;
        let ok = unsafe {
            SendMessageTimeoutW(hwnd, WM_GETICON, WPARAM(kind), LPARAM(0), SMTO_ABORTIFHUNG | SMTO_BLOCK, 150, Some(&mut res))
        };
        (ok.0 != 0 && res != 0).then(|| Icon { h: HICON(res as *mut _), owned: false })
    };
    if let Some(i) = ask(ICON_BIG as usize) {
        return Some(i);
    }
    let class = |idx| {
        let v = unsafe { GetClassLongPtrW(hwnd, idx) };
        (v != 0).then(|| Icon { h: HICON(v as *mut _), owned: false })
    };
    if let Some(i) = class(GCLP_HICON) {
        return Some(i);
    }
    if let Some(i) = ask(ICON_SMALL2 as usize) {
        return Some(i);
    }
    if let Some(i) = class(GCLP_HICONSM) {
        return Some(i);
    }
    // The program's own icon.
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 || pid == unsafe { GetCurrentProcessId() } {
        return None;
    }
    let path = exe_path_of(pid)?;
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut info = SHFILEINFOW::default();
    let r = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    (r != 0 && !info.hIcon.is_invalid()).then_some(Icon { h: info.hIcon, owned: true })
}

/// Natural size of an icon in pixels (32x32 when it cannot be read).
fn icon_size(h: HICON) -> (u32, u32) {
    let mut info = ICONINFO::default();
    if unsafe { GetIconInfo(h, &mut info) }.is_err() {
        return (32, 32);
    }
    let mut size = (32, 32);
    let probe = if info.hbmColor.is_invalid() { info.hbmMask } else { info.hbmColor };
    let mut bm = BITMAP::default();
    if unsafe { GetObjectW(HGDIOBJ(probe.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) } != 0 && bm.bmWidth > 0 {
        // A monochrome icon keeps its AND and XOR masks stacked in one bitmap.
        let h = if info.hbmColor.is_invalid() { bm.bmHeight / 2 } else { bm.bmHeight };
        size = (bm.bmWidth as u32, h.max(1) as u32);
    }
    unsafe {
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        }
    }
    size
}

fn render_icon(icon: HICON, max_size: u32) -> Result<RgbaImage> {
    let (nw, nh) = icon_size(icon);
    let s = (max_size as f32 / nw.max(nh) as f32).min(1.0);
    let (w, h) = (((nw as f32 * s) as u32).max(1), ((nh as f32 * s) as u32).max(1));
    let over_black = draw(icon, w, h, 0x00_00_00_00)?;
    let over_white = draw(icon, w, h, 0x00_FF_FF_FF)?;
    Ok(recover_alpha(&over_black, &over_white, w, h))
}

/// Draw `icon` on a `w`x`h` 32-bit surface filled with `bg` (0x00RRGGBB); returns BGRA bytes.
fn draw(icon: HICON, w: u32, h: u32, bg: u32) -> Result<Vec<u8>> {
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return Err(CaptureError::Os("CreateCompatibleDC failed".into()));
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = match CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(d) if !bits.is_null() => d,
            _ => {
                let _ = DeleteDC(dc);
                return Err(CaptureError::Os("CreateDIBSection failed".into()));
            }
        };
        let old = SelectObject(dc, HGDIOBJ(dib.0));
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        px.fill(bg);
        let drawn = DrawIconEx(dc, 0, 0, icon, w as i32, h as i32, 0, None, DI_NORMAL);
        let _ = GdiFlush();
        let out = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(dc);
        drawn.map_err(|e| os_err("DrawIconEx", e))?;
        Ok(out)
    }
}

fn os_err(ctx: &str, e: windows_core::Error) -> CaptureError {
    super::os(ctx, e)
}

/// The same icon drawn over black and over white gives the alpha: white-over minus black-over is
/// `255 - alpha` for every channel; the colour is the black-over value divided by alpha.
pub(crate) fn recover_alpha(over_black: &[u8], over_white: &[u8], w: u32, h: u32) -> RgbaImage {
    let n = (w * h) as usize;
    let mut data = vec![0u8; n * 4];
    for i in 0..n {
        let (b, w_) = (&over_black[i * 4..i * 4 + 4], &over_white[i * 4..i * 4 + 4]);
        // Use the channel with the least spread so one odd channel cannot make the pixel opaque.
        let diff = (0..3).map(|c| w_[c].saturating_sub(b[c])).min().unwrap_or(0) as u32;
        let a = 255 - diff;
        let o = i * 4;
        if a == 0 {
            continue;
        }
        data[o] = (b[2] as u32 * 255 / a).min(255) as u8;
        data[o + 1] = (b[1] as u32 * 255 / a).min(255) as u8;
        data[o + 2] = (b[0] as u32 * 255 / a).min(255) as u8;
        data[o + 3] = a as u8;
    }
    RgbaImage { width: w, height: h, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_is_recovered_from_two_backgrounds() {
        // Pixel 0: opaque red. Pixel 1: fully transparent. Pixel 2: half-transparent white
        // (premultiplied: over black = 128, over white = 255).
        let black = [0, 0, 255, 0, /**/ 0, 0, 0, 0, /**/ 128, 128, 128, 0];
        let white = [0, 0, 255, 0, /**/ 255, 255, 255, 0, /**/ 255, 255, 255, 0];
        let img = recover_alpha(&black, &white, 3, 1);
        assert_eq!(&img.data[0..4], &[255, 0, 0, 255]);
        assert_eq!(img.data[7], 0, "transparent");
        let a = img.data[11];
        assert!((126..=129).contains(&a), "alpha {a}");
        assert!(img.data[8] >= 250, "colour restored to white: {}", img.data[8]);
    }
}
