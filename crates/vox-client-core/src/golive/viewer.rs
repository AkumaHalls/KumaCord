//! Minimal Go Live viewer: one winit window drawn with D3D11 (softbuffer as fallback), and the
//! [`Viewer`] facade over it and the native (Win32, N streams, inline-capable) viewer.
//!
//! - Runs on its own thread with `ControlFlow::Wait`: zero CPU while no frame arrives.
//! - Decoded frames are handed over through a single-slot mailbox; the event loop is woken only
//!   when the slot goes from empty to full, so a slow window never queues frames up.
//! - D3D11 path (default on Windows, see [`super::d3d`]): one memcpy per plane into reused
//!   textures; YUV -> RGB and scaling run in a pixel shader; presents only on a new frame or a
//!   resize, never while minimized.
//! - Softbuffer path (D3D11 unavailable, or `VOX_VIEWER=soft`): libyuv scales the I420 frame to
//!   the window and converts to ARGB straight into softbuffer's 0RGB layout.
//!
//! winit allows a single event loop per process, so the thread is started lazily and lives for
//! the rest of the process; windows are opened/closed through user events.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use anyhow::{anyhow, Result};
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::video_frame::{BoxVideoFrame, I420Buffer, VideoBuffer};
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{Window, WindowId};

enum Cmd {
    Open { key: String, title: String, notify: UnboundedSender<ViewerNotice> },
    Close,
    Frame,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerNotice {
    /// The user closed the stream's own window (and it had no place in the app to go back to).
    Closed { key: String },
}

struct Mailbox {
    frame: Mutex<Option<BoxVideoFrame>>,
    pending: AtomicBool,
    /// Decoded frames replaced before they were drawn (the renderer fell behind the decoder).
    replaced: AtomicU64,
}

/// Handle to the process-wide viewer thread.
#[derive(Clone)]
pub struct Viewer(Impl);

#[derive(Clone)]
enum Impl {
    /// (Never made on macOS, where winit's event loop belongs to the app's main thread.)
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Winit(WinitViewer),
    #[cfg(windows)]
    Native(super::viewer_native::NativeViewer),
    /// macOS: the app draws the frames itself (textures in its own window).
    #[cfg(target_os = "macos")]
    Tex(super::viewer_tex::TexViewer),
}

#[derive(Clone)]
struct WinitViewer {
    proxy: EventLoopProxy<Cmd>,
    mailbox: Arc<Mailbox>,
}

/// Where one watched stream's decoded frames go ([`Viewer::open`]).
#[derive(Clone)]
pub struct ViewerStream(StreamImpl);

#[derive(Clone)]
enum StreamImpl {
    Winit(WinitViewer),
    #[cfg(windows)]
    Native(super::viewer_native::NativeStream),
    #[cfg(target_os = "macos")]
    Tex(super::viewer_tex::TexStream),
    /// Nowhere to show it (camera tiles need the native viewer): frames are dropped.
    Null,
}

/// How a camera tile is drawn (see [`Viewer::open_tile`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TileStyle {
    /// Mirror horizontally (the local camera preview, like Discord).
    pub mirror: bool,
    /// Crop to fill the tile instead of letterboxing.
    pub fill: bool,
}

impl ViewerStream {
    /// Hand over the newest frame. Never blocks; replaces an undrawn frame.
    pub fn submit(&self, frame: BoxVideoFrame) {
        match &self.0 {
            StreamImpl::Winit(v) => {
                if v.mailbox.frame.lock().replace(frame).is_some() {
                    v.mailbox.replaced.fetch_add(1, Ordering::Relaxed);
                }
                if !v.mailbox.pending.swap(true, Ordering::AcqRel) {
                    let _ = v.proxy.send_event(Cmd::Frame);
                }
            }
            #[cfg(windows)]
            StreamImpl::Native(s) => s.submit(frame),
            #[cfg(target_os = "macos")]
            StreamImpl::Tex(s) => s.submit(frame),
            StreamImpl::Null => drop(frame),
        }
    }

