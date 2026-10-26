//! Go Live viewer on plain Win32 windows (no winit), for hosts that already own a winit event
//! loop (winit allows one per process, so the winit viewer cannot start next to an egui app).
//!
//! One thread with a blocking message loop (zero CPU while no frame arrives) drives any number
//! of video streams. Each stream has:
//! - a single-slot frame mailbox (a slow window never queues frames up);
//! - its own plane textures on the thread's one shared D3D11 device ([`D3dGpu`]);
//! - one display window at a time: the app's child window for that stream ("inline", a
//!   [`super::host::VideoHost`] owned by the UI thread) or its own top-level pop-out window
//!   (owned by this thread). Moving between them only swaps the swapchain; the picture stays.
//!
//! Frames are uploaded and presented only on a new frame or a resize, never while the display
//! window is hidden or minimized (the newest frame is kept for when it comes back). A stream can
//! be capped to a lower drawing rate ([`NativeViewer::set_max_fps`], small tiles): frames in
//! between are replaced undrawn and a one-shot timer shows the newest one.
//! A softbuffer (GDI) fallback is used when D3D11 is unavailable or keeps failing.
//!
//! Other threads talk to the viewer through a hidden message-only window: `WM_VOX_CMD` (boxed
//! [`Cmd`]), `WM_VOX_FRAME` (stream id), `WM_VOX_REDRAW` (a host window that moved or resized).

use std::cell::RefCell;
use std::collections::HashMap;
use std::num::{NonZeroIsize, NonZeroU32};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use livekit::webrtc::video_frame::{BoxVideoFrame, NV12Buffer, VideoBuffer};
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, InvalidateRect, MonitorFromWindow, ValidateRect, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use winit::dpi::PhysicalSize;
use winit::raw_window_handle::{DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};

use super::d3d::{D3dGpu, D3dTarget, DrawStyle, RenderError, VideoPlanes};
use super::viewer::{draw_soft, unsupported_buffer, ViewerNotice};

const WM_VOX_CMD: u32 = WM_APP + 1;
const WM_VOX_FRAME: u32 = WM_APP + 3;
pub(super) const WM_VOX_REDRAW: u32 = WM_APP + 4;
const VK_ESCAPE: WPARAM = 0x1B;
const VK_F11: WPARAM = 0x7A;

const CLASS_NAME: &[u16] = &[b'V' as u16, b'o' as u16, b'x' as u16, b'H' as u16, b'u' as u16, b'b' as u16, b'V' as u16, b'i' as u16, b'e' as u16, b'w' as u16, 0];

/// The viewer thread's control window, for [`super::host`] (0 until the viewer starts).
pub(super) static CTRL: AtomicIsize = AtomicIsize::new(0);

/// A picture on its way to the screen: a decoded frame from the network, or a local preview
/// (our own webcam / screen) whose buffer is recycled so a steady preview allocates nothing.
pub(super) enum Picture {
    Decoded(BoxVideoFrame),
    Nv12(NV12Buffer),
}

impl Picture {
    fn buffer(&self) -> &dyn VideoBuffer {
        match self {
            Picture::Decoded(f) => f.buffer.as_ref(),
            Picture::Nv12(b) => b,
        }
    }
}

struct Mailbox {
    frame: Mutex<Option<Picture>>,
    pending: AtomicBool,
    /// Decoded frames replaced before they were drawn (the renderer fell behind the decoder).
    replaced: AtomicU64,
    /// A drawn (or replaced) preview buffer waiting for the next preview frame.
    spare: Mutex<Option<NV12Buffer>>,
}

impl Mailbox {
    fn new() -> Self {
        Self { frame: Mutex::new(None), pending: AtomicBool::new(false), replaced: AtomicU64::new(0), spare: Mutex::new(None) }
    }

    /// Keep a finished preview buffer for reuse (anything else is simply dropped).
    fn recycle(&self, p: Picture) {
        if let Picture::Nv12(b) = p {
            *self.spare.lock() = Some(b);
        }
    }
}

/// What other threads may read about a stream (see [`super::viewer::StreamState`]).
#[derive(Default)]
pub(super) struct Shared {
    pub popped_out: AtomicBool,
    pub popout_hwnd: AtomicIsize,
    pub video_w: AtomicU32,
    pub video_h: AtomicU32,
    pub presented: AtomicU64,
    /// The pop-out window covers its monitor (double-click / F11; Esc leaves).
    pub fullscreen: AtomicBool,
    /// Draw at most this many frames per second (0: every frame). Small tiles (the strip under
    /// a focused stream) are drawn less often; the newest frame is always shown in the end.
    pub max_fps: AtomicU32,
}

