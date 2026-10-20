//! ScreenCaptureKit streams: the video of a screen or window, and the sound of an app.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::CFString;
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamFrameInfoContentRect, SCStreamFrameInfoScaleFactor, SCStreamFrameInfoStatus,
    SCStreamOutput, SCStreamOutputType,
};

use super::content::{self, ns_error, permission_error};
use super::{ffi, lock, os, with_nv12, PreviewAttach, SendBox};
use crate::pixels::AudioChunker;
use crate::{AudioCallback, AudioChunk, AudioTarget, FrameCallback, Result, VideoConfig, VideoStats, VideoTarget, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};

/// `SCFrameStatus.complete`: a new picture (the others are idle / blank / suspended markers).
const FRAME_COMPLETE: i64 = 0;
/// Buffers ScreenCaptureKit may have out at once. libwebrtc keeps a frame until it is encoded, so
/// a few more than the default 3 keep the stream from starving.
const QUEUE_DEPTH: isize = 6;
const START_TIMEOUT: Duration = Duration::from_secs(8);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

type SampleHandler = Box<dyn FnMut(ffi::CMSampleBufferRef) + Send>;

pub(crate) struct OutputIvars {
    video: Mutex<Option<SampleHandler>>,
    audio: Mutex<Option<SampleHandler>>,
    running: Arc<AtomicBool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "ConcordCaptureStreamOutput"]
    #[ivars = OutputIvars]
    pub(crate) struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            let slot = if kind == SCStreamOutputType::Screen {
                &self.ivars().video
            } else if kind == SCStreamOutputType::Audio {
                &self.ivars().audio
            } else {
                return;
            };
            if let Some(h) = lock(slot).as_mut() {
                h(sample as *const CMSampleBuffer as ffi::CMSampleBufferRef);
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let (text, denied) = ns_error(error);
            if denied {
                super::note_screen_permission(false);
            }
            tracing::warn!(error = %text, "screen capture stream stopped by the system");
            self.ivars().running.store(false, Ordering::Relaxed);
        }
    }
);

impl StreamOutput {
    fn new(video: Option<SampleHandler>, audio: Option<SampleHandler>, running: Arc<AtomicBool>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { video: Mutex::new(video), audio: Mutex::new(audio), running });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    /// No more callbacks into the client (whatever the stream still delivers is dropped).
    fn disarm(&self) {
        lock(&self.ivars().video).take();
        lock(&self.ivars().audio).take();
    }
}

/// Wait for a ScreenCaptureKit completion handler `fn(NSError?)`.
fn wait_completion(call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>), timeout: Duration) -> std::result::Result<(), (String, bool)> {
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(move |err: *mut NSError| {
        // SAFETY: the error is valid (or null) for the handler's duration.
        let r = match unsafe { err.as_ref() } {
            None => Ok(()),
            Some(e) => Err(ns_error(e)),
        };
        let _ = tx.try_send(r);
    });
    call(&block);
    rx.recv_timeout(timeout).unwrap_or_else(|_| Err(("no answer from ScreenCaptureKit".into(), false)))
}

/// A started stream. Dropping it stops the stream (waiting for ScreenCaptureKit) and disarms the
/// callbacks.
struct Running {
    stream: Retained<SCStream>,
    output: Retained<StreamOutput>,
    // Kept alive as long as the stream delivers on it.
    _queue: DispatchRetained<DispatchQueue>,
}

impl Running {
    fn start(filter: &SCContentFilter, cfg: &SCStreamConfiguration, output: Retained<StreamOutput>, audio: bool, label: &str) -> Result<Self> {
        let queue = DispatchQueue::new(label, None);
        // SAFETY: valid filter / configuration / delegate; the stream keeps the delegate weakly, we
        // keep it (`output`) as long as the stream.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), filter, cfg, Some(ProtocolObject::from_ref(&*output)))
        };
        let po = ProtocolObject::from_ref(&*output);
        // ScreenCaptureKit logs an error per dropped frame when a stream has no screen output, so
        // an audio stream gets one too (its handler is empty).
        let mut kinds = vec![SCStreamOutputType::Screen];
        if audio {
            kinds.push(SCStreamOutputType::Audio);
        }
        for kind in kinds {
            // SAFETY: the output object lives as long as the stream (see above).
            unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(po, kind, Some(&queue)) }.map_err(|e| os("addStreamOutput", ns_error(&e).0))?;
        }
        match wait_completion(|b| unsafe { stream.startCaptureWithCompletionHandler(Some(b)) }, START_TIMEOUT) {
            Ok(()) => {
                super::note_screen_permission(true);
                Ok(Self { stream, output, _queue: queue })
            }
            Err((_, true)) => {
                output.disarm();
                Err(permission_error("start capture"))
            }
            Err((text, false)) => {
                output.disarm();
                Err(os("SCStream start", text))
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let r = wait_completion(|b| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(b)) }, STOP_TIMEOUT);
        if let Err((e, _)) = r {
            tracing::debug!(error = %e, "stopping a capture stream");
        }
        self.output.disarm();
    }
}