    /// Hand over a local preview frame (our own webcam / screen share, NV12). On the native
    /// viewer it is copied into a recycled buffer, so a steady preview allocates nothing; the
    /// borrowed frame is not kept. Never blocks; replaces an undrawn frame.
    pub fn submit_nv12(&self, f: &vox_capture::Nv12Frame<'_>) {
        match &self.0 {
            #[cfg(windows)]
            StreamImpl::Native(s) => s.submit_nv12(f),
            #[cfg(target_os = "macos")]
            StreamImpl::Tex(s) => s.submit_nv12(f),
            StreamImpl::Winit(_) => {
                if let Some(frame) = super::camera::preview_frame(f) {
                    self.submit(frame);
                }
            }
            StreamImpl::Null => {}
        }
    }

    /// A stream that shows nothing (headless tools).
    pub fn null() -> ViewerStream {
        ViewerStream(StreamImpl::Null)
    }
}

/// What the app can read about an open stream (native viewer only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamState {
    /// Shown in its own window instead of inside the app.
    pub popped_out: bool,
    /// That window (0 when there is none).
    pub popout_hwnd: isize,
    /// Size of the newest decoded frame.
    pub video: (u32, u32),
    /// Frames presented so far (any window).
    pub presented: u64,
    /// The pop-out window is fullscreen (double-click / F11 in it; Esc leaves).
    pub fullscreen: bool,
    /// Drawing-rate cap (0: none), see [`Viewer::set_max_fps`].
    pub max_fps: u32,
}

static VIEWER: OnceLock<std::result::Result<Viewer, String>> = OnceLock::new();
static PREFER_NATIVE: AtomicBool = AtomicBool::new(false);
static OPEN_INLINE: AtomicBool = AtomicBool::new(false);
type Waker = Box<dyn Fn() + Send + Sync>;
static WAKER: OnceLock<Waker> = OnceLock::new();

/// Tell the UI that a stream's state changed on its own (pop-out window closed by the user).
pub(super) fn wake_ui() {
    if let Some(w) = WAKER.get() {
        w();
    }
}

impl Viewer {
    /// Use plain Win32 windows (own message loop, no winit) for the viewer. Call before the
    /// first [`Viewer::get`]. Needed by hosts that already run their own winit event loop
    /// (e.g. an egui app): winit allows one event loop per process, so the winit viewer
    /// cannot start there. It is also what makes inline video ([`super::host::VideoHost`])
    /// possible. No effect off Windows.
    pub fn prefer_native_window() {
        PREFER_NATIVE.store(true, Ordering::Release);
    }

    /// Streams opened from now on start inside the app (`true`: shown once the app attaches a
    /// [`super::host::VideoHost`]) or in their own window (`false`, the default). Native only.
    pub fn set_open_inline(inline: bool) {
        OPEN_INLINE.store(inline, Ordering::Release);
    }

    /// Called (from the viewer thread) when a stream's [`StreamState`] changes by itself.
    pub fn set_waker(f: impl Fn() + Send + Sync + 'static) {
        let _ = WAKER.set(Box::new(f));
    }

    /// Get (starting on first use) the viewer thread.
    pub fn get() -> Result<Viewer> {
        VIEWER.get_or_init(spawn_viewer).clone().map_err(|e| anyhow!(e))
    }

    pub fn is_native(&self) -> bool {
        match &self.0 {
            Impl::Winit(_) => false,
            #[cfg(windows)]
            Impl::Native(_) => true,
            // (Tiles, inline video and pop-outs: what callers ask "native" for.)
            #[cfg(target_os = "macos")]
            Impl::Tex(_) => true,
        }
    }

    /// Start showing the stream `key` (opening it again just renames it). The winit viewer has
    /// a single window: a new key replaces the previous stream there.
    pub fn open(&self, key: &str, title: &str, notify: UnboundedSender<ViewerNotice>) -> ViewerStream {
        match &self.0 {
            Impl::Winit(v) => {
                let _ = v.proxy.send_event(Cmd::Open { key: key.to_string(), title: title.to_string(), notify });
                ViewerStream(StreamImpl::Winit(v.clone()))
            }
            #[cfg(windows)]
            Impl::Native(v) => ViewerStream(StreamImpl::Native(v.open(key, title, OPEN_INLINE.load(Ordering::Acquire), Default::default(), notify))),
            #[cfg(target_os = "macos")]
            Impl::Tex(v) => ViewerStream(StreamImpl::Tex(v.open(key, title, OPEN_INLINE.load(Ordering::Acquire), TileStyle::default(), false, notify))),
        }
    }

