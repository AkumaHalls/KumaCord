//! Go Live viewer on macOS: no windows or threads of its own. The app draws the video itself, as
//! textures in its own (OpenGL) window, inside the tiles of the call view, and in pop-out windows
//! it opens as extra egui viewports.
//!
//! - Each stream has a single-slot mailbox: a newer frame replaces one the app has not taken yet,
//!   so a slow UI never queues frames up. The UI is woken (repaint) only when the slot goes from
//!   empty to full, so a stream that is not on screen (never taken) costs one wake-up, not one per
//!   frame.
//! - Decoded frames from VideoToolbox arrive as native `CVPixelBuffer`s (NV12, video or full
//!   range): [`TexFrame::with_planes`] locks one read-only and lends its planes out, so the app
//!   uploads them to the GPU as they are (no CPU colour conversion, no copy into a Rust buffer).
//!   I420 / NV12 buffers (software decoders, our own camera preview) are lent the same way.
//! - The pop-out and fullscreen flags are only state here: the app opens and closes the window.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use livekit::webrtc::video_frame::native::VideoFrameBufferExt;
use livekit::webrtc::video_frame::{BoxVideoFrame, NV12Buffer, VideoBuffer};
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use super::viewer::{TileStyle, ViewerNotice};

/// A picture on its way to the screen: a decoded frame from the network, or a local preview
/// (our own webcam / screen) whose buffer is recycled so a steady preview allocates nothing.
enum Picture {
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

/// What other threads may read about a stream (see [`super::viewer::StreamState`]).
#[derive(Default)]
pub(super) struct Shared {
    pub popped_out: AtomicBool,
    pub fullscreen: AtomicBool,
    pub video_w: AtomicU32,
    pub video_h: AtomicU32,
    pub presented: AtomicU64,
    pub max_fps: AtomicU32,
}

struct Entry {
    frame: Mutex<Option<Picture>>,
    pending: AtomicBool,
    /// Frames replaced before the app took them (it fell behind, or the stream is not shown).
    replaced: AtomicU64,
    /// Pictures that reached the mailbox (decoded), for the logs: none at all means the decoder
    /// never got a keyframe (the sharer sent nothing), not a drawing problem.
    received: AtomicU64,
    opened: std::time::Instant,
    /// A drawn preview buffer waiting for the next preview frame.
    spare: Mutex<Option<NV12Buffer>>,
    shared: Arc<Shared>,
    style: TileStyle,
    title: Mutex<String>,
    /// A camera tile: never in its own window.
    tile_only: bool,
}

impl Entry {
    fn put(&self, p: Picture) {
        let (w, h) = (p.buffer().width(), p.buffer().height());
        if self.received.fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::info!(title = %self.title.lock(), w, h, kind = %describe(p.buffer()), ms = self.opened.elapsed().as_millis() as u64, "video stream: first picture");
        }
        self.shared.video_w.store(w, Ordering::Relaxed);
        self.shared.video_h.store(h, Ordering::Relaxed);
        match self.frame.lock().replace(p) {
            Some(Picture::Nv12(old)) => {
                self.replaced.fetch_add(1, Ordering::Relaxed);
                *self.spare.lock() = Some(old);
            }
            Some(Picture::Decoded(_)) => {
                self.replaced.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
        if !self.pending.swap(true, Ordering::AcqRel) {
            super::viewer::wake_ui();
        }
    }
}

/// The streams the app can draw.
#[derive(Clone, Default)]
pub(super) struct TexViewer {
    streams: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
}

/// Where one stream's frames go (see [`super::viewer::ViewerStream`]).
#[derive(Clone)]
pub(super) struct TexStream(Arc<Entry>);

impl TexStream {
    pub(super) fn submit(&self, frame: BoxVideoFrame) {
        self.0.put(Picture::Decoded(frame));
    }