fn frame_interval(fps: u32) -> CMTime {
    CMTime { value: 1, timescale: fps.clamp(1, 240) as i32, flags: CMTimeFlags::Valid, epoch: 0 }
}

/// The frame status attachment of a screen sample (`None`: not a ScreenCaptureKit frame).
///
/// # Safety
/// `sb` is a valid sample buffer.
unsafe fn frame_status(sb: ffi::CMSampleBufferRef) -> Option<i64> {
    ffi::cf_number_i64(frame_info(sb, SCStreamFrameInfoStatus))
}

/// One frame-info attachment of a screen sample (null when absent).
///
/// # Safety
/// `sb` is a valid sample buffer.
unsafe fn frame_info(sb: ffi::CMSampleBufferRef, key: &objc2_foundation::NSString) -> *const std::ffi::c_void {
    let arr = ffi::CMSampleBufferGetSampleAttachmentsArray(sb, 0);
    if arr.is_null() || ffi::CFArrayGetCount(arr) < 1 {
        return std::ptr::null();
    }
    let dict = ffi::CFArrayGetValueAtIndex(arr, 0);
    ffi::CFDictionaryGetValue(dict, key as *const _ as *const _)
}

/// A shared window's size in pixels as this frame shows it (content rectangle in points times the
/// display's scale), when the frame says.
///
/// # Safety
/// `sb` is a valid sample buffer.
unsafe fn window_pixels(sb: ffi::CMSampleBufferRef) -> Option<(u32, u32)> {
    let rect = frame_info(sb, SCStreamFrameInfoContentRect);
    let scale = ffi::cf_number_f64(frame_info(sb, SCStreamFrameInfoScaleFactor))?;
    let mut r = ffi::CGRect::default();
    if rect.is_null() || !ffi::CGRectMakeWithDictionaryRepresentation(rect, &mut r) {
        return None;
    }
    let (w, h) = ((r.size.width * scale).round(), (r.size.height * scale).round());
    (w >= 2.0 && h >= 2.0).then_some((w as u32, h as u32))
}

/// Follows a shared window's size: when it changes for good, the stream is reconfigured to the new
/// size (still inside the wanted box), like Windows' capture which always sends the window's own
/// size. ScreenCaptureKit would otherwise scale the resized window into the first size.
struct ResizeFollower {
    cfg: VideoConfig,
    current: (u32, u32),
    /// A different size seen since then (it must hold a moment before the stream changes).
    pending: Option<((u32, u32), Instant)>,
    stream: Arc<Mutex<Option<SendBox<Retained<SCStream>>>>>,
}

/// How long a new window size must hold before the stream follows it (a drag resize sends many).
const RESIZE_SETTLE: Duration = Duration::from_millis(400);

impl ResizeFollower {
    fn offer(&mut self, window_px: (u32, u32)) {
        let want = content::output_size(window_px, self.cfg.max_width, self.cfg.max_height);
        let close = |a: (u32, u32), b: (u32, u32)| a.0.abs_diff(b.0) <= 8 && a.1.abs_diff(b.1) <= 8;
        if close(want, self.current) {
            self.pending = None;
            return;
        }
        match self.pending {
            Some((p, t)) if close(p, want) && t.elapsed() >= RESIZE_SETTLE => {
                self.pending = None;
                self.current = want;
                if let Some(s) = lock(&self.stream).as_ref() {
                    let sc = video_configuration(want, &self.cfg);
                    // SAFETY: an asynchronous reconfiguration of a running stream.
                    unsafe { s.0.updateConfiguration_completionHandler(&sc, None) };
                    tracing::info!(width = want.0, height = want.1, "shared window resized: capture follows");
                }
            }
            Some((p, _)) if close(p, want) => {}
            _ => self.pending = Some((want, Instant::now())),
        }
    }
}