    /// A video tile that lives only inside the app (a webcam): shown once the app attaches a
    /// [`super::host::VideoHost`] for `key`, never in its own window. The winit viewer has no
    /// tiles: frames are dropped there.
    pub fn open_tile(&self, key: &str, style: TileStyle, notify: UnboundedSender<ViewerNotice>) -> ViewerStream {
        match &self.0 {
            Impl::Winit(_) => {
                let _ = (key, style, notify);
                ViewerStream(StreamImpl::Null)
            }
            #[cfg(windows)]
            Impl::Native(v) => {
                let style = super::d3d::DrawStyle { mirror: style.mirror, fill: style.fill };
                ViewerStream(StreamImpl::Native(v.open(key, key, true, style, notify)))
            }
            #[cfg(target_os = "macos")]
            Impl::Tex(v) => ViewerStream(StreamImpl::Tex(v.open(key, key, true, style, true, notify))),
        }
    }

    pub fn close(&self, key: &str) {
        match &self.0 {
            Impl::Winit(v) => {
                let _ = key;
                v.mailbox.frame.lock().take();
                let _ = v.proxy.send_event(Cmd::Close);
            }
            #[cfg(windows)]
            Impl::Native(v) => v.close(key),
            #[cfg(target_os = "macos")]
            Impl::Tex(v) => v.close(key),
        }
    }

    /// Title of the stream's own window.
    pub fn set_title(&self, key: &str, title: &str) {
        #[cfg(windows)]
        if let Impl::Native(v) = &self.0 {
            v.retitle(key, title);
        }
        #[cfg(target_os = "macos")]
        if let Impl::Tex(v) = &self.0 {
            v.retitle(key, title);
        }
        let _ = (key, title);
    }

    /// Show `key` inside the app, in this host window (`None`: not inside the app any more;
    /// returns once the viewer let go of the old one). Native only.
    pub fn attach(&self, key: &str, host: Option<isize>) {
        #[cfg(windows)]
        if let Impl::Native(v) = &self.0 {
            v.attach(key, host);
        }
        let _ = (key, host);
    }

    /// Forget a host window everywhere (it is about to be destroyed). Synchronous.
    pub fn drop_host(&self, host: isize) {
        #[cfg(windows)]
        if let Impl::Native(v) = &self.0 {
            v.drop_host(host);
        }
        let _ = host;
    }

    /// Move `key` into its own window (`true`) or back inside the app (`false`). Closing that
    /// window brings it back too. In that window, a double-click or F11 toggles fullscreen (on
    /// the window's monitor) and Esc leaves it. Native only.
    pub fn pop_out(&self, key: &str, out: bool) {
        #[cfg(windows)]
        if let Impl::Native(v) = &self.0 {
            v.pop_out(key, out);
        }
        #[cfg(target_os = "macos")]
        if let Impl::Tex(v) = &self.0 {
            v.pop_out(key, out);
        }
        let _ = (key, out);
    }

    /// Draw `key` at most `fps` times per second (0: every frame). For small tiles (the strip
    /// under a focused stream): uploads and presents less, the decoder is not affected. The
    /// newest frame is always shown in the end. Native only.
    pub fn set_max_fps(&self, key: &str, fps: u32) {
        #[cfg(windows)]
        if let Impl::Native(v) = &self.0 {
            v.set_max_fps(key, fps);
        }
        #[cfg(target_os = "macos")]
        if let Impl::Tex(v) = &self.0 {
            v.set_max_fps(key, fps);
        }
        let _ = (key, fps);
    }

    /// Open streams (native only).
    pub fn keys(&self) -> Vec<String> {
        match &self.0 {
            Impl::Winit(_) => Vec::new(),
            #[cfg(windows)]
            Impl::Native(v) => v.keys(),
            #[cfg(target_os = "macos")]
            Impl::Tex(v) => v.keys(),
        }
    }

