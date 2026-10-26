//! Cameras on macOS: AVFoundation, plus the synthetic test camera.
//!
//! An `AVCaptureSession` opens the chosen native mode and an `AVCaptureVideoDataOutput` scales it
//! to the wanted size as NV12 (`420v`) IOSurface-backed pixel buffers, delivered on a serial
//! dispatch queue straight to the callback (zero copy up to libwebrtc's VideoToolbox encoder).
//! The self-view is a small CPU copy of each frame (mirrored when asked). "Mirror for the others"
//! and quarter turns are done by the capture connection where the camera supports them; the
//! digital zoom and the driver's image controls (UVC) are not available here, so no controls are
//! listed and the settings show none.
//!
//! The synthetic camera renders a moving test card into a pool of IOSurface-backed buffers on its
//! own thread, at the mode's rate, so it goes through exactly the same delivery path.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceFormat, AVCaptureDeviceInput,
    AVCaptureDeviceWasDisconnectedNotification, AVCaptureOutput, AVCaptureSession, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSDictionary, NSNotification, NSNotificationCenter, NSNumber, NSString};

use super::{deliver_small, ffi, lock, with_nv12, SendBox};
use crate::pixels::{choose_mode, render_test_card};
use crate::{
    camera_geometry, sort_camera_modes, CameraAdjust, CameraControl, CameraControlId, CameraError, CameraFault, CameraFormat,
    CameraInfo, CameraMode, CameraStats, CameraSubtype, CameraTransform, EncodedCallback, EncodedStats,
    EncoderBackend, EncoderControl, FrameCallback, LowLightDetector, Permission, Result, SYNTHETIC_CAMERA_ID,
};

/// How long a start waits for the first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(6);

fn err(fault: CameraFault, detail: impl Into<String>) -> CameraError {
    CameraError { fault, detail: detail.into() }
}

fn fake_camera_env() -> bool {
    std::env::var("VOX_FAKE_CAMERA").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn video_type() -> &'static NSString {
    // SAFETY: a framework constant, present on every supported macOS.
    unsafe { AVMediaTypeVideo }.expect("AVMediaTypeVideo")
}

// ---------------------------------------------------------------------------------------------
// Permission
// ---------------------------------------------------------------------------------------------

pub(super) fn permission() -> Permission {
    // SAFETY: class method without side effects.
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(video_type()) } {
        AVAuthorizationStatus::Authorized => Permission::Granted,
        AVAuthorizationStatus::NotDetermined => Permission::NotDetermined,
        _ => Permission::Denied,
    }
}

pub(super) fn request_permission(timeout: Duration) -> bool {
    match permission() {
        Permission::Granted => return true,
        Permission::Denied => return false,
        Permission::NotDetermined => {}
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(move |granted: Bool| {
        let _ = tx.try_send(granted.as_bool());
    });
    // SAFETY: AVFoundation copies the block and calls it once.
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(video_type(), &block) };
    let ok = rx.recv_timeout(timeout).unwrap_or(false);
    tracing::info!(granted = ok, "camera permission asked");
    ok
}

// ---------------------------------------------------------------------------------------------
// Devices and modes
// ---------------------------------------------------------------------------------------------

fn synthetic_info() -> CameraInfo {
    CameraInfo { id: SYNTHETIC_CAMERA_ID.into(), name: "Câmera de teste (sintética)".into() }
}

/// The modes the synthetic camera pretends to have (the same as on Windows).
fn synthetic_modes() -> Vec<CameraMode> {
    let m = |width: u32, height: u32, fps: u32, subtype| CameraMode { width, height, fps_milli: fps * 1000, subtype };
    let mut v = vec![
        m(1920, 1080, 15, CameraSubtype::Nv12),
        m(1280, 720, 30, CameraSubtype::Nv12),
        m(1280, 720, 15, CameraSubtype::Yuy2),
        m(960, 540, 30, CameraSubtype::Mjpg),
        m(640, 360, 30, CameraSubtype::Nv12),
    ];
    sort_camera_modes(&mut v);
    v
}

fn is_synthetic(device: &str) -> bool {
    device == SYNTHETIC_CAMERA_ID || (device.is_empty() && fake_camera_env())
}

fn all_devices() -> Vec<Retained<AVCaptureDevice>> {
    // SAFETY: class method; deprecated in favour of discovery sessions, but it lists every video
    // device (built-in, USB, Continuity) on every supported macOS without newer symbols.
    #[allow(deprecated)]
    unsafe {
        AVCaptureDevice::devicesWithMediaType(video_type()).to_vec()
    }
}

