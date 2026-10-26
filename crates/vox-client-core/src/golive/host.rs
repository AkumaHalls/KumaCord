//! A video surface inside the app's own window: a child HWND that the native viewer thread
//! presents into with its D3D11 swapchain (no CPU colour conversion, no texture upload into the
//! UI toolkit).
//!
//! - Created on the UI thread (the parent window's thread), so the two never share input queues
//!   across threads. It answers `WM_NCHITTEST` with `HTTRANSPARENT`: every click, double-click
//!   and hover over the video reaches the app's own window, and it never takes focus.
//! - [`VideoHost::place`] moves/sizes it (physical pixels, relative to the parent's client
//!   area) and hides it; only real changes call `SetWindowPos`. After a change the viewer thread
//!   is asked to redraw, which resizes the swapchain.
//! - Native windows always draw over the parent's own drawing (airspace): while one of the app's
//!   menus or dialogs overlaps the host, [`VideoHost::place_clipped`] cuts that part out of it
//!   (a window region), so the popup shows through the hole and the video plays around it. The
//!   app hides the host only when it is covered entirely.
//! - Dropping it detaches it from the viewer first (synchronously), then destroys the window.
//!
//! One host per video tile: item 9's grid simply makes several.

use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use std::sync::Once;

use anyhow::{anyhow, Result};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject, GetWindowRgn, GetWindowRgnBox, PtInRegion, SetWindowRgn, ValidateRect, RGN_DIFF};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::viewer::Viewer;
use super::viewer_native::{CTRL, WM_VOX_REDRAW};

const CLASS_NAME: &[u16] = &[
    b'V' as u16, b'o' as u16, b'x' as u16, b'H' as u16, b'u' as u16, b'b' as u16, b'V' as u16, b'i' as u16, b'd' as u16, b'e' as u16, b'o' as u16, b'H' as u16,
    b'o' as u16, b's' as u16, b't' as u16, 0,
];

/// A rectangle in physical pixels relative to the parent's client area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub struct VideoHost {
    hwnd: HWND,
    placed: Option<PxRect>,
    /// The rects cut out of the window (relative to its top-left corner) and the window size
    /// the region was made for; empty: no region.
    clip: Vec<PxRect>,
    clip_size: (i32, i32),
    viewer: Viewer,
}