struct Entry {
    id: u64,
    mailbox: Arc<Mailbox>,
    shared: Arc<Shared>,
}

enum Cmd {
    Open { id: u64, key: String, title: String, inline: bool, style: DrawStyle, notify: UnboundedSender<ViewerNotice>, mailbox: Arc<Mailbox>, shared: Arc<Shared> },
    Close { key: String },
    Retitle { key: String, title: String },
    /// Show `key` in this app child window when it is not popped out (None: forget the host).
    Attach { key: String, host: Option<isize> },
    /// Forget every use of this host window (it is about to be destroyed).
    DropHost { host: isize },
    PopOut { key: String, out: bool },
}

/// Handle to the viewer thread (the hidden control window lives on it).
#[derive(Clone)]
pub(super) struct NativeViewer {
    ctrl: isize,
    streams: Arc<Mutex<HashMap<String, Entry>>>,
}

/// Where one stream's frames go (see [`super::viewer::ViewerStream`]).
#[derive(Clone)]
pub(super) struct NativeStream {
    ctrl: isize,
    id: u64,
    mailbox: Arc<Mailbox>,
}

impl NativeStream {
    pub(super) fn submit(&self, frame: BoxVideoFrame) {
        self.put(Picture::Decoded(frame));
    }

    /// A local NV12 preview frame, copied into a recycled buffer (no allocation once the size is
    /// steady). Frames the viewer has not shown yet are replaced, never queued.
    pub(super) fn submit_nv12(&self, f: &vox_capture::Nv12Frame<'_>) {
        let Some(buf) = self.fill_nv12(f) else { return };
        self.put(Picture::Nv12(buf));
    }

    fn fill_nv12(&self, f: &vox_capture::Nv12Frame<'_>) -> Option<NV12Buffer> {
        let mut buf = match self.mailbox.spare.lock().take() {
            Some(b) if b.width() == f.width && b.height() == f.height => b,
            _ => NV12Buffer::new(f.width, f.height),
        };
        super::camera::fill_nv12(&mut buf, f).then_some(buf)
    }