/// A camera by unique id, by name (case-insensitive substring), or the first one.
fn find_device(device: &str) -> std::result::Result<Retained<AVCaptureDevice>, CameraError> {
    let all = all_devices();
    let found = if device.is_empty() {
        all.into_iter().next()
    } else {
        let want = device.to_lowercase();
        let mut by_name = None;
        let mut by_id = None;
        for d in all {
            // SAFETY: getters.
            let (id, name) = unsafe { (d.uniqueID().to_string(), d.localizedName().to_string()) };
            if id == device {
                by_id = Some(d);
                break;
            }
            if by_name.is_none() && name.to_lowercase().contains(&want) {
                by_name = Some(d);
            }
        }
        by_id.or(by_name)
    };
    found.ok_or_else(|| err(CameraFault::NotFound, format!("no camera matching {device:?}")))
}

fn subtype_of(fourcc: u32) -> CameraSubtype {
    match &fourcc.to_be_bytes() {
        b"420v" | b"420f" => CameraSubtype::Nv12,
        b"yuvs" | b"2vuy" | b"yuv2" => CameraSubtype::Yuy2,
        _ => CameraSubtype::Mjpg,
    }
}

/// One native mode and what opens it: the device format and the lowest rate of its range.
struct NativeMode {
    mode: CameraMode,
    format: Retained<AVCaptureDeviceFormat>,
    min_fps_milli: u32,
}

impl NativeMode {
    /// The frame duration for `fps`, inside this mode's range (AVFoundation throws on any other).
    fn frame_duration(&self, fps: u32) -> CMTime {
        let milli = (fps.max(1) * 1000).clamp(self.min_fps_milli.max(1), self.mode.fps_milli.max(1));
        CMTime { value: 1000, timescale: milli as i32, flags: CMTimeFlags::Valid, epoch: 0 }
    }
}

/// Every native mode a device offers: one per format and frame-rate range (at the range's top
/// rate).
fn device_modes(d: &AVCaptureDevice) -> Vec<NativeMode> {
    let mut out = Vec::new();
    // SAFETY: getters of the device's format list.
    unsafe {
        for f in d.formats().iter() {
            let desc = f.formatDescription();
            let desc = &*desc as *const _ as ffi::CMFormatDescriptionRef;
            let dims = ffi::CMVideoFormatDescriptionGetDimensions(desc);
            let subtype = subtype_of(ffi::CMFormatDescriptionGetMediaSubType(desc));
            if dims.width <= 0 || dims.height <= 0 {
                continue;
            }
            for r in f.videoSupportedFrameRateRanges().iter() {
                let fps_milli = (r.maxFrameRate() * 1000.0).round() as u32;
                if fps_milli == 0 {
                    continue;
                }
                let min_fps_milli = ((r.minFrameRate() * 1000.0).round() as u32).min(fps_milli);
                let mode = CameraMode { width: dims.width as u32, height: dims.height as u32, fps_milli, subtype };
                if !out.iter().any(|n: &NativeMode| n.mode == mode) {
                    out.push(NativeMode { mode, format: f.clone(), min_fps_milli });
                }
            }
        }
    }
    out
}

pub fn list_cameras() -> Result<Vec<CameraInfo>> {
    objc2::rc::autoreleasepool(|_| {
        let mut out = Vec::new();
        if fake_camera_env() {
            out.push(synthetic_info());
        }
        for d in all_devices() {
            // SAFETY: getters.
            let (id, name) = unsafe { (d.uniqueID().to_string(), d.localizedName().to_string()) };
            out.push(CameraInfo { id, name });
        }
        Ok(out)
    })
}

pub fn list_camera_formats(device: &str) -> std::result::Result<Vec<CameraMode>, CameraError> {
    if is_synthetic(device) {
        return Ok(synthetic_modes());
    }
    objc2::rc::autoreleasepool(|_| {
        let d = find_device(device)?;
        let mut v: Vec<CameraMode> = device_modes(&d).into_iter().map(|n| n.mode).collect();
        sort_camera_modes(&mut v);
        Ok(v)
    })
}

/// macOS has no driver settings dialog.
pub fn camera_property_pages(_: &str, _: Option<&str>) -> std::result::Result<u32, CameraError> {
    Err(err(CameraFault::Unsupported, "macOS cameras have no driver settings dialog"))
}

pub fn camera_diagnostics(device: &str) -> std::result::Result<String, CameraError> {
    use std::fmt::Write;
    if is_synthetic(device) {
        let mut s = "camera: synthetic\n".to_string();
        for m in synthetic_modes() {
            let _ = writeln!(s, "  {}", m.label());
        }
        return Ok(s);
    }
    objc2::rc::autoreleasepool(|_| {
        let d = find_device(device)?;
        // SAFETY: getters.
        let name = unsafe { d.localizedName().to_string() };
        let mut s = format!("camera: {name} (AVFoundation)\nauthorization: {:?}\n", permission());
        for n in device_modes(&d) {
            let _ = writeln!(s, "  {} (from {} fps)", n.mode.label(), crate::fps_text(n.min_fps_milli as f32 / 1000.0));
        }
        s.push_str("image controls: none (macOS does not expose UVC controls to apps)\n");
        Ok(s)
    })
}