    pub fn state(&self, key: &str) -> Option<StreamState> {
        match &self.0 {
            Impl::Winit(_) => None,
            #[cfg(windows)]
            Impl::Native(v) => v.shared(key).map(|s| StreamState {
                popped_out: s.popped_out.load(Ordering::Acquire),
                popout_hwnd: s.popout_hwnd.load(Ordering::Acquire),
                video: (s.video_w.load(Ordering::Relaxed), s.video_h.load(Ordering::Relaxed)),
                presented: s.presented.load(Ordering::Relaxed),
                fullscreen: s.fullscreen.load(Ordering::Acquire),
                max_fps: s.max_fps.load(Ordering::Relaxed),
            }),
            #[cfg(target_os = "macos")]
            Impl::Tex(v) => v.shared(key).map(|s| StreamState {
                popped_out: s.popped_out.load(Ordering::Acquire),
                popout_hwnd: 0,
                video: (s.video_w.load(Ordering::Relaxed), s.video_h.load(Ordering::Relaxed)),
                presented: s.presented.load(Ordering::Relaxed),
                fullscreen: s.fullscreen.load(Ordering::Acquire),
                max_fps: s.max_fps.load(Ordering::Relaxed),
            }),
        }
    }
}

/// macOS: the app draws every stream itself ([`super::viewer_tex`]).
#[cfg(target_os = "macos")]
impl Viewer {
    fn tex(&self) -> Option<&super::viewer_tex::TexViewer> {
        match &self.0 {
            Impl::Tex(v) => Some(v),
            Impl::Winit(_) => None,
        }
    }

    /// A frame is waiting to be drawn for `key`.
    pub fn has_frame(&self, key: &str) -> bool {
        self.tex().is_some_and(|v| v.has_frame(key))
    }

    /// The newest frame of `key` not drawn yet (taking it lets the next one wake the UI again).
    pub fn take_frame(&self, key: &str) -> Option<super::viewer_tex::TexFrame> {
        self.tex()?.take_frame(key)
    }

    /// How `key` is drawn, its window title, and the frames replaced undrawn since the last call.
    pub fn draw_info(&self, key: &str) -> Option<(TileStyle, String, u64)> {
        self.tex()?.info(key)
    }

    /// The app made `key`'s own window fullscreen (or left it).
    pub fn set_fullscreen(&self, key: &str, on: bool) {
        if let Some(v) = self.tex() {
            v.set_fullscreen(key, on);
        }
    }
}

fn spawn_viewer() -> std::result::Result<Viewer, String> {
    // macOS: winit's event loop must live on the main thread (the app's), so there is no viewer
    // thread at all; the app draws the frames.
    #[cfg(target_os = "macos")]
    return Ok(Viewer(Impl::Tex(super::viewer_tex::TexViewer::default())));
    #[cfg(windows)]
    if PREFER_NATIVE.load(Ordering::Acquire) {
        return super::viewer_native::NativeViewer::spawn().map(|v| Viewer(Impl::Native(v)));
    }
    #[cfg(not(target_os = "macos"))]
    return spawn_winit_viewer().map(|v| Viewer(Impl::Winit(v)));
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn spawn_winit_viewer() -> std::result::Result<WinitViewer, String> {
    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<EventLoopProxy<Cmd>, String>>();
    let mailbox = Arc::new(Mailbox { frame: Mutex::new(None), pending: AtomicBool::new(false), replaced: AtomicU64::new(0) });
    let mb = mailbox.clone();
    std::thread::Builder::new()
        .name("vox-viewer".into())
        .spawn(move || {
            let mut builder = EventLoop::<Cmd>::with_user_event();
            #[cfg(windows)]
            {
                use winit::platform::windows::EventLoopBuilderExtWindows;
                builder.with_any_thread(true);
            }
            let event_loop = match builder.build() {
                Ok(el) => el,
                Err(e) => {
                    let _ = tx.send(Err(format!("cannot create viewer event loop: {e}")));
                    return;
                }
            };
            event_loop.set_control_flow(ControlFlow::Wait);
            let _ = tx.send(Ok(event_loop.create_proxy()));
            let force_soft = std::env::var("VOX_VIEWER").map(|v| v.eq_ignore_ascii_case("soft")).unwrap_or(false);
            let mut app = App {
                mailbox: mb,
                win: None,
                key: String::new(),
                title: String::new(),
                notify: None,
                last: None,
                scaled: Vec::new(),
                force_soft,
                d3d_failures: 0,
                stats: ViewStats::default(),
            };
            if let Err(e) = event_loop.run_app(&mut app) {
                warn!(error = %e, "viewer event loop exited");
            }
        })
        .map_err(|e| e.to_string())?;
    let proxy = rx.recv().map_err(|e| e.to_string())??;
    Ok(WinitViewer { proxy, mailbox })
}

enum Backend {
    #[cfg(windows)]
    D3d(super::d3d::D3dRenderer),
    Soft {
        surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
        _context: softbuffer::Context<Rc<Window>>,
    },
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            #[cfg(windows)]
            Backend::D3d(_) => "d3d11",
            Backend::Soft { .. } => "softbuffer",
        }
    }
}