/// The stream configuration of a video capture at `size`.
fn video_configuration(size: (u32, u32), cfg: &VideoConfig) -> Retained<SCStreamConfiguration> {
    // SAFETY: a fresh configuration and its setters; the matrix constant is a CFString.
    unsafe {
        let sc = SCStreamConfiguration::new();
        sc.setWidth(size.0 as usize);
        sc.setHeight(size.1 as usize);
        sc.setMinimumFrameInterval(frame_interval(cfg.max_fps));
        sc.setPixelFormat(ffi::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        sc.setColorMatrix(&*(ffi::kCGDisplayStreamYCbCrMatrix_ITU_R_709_2 as *const CFString));
        sc.setShowsCursor(cfg.capture_cursor);
        sc.setQueueDepth(QUEUE_DEPTH);
        sc.setScalesToFit(true);
        sc.setPreservesAspectRatio(true);
        sc
    }
}

/// The capture time of a sample on the `monotonic_now_us` clock.
///
/// # Safety
/// `sb` is a valid sample buffer.
pub(crate) unsafe fn sample_time_us(sb: ffi::CMSampleBufferRef) -> i64 {
    ffi::CMSampleBufferGetPresentationTimeStamp(sb).micros().unwrap_or_else(ffi::host_time_us)
}

// ---------------------------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------------------------

struct VideoShared {
    running: Arc<AtomicBool>,
    delivered: AtomicU64,
    dropped: AtomicU64,
}

pub struct VideoInner {
    shared: Arc<VideoShared>,
    _run: SendBox<Running>,
}

impl VideoInner {
    pub fn stats(&self) -> VideoStats {
        VideoStats {
            frames_delivered: self.shared.delivered.load(Ordering::Relaxed),
            frames_dropped: self.shared.dropped.load(Ordering::Relaxed),
            avg_gpu_convert_us: 0,
        }
    }
    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }
}