// ---------------------------------------------------------------------------------------------
// Shared state and the frame sink
// ---------------------------------------------------------------------------------------------

/// What every camera keeps for the handle (read from any thread).
struct Shared {
    running: AtomicBool,
    failure: Mutex<Option<CameraError>>,
    captured: AtomicU64,
    delivered: AtomicU64,
    preview_frames: AtomicU64,
    preview_on: AtomicBool,
    transform: Mutex<CameraTransform>,
    /// Measured rate (frames per second times 1000).
    fps_milli: AtomicU32,
    low_light: AtomicBool,
    /// The synthetic camera's fake controls (real cameras list none).
    controls: Mutex<Vec<CameraControl>>,
    first: (Mutex<bool>, Condvar),
}

impl Shared {
    fn new(transform: CameraTransform, controls: Vec<CameraControl>) -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(true),
            failure: Mutex::new(None),
            captured: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            preview_frames: AtomicU64::new(0),
            preview_on: AtomicBool::new(true),
            transform: Mutex::new(transform),
            fps_milli: AtomicU32::new(0),
            low_light: AtomicBool::new(false),
            controls: Mutex::new(controls),
            first: (Mutex::new(false), Condvar::new()),
        })
    }

    fn fail(&self, e: CameraError) {
        tracing::warn!(error = %e, "camera stopped");
        lock(&self.failure).get_or_insert(e);
        self.running.store(false, Ordering::Relaxed);
        self.signal_first();
    }

    fn signal_first(&self) {
        *lock(&self.first.0) = true;
        self.first.1.notify_all();
    }

    fn wait_first(&self, timeout: Duration) -> bool {
        let g = lock(&self.first.0);
        let (g, _) = self.first.1.wait_timeout_while(g, timeout, |seen| !*seen).unwrap_or_else(|p| p.into_inner());
        *g && self.running.load(Ordering::Relaxed)
    }

    fn exposure_auto(&self) -> bool {
        lock(&self.controls).iter().find(|c| c.id == CameraControlId::Exposure).is_some_and(|c| c.auto)
    }
}

/// Where the frames go: the main callback (raw path) and the self-view.
struct Sink {
    shared: Arc<Shared>,
    main: Option<FrameCallback>,
    preview: Option<FrameCallback>,
    preview_max: (u32, u32),
    /// The capture connection mirrors the picture itself (mirror for the others).
    source_mirrored: Arc<AtomicBool>,
    buf: Vec<u8>,
    meter: (Instant, u32),
    requested_fps: f32,
    low: LowLightDetector,
}