    /// A local NV12 preview frame, copied into a recycled buffer (no allocation once the size is
    /// steady).
    pub(super) fn submit_nv12(&self, f: &vox_capture::Nv12Frame<'_>) {
        let mut buf = match self.0.spare.lock().take() {
            Some(b) if b.width() == f.width && b.height() == f.height => b,
            _ => NV12Buffer::new(f.width, f.height),
        };
        if super::camera::fill_nv12(&mut buf, f) {
            self.0.put(Picture::Nv12(buf));
        }
    }
}

impl TexViewer {
    pub(super) fn open(&self, key: &str, title: &str, inline: bool, style: TileStyle, tile_only: bool, _notify: UnboundedSender<ViewerNotice>) -> TexStream {
        let mut streams = self.streams.lock();
        if let Some(e) = streams.get(key) {
            *e.title.lock() = title.to_string();
            return TexStream(e.clone());
        }
        let shared = Arc::new(Shared::default());
        shared.popped_out.store(!inline && !tile_only, Ordering::Release);
        let e = Arc::new(Entry {
            frame: Mutex::new(None),
            pending: AtomicBool::new(false),
            replaced: AtomicU64::new(0),
            received: AtomicU64::new(0),
            opened: std::time::Instant::now(),
            spare: Mutex::new(None),
            shared,
            style,
            title: Mutex::new(title.to_string()),
            tile_only,
        });
        streams.insert(key.to_string(), e.clone());
        drop(streams);
        tracing::info!(key, inline, tile_only, "video stream opened (drawn by the app)");
        super::viewer::wake_ui();
        TexStream(e)
    }

    pub(super) fn close(&self, key: &str) {
        if let Some(e) = self.streams.lock().remove(key) {
            e.frame.lock().take();
            let received = e.received.load(Ordering::Relaxed);
            let presented = e.shared.presented.load(Ordering::Relaxed);
            let secs = e.opened.elapsed().as_secs();
            tracing::info!(key, received, replaced = e.replaced.load(Ordering::Relaxed), presented, secs, "video stream closed");
            if received == 0 && secs >= 5 {
                tracing::warn!(key, secs, "video stream closed without a single picture: nothing decodable arrived (no keyframe from the sharer)");
            } else if received > 0 && presented == 0 && secs >= 5 {
                tracing::warn!(key, received, secs, "video stream closed: pictures arrived but none was drawn");
            }
        }
        super::viewer::wake_ui();
    }

    fn get(&self, key: &str) -> Option<Arc<Entry>> {
        self.streams.lock().get(key).cloned()
    }

    pub(super) fn retitle(&self, key: &str, title: &str) {
        if let Some(e) = self.get(key) {
            *e.title.lock() = title.to_string();
        }
    }

    pub(super) fn pop_out(&self, key: &str, out: bool) {
        if let Some(e) = self.get(key).filter(|e| !e.tile_only) {
            e.shared.popped_out.store(out, Ordering::Release);
            if !out {
                e.shared.fullscreen.store(false, Ordering::Release);
            }
        }
        super::viewer::wake_ui();
    }

    pub(super) fn set_fullscreen(&self, key: &str, on: bool) {
        if let Some(e) = self.get(key) {
            e.shared.fullscreen.store(on, Ordering::Release);
        }
    }

    pub(super) fn set_max_fps(&self, key: &str, fps: u32) {
        if let Some(e) = self.get(key) {
            e.shared.max_fps.store(fps, Ordering::Relaxed);
        }
    }

    pub(super) fn shared(&self, key: &str) -> Option<Arc<Shared>> {
        self.get(key).map(|e| e.shared.clone())
    }

    pub(super) fn keys(&self) -> Vec<String> {
        self.streams.lock().keys().cloned().collect()
    }

    pub(super) fn has_frame(&self, key: &str) -> bool {
        self.get(key).is_some_and(|e| e.pending.load(Ordering::Acquire))
    }

    pub(super) fn take_frame(&self, key: &str) -> Option<TexFrame> {
        let e = self.get(key)?;
        e.pending.store(false, Ordering::Release);
        let pic = e.frame.lock().take()?;
        Some(TexFrame { pic: Some(pic), entry: e })
    }