    fn put(&self, p: Picture) {
        if let Some(old) = self.mailbox.frame.lock().replace(p) {
            self.mailbox.replaced.fetch_add(1, Ordering::Relaxed);
            self.mailbox.recycle(old);
        }
        if !self.mailbox.pending.swap(true, Ordering::AcqRel) {
            // SAFETY: PostMessageW to a window owned by the (never-exiting) viewer thread.
            unsafe { PostMessageW(self.ctrl as HWND, WM_VOX_FRAME, 0, self.id as LPARAM) };
        }
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl NativeViewer {
    pub(super) fn spawn() -> Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<isize, String>>();
        std::thread::Builder::new()
            .name("vox-viewer".into())
            .spawn(move || run_thread(tx))
            .map_err(|e| e.to_string())?;
        let ctrl = rx.recv().map_err(|e| e.to_string())??;
        CTRL.store(ctrl, Ordering::Release);
        Ok(Self { ctrl, streams: Arc::new(Mutex::new(HashMap::new())) })
    }

    fn post(&self, cmd: Cmd) {
        let cmd = Box::into_raw(Box::new(cmd));
        // SAFETY: PostMessageW to a window owned by the (never-exiting) viewer thread.
        if unsafe { PostMessageW(self.ctrl as HWND, WM_VOX_CMD, 0, cmd as LPARAM) } == 0 {
            // SAFETY: the message was not queued, so we still own the allocation.
            drop(unsafe { Box::from_raw(cmd) });
        }
    }

    /// Like `post`, but returns once the viewer thread has handled it (bounded wait).
    fn send(&self, cmd: Cmd) {
        let cmd = Box::into_raw(Box::new(cmd));
        let mut result = 0usize;
        // SAFETY: the viewer thread takes ownership of the box when it handles the message.
        // If the call fails before delivery (the thread is gone) the box leaks, which is fine
        // for a message sent at most once per window.
        unsafe { SendMessageTimeoutW(self.ctrl as HWND, WM_VOX_CMD, 0, cmd as LPARAM, SMTO_NORMAL, 2000, &mut result) };
    }

    pub(super) fn open(&self, key: &str, title: &str, inline: bool, style: DrawStyle, notify: UnboundedSender<ViewerNotice>) -> NativeStream {
        let mut streams = self.streams.lock();
        if let Some(e) = streams.get(key) {
            let s = NativeStream { ctrl: self.ctrl, id: e.id, mailbox: e.mailbox.clone() };
            drop(streams);
            self.post(Cmd::Retitle { key: key.to_string(), title: title.to_string() });
            return s;
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mailbox = Arc::new(Mailbox::new());
        let shared = Arc::new(Shared::default());
        shared.popped_out.store(!inline, Ordering::Release);
        streams.insert(key.to_string(), Entry { id, mailbox: mailbox.clone(), shared: shared.clone() });
        drop(streams);
        self.post(Cmd::Open { id, key: key.to_string(), title: title.to_string(), inline, style, notify, mailbox: mailbox.clone(), shared });
        NativeStream { ctrl: self.ctrl, id, mailbox }
    }

    pub(super) fn close(&self, key: &str) {
        if let Some(e) = self.streams.lock().remove(key) {
            e.mailbox.frame.lock().take();
        }
        self.post(Cmd::Close { key: key.to_string() });
    }

    pub(super) fn retitle(&self, key: &str, title: &str) {
        self.post(Cmd::Retitle { key: key.to_string(), title: title.to_string() });
    }

    pub(super) fn attach(&self, key: &str, host: Option<isize>) {
        let cmd = Cmd::Attach { key: key.to_string(), host };
        if host.is_none() {
            // The caller may destroy the window next: let go of it first.
            self.send(cmd);
        } else {
            self.post(cmd);
        }
    }

    pub(super) fn drop_host(&self, host: isize) {
        self.send(Cmd::DropHost { host });
    }

    pub(super) fn pop_out(&self, key: &str, out: bool) {
        if let Some(e) = self.streams.lock().get(key) {
            e.shared.popped_out.store(out, Ordering::Release);
        }
        self.post(Cmd::PopOut { key: key.to_string(), out });
    }

    pub(super) fn set_max_fps(&self, key: &str, fps: u32) {
        if let Some(e) = self.streams.lock().get(key) {
            e.shared.max_fps.store(fps, Ordering::Relaxed);
        }
    }

    pub(super) fn shared(&self, key: &str) -> Option<Arc<Shared>> {
        self.streams.lock().get(key).map(|e| e.shared.clone())
    }

    pub(super) fn keys(&self) -> Vec<String> {
        self.streams.lock().keys().cloned().collect()
    }
}

// ---------------------------------------------------------------- window handle for softbuffer

#[derive(Clone, Copy)]
struct Win32Handle(isize);

impl HasWindowHandle for Win32Handle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let hwnd = NonZeroIsize::new(self.0).ok_or(HandleError::Unavailable)?;
        let raw = RawWindowHandle::Win32(Win32WindowHandle::new(hwnd));
        // SAFETY: the HWND stays valid while a surface uses it (surfaces are dropped before
        // their window is destroyed).
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for Win32Handle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}

// ---------------------------------------------------------------- state (viewer thread only)

enum Backend {
    D3d(D3dTarget),
    Soft { surface: softbuffer::Surface<Win32Handle, Win32Handle>, _context: softbuffer::Context<Win32Handle> },
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Backend::D3d(_) => "d3d11",
            Backend::Soft { .. } => "softbuffer",
        }
    }
}

/// The window a stream currently draws into, with its swapchain / GDI surface.
struct Surface {
    hwnd: HWND,
    backend: Backend,
}

#[derive(Default)]
struct ViewStats {
    presented: u64,
    uploads: u64,
    since: Option<Instant>,
}

struct Stream {
    id: u64,
    key: String,
    title: String,
    notify: Option<UnboundedSender<ViewerNotice>>,
    mailbox: Arc<Mailbox>,
    shared: Arc<Shared>,
    /// Show in the pop-out window (else in the app's host window, if any).
    popped: bool,
    /// Closing the pop-out brings the stream back into the app (it was popped out from there).
    return_on_close: bool,
    popout: Option<HWND>,
    /// The pop-out is fullscreen: its style and placement to go back to.
    fs_restore: Option<(isize, WINDOWPLACEMENT)>,
    sized_to_video: bool,
    surface: Option<Surface>,
    planes: Option<VideoPlanes>,
    /// Mirror / crop-to-fill (camera tiles); D3D11 only.
    style: DrawStyle,
    /// Newest frame not yet shown. The D3D path drops it once uploaded (the textures keep the
    /// picture for redraws); softbuffer keeps it so resizes can redraw.
    last: Option<Picture>,
    /// Scratch ARGB buffer for letterboxed softbuffer draws.
    scaled: Vec<u8>,
    stats: ViewStats,
    /// When the last new picture was uploaded (frame-rate cap, `Shared::max_fps`).
    last_upload: Option<Instant>,
    /// A timer will draw the held-back newest frame (frame-rate cap).
    timer_armed: bool,
}