impl Sink {
    fn frame(&mut self, f: &crate::Nv12Frame<'_>) {
        let sh = &self.shared;
        sh.captured.fetch_add(1, Ordering::Relaxed);
        if let Some(cb) = self.main.as_mut() {
            cb(f);
            sh.delivered.fetch_add(1, Ordering::Relaxed);
        }
        if sh.preview_on.load(Ordering::Relaxed) {
            if let Some(cb) = self.preview.as_mut() {
                let want = lock(&sh.transform).mirror_preview;
                let mirror = want != self.source_mirrored.load(Ordering::Relaxed);
                deliver_small(f, self.preview_max.0, self.preview_max.1, mirror, &mut self.buf, cb);
                sh.preview_frames.fetch_add(1, Ordering::Relaxed);
            }
        } else if !self.buf.is_empty() {
            self.buf = Vec::new();
        }
        // Rate over about a second, and the low-light hint.
        self.meter.1 += 1;
        let dt = self.meter.0.elapsed().as_secs_f32();
        if dt >= 1.0 {
            let fps = self.meter.1 as f32 / dt;
            sh.fps_milli.store((fps * 1000.0) as u32, Ordering::Relaxed);
            let low = self.low.update(dt, fps, self.requested_fps, sh.exposure_auto());
            sh.low_light.store(low, Ordering::Relaxed);
            self.meter = (Instant::now(), 0);
        }
        if sh.captured.load(Ordering::Relaxed) == 1 {
            sh.signal_first();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The handle
// ---------------------------------------------------------------------------------------------

pub enum CameraOutput {
    /// Refused on macOS (no encoder in this crate).
    #[allow(dead_code)]
    Encoded(EncodedCallback),
    Raw(FrameCallback),
    None,
}

/// Held for its `Drop` (the session or the synthetic thread stops with it).
#[allow(dead_code)]
enum Source {
    Device(SendBox<DeviceCapture>),
    Synthetic(SyntheticCapture),
}

pub struct CameraInner {
    shared: Arc<Shared>,
    format: CameraFormat,
    modes: Vec<CameraMode>,
    control: EncoderControl,
    transform_ok: bool,
    source: Option<Source>,
}

impl CameraInner {
    pub fn control(&self) -> EncoderControl {
        self.control.clone()
    }
    /// Never a vox-capture encoder on macOS (VideoToolbox runs in libwebrtc).
    pub fn backend(&self) -> Option<EncoderBackend> {
        None
    }
    pub fn format(&self) -> CameraFormat {
        self.format.clone()
    }
    pub fn stats(&self) -> CameraStats {
        let sh = &self.shared;
        CameraStats {
            frames_captured: sh.captured.load(Ordering::Relaxed),
            preview_frames: sh.preview_frames.load(Ordering::Relaxed),
            encoded: EncodedStats { frames_encoded: sh.delivered.load(Ordering::Relaxed), ..Default::default() },
            fps: sh.fps_milli.load(Ordering::Relaxed) as f32 / 1000.0,
            low_light: sh.low_light.load(Ordering::Relaxed),
        }
    }
    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }
    pub fn failure(&self) -> Option<CameraError> {
        lock(&self.shared.failure).clone()
    }
    pub fn set_preview(&self, on: bool) {
        self.shared.preview_on.store(on, Ordering::Relaxed);
    }
    pub fn controls(&self) -> Vec<CameraControl> {
        lock(&self.shared.controls).clone()
    }
    pub fn set_controls(&self, adjust: &[CameraAdjust]) {
        let mut list = lock(&self.shared.controls);
        for a in adjust {
            if let Some(c) = list.iter_mut().find(|c| c.id == a.id) {
                if a.auto && c.can_auto {
                    c.auto = true;
                } else if !a.auto && c.can_manual {
                    c.auto = false;
                    c.value = c.snap(a.value);
                }
            }
        }
    }
    pub fn reset_controls(&self, id: Option<CameraControlId>) {
        for c in lock(&self.shared.controls).iter_mut().filter(|c| id.is_none_or(|i| i == c.id)) {
            let (v, auto) = c.default_setting();
            c.value = v;
            c.auto = auto;
        }
    }
    pub fn refresh_controls(&self) {}
    pub fn native_modes(&self) -> Vec<CameraMode> {
        self.modes.clone()
    }
    pub fn set_transform(&self, t: CameraTransform) {
        let old = std::mem::replace(&mut *lock(&self.shared.transform), t);
        if old != t {
            if let Some(Source::Device(d)) = &self.source {
                d.0.apply_transform(&t);
            }
        }
    }
    pub fn transform(&self) -> CameraTransform {
        *lock(&self.shared.transform)
    }
    pub fn transform_supported(&self) -> bool {
        self.transform_ok
    }
}

impl Drop for CameraInner {
    fn drop(&mut self) {
        // Stops the session (the camera light goes off) or joins the synthetic thread.
        self.source.take();
        self.shared.running.store(false, Ordering::Relaxed);
    }
}

pub fn start_camera(device: &str, cfg: crate::CameraConfig, output: CameraOutput, on_preview: Option<FrameCallback>) -> std::result::Result<CameraInner, CameraError> {
    let main = match output {
        CameraOutput::Encoded(_) => return Err(err(CameraFault::Unsupported, "macOS encodes the camera in libwebrtc (VideoToolbox): use the raw path")),
        CameraOutput::Raw(cb) => Some(cb),
        CameraOutput::None => None,
    };
    let t = cfg.transform.sanitized();
    if is_synthetic(device) {
        return start_synthetic(cfg, t, main, on_preview);
    }
    objc2::rc::autoreleasepool(|_| start_device(device, cfg, t, main, on_preview))
}

// ---------------------------------------------------------------------------------------------
// Real cameras
// ---------------------------------------------------------------------------------------------

struct DelegateIvars {
    sink: Mutex<Option<Sink>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "ConcordCaptureCameraDelegate"]
    #[ivars = DelegateIvars]
    struct CameraDelegate;

    unsafe impl NSObjectProtocol for CameraDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: &AVCaptureOutput, sample: &CMSampleBuffer, _connection: &AVCaptureConnection) {
            let mut g = lock(&self.ivars().sink);
            let Some(sink) = g.as_mut() else { return };
            let sb = sample as *const CMSampleBuffer as ffi::CMSampleBufferRef;
            // SAFETY: AVFoundation hands a valid sample buffer for the callback's duration.
            unsafe {
                let pb = ffi::CMSampleBufferGetImageBuffer(sb);
                if !pb.is_null() {
                    let ts = super::stream::sample_time_us(sb);
                    with_nv12(pb, ts, |f| sink.frame(f));
                }
            }
        }
    }
);