    pub(super) fn info(&self, key: &str) -> Option<(TileStyle, String, u64)> {
        self.get(key).map(|e| (e.style, e.title.lock().clone(), e.replaced.swap(0, Ordering::Relaxed)))
    }
}

/// The planes of one picture, borrowed for an upload.
#[derive(Debug)]
pub enum Planes<'a> {
    I420 { y: &'a [u8], u: &'a [u8], v: &'a [u8], strides: (u32, u32, u32) },
    Nv12 { y: &'a [u8], uv: &'a [u8], strides: (u32, u32) },
}

/// A picture's planes and how to read them.
#[derive(Debug)]
pub struct PlaneView<'a> {
    pub width: u32,
    pub height: u32,
    /// Full-range YUV (0-255, what VideoToolbox gives as `420f`) instead of video range (16-235).
    pub full_range: bool,
    pub planes: Planes<'a>,
    /// Where it came from, for logs: "cvpixelbuffer", "i420", "nv12" or "converted".
    pub source: &'static str,
}

impl PlaneView<'_> {
    /// CPU fallback (no GPU conversion available): the picture as RGBA bytes, `width * 4` per
    /// row, into `dst` (resized). libyuv, BT.601 limited range.
    pub fn to_rgba(&self, dst: &mut Vec<u8>) {
        use livekit::webrtc::native::yuv_helper;
        let (w, h) = (self.width as usize, self.height as usize);
        dst.resize(w * h * 4, 0);
        match self.planes {
            Planes::I420 { y, u, v, strides: (sy, su, sv) } => yuv_helper::i420_to_abgr(y, sy, u, su, v, sv, dst, (w * 4) as u32, w as i32, h as i32),
            Planes::Nv12 { y, uv, strides: (sy, suv) } => yuv_helper::nv12_to_abgr(y, sy, uv, suv, dst, (w * 4) as u32, w as i32, h as i32),
        }
    }
}

/// One frame taken from a stream for drawing. Dropping it gives a preview buffer back for reuse.
pub struct TexFrame {
    pic: Option<Picture>,
    entry: Arc<Entry>,
}

impl Drop for TexFrame {
    fn drop(&mut self) {
        if let Some(Picture::Nv12(b)) = self.pic.take() {
            *self.entry.spare.lock() = Some(b);
        }
    }
}

impl TexFrame {
    fn buffer(&self) -> &dyn VideoBuffer {
        self.pic.as_ref().expect("picture present until drop").buffer()
    }

    pub fn size(&self) -> (u32, u32) {
        let b = self.buffer();
        (b.width(), b.height())
    }

    /// What kind of picture this is, for logs ("cvpixelbuffer 420f iosurface 1920x1032", "i420 ...").
    pub fn describe(&self) -> String {
        describe(self.buffer())
    }

    /// Count it as shown (the app drew it).
    pub fn presented(&self) {
        self.entry.shared.presented.fetch_add(1, Ordering::Relaxed);
    }

    /// The decoded picture as an IOSurface-backed NV12 `CVPixelBuffer` (what VideoToolbox gives),
    /// for a zero-copy upload: the GPU reads the surface's planes directly. Valid while this frame
    /// is alive (keep it until the GPU is done with it). `None` for anything else.
    pub fn native_nv12(&self) -> Option<NativeNv12> {
        let b = self.buffer();
        let pb = b.as_native()?.get_cv_pixel_buffer();
        if pb.is_null() {
            return None;
        }
        // SAFETY: the frame keeps its pixel buffer alive while we hold it.
        unsafe { cv::native_nv12(pb, b.width(), b.height()) }
    }