pub fn start_video(target: VideoTarget, cfg: VideoConfig, mut on_frame: FrameCallback, mut preview: Option<PreviewAttach>) -> Result<VideoInner> {
    objc2::rc::autoreleasepool(|_| {
        let r = content::resolve(target, true)?;
        let (w, h) = content::output_size(r.pixels, cfg.max_width, cfg.max_height);
        let sc = video_configuration((w, h), &cfg);
        let stream_slot = Arc::new(Mutex::new(None));
        let mut resize = matches!(target, VideoTarget::Window(_)).then(|| ResizeFollower { cfg, current: (w, h), pending: None, stream: stream_slot.clone() });
        let running = Arc::new(AtomicBool::new(true));
        let shared = Arc::new(VideoShared { running: running.clone(), delivered: AtomicU64::new(0), dropped: AtomicU64::new(0) });
        let sh = shared.clone();
        let handler: SampleHandler = Box::new(move |sb| {
            // SAFETY: ScreenCaptureKit hands a valid sample buffer for the callback's duration.
            unsafe {
                if frame_status(sb).is_some_and(|s| s != FRAME_COMPLETE) {
                    return;
                }
                let pb = ffi::CMSampleBufferGetImageBuffer(sb);
                if pb.is_null() {
                    return;
                }
                if let Some(rf) = resize.as_mut() {
                    if let Some(px) = window_pixels(sb) {
                        rf.offer(px);
                    }
                }
                let ts = sample_time_us(sb);
                let ok = with_nv12(pb, ts, |f| {
                    on_frame(f);
                    if let Some(p) = preview.as_mut() {
                        p.offer(f);
                    }
                });
                if ok {
                    sh.delivered.fetch_add(1, Ordering::Relaxed);
                } else {
                    sh.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        let output = StreamOutput::new(Some(handler), None, running);
        let run = Running::start(&r.filter, &sc, output, false, "concord.capture.video")?;
        *lock(&stream_slot) = Some(SendBox(run.stream.clone()));
        tracing::info!(?target, width = w, height = h, fps = cfg.max_fps, source_w = r.pixels.0, source_h = r.pixels.1, "screen capture stream started (ScreenCaptureKit, NV12)");
        Ok(VideoInner { shared, _run: SendBox(run) })
    })
}

// ---------------------------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------------------------

pub struct AudioInner {
    running: Arc<AtomicBool>,
    _run: SendBox<Running>,
}

impl AudioInner {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

/// Read one audio sample buffer and feed it to the chunker.
///
/// # Safety
/// `sb` is a valid audio sample buffer.
unsafe fn push_audio(sb: ffi::CMSampleBufferRef, chunker: &mut AudioChunker, emit: &mut dyn FnMut(&[i16], i64)) -> bool {
    use ffi::*;
    let desc = CMSampleBufferGetFormatDescription(sb);
    let asbd = if desc.is_null() { std::ptr::null() } else { CMAudioFormatDescriptionGetStreamBasicDescription(desc) };
    let Some(asbd) = asbd.as_ref() else { return false };
    if asbd.mFormatID != kAudioFormatLinearPCM {
        return false;
    }
    let float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
    let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
    let bits = asbd.mBitsPerChannel;
    if !(float && bits == 32) && !(!float && bits == 16) {
        return false;
    }
    let channels = asbd.mChannelsPerFrame.max(1) as usize;
    let mut list = AudioBufferList8 { mNumberBuffers: 0, mBuffers: [AudioBuffer { mNumberChannels: 0, mDataByteSize: 0, mData: std::ptr::null_mut() }; 8] };
    let mut block: CFTypeRef = std::ptr::null();
    let st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, std::ptr::null_mut(), &mut list, std::mem::size_of::<AudioBufferList8>(), std::ptr::null(), std::ptr::null(), 0, &mut block);
    if st != 0 {
        return false;
    }
    let n = (list.mNumberBuffers as usize).min(8);
    let bytes_per_sample = (bits / 8) as usize;
    // Frames: from the first buffer's size.
    let frames = if n == 0 {
        0
    } else if planar {
        list.mBuffers[0].mDataByteSize as usize / bytes_per_sample
    } else {
        list.mBuffers[0].mDataByteSize as usize / (bytes_per_sample * channels)
    };
    let bufs = &list.mBuffers[..n];
    let read = |buf: &AudioBuffer, i: usize| -> f32 {
        if float {
            *(buf.mData as *const f32).add(i)
        } else {
            *(buf.mData as *const i16).add(i) as f32 / 32768.0
        }
    };
    let sample = |f: usize, c: usize| -> f32 {
        let c = c.min(channels - 1);
        if planar {
            bufs.get(c).or(bufs.first()).map_or(0.0, |b| read(b, f))
        } else {
            read(&bufs[0], f * channels + c)
        }
    };
    if frames > 0 && bufs.iter().all(|b| !b.mData.is_null()) {
        chunker.push(frames, asbd.mSampleRate.round() as u32, sample_time_us(sb), sample, emit);
    }
    if !block.is_null() {
        CFRelease(block);
    }
    true
}

pub fn start_audio(target: AudioTarget, mut on_audio: AudioCallback) -> Result<AudioInner> {
    objc2::rc::autoreleasepool(|_| {
        let filter = match target {
            AudioTarget::ProcessTree { pid } => content::audio_filter(pid, true)?,
            AudioTarget::SystemExcludingProcessTree { pid } => content::audio_filter(pid, false)?,
        };
        // SAFETY: a plain object constructor.
        let sc = unsafe { SCStreamConfiguration::new() };
        // SAFETY: setters of a fresh configuration.
        unsafe {
            sc.setCapturesAudio(true);
            sc.setExcludesCurrentProcessAudio(true);
            sc.setSampleRate(AUDIO_SAMPLE_RATE as isize);
            sc.setChannelCount(AUDIO_CHANNELS as isize);
            // The picture of an audio stream is thrown away: as small and rare as allowed.
            sc.setWidth(64);
            sc.setHeight(64);
            sc.setMinimumFrameInterval(frame_interval(1));
            sc.setShowsCursor(false);
        }
        let running = Arc::new(AtomicBool::new(true));
        let mut chunker = AudioChunker::default();
        let mut warned = false;
        let handler: SampleHandler = Box::new(move |sb| {
            let mut emit = |samples: &[i16], ts: i64| {
                on_audio(&AudioChunk { samples, sample_rate: AUDIO_SAMPLE_RATE, channels: AUDIO_CHANNELS, timestamp_us: ts });
            };
            // SAFETY: a valid audio sample buffer for the callback's duration.
            if !unsafe { push_audio(sb, &mut chunker, &mut emit) } && !warned {
                warned = true;
                tracing::warn!("screen capture audio: a sample buffer in an unexpected format was skipped");
            }
        });
        let ignore: SampleHandler = Box::new(|_| {});
        let output = StreamOutput::new(Some(ignore), Some(handler), running.clone());
        let run = Running::start(&filter, &sc, output, true, "concord.capture.audio")?;
        tracing::info!(?target, "app audio capture started (ScreenCaptureKit, 48 kHz stereo, our own sound excluded)");
        Ok(AudioInner { running, _run: SendBox(run) })
    })
}