impl CameraDelegate {
    fn new(sink: Sink) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { sink: Mutex::new(Some(sink)) });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

struct DeviceCapture {
    session: Retained<AVCaptureSession>,
    output: Retained<AVCaptureVideoDataOutput>,
    delegate: Retained<CameraDelegate>,
    observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    mirrored: Arc<AtomicBool>,
    _queue: DispatchRetained<DispatchQueue>,
}

impl DeviceCapture {
    fn connection(&self) -> Option<Retained<AVCaptureConnection>> {
        // SAFETY: getter of a configured output.
        unsafe { self.output.connectionWithMediaType(video_type()) }
    }

    /// Mirror for the others and quarter turns, where the connection can do them. Returns whether
    /// it can (mirroring at least).
    fn apply_transform(&self, t: &CameraTransform) -> bool {
        let Some(c) = self.connection() else { return false };
        // SAFETY: connection setters, guarded by their "supported" getters.
        unsafe {
            let mirror_ok = c.isVideoMirroringSupported();
            if mirror_ok {
                c.setAutomaticallyAdjustsVideoMirroring(false);
                c.setVideoMirrored(t.mirror_others);
                self.mirrored.store(t.mirror_others, Ordering::Relaxed);
            }
            if c.respondsToSelector(sel!(setVideoRotationAngle:)) {
                let angle = t.rotation.degrees() as f64;
                if c.isVideoRotationAngleSupported(angle) {
                    c.setVideoRotationAngle(angle);
                }
            }
            mirror_ok
        }
    }
}

impl Drop for DeviceCapture {
    fn drop(&mut self) {
        // SAFETY: stopping a session we own; stopRunning blocks until the device is released.
        unsafe {
            if let Some(o) = self.observer.take() {
                NSNotificationCenter::defaultCenter().removeObserver(AsRef::<AnyObject>::as_ref(&*o));
            }
            self.session.stopRunning();
            self.output.setSampleBufferDelegate_queue(None, None);
        }
        lock(&self.delegate.ivars().sink).take();
    }
}