    /// Lend the picture's planes to `f`. A native `CVPixelBuffer` (NV12) is locked read-only for
    /// the duration of the call and read in place; anything else that is neither I420 nor NV12 is
    /// converted to I420 first (a CPU copy, logged once). `None` when there is nothing to read.
    pub fn with_planes<R>(&self, f: impl FnOnce(&PlaneView<'_>) -> R) -> Option<R> {
        let b = self.buffer();
        let (w, h) = (b.width(), b.height());
        if w < 2 || h < 2 {
            return None;
        }
        if let Some(i) = b.as_i420() {
            let (y, u, v) = i.data();
            return Some(f(&PlaneView { width: w, height: h, full_range: false, planes: Planes::I420 { y, u, v, strides: i.strides() }, source: "i420" }));
        }
        if let Some(n) = b.as_nv12() {
            let (y, uv) = n.data();
            return Some(f(&PlaneView { width: w, height: h, full_range: false, planes: Planes::Nv12 { y, uv, strides: n.strides() }, source: "nv12" }));
        }
        let n = b.as_native()?;
        let mut f = Some(f);
        let pb = n.get_cv_pixel_buffer();
        if !pb.is_null() {
            // SAFETY: the frame keeps its pixel buffer alive while we hold it.
            if let Some(r) = unsafe { cv::with_nv12(pb, w, h, |v| (f.take().expect("called once"))(v)) } {
                return Some(r);
            }
        }
        let f = f?;
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!("viewer: decoded picture needs a CPU conversion to I420"));
        let i = VideoFrameBufferExt::to_i420(n);
        let (y, u, v) = i.data();
        Some(f(&PlaneView { width: i.width(), height: i.height(), full_range: false, planes: Planes::I420 { y, u, v, strides: i.strides() }, source: "converted" }))
    }
}

/// A picture's buffer kind, pixel format and size, for logs.
fn describe(b: &dyn VideoBuffer) -> String {
    let (w, h) = (b.width(), b.height());
    if b.as_i420().is_some() {
        return format!("i420 {w}x{h}");
    }
    if b.as_nv12().is_some() {
        return format!("nv12 {w}x{h}");
    }
    match b.as_native() {
        Some(n) => {
            let pb = n.get_cv_pixel_buffer();
            if pb.is_null() {
                format!("native (no CVPixelBuffer) {w}x{h}")
            } else {
                // SAFETY: the frame keeps its pixel buffer alive while we hold it.
                unsafe { cv::describe(pb, w, h) }
            }
        }
        None => format!("{:?} {w}x{h}", b.buffer_type()),
    }
}

/// A decoded NV12 picture still in its IOSurface (see [`TexFrame::native_nv12`]).
#[derive(Debug, Clone, Copy)]
pub struct NativeNv12 {
    /// `IOSurfaceRef` (borrowed from the frame).
    pub io_surface: *mut std::ffi::c_void,
    pub width: u32,
    pub height: u32,
    pub full_range: bool,
}

/// CoreVideo: read a decoded `CVPixelBuffer` in place.
mod cv {
    use std::ffi::c_void;

    use super::{PlaneView, Planes};

    type CVPixelBufferRef = *mut c_void;
    const LOCK_READ_ONLY: u64 = 1;
    /// '420v': NV12, video range.
    const NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
    /// '420f': NV12, full range (what WebRTC's VideoToolbox decoder asks for).
    const NV12_FULL: u32 = u32::from_be_bytes(*b"420f");

    #[link(name = "CoreVideo", kind = "framework")]
    extern "C" {
        fn CVPixelBufferLockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> i32;
        fn CVPixelBufferGetPixelFormatType(pb: CVPixelBufferRef) -> u32;
        fn CVPixelBufferGetPlaneCount(pb: CVPixelBufferRef) -> usize;
        fn CVPixelBufferGetBaseAddressOfPlane(pb: CVPixelBufferRef, plane: usize) -> *mut c_void;
        fn CVPixelBufferGetBytesPerRowOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
        fn CVPixelBufferGetHeightOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
        fn CVPixelBufferGetIOSurface(pb: CVPixelBufferRef) -> *mut c_void;
    }