struct Win {
    window: Rc<Window>,
    /// `None` only transiently while a failed renderer is being replaced.
    backend: Option<Backend>,
    sized_to_video: bool,
}

/// Aggregated viewer counters, logged at most every `STATS_INTERVAL`.
#[derive(Default)]
struct ViewStats {
    presented: u64,
    uploads: u64,
    since: Option<Instant>,
}

struct App {
    mailbox: Arc<Mailbox>,
    win: Option<Win>,
    key: String,
    title: String,
    notify: Option<UnboundedSender<ViewerNotice>>,
    /// Newest frame not yet shown. The D3D path drops it once uploaded (the textures keep the
    /// picture for redraws); softbuffer keeps it so resizes/expose events can redraw.
    last: Option<BoxVideoFrame>,
    /// Scratch ARGB buffer for letterboxed softbuffer draws.
    scaled: Vec<u8>,
    /// Use softbuffer even if D3D11 works (`VOX_VIEWER=soft`, or after D3D11 kept failing).
    #[cfg_attr(not(windows), allow(dead_code))]
    force_soft: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    d3d_failures: u32,
    stats: ViewStats,
}

impl App {
    fn open(&mut self, el: &ActiveEventLoop, title: &str) {
        self.title = title.to_string();
        if let Some(w) = &self.win {
            w.window.set_title(title);
            return;
        }
        let attrs = Window::default_attributes().with_title(title).with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                warn!(error = %e, "cannot create viewer window");
                return;
            }
        };
        let Some(backend) = self.make_backend(&window) else { return };
        info!(title, renderer = backend.name(), "viewer window opened");
        self.win = Some(Win { window, backend: Some(backend), sized_to_video: false });
    }

    fn make_backend(&mut self, window: &Rc<Window>) -> Option<Backend> {
        #[cfg(windows)]
        if !self.force_soft {
            if let Some(hwnd) = hwnd_of(window) {
                let size = window.inner_size();
                match super::d3d::D3dRenderer::new(hwnd, size.width, size.height) {
                    Ok(r) => return Some(Backend::D3d(r)),
                    Err(e) => {
                        // Init failures happen before or at swapchain creation; a half-made
                        // swapchain is released on drop, so GDI presents work on this window.
                        self.d3d_failures += 1;
                        warn!(error = %e, "D3D11 viewer init failed; falling back to softbuffer");
                    }
                }
            }
        }
        soft_backend(window)
    }

    fn close(&mut self) {
        if self.win.take().is_some() {
            info!("viewer window closed");
        }
        self.last = None;
        self.notify = None;
        self.stats = ViewStats::default();
    }

    /// D3D11 failed mid-stream (device lost, driver update, TDR): rebuild the renderer, or after
    /// repeated failures recreate the window with softbuffer.
    #[cfg(windows)]
    fn recover(&mut self, el: &ActiveEventLoop, err: super::d3d::RenderError) {
        self.d3d_failures += 1;
        let lost = matches!(err, super::d3d::RenderError::DeviceLost(_));
        warn!(error = %err, device_lost = lost, failures = self.d3d_failures, "viewer D3D11 render failed; recovering");
        let Some(win) = self.win.as_mut() else { return };
        // Release the old swapchain/device before creating new ones on the same HWND.
        win.backend = None;
        if self.d3d_failures <= 3 {
            if let Some(hwnd) = hwnd_of(&win.window) {
                let size = win.window.inner_size();
                match super::d3d::D3dRenderer::new(hwnd, size.width, size.height) {
                    Ok(r) => {
                        win.backend = Some(Backend::D3d(r));
                        win.window.request_redraw();
                        info!("viewer D3D11 renderer recreated");
                        return;
                    }
                    Err(e) => warn!(error = %e, "viewer D3D11 re-init failed"),
                }
            }
        }
        // Give up on D3D11 for this process: fresh window (no swapchain ever attached) +
        // softbuffer.
        self.force_soft = true;
        let notify = self.notify.take();
        let title = self.title.clone();
        self.win = None;
        self.open(el, &title);
        self.notify = notify;
    }

    fn draw(&mut self, el: &ActiveEventLoop) {
        let _ = el;
        let Some(win) = self.win.as_mut() else { return };
        let size = win.window.inner_size();
        if size.width == 0 || size.height == 0 || win.window.is_minimized() == Some(true) {
            // Minimized: keep the newest frame for when the window comes back, present nothing.
            return;
        }
        match win.backend.as_mut() {
            None => return,
            #[cfg(windows)]
            Some(Backend::D3d(r)) => {
                let last = &mut self.last;
                let stats = &mut self.stats;
                let res = (|| {
                    r.resize(size.width, size.height)?;
                    if let Some(frame) = last.take() {
                        if r.upload(frame.buffer.as_ref())? {
                            stats.uploads += 1;
                        } else {
                            unsupported_buffer(frame.buffer.as_ref());
                        }
                    }
                    r.draw()
                })();
                match res {
                    Ok(true) => self.stats.presented += 1,
                    Ok(false) => {}
                    Err(e) => {
                        self.recover(el, e);
                        return;
                    }
                }
            }
            Some(Backend::Soft { surface, .. }) => {
                if draw_soft(surface, size, self.last.as_ref().map(|f| f.buffer.as_ref()), &mut self.scaled) {
                    self.stats.presented += 1;
                    self.stats.uploads += 1;
                }
            }
        }
        self.log_stats();
    }

    fn log_stats(&mut self) {
        let now = Instant::now();
        let since = *self.stats.since.get_or_insert(now);
        if now.duration_since(since) < vox_log::STATS_INTERVAL {
            return;
        }
        let Some(win) = self.win.as_ref() else { return };
        let secs = now.duration_since(since).as_secs_f64().max(1e-3);
        let size = win.window.inner_size();
        #[allow(unused_mut)]
        let mut video = (0, 0);
        #[cfg(windows)]
        if let Some(Backend::D3d(r)) = &win.backend {
            video = r.video_size().unwrap_or((0, 0));
        }
        info!(
            renderer = win.backend.as_ref().map(Backend::name).unwrap_or("-"),
            presented_fps = format!("{:.1}", self.stats.presented as f64 / secs),
            uploaded_fps = format!("{:.1}", self.stats.uploads as f64 / secs),
            replaced_undrawn = self.mailbox.replaced.swap(0, Ordering::Relaxed),
            video = format!("{}x{}", video.0, video.1),
            window = format!("{}x{}", size.width, size.height),
            "viewer stats"
        );
        self.stats = ViewStats { since: Some(now), ..Default::default() };
    }
}