fn start_device(device: &str, cfg: crate::CameraConfig, t: CameraTransform, main: Option<FrameCallback>, on_preview: Option<FrameCallback>) -> std::result::Result<CameraInner, CameraError> {
    let d = find_device(device)?;
    match permission() {
        Permission::Granted => {}
        Permission::Denied => return Err(err(CameraFault::AccessDenied, "camera access denied in System Settings > Privacy & Security > Camera")),
        Permission::NotDetermined => {
            if !request_permission(Duration::from_secs(60)) {
                return Err(err(CameraFault::AccessDenied, "camera access not granted"));
            }
        }
    }
    // SAFETY: getters.
    let name = unsafe { d.localizedName().to_string() };
    let modes = device_modes(&d);
    let mut listed: Vec<CameraMode> = modes.iter().map(|n| n.mode).collect();
    sort_camera_modes(&mut listed);
    let chosen = choose_mode(&listed, cfg.width, cfg.height, cfg.fps, cfg.mode).ok_or_else(|| err(CameraFault::Unsupported, "the camera lists no video format"))?;
    let native = modes.iter().find(|n| n.mode == chosen);
    let fps = (chosen.fps().round() as u32).clamp(1, cfg.fps.max(1)).max(1);
    let geo = camera_geometry(chosen.width, chosen.height, &CameraTransform { zoom_pct: 100, ..t }, cfg.width, cfg.height);
    // The output is scaled before the connection turns it.
    let (ow, oh) = if t.rotation.swaps_axes() { (geo.out.1, geo.out.0) } else { geo.out };

    // SAFETY: AVFoundation setup on objects we own; errors are returned, not thrown.
    unsafe {
        let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&d).map_err(|e| {
            let code = e.code();
            // AVErrorDeviceAlreadyUsedByAnotherSession / AVErrorDeviceInUseByAnotherApplication.
            let fault = if code == -11804 || code == -11853 { CameraFault::InUse } else { CameraFault::Other };
            err(fault, format!("open {name}: {} ({code})", e.localizedDescription()))
        })?;
        let session = AVCaptureSession::new();
        session.beginConfiguration();
        if !session.canAddInput(&input) {
            session.commitConfiguration();
            return Err(err(CameraFault::InUse, "the capture session cannot take this camera"));
        }
        session.addInput(&input);
        let output = AVCaptureVideoDataOutput::new();
        let keys: [&NSString; 3] = [
            &*(ffi::kCVPixelBufferPixelFormatTypeKey as *const NSString),
            &*(ffi::kCVPixelBufferWidthKey as *const NSString),
            &*(ffi::kCVPixelBufferHeightKey as *const NSString),
        ];
        let values = [NSNumber::new_u32(ffi::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange), NSNumber::new_u32(ow), NSNumber::new_u32(oh)];
        let values: Vec<&AnyObject> = values.iter().map(|v| &***v as &AnyObject).collect();
        let settings = NSDictionary::from_slices(&keys, &values);
        output.setVideoSettings(Some(&settings));
        output.setAlwaysDiscardsLateVideoFrames(true);
        if !session.canAddOutput(&output) {
            session.commitConfiguration();
            return Err(err(CameraFault::Unsupported, "the capture session cannot deliver NV12 frames"));
        }
        session.addOutput(&output);
        if let Some(n) = native {
            match d.lockForConfiguration() {
                Ok(()) => {
                    d.setActiveFormat(&n.format);
                    d.setActiveVideoMinFrameDuration(n.frame_duration(cfg.fps));
                    d.unlockForConfiguration();
                }
                Err(e) => tracing::warn!(error = %e.localizedDescription(), "camera: cannot pick its mode; using its default"),
            }
        }
        session.commitConfiguration();

        let shared = Shared::new(t, Vec::new());
        let mirrored = Arc::new(AtomicBool::new(false));
        let sink = Sink {
            shared: shared.clone(),
            main,
            preview: on_preview,
            preview_max: (cfg.preview_max_width, cfg.preview_max_height),
            source_mirrored: mirrored.clone(),
            buf: Vec::new(),
            meter: (Instant::now(), 0),
            requested_fps: fps as f32,
            low: LowLightDetector::new(),
        };
        let delegate = CameraDelegate::new(sink);
        let queue = DispatchQueue::new("concord.capture.camera", None);
        output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));

        // Unplugged while running.
        let sh = shared.clone();
        let block = RcBlock::new(move |_n: std::ptr::NonNull<NSNotification>| {
            sh.fail(err(CameraFault::Disconnected, "the camera was disconnected"));
        });
        let observer = NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(AVCaptureDeviceWasDisconnectedNotification),
            Some(&*d as &AnyObject),
            None,
            &block,
        );

        let cap = DeviceCapture { session, output, delegate, observer: Some(observer), mirrored, _queue: queue };
        let transform_ok = cap.apply_transform(&t);
        cap.session.startRunning();
        if !shared.wait_first(FIRST_FRAME_TIMEOUT) {
            let e = lock(&shared.failure).clone().unwrap_or_else(|| err(CameraFault::NoFrames, format!("{name}: no frame in {FIRST_FRAME_TIMEOUT:?}")));
            drop(cap);
            return Err(e);
        }
        let format = CameraFormat {
            device: name,
            native_width: chosen.width,
            native_height: chosen.height,
            native_fps: chosen.fps(),
            subtype: chosen.subtype.name().into(),
            gpu_frames: true,
            width: geo.out.0,
            height: geo.out.1,
        };
        tracing::info!(mode = %chosen.label(), out = format!("{}x{}", geo.out.0, geo.out.1), fps, transform_ok, "camera started (AVFoundation, NV12)");
        Ok(CameraInner { shared, format, modes: listed, control: EncoderControl::default(), transform_ok, source: Some(Source::Device(SendBox(cap))) })
    }
}

// ---------------------------------------------------------------------------------------------
// Synthetic camera
// ---------------------------------------------------------------------------------------------

/// The synthetic camera's fake image controls (the same as on Windows), so the settings and the
/// low-light hint can be exercised.
fn fake_controls() -> Vec<CameraControl> {
    let mk = |id, min, max, step, default, can_auto: bool| CameraControl { id, min, max, step, default, value: default, auto: can_auto, can_auto, can_manual: true };
    use CameraControlId::*;
    vec![
        mk(Brightness, 0, 255, 1, 128, false),
        mk(Contrast, 0, 255, 1, 128, false),
        mk(Saturation, 0, 255, 1, 128, false),
        mk(Sharpness, 0, 255, 1, 128, false),
        mk(Gamma, 72, 500, 1, 100, false),
        mk(WhiteBalance, 2000, 6500, 10, 4000, true),
        mk(Backlight, 0, 1, 1, 0, false),
        mk(Exposure, -11, -2, 1, -6, true),
        mk(Gain, 0, 255, 1, 0, false),
        mk(Focus, 0, 250, 5, 0, true),
        mk(Zoom, 100, 500, 1, 100, false),
        mk(ExposurePriority, 0, 1, 1, 1, false),
    ]
}