fn post_redraw(hwnd: HWND) {
    let ctrl = CTRL.load(Ordering::Acquire);
    if ctrl != 0 {
        // SAFETY: posting to the viewer thread's control window, which never goes away.
        unsafe { PostMessageW(ctrl as HWND, WM_VOX_REDRAW, 0, hwnd as LPARAM) };
    }
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Mouse input goes to the app window underneath (same thread), as if we were not here.
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            // The viewer thread owns the picture: ask it to present again.
            // SAFETY: validating our own window's update region.
            unsafe { ValidateRect(hwnd, null()) };
            post_redraw(hwnd);
            0
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

impl VideoHost {
    /// Make a hidden child window of `parent` (an HWND). Call on the parent's own thread.
    pub fn new(parent: isize) -> Result<VideoHost> {
        let viewer = Viewer::get()?;
        if !viewer.is_native() {
            return Err(anyhow!("inline video needs the native viewer (Viewer::prefer_native_window)"));
        }
        static REGISTER: Once = Once::new();
        // SAFETY: class registration and window creation with valid, NUL-terminated strings.
        let hwnd = unsafe {
            let hinst = GetModuleHandleW(null());
            REGISTER.call_once(|| {
                let class = WNDCLASSW {
                    style: 0,
                    lpfnWndProc: Some(host_proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: hinst,
                    hIcon: null_mut(),
                    hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                    hbrBackground: null_mut(),
                    lpszMenuName: null(),
                    lpszClassName: CLASS_NAME.as_ptr(),
                };
                RegisterClassW(&class);
            });
            CreateWindowExW(
                WS_EX_NOPARENTNOTIFY | WS_EX_NOACTIVATE,
                CLASS_NAME.as_ptr(),
                null(),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                1,
                1,
                parent as HWND,
                null_mut(),
                hinst,
                null(),
            )
        };
        if hwnd.is_null() {
            return Err(anyhow!("cannot create the video child window"));
        }
        tracing::info!(hwnd = hwnd as isize, parent, "video host created");
        Ok(VideoHost { hwnd, placed: None, clip: Vec::new(), clip_size: (0, 0), viewer })
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd as isize
    }

    /// Where the video is now (None: hidden).
    pub fn placed(&self) -> Option<PxRect> {
        self.placed
    }

    /// Show the video at `rect`, or hide it (`None`, or an empty rect). Returns true when
    /// something changed.
    pub fn place(&mut self, rect: Option<PxRect>) -> bool {
        self.place_clipped(rect, &[])
    }

    /// [`Self::place`] with `cuts` (relative to the window's top-left corner) cut out of the
    /// window, where the app's own popups are drawn: the region is set before the window is
    /// shown, and only when the cuts or the size changed. No cuts: no region. Hiding leaves the
    /// region alone (it is redone when the window comes back). Returns true when the placement
    /// changed (a new region alone does not count).
    pub fn place_clipped(&mut self, rect: Option<PxRect>, cuts: &[PxRect]) -> bool {
        let rect = rect.filter(|r| r.w > 0 && r.h > 0);
        if let Some(r) = rect {
            self.set_clip(r.w, r.h, cuts);
        }
        if rect == self.placed {
            return false;
        }
        // SAFETY: our own child window, on its owning thread.
        unsafe {
            match rect {
                Some(r) => {
                    SetWindowPos(self.hwnd, HWND_TOP, r.x, r.y, r.w, r.h, SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOOWNERZORDER);
                }
                None => {
                    SetWindowPos(self.hwnd, null_mut(), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_HIDEWINDOW | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER);
                }
            }
        }
        self.placed = rect;
        if rect.is_some() {
            post_redraw(self.hwnd);
        }
        true
    }

    /// The window region: the `w` x `h` window minus `cuts` (nothing when there are none).
    fn set_clip(&mut self, w: i32, h: i32, cuts: &[PxRect]) {
        if cuts == self.clip.as_slice() && (cuts.is_empty() || self.clip_size == (w, h)) {
            return;
        }
        let redraw = self.placed.is_some() as i32;
        // SAFETY: GDI regions we create and either hand to the window (which then owns it) or
        // delete; our own child window, on its owning thread.
        unsafe {
            if cuts.is_empty() {
                SetWindowRgn(self.hwnd, null_mut(), redraw);
            } else {
                let rgn = CreateRectRgn(0, 0, w, h);
                if rgn.is_null() {
                    return;
                }
                for c in cuts {
                    let cut = CreateRectRgn(c.x, c.y, c.x + c.w, c.y + c.h);
                    if !cut.is_null() {
                        CombineRgn(rgn, rgn, cut, RGN_DIFF);
                        DeleteObject(cut);
                    }
                }
                if SetWindowRgn(self.hwnd, rgn, redraw) == 0 {
                    DeleteObject(rgn);
                    return;
                }
            }
        }
        self.clip = cuts.to_vec();
        self.clip_size = (w, h);
    }

    /// What the system says about the window (for tests): shown, and the bounding box of its
    /// region relative to the window (`None`: no region, the whole window shows).
    pub fn window_state(&self) -> (bool, Option<PxRect>) {
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: read-only queries on our own window with a valid out-pointer.
        let (shown, kind) = unsafe { (IsWindowVisible(self.hwnd) != 0, GetWindowRgnBox(self.hwnd, &mut r)) };
        // ERROR (0): the window has no region.
        let region = (kind != 0).then_some(PxRect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top });
        (shown, region)
    }

    /// Whether the window's region holds the point (`x`, `y`), relative to the window (for
    /// tests). `None`: the window has no region.
    pub fn region_contains(&self, x: i32, y: i32) -> Option<bool> {
        // SAFETY: a scratch region we delete again; read-only query on our own window.
        unsafe {
            let rgn = CreateRectRgn(0, 0, 0, 0);
            if rgn.is_null() {
                return None;
            }
            let hit = (GetWindowRgn(self.hwnd, rgn) != 0).then(|| PtInRegion(rgn, x, y) != 0);
            DeleteObject(rgn);
            hit
        }
    }
}

impl Drop for VideoHost {
    fn drop(&mut self) {
        // The viewer thread lets go of the window (swapchain released) before it is destroyed.
        self.viewer.drop_host(self.hwnd as isize);
        // SAFETY: our own child window, destroyed on its owning thread.
        unsafe { DestroyWindow(self.hwnd) };
    }
}