fn soft_backend(window: &Rc<Window>) -> Option<Backend> {
    let context = match softbuffer::Context::new(window.clone()) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "softbuffer context failed");
            return None;
        }
    };
    let surface = match softbuffer::Surface::new(&context, window.clone()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "softbuffer surface failed");
            return None;
        }
    };
    Some(Backend::Soft { surface, _context: context })
}

#[cfg(windows)]
fn hwnd_of(window: &Window) -> Option<isize> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

pub(super) fn unsupported_buffer(src: &dyn VideoBuffer) {
    static UNSUPPORTED: vox_log::RateLimiter = vox_log::RateLimiter::new();
    if UNSUPPORTED.ready(vox_log::STATS_INTERVAL) {
        warn!(buffer_type = ?src.buffer_type(), "viewer: unsupported decoded buffer type");
    }
}

/// CPU fallback: libyuv scale + convert into softbuffer. Returns true when a frame was shown.
pub(super) fn draw_soft<D: HasDisplayHandle, W: HasWindowHandle>(
    surface: &mut softbuffer::Surface<D, W>,
    size: PhysicalSize<u32>,
    last: Option<&dyn VideoBuffer>,
    scaled: &mut Vec<u8>,
) -> bool {
    let (Some(ww), Some(wh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return false };
    if surface.resize(ww, wh).is_err() {
        return false;
    }
    let Ok(mut buf) = surface.buffer_mut() else { return false };
    let (ww, wh) = (ww.get() as usize, wh.get() as usize);
    let Some(frame) = last else {
        buf.fill(0);
        let _ = buf.present();
        return false;
    };
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    if fw == 0 || fh == 0 {
        return false;
    }
    let (dw, dh, x0, y0) = fit_rect(fw, fh, ww, wh);
    let scale_to = ((dw, dh) != (fw, fh)).then_some((dw, dh));
    let src: &dyn VideoBuffer = frame;
    let ok = if scale_to.is_none() && dw == ww && dh == wh {
        // Frame already matches the window exactly: convert straight into the window buffer.
        convert_to_argb(src, None, as_bytes_mut(&mut buf), ww * 4)
    } else {
        // Scale (if needed) into scratch, then letterbox-copy into the window buffer.
        scaled.resize(dw * dh * 4, 0);
        let ok = convert_to_argb(src, scale_to, scaled, dw * 4);
        buf.fill(0);
        let bytes = as_bytes_mut(&mut buf);
        for row in 0..dh {
            let dst = ((y0 + row) * ww + x0) * 4;
            bytes[dst..dst + dw * 4].copy_from_slice(&scaled[row * dw * 4..(row + 1) * dw * 4]);
        }
        ok
    };
    if !ok {
        unsupported_buffer(src);
    }
    buf.present().is_ok() && ok
}

/// Convert a decoded frame into ARGB (softbuffer 0RGB byte order), optionally scaling to `to`.
/// Returns false for buffer types we cannot read (e.g. GPU-native buffers).
fn convert_to_argb(src: &dyn VideoBuffer, to: Option<(usize, usize)>, dst: &mut [u8], dst_stride: usize) -> bool {
    let (fw, fh) = (src.width(), src.height());
    match to {
        None => {
            if let Some(i) = src.as_i420() {
                let (sy, su, sv) = i.strides();
                let (py, pu, pv) = i.data();
                yuv_helper::i420_to_argb(py, sy, pu, su, pv, sv, dst, dst_stride as u32, fw as i32, fh as i32);
                true
            } else if let Some(n) = src.as_nv12() {
                let (sy, suv) = n.strides();
                let (py, puv) = n.data();
                yuv_helper::nv12_to_argb(py, sy, puv, suv, dst, dst_stride as u32, fw as i32, fh as i32);
                true
            } else {
                false
            }
        }
        Some((w, h)) => {
            // `scale` needs an owned/mutable I420 buffer; a borrowed trait object can't give one,
            // so copy (or NV12->I420 convert) once, then let libyuv scale.
            let mut full = I420Buffer::new(fw, fh);
            {
                let (dsy, dsu, dsv) = full.strides();
                let (dy, du, dv) = full.data_mut();
                if let Some(i) = src.as_i420() {
                    let (sy, su, sv) = i.strides();
                    let (py, pu, pv) = i.data();
                    let (cw, ch) = (fw.div_ceil(2) as usize, fh.div_ceil(2) as usize);
                    copy_plane(py, sy as usize, dy, dsy as usize, fw as usize, fh as usize);
                    copy_plane(pu, su as usize, du, dsu as usize, cw, ch);
                    copy_plane(pv, sv as usize, dv, dsv as usize, cw, ch);
                } else if let Some(n) = src.as_nv12() {
                    let (sy, suv) = n.strides();
                    let (py, puv) = n.data();
                    yuv_helper::nv12_to_i420(py, sy, puv, suv, dy, dsy, du, dsu, dv, dsv, fw as i32, fh as i32);
                } else {
                    return false;
                }
            }
            let scaled = full.scale(w as i32, h as i32);
            let (sy, su, sv) = scaled.strides();
            let (py, pu, pv) = scaled.data();
            yuv_helper::i420_to_argb(py, sy, pu, su, pv, sv, dst, dst_stride as u32, w as i32, h as i32);
            true
        }
    }
}

/// Fit a `fw x fh` frame inside a `ww x wh` window keeping aspect. Returns the drawn size
/// (even, so I420 scaling is exact) and its top-left offset. A frame that already fits exactly
/// keeps its size, so no scaling happens.
fn fit_rect(fw: usize, fh: usize, ww: usize, wh: usize) -> (usize, usize, usize, usize) {
    if fw == ww && fh == wh {
        return (fw, fh, 0, 0);
    }
    let scale = (ww as f64 / fw as f64).min(wh as f64 / fh as f64);
    let dw = (((fw as f64 * scale).round() as usize).clamp(2, ww)) & !1;
    let dh = (((fh as f64 * scale).round() as usize).clamp(2, wh)) & !1;
    (dw, dh, (ww - dw) / 2, (wh - dh) / 2)
}

fn copy_plane(src: &[u8], src_stride: usize, dst: &mut [u8], dst_stride: usize, width: usize, rows: usize) {
    for r in 0..rows {
        dst[r * dst_stride..r * dst_stride + width].copy_from_slice(&src[r * src_stride..r * src_stride + width]);
    }
}

fn as_bytes_mut(px: &mut [u32]) -> &mut [u8] {
    // SAFETY: u32 has no padding and any byte pattern is valid; alignment of u8 is 1.
    unsafe { std::slice::from_raw_parts_mut(px.as_mut_ptr() as *mut u8, std::mem::size_of_val(px)) }
}

impl ApplicationHandler<Cmd> for App {
    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn user_event(&mut self, el: &ActiveEventLoop, cmd: Cmd) {
        match cmd {
            Cmd::Open { key, title, notify } => {
                self.key = key;
                self.notify = Some(notify);
                self.open(el, &title);
            }
            Cmd::Close => self.close(),
            Cmd::Frame => {
                self.mailbox.pending.store(false, Ordering::Release);
                let frame = self.mailbox.frame.lock().take();
                if let (Some(frame), Some(win)) = (frame, self.win.as_mut()) {
                    if !win.sized_to_video {
                        // First frame: size the window to the video in physical pixels (exact fit = no
                        // scaling); large videos are capped to 1600x900.
                        win.sized_to_video = true;
                        let (fw, fh) = (frame.buffer.width() as f64, frame.buffer.height() as f64);
                        let s = (1600.0 / fw).min(900.0 / fh).min(1.0);
                        let _ = win.window.request_inner_size(PhysicalSize::new(fw * s, fh * s));
                    }
                    self.last = Some(frame);
                    win.window.request_redraw();
                }
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.win.as_ref().map(|w| w.window.id()) != Some(id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                if let Some(n) = self.notify.take() {
                    let _ = n.send(ViewerNotice::Closed { key: self.key.clone() });
                }
                self.close();
            }
            WindowEvent::RedrawRequested => self.draw(el),
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.win {
                    w.window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fit_rect;

    #[test]
    fn fit_rect_stays_inside_and_keeps_aspect() {
        // Exact match: no scaling.
        assert_eq!(fit_rect(1280, 720, 1280, 720), (1280, 720, 0, 0));
        // 1080p into the window sized for it at 1600x900: scaled, no letterbox.
        assert_eq!(fit_rect(1920, 1080, 1600, 900), (1600, 900, 0, 0));
        // Pillarbox / letterbox.
        assert_eq!(fit_rect(1280, 720, 1000, 720), (1000, 562, 0, 79));
        let (dw, dh, x0, y0) = fit_rect(960, 600, 1280, 720);
        assert_eq!((dw, dh), (1152, 720));
        assert_eq!((x0, y0), (64, 0));
        for (fw, fh, ww, wh) in [(1919, 1081, 1600, 900), (3, 3, 2, 2), (1080, 1920, 1601, 899), (640, 480, 641, 481)] {
            let (dw, dh, x0, y0) = fit_rect(fw, fh, ww, wh);
            assert!(x0 + dw <= ww && y0 + dh <= wh, "{fw}x{fh} in {ww}x{wh}");
            assert!((fw == ww && fh == wh) || (dw % 2 == 0 && dh % 2 == 0));
        }
    }
}