struct SyntheticCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for SyntheticCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A pool of IOSurface-backed NV12 buffers of one size.
struct Pool(ffi::CVPixelBufferPoolRef);

// SAFETY: CVPixelBufferPool is thread-safe; used by one thread anyway.
unsafe impl Send for Pool {}

impl Pool {
    fn new(w: u32, h: u32) -> Option<Self> {
        // SAFETY: CoreVideo keys are CFStrings (toll-free bridged with NSString); the dictionary
        // outlives the call.
        unsafe {
            let keys: [&NSString; 4] = [
                &*(ffi::kCVPixelBufferPixelFormatTypeKey as *const NSString),
                &*(ffi::kCVPixelBufferWidthKey as *const NSString),
                &*(ffi::kCVPixelBufferHeightKey as *const NSString),
                &*(ffi::kCVPixelBufferIOSurfacePropertiesKey as *const NSString),
            ];
            let empty: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
            let nums = [NSNumber::new_u32(ffi::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange), NSNumber::new_u32(w), NSNumber::new_u32(h)];
            let values: [&AnyObject; 4] = [&nums[0], &nums[1], &nums[2], &empty];
            let attrs = NSDictionary::from_slices(&keys, &values);
            let mut pool: ffi::CVPixelBufferPoolRef = std::ptr::null_mut();
            let st = ffi::CVPixelBufferPoolCreate(std::ptr::null(), std::ptr::null(), &*attrs as *const _ as ffi::CFDictionaryRef, &mut pool);
            (st == 0 && !pool.is_null()).then_some(Pool(pool))
        }
    }