    /// `pb`'s IOSurface when it is an NV12 one (VideoToolbox's output).
    ///
    /// # Safety
    /// `pb` must be a valid `CVPixelBufferRef` that stays alive for the call.
    pub(super) unsafe fn native_nv12(pb: CVPixelBufferRef, w: u32, h: u32) -> Option<super::NativeNv12> {
        // SAFETY: `pb` is valid per the contract; these are plain getters.
        unsafe {
            let fmt = CVPixelBufferGetPixelFormatType(pb);
            if (fmt != NV12_VIDEO && fmt != NV12_FULL) || CVPixelBufferGetPlaneCount(pb) != 2 {
                return None;
            }
            let io_surface = CVPixelBufferGetIOSurface(pb);
            (!io_surface.is_null()).then_some(super::NativeNv12 { io_surface, width: w, height: h, full_range: fmt == NV12_FULL })
        }
    }

    /// `pb`'s pixel format, size and whether it is IOSurface-backed, for logs.
    ///
    /// # Safety
    /// `pb` must be a valid `CVPixelBufferRef` that stays alive for the call.
    pub(super) unsafe fn describe(pb: CVPixelBufferRef, w: u32, h: u32) -> String {
        // SAFETY: plain getters on a valid buffer.
        unsafe {
            let fmt = CVPixelBufferGetPixelFormatType(pb);
            let planes = CVPixelBufferGetPlaneCount(pb);
            let surface = if CVPixelBufferGetIOSurface(pb).is_null() { "no iosurface" } else { "iosurface" };
            format!("cvpixelbuffer {} {planes} planes {surface} {w}x{h}", fourcc(fmt))
        }
    }

    fn fourcc(fmt: u32) -> String {
        let b = fmt.to_be_bytes();
        if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
            String::from_utf8_lossy(&b).into_owned()
        } else {
            format!("0x{fmt:08x}")
        }
    }

    /// Lock `pb` read-only and lend its two NV12 planes to `f`. `None` (and `f` not called) when
    /// it is not NV12 or cannot be locked.
    ///
    /// # Safety
    /// `pb` must be a valid `CVPixelBufferRef` that stays alive for the call.
    pub(super) unsafe fn with_nv12<R>(pb: CVPixelBufferRef, w: u32, h: u32, f: impl FnOnce(&PlaneView<'_>) -> R) -> Option<R> {
        // SAFETY: (all calls) `pb` is valid per the contract; the plane pointers are only read
        // while the base address is locked, within the lengths CoreVideo reports.
        unsafe {
            let fmt = CVPixelBufferGetPixelFormatType(pb);
            if (fmt != NV12_VIDEO && fmt != NV12_FULL) || CVPixelBufferGetPlaneCount(pb) != 2 {
                log_format(fmt);
                return None;
            }
            if CVPixelBufferLockBaseAddress(pb, LOCK_READ_ONLY) != 0 {
                return None;
            }
            let plane = |i: usize| {
                let base = CVPixelBufferGetBaseAddressOfPlane(pb, i) as *const u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, i);
                let rows = CVPixelBufferGetHeightOfPlane(pb, i);
                (base, stride, rows)
            };
            let (y, sy, ry) = plane(0);
            let (uv, suv, ruv) = plane(1);
            let ok = !y.is_null() && !uv.is_null() && ry >= h as usize && ruv >= h.div_ceil(2) as usize && sy >= w as usize && suv >= w.div_ceil(2) as usize * 2;
            let out = ok.then(|| {
                let view = PlaneView {
                    width: w,
                    height: h,
                    full_range: fmt == NV12_FULL,
                    planes: Planes::Nv12 { y: std::slice::from_raw_parts(y, sy * ry), uv: std::slice::from_raw_parts(uv, suv * ruv), strides: (sy as u32, suv as u32) },
                    source: "cvpixelbuffer",
                };
                f(&view)
            });
            CVPixelBufferUnlockBaseAddress(pb, LOCK_READ_ONLY);
            out
        }
    }

    fn log_format(fmt: u32) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let code = fourcc(fmt);
        ONCE.call_once(|| tracing::warn!(format = code, "viewer: decoded CVPixelBuffer is not NV12; converting on the CPU"));
    }
}