impl Stream {
    /// With a frame-rate cap: how long the next new picture has to wait (None: draw now).
    fn throttle_wait(&self) -> Option<Duration> {
        let max = self.shared.max_fps.load(Ordering::Relaxed);
        if max == 0 {
            return None;
        }
        let interval = Duration::from_micros(1_000_000 / max as u64);
        let since = self.last_upload?.elapsed();
        (since < interval).then(|| interval - since)
    }
}

struct State {
    gpu: Option<D3dGpu>,
    streams: Vec<Stream>,
    /// App child windows, by stream key.
    hosts: HashMap<String, HWND>,
    force_soft: bool,
    d3d_failures: u32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Run `f` on the viewer state unless it is already borrowed (a message dispatched from inside
/// one of our own Win32 calls); such nested messages are simply skipped.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut s = s.try_borrow_mut().ok()?;
        s.as_mut().map(f)
    })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn run_thread(tx: std::sync::mpsc::Sender<Result<isize, String>>) {
    // SAFETY: standard Win32 window-class registration and message loop on this thread; every
    // pointer passed is valid for the duration of the call.
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = WNDCLASSW {
            // Double-clicks toggle a pop-out's fullscreen.
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: CLASS_NAME.as_ptr(),
        };
        if RegisterClassW(&class) == 0 {
            let _ = tx.send(Err("cannot register viewer window class".into()));
            return;
        }
        let ctrl = CreateWindowExW(0, CLASS_NAME.as_ptr(), null(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), hinst, null());
        if ctrl.is_null() {
            let _ = tx.send(Err("cannot create viewer control window".into()));
            return;
        }
        let force_soft = std::env::var("VOX_VIEWER").map(|v| v.eq_ignore_ascii_case("soft")).unwrap_or(false);
        STATE.with(|s| *s.borrow_mut() = Some(State { gpu: None, streams: Vec::new(), hosts: HashMap::new(), force_soft, d3d_failures: 0 }));
        let _ = tx.send(Ok(ctrl as isize));
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_VOX_CMD => {
            // SAFETY: the pointer came from `Box::into_raw` in `NativeViewer::post`/`send`.
            let cmd = unsafe { Box::from_raw(lparam as *mut Cmd) };
            if with_state(|s| s.command(*cmd)).is_none() {
                debug!("viewer command skipped (nested message)");
            }
            0
        }
        WM_VOX_FRAME => {
            with_state(|s| s.on_frame(lparam as u64));
            0
        }
        WM_VOX_REDRAW => {
            with_state(|s| s.redraw_window(lparam as HWND));
            0
        }
        WM_TIMER if hwnd as isize == CTRL.load(Ordering::Acquire) => {
            // One-shot: a frame-rate-capped stream's held-back frame (timer id = stream id).
            // SAFETY: killing our own timer on our own window.
            unsafe { KillTimer(hwnd, wparam) };
            with_state(|s| s.on_timer(wparam as u64));
            0
        }
        WM_PAINT => {
            with_state(|s| s.redraw_window(hwnd));
            // SAFETY: validating our own window's update region.
            unsafe { ValidateRect(hwnd, null()) };
            0
        }
        WM_SIZE => {
            // SAFETY: invalidating our own window.
            unsafe { InvalidateRect(hwnd, null(), 0) };
            0
        }
        WM_ERASEBKGND => 1,
        // Pop-out fullscreen: double-click or F11 toggle, Esc leaves.
        WM_LBUTTONDBLCLK => {
            with_state(|s| s.popout_fullscreen(hwnd, None));
            0
        }
        WM_KEYDOWN if wparam == VK_F11 || wparam == VK_ESCAPE => {
            with_state(|s| s.popout_fullscreen(hwnd, if wparam == VK_ESCAPE { Some(false) } else { None }));
            0
        }
        WM_CLOSE => {
            if with_state(|s| s.popout_closed(hwnd)) != Some(true) {
                // SAFETY: default handling for windows we do not track.
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            0
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `r` is a valid out-pointer; GetClientRect works on other threads' windows too.
    unsafe { GetClientRect(hwnd, &mut r) };
    ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
}

/// Outer window size for a client area of `w x h` physical pixels.
fn outer_size(w: i32, h: i32) -> (i32, i32) {
    // SAFETY: plain metric queries on a stack RECT.
    unsafe {
        let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
        AdjustWindowRectExForDpi(&mut r, WS_OVERLAPPEDWINDOW, 0, 0, GetDpiForSystem());
        (r.right - r.left, r.bottom - r.top)
    }
}

/// Whether anything drawn into `hwnd` can be seen: visible (with all its parents) and its
/// top-level window not minimized.
fn shown(hwnd: HWND) -> bool {
    // SAFETY: read-only window queries; they do not send messages.
    unsafe { IsWindowVisible(hwnd) != 0 && IsIconic(GetAncestor(hwnd, GA_ROOT)) == 0 }
}

fn create_popout(title: &str) -> Option<HWND> {
    // SAFETY: window creation on the thread that owns the message loop.
    let hwnd = unsafe {
        let dpi = GetDpiForSystem() as i32;
        let (w, h) = outer_size(1280 * dpi / 96, 720 * dpi / 96);
        // Background test mode: created hidden and never activating, then shown behind the rest.
        let bg = crate::testmode::background();
        CreateWindowExW(
            if bg { WS_EX_NOACTIVATE | WS_EX_APPWINDOW } else { 0 },
            CLASS_NAME.as_ptr(),
            wide(title).as_ptr(),
            WS_OVERLAPPEDWINDOW | if bg { 0 } else { WS_VISIBLE },
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            w,
            h,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    };
    if hwnd.is_null() {
        warn!("cannot create viewer window");
        return None;
    }
    if crate::testmode::background() {
        crate::testmode::show_behind(hwnd as isize);
    }
    Some(hwnd)
}

impl Stream {
    /// The window this stream should draw into right now.
    fn display(&self, hosts: &HashMap<String, HWND>) -> Option<HWND> {
        if self.popped {
            self.popout
        } else {
            hosts.get(&self.key).copied()
        }
    }

    fn destroy_popout(&mut self, gpu: Option<&D3dGpu>) {
        if let Some(p) = self.popout.take() {
            if self.surface.as_ref().is_some_and(|s| s.hwnd == p) {
                release_surface(gpu, self.surface.take());
            }
            // SAFETY: our own window, destroyed on its owning thread.
            unsafe { DestroyWindow(p) };
            self.fs_restore = None;
            self.shared.fullscreen.store(false, Ordering::Release);
            self.shared.popout_hwnd.store(0, Ordering::Release);
            info!(key = %self.key, "viewer window closed");
        }
    }

    /// Pop-out fullscreen on (`Some(true)`), off, or toggled (`None`): borderless over the
    /// monitor the window is on; leaving it restores the window exactly as it was.
    fn set_fullscreen(&mut self, want: Option<bool>) {
        let Some(p) = self.popout else { return };
        let on = want.unwrap_or(self.fs_restore.is_none());
        if on == self.fs_restore.is_some() {
            return;
        }
        // SAFETY: style / placement changes on our own live window, on its owning thread; the
        // messages they send back are handled without the state (it is borrowed here).
        unsafe {
            if on {
                let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
                wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
                let mut mi: MONITORINFO = std::mem::zeroed();
                mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                if GetWindowPlacement(p, &mut wp) == 0 || GetMonitorInfoW(MonitorFromWindow(p, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
                    warn!(key = %self.key, "pop-out fullscreen: cannot read the window / monitor");
                    return;
                }
                let style = GetWindowLongPtrW(p, GWL_STYLE);
                SetWindowLongPtrW(p, GWL_STYLE, (style & !(WS_OVERLAPPEDWINDOW as isize)) | WS_POPUP as isize);
                let r = mi.rcMonitor;
                // Background test mode: over the monitor, but behind everything and not activated.
                let (after, quiet) = if crate::testmode::background() { (HWND_BOTTOM, SWP_NOACTIVATE) } else { (HWND_TOP, 0) };
                SetWindowPos(p, after, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_FRAMECHANGED | SWP_NOOWNERZORDER | SWP_SHOWWINDOW | quiet);
                self.fs_restore = Some((style, wp));
            } else if let Some((style, mut wp)) = self.fs_restore.take() {
                SetWindowLongPtrW(p, GWL_STYLE, style);
                let quiet = if crate::testmode::background() {
                    wp.showCmd = SW_SHOWNOACTIVATE as u32;
                    SWP_NOACTIVATE
                } else {
                    0
                };
                SetWindowPlacement(p, &wp);
                SetWindowPos(p, null_mut(), 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | quiet);
            }
        }
        self.shared.fullscreen.store(on, Ordering::Release);
        info!(key = %self.key, on, "pop-out fullscreen");
    }

    fn log_stats(&mut self) {
        let now = Instant::now();
        let since = *self.stats.since.get_or_insert(now);
        if now.duration_since(since) < vox_log::STATS_INTERVAL {
            return;
        }
        let secs = now.duration_since(since).as_secs_f64().max(1e-3);
        let (ww, wh) = self.surface.as_ref().map(|s| client_size(s.hwnd)).unwrap_or((0, 0));
        let video = self.planes.as_ref().map(VideoPlanes::size).unwrap_or((0, 0));
        info!(
            key = %self.key,
            renderer = self.surface.as_ref().map(|s| s.backend.name()).unwrap_or("-"),
            place = if self.popped { "window" } else { "inline" },
            presented_fps = format!("{:.1}", self.stats.presented as f64 / secs),
            uploaded_fps = format!("{:.1}", self.stats.uploads as f64 / secs),
            replaced_undrawn = self.mailbox.replaced.swap(0, Ordering::Relaxed),
            video = format!("{}x{}", video.0, video.1),
            window = format!("{ww}x{wh}"),
            "viewer stats"
        );
        self.stats = ViewStats { since: Some(now), ..Default::default() };
    }
}

/// Drop a surface; a D3D swapchain is fully released (flush) so the window can get a new one.
fn release_surface(gpu: Option<&D3dGpu>, surface: Option<Surface>) {
    let was_d3d = matches!(surface, Some(Surface { backend: Backend::D3d(_), .. }));
    drop(surface);
    if was_d3d {
        if let Some(g) = gpu {
            g.flush_released();
        }
    }
}

impl State {
    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Open { id, key, title, inline, style, notify, mailbox, shared } => {
                if let Some(i) = self.streams.iter().position(|s| s.key == key) {
                    // Re-opened under the same key: replace (the handle side already did).
                    self.remove(i);
                }
                let mut s = Stream {
                    id,
                    key,
                    title,
                    notify: Some(notify),
                    mailbox,
                    shared,
                    popped: !inline,
                    return_on_close: false,
                    popout: None,
                    fs_restore: None,
                    sized_to_video: false,
                    surface: None,
                    planes: None,
                    style,
                    last: None,
                    scaled: Vec::new(),
                    stats: ViewStats::default(),
                    last_upload: None,
                    timer_armed: false,
                };
                if s.popped {
                    s.popout = create_popout(&s.title);
                    s.shared.popout_hwnd.store(s.popout.map(|h| h as isize).unwrap_or(0), Ordering::Release);
                }
                info!(key = %s.key, inline, "viewer stream opened");
                self.streams.push(s);
                let i = self.streams.len() - 1;
                self.retarget(i);
            }
            Cmd::Close { key } => {
                if let Some(i) = self.streams.iter().position(|s| s.key == key) {
                    self.remove(i);
                }
            }
            Cmd::Retitle { key, title } => {
                if let Some(s) = self.streams.iter_mut().find(|s| s.key == key) {
                    s.title = title;
                    if let Some(p) = s.popout {
                        // SAFETY: our own live window.
                        unsafe { SetWindowTextW(p, wide(&s.title).as_ptr()) };
                    }
                }
            }
            Cmd::Attach { key, host } => {
                match host {
                    Some(h) => {
                        self.hosts.insert(key.clone(), h as HWND);
                    }
                    None => {
                        self.hosts.remove(&key);
                    }
                }
                if let Some(i) = self.streams.iter().position(|s| s.key == key) {
                    self.retarget(i);
                }
            }
            Cmd::DropHost { host } => {
                let host = host as HWND;
                self.hosts.retain(|_, h| *h != host);
                for i in 0..self.streams.len() {
                    if self.streams[i].surface.as_ref().is_some_and(|s| s.hwnd == host) {
                        self.retarget(i);
                    }
                }
            }
            Cmd::PopOut { key, out } => {
                let Some(i) = self.streams.iter().position(|s| s.key == key) else { return };
                let s = &mut self.streams[i];
                if out {
                    s.popped = true;
                    s.return_on_close = true;
                    if s.popout.is_none() {
                        s.popout = create_popout(&s.title);
                        s.sized_to_video = false;
                        s.shared.popout_hwnd.store(s.popout.map(|h| h as isize).unwrap_or(0), Ordering::Release);
                    }
                    info!(key = %s.key, "viewer popped out");
                } else {
                    s.popped = false;
                    info!(key = %s.key, "viewer back inline");
                }
                s.shared.popped_out.store(s.popped, Ordering::Release);
                self.retarget(i);
                if !out {
                    let gpu = self.gpu.as_ref();
                    self.streams[i].destroy_popout(gpu);
                }
            }
        }
    }

    fn remove(&mut self, i: usize) {
        let mut s = self.streams.remove(i);
        release_surface(self.gpu.as_ref(), s.surface.take());
        s.destroy_popout(self.gpu.as_ref());
        info!(key = %s.key, "viewer stream closed");
        if self.streams.is_empty() {
            // Nothing to show: let the device go (it is cheap to make again).
            self.gpu = None;
        }
    }

    fn popout_fullscreen(&mut self, hwnd: HWND, want: Option<bool>) {
        if let Some(s) = self.streams.iter_mut().find(|s| s.popout == Some(hwnd)) {
            s.set_fullscreen(want);
        }
    }

    /// The user closed a pop-out window. Returns false for windows we do not track.
    fn popout_closed(&mut self, hwnd: HWND) -> bool {
        let Some(i) = self.streams.iter().position(|s| s.popout == Some(hwnd)) else { return false };
        let back_inline = self.streams[i].return_on_close && self.hosts.contains_key(&self.streams[i].key);
        if back_inline {
            let s = &mut self.streams[i];
            s.popped = false;
            s.shared.popped_out.store(false, Ordering::Release);
            info!(key = %s.key, "viewer window closed by the user; back inline");
            self.retarget(i);
            let gpu = self.gpu.as_ref();
            self.streams[i].destroy_popout(gpu);
        } else {
            let gpu = self.gpu.as_ref();
            let s = &mut self.streams[i];
            info!(key = %s.key, "viewer window closed by the user");
            s.destroy_popout(gpu);
            if let Some(n) = &s.notify {
                let _ = n.send(ViewerNotice::Closed { key: s.key.clone() });
            }
        }
        super::viewer::wake_ui();
        true
    }

    /// Point stream `i` at the window it should use now (making or dropping its surface).
    fn retarget(&mut self, i: usize) {
        let want = self.streams[i].display(&self.hosts);
        let have = self.streams[i].surface.as_ref().map(|s| s.hwnd);
        if want != have {
            release_surface(self.gpu.as_ref(), self.streams[i].surface.take());
            if let Some(hwnd) = want {
                let surface = self.make_surface(hwnd);
                let s = &mut self.streams[i];
                if let Some(sf) = &surface {
                    info!(key = %s.key, renderer = sf.backend.name(), place = if s.popped { "window" } else { "inline" }, "viewer surface ready");
                }
                s.surface = surface;
            }
        }
        self.draw(i);
    }

    fn make_surface(&mut self, hwnd: HWND) -> Option<Surface> {
        if !self.force_soft {
            if self.gpu.is_none() {
                match D3dGpu::new() {
                    Ok(g) => self.gpu = Some(g),
                    Err(e) => {
                        self.d3d_failures += 1;
                        warn!(error = %e, "D3D11 viewer device failed; falling back to softbuffer");
                    }
                }
            }
            if let Some(gpu) = &self.gpu {
                let (w, h) = client_size(hwnd);
                match D3dTarget::new(gpu, hwnd as isize, w, h) {
                    Ok(t) => return Some(Surface { hwnd, backend: Backend::D3d(t) }),
                    Err(e) => {
                        self.d3d_failures += 1;
                        warn!(error = %e, "D3D11 viewer swapchain failed; falling back to softbuffer");
                    }
                }
            }
        }
        let handle = Win32Handle(hwnd as isize);
        let context = match softbuffer::Context::new(handle) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "softbuffer context failed");
                return None;
            }
        };
        let surface = match softbuffer::Surface::new(&context, handle) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "softbuffer surface failed");
                return None;
            }
        };
        Some(Surface { hwnd, backend: Backend::Soft { surface, _context: context } })
    }

    fn on_frame(&mut self, id: u64) {
        let Some(i) = self.streams.iter().position(|s| s.id == id) else { return };
        let s = &mut self.streams[i];
        s.mailbox.pending.store(false, Ordering::Release);
        let Some(frame) = s.mailbox.frame.lock().take() else { return };
        let (fw, fh) = (frame.buffer().width(), frame.buffer().height());
        s.shared.video_w.store(fw, Ordering::Relaxed);
        s.shared.video_h.store(fh, Ordering::Relaxed);
        if let (Some(p), false) = (s.popout, s.sized_to_video) {
            // First frame in the pop-out: size it to the video in physical pixels (exact fit =
            // no scaling); large videos are capped to 1600x900.
            s.sized_to_video = true;
            let (fw, fh) = (fw as f64, fh as f64);
            let k = (1600.0 / fw).min(900.0 / fh).min(1.0);
            let (w, h) = outer_size((fw * k).round() as i32, (fh * k).round() as i32);
            // SAFETY: our own live window.
            unsafe { SetWindowPos(p, null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) };
        }
        if let Some(old) = s.last.replace(frame) {
            s.mailbox.recycle(old);
        }
        if let Some(wait) = s.throttle_wait() {
            // Frame-rate cap: hold the newest frame back; a timer shows it if no newer one
            // comes first (a still screen sends no more frames).
            if !s.timer_armed {
                s.timer_armed = true;
                let ms = (wait.as_millis() as u32).max(1);
                // SAFETY: a timer on the viewer thread's own control window.
                unsafe { SetTimer(CTRL.load(Ordering::Acquire) as HWND, s.id as usize, ms, None) };
            }
            return;
        }
        self.draw(i);
    }

    /// A frame-rate-capped stream's timer fired: show its newest frame.
    fn on_timer(&mut self, id: u64) {
        let Some(i) = self.streams.iter().position(|s| s.id == id) else { return };
        self.streams[i].timer_armed = false;
        if self.streams[i].last.is_some() {
            self.draw(i);
        }
    }

    fn redraw_window(&mut self, hwnd: HWND) {
        if let Some(i) = self.streams.iter().position(|s| s.surface.as_ref().is_some_and(|sf| sf.hwnd == hwnd)) {
            self.draw(i);
        }
    }

    fn draw(&mut self, i: usize) {
        let gpu = self.gpu.as_ref();
        let s = &mut self.streams[i];
        let Some(surface) = s.surface.as_mut() else { return };
        if !shown(surface.hwnd) {
            // Hidden (other tab, menu over it) or minimized: keep the newest frame, present
            // nothing, upload nothing.
            return;
        }
        let (w, h) = client_size(surface.hwnd);
        if w == 0 || h == 0 {
            return;
        }
        match &mut surface.backend {
            Backend::D3d(t) => {
                let Some(gpu) = gpu else { return };
                let last = &mut s.last;
                let planes = &mut s.planes;
                let stats = &mut s.stats;
                let style = s.style;
                let mailbox = &s.mailbox;
                let last_upload = &mut s.last_upload;
                let res = (|| {
                    t.resize(gpu, w, h)?;
                    if let Some(frame) = last.take() {
                        *last_upload = Some(Instant::now());
                        let uploaded = VideoPlanes::upload(gpu, planes, frame.buffer());
                        if let Ok(false) = uploaded {
                            unsupported_buffer(frame.buffer());
                        }
                        // The textures hold the picture now; the buffer goes back to the preview.
                        mailbox.recycle(frame);
                        if uploaded? {
                            stats.uploads += 1;
                        }
                    }
                    gpu.draw_styled(t, planes.as_ref(), style)
                })();
                match res {
                    Ok(true) => {
                        s.stats.presented += 1;
                        s.shared.presented.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        self.recover(e);
                        return;
                    }
                }
            }
            Backend::Soft { surface: sb, .. } => {
                s.last_upload = Some(Instant::now());
                if NonZeroU32::new(w).is_some() && draw_soft(sb, PhysicalSize::new(w, h), s.last.as_ref().map(Picture::buffer), &mut s.scaled) {
                    s.stats.presented += 1;
                    s.stats.uploads += 1;
                    s.shared.presented.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.streams[i].log_stats();
    }

    /// D3D11 failed mid-stream (device lost, driver update, TDR). The device is shared, so every
    /// stream starts over: a new device, or after repeated failures softbuffer in fresh pop-out
    /// windows (a window that had a flip-model swapchain cannot take GDI presents).
    fn recover(&mut self, err: RenderError) {
        self.d3d_failures += 1;
        let lost = matches!(err, RenderError::DeviceLost(_));
        warn!(error = %err, device_lost = lost, failures = self.d3d_failures, "viewer D3D11 render failed; recovering");
        for s in &mut self.streams {
            s.surface = None;
            s.planes = None;
        }
        self.gpu = None;
        if self.d3d_failures > 3 {
            self.force_soft = true;
            for s in &mut self.streams {
                s.destroy_popout(None);
                s.popped = true;
                s.return_on_close = false;
                s.popout = create_popout(&s.title);
                s.sized_to_video = false;
                s.shared.popped_out.store(true, Ordering::Release);
                s.shared.popout_hwnd.store(s.popout.map(|h| h as isize).unwrap_or(0), Ordering::Release);
            }
            super::viewer::wake_ui();
        }
        for i in 0..self.streams.len() {
            self.retarget(i);
        }
    }
}