    /// A buffer with the test card `index` drawn in it (caller releases it).
    fn render(&self, index: u64) -> Option<ffi::CVPixelBufferRef> {
        // SAFETY: a fresh buffer from our pool, locked while it is written.
        unsafe {
            let mut pb: ffi::CVPixelBufferRef = std::ptr::null_mut();
            if ffi::CVPixelBufferPoolCreatePixelBuffer(std::ptr::null(), self.0, &mut pb) != 0 || pb.is_null() {
                return None;
            }
            if ffi::CVPixelBufferLockBaseAddress(pb, 0) == 0 {
                let (w, h) = (ffi::CVPixelBufferGetWidth(pb), ffi::CVPixelBufferGetHeight(pb));
                let (ys, uvs) = (ffi::CVPixelBufferGetBytesPerRowOfPlane(pb, 0), ffi::CVPixelBufferGetBytesPerRowOfPlane(pb, 1));
                let (yp, uvp) = (ffi::CVPixelBufferGetBaseAddressOfPlane(pb, 0), ffi::CVPixelBufferGetBaseAddressOfPlane(pb, 1));
                if !yp.is_null() && !uvp.is_null() {
                    let y = std::slice::from_raw_parts_mut(yp, ys * h);
                    let uv = std::slice::from_raw_parts_mut(uvp, uvs * h.div_ceil(2));
                    render_test_card(index, w, h, y, ys, uv, uvs);
                }
                ffi::CVPixelBufferUnlockBaseAddress(pb, 0);
            }
            Some(pb)
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: our own reference.
        unsafe { ffi::CVPixelBufferPoolRelease(self.0) };
    }
}

fn start_synthetic(cfg: crate::CameraConfig, t: CameraTransform, main: Option<FrameCallback>, on_preview: Option<FrameCallback>) -> std::result::Result<CameraInner, CameraError> {
    let modes = synthetic_modes();
    let chosen = choose_mode(&modes, cfg.width, cfg.height, cfg.fps, cfg.mode).unwrap_or(modes[0]);
    let geo = camera_geometry(chosen.width, chosen.height, &CameraTransform { rotation: crate::CameraRotation::None, zoom_pct: 100, ..t }, cfg.width, cfg.height);
    let (w, h) = geo.out;
    let pool = Pool::new(w, h).ok_or_else(|| err(CameraFault::Other, "CVPixelBufferPoolCreate failed"))?;
    let shared = Shared::new(t, fake_controls());
    let fps = chosen.fps().max(1.0);
    let mut sink = Sink {
        shared: shared.clone(),
        main,
        preview: on_preview,
        preview_max: (cfg.preview_max_width, cfg.preview_max_height),
        source_mirrored: Arc::new(AtomicBool::new(false)),
        buf: Vec::new(),
        meter: (Instant::now(), 0),
        requested_fps: fps,
        low: LowLightDetector::new(),
    };
    let lowlight_env = std::env::var("VOX_FAKE_CAMERA_LOWLIGHT").is_ok_and(|v| !v.is_empty() && v != "0");
    let stop = Arc::new(AtomicBool::new(false));
    let st = stop.clone();
    let sh = shared.clone();
    let thread = std::thread::Builder::new()
        .name("vox-synthetic-cam".into())
        .spawn(move || {
            let period = Duration::from_secs_f32(1.0 / fps);
            let mut next = Instant::now();
            let mut index = 0u64;
            let mut slow = false;
            while !st.load(Ordering::Relaxed) {
                // Dim-room simulation: with automatic exposure on, a third of the frames.
                let want_slow = lowlight_env && sh.exposure_auto();
                if want_slow != slow {
                    slow = want_slow;
                    tracing::info!(slow, "synthetic camera rate");
                }
                if !slow || index % 3 == 0 {
                    if let Some(pb) = pool.render(index) {
                        // SAFETY: our own buffer, released after delivery (a consumer that kept it
                        // retained its own reference).
                        unsafe {
                            with_nv12(pb, ffi::host_time_us(), |f| sink.frame(f));
                            ffi::CVPixelBufferRelease(pb);
                        }
                    }
                }
                index += 1;
                next += period;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    next = now;
                }
            }
        })
        .map_err(|e| err(CameraFault::Other, format!("spawn: {e}")))?;
    let cap = SyntheticCapture { stop, thread: Some(thread) };
    if !shared.wait_first(FIRST_FRAME_TIMEOUT) {
        drop(cap);
        return Err(err(CameraFault::NoFrames, "synthetic camera produced no frame"));
    }
    let format = CameraFormat {
        device: "synthetic".into(),
        native_width: chosen.width,
        native_height: chosen.height,
        native_fps: chosen.fps(),
        subtype: "synthetic".into(),
        gpu_frames: true,
        width: w,
        height: h,
    };
    tracing::info!(mode = %chosen.label(), out = format!("{w}x{h}"), "synthetic camera started");
    Ok(CameraInner { shared, format, modes, control: EncoderControl::default(), transform_ok: false, source: Some(Source::Synthetic(cap)) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{start_camera_raw, CameraConfig};

    /// The synthetic camera goes through the real delivery path: IOSurface-backed NV12 buffers
    /// that `retained_pixel_buffer` hands out, the self-view (mirrored), the fake controls.
    #[test]
    fn synthetic_camera_delivers_pixel_buffers() {
        let seen = Arc::new(Mutex::new((0u32, 0u32, 0u32, (0u32, 0u32))));
        let s = seen.clone();
        let main: FrameCallback = Box::new(move |f| {
            let mut g = lock(&s);
            g.0 += 1;
            if let Some(pb) = crate::retained_pixel_buffer(f) {
                g.1 += 1;
                // SAFETY: the reference we were given.
                unsafe { ffi::CVPixelBufferRelease(pb.as_ptr()) };
            }
        });
        let p = seen.clone();
        let preview: FrameCallback = Box::new(move |f| {
            let mut g = lock(&p);
            g.2 += 1;
            g.3 = (f.width, f.height);
            assert!(crate::retained_pixel_buffer(f).is_none(), "a preview is our own copy");
        });
        let cfg = CameraConfig { width: 640, height: 360, fps: 30, preview_max_width: 320, preview_max_height: 180, ..Default::default() };
        let cap = start_camera_raw(SYNTHETIC_CAMERA_ID, cfg, main, Some(preview)).expect("synthetic camera");
        std::thread::sleep(Duration::from_millis(600));
        let st = cap.stats();
        let f = cap.format();
        assert_eq!((f.width, f.height), (640, 360));
        assert!(st.frames_captured >= 8, "{st:?}");
        let g = *lock(&seen);
        assert!(g.0 >= 8 && g.1 == g.0, "every frame has its pixel buffer: {g:?}");
        assert!(g.2 >= 8 && g.3 == (320, 180), "{g:?}");
        // Controls: set, then back to the defaults.
        cap.set_control(CameraControlId::Brightness, 300, false);
        assert_eq!(cap.controls().iter().find(|c| c.id == CameraControlId::Brightness).unwrap().value, 255);
        cap.reset_controls();
        assert_eq!(cap.controls().iter().find(|c| c.id == CameraControlId::Brightness).unwrap().value, 128);
        // The self-view stops when hidden.
        cap.set_preview(false);
        std::thread::sleep(Duration::from_millis(100));
        let before = lock(&seen).2;
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(lock(&seen).2, before);
        drop(cap);
    }

    #[test]
    fn encoded_output_is_refused() {
        let r = crate::start_camera_encoded(SYNTHETIC_CAMERA_ID, CameraConfig::default(), Box::new(|_| {}), None);
        assert_eq!(r.err().map(|e| e.fault), Some(CameraFault::Unsupported));
    }
}
