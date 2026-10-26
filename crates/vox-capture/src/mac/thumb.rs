//! Picker pictures: one ScreenCaptureKit screenshot of a window or display (macOS 14+), and the
//! icon of a window's app.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2_app_kit::NSRunningApplication;
use objc2_core_media::CMSampleBuffer;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCScreenshotManager, SCStreamConfiguration};

use super::content::{self, ns_error, permission_error};
use super::{ffi, os, SendBox};
use crate::pixels::{bgra_to_rgba, unpremultiply};
use crate::{CaptureError, Result, RgbaImage, VideoTarget};

pub struct GrabberInner {
    timeout: Duration,
}

impl GrabberInner {
    pub fn new() -> Result<Self> {
        if AnyClass::get(c"SCScreenshotManager").is_none() {
            return Err(CaptureError::Unsupported("picker thumbnails need macOS 14".into()));
        }
        Ok(Self { timeout: Duration::from_millis(2500) })
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub fn grab(&mut self, target: VideoTarget, max_width: u32, max_height: u32) -> Result<RgbaImage> {
        objc2::rc::autoreleasepool(|_| {
            let r = content::resolve(target, false)?;
            let (w, h) = content::output_size(r.pixels, max_width, max_height);
            // SAFETY: a plain object constructor.
        let sc = unsafe { SCStreamConfiguration::new() };
            // SAFETY: setters of a fresh configuration.
            unsafe {
                sc.setWidth(w as usize);
                sc.setHeight(h as usize);
                sc.setPixelFormat(ffi::kCVPixelFormatType_32BGRA);
                sc.setShowsCursor(false);
                sc.setScalesToFit(true);
                sc.setPreservesAspectRatio(true);
            }
            let (tx, rx) = mpsc::sync_channel::<std::result::Result<SendBox<Retained<CMSampleBuffer>>, (String, bool)>>(1);
            let block = RcBlock::new(move |sb: *mut CMSampleBuffer, err: *mut NSError| {
                // SAFETY: the handler's arguments are valid (or null) for its duration.
                let r = match unsafe { Retained::retain(sb) } {
                    Some(sb) => Ok(SendBox(sb)),
                    None => Err(unsafe { err.as_ref() }.map(ns_error).unwrap_or_else(|| ("no picture".into(), false))),
                };
                let _ = tx.try_send(r);
            });
            // SAFETY: valid filter and configuration; ScreenCaptureKit copies the block.
            unsafe { SCScreenshotManager::captureSampleBufferWithFilter_configuration_completionHandler(&r.filter, &sc, Some(&block)) };
            let sb = match rx.recv_timeout(self.timeout) {
                Ok(Ok(sb)) => sb.0,
                Ok(Err((_, true))) => return Err(permission_error("thumbnail")),
                Ok(Err((e, false))) => return Err(os("SCScreenshotManager", e)),
                Err(_) => return Err(CaptureError::Stopped("no picture in time".into())),
            };
            // SAFETY: a valid sample buffer; the pixel buffer is locked while it is read.
            unsafe {
                let pb = ffi::CMSampleBufferGetImageBuffer(&*sb as *const CMSampleBuffer as ffi::CMSampleBufferRef);
                if pb.is_null() || ffi::CVPixelBufferGetPixelFormatType(pb) != ffi::kCVPixelFormatType_32BGRA {
                    return Err(CaptureError::Stopped("the screenshot has no BGRA picture".into()));
                }
                if ffi::CVPixelBufferLockBaseAddress(pb, ffi::kCVPixelBufferLock_ReadOnly) != 0 {
                    return Err(os("CVPixelBufferLockBaseAddress", "failed"));
                }
                let (pw, ph, stride) = (ffi::CVPixelBufferGetWidth(pb), ffi::CVPixelBufferGetHeight(pb), ffi::CVPixelBufferGetBytesPerRow(pb));
                let base = ffi::CVPixelBufferGetBaseAddress(pb);
                let out = (!base.is_null() && pw > 0 && ph > 0).then(|| bgra_to_rgba(std::slice::from_raw_parts(base, stride * ph), stride, pw, ph));
                ffi::CVPixelBufferUnlockBaseAddress(pb, ffi::kCVPixelBufferLock_ReadOnly);
                out.map(|data| RgbaImage { width: pw as u32, height: ph as u32, data }).ok_or_else(|| CaptureError::Stopped("empty screenshot".into()))
            }
        })
    }
}

/// The icon of the app that owns a listed window, drawn at `max_size` pixels (at most 256).
pub fn window_icon(hwnd: isize, max_size: u32) -> Result<RgbaImage> {
    let pid = u32::try_from(hwnd).ok().and_then(content::window_pid).ok_or_else(|| CaptureError::NotFound(format!("window {hwnd}")))?;
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: NSRunningApplication and NSImage may be used from any thread.
        unsafe {
            let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid).ok_or_else(|| CaptureError::NotFound(format!("pid {pid}")))?;
            let icon = app.icon().ok_or_else(|| CaptureError::NotFound("app has no icon".into()))?;
            let image = icon.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None).ok_or_else(|| os("CGImageForProposedRect", "no image"))?;
            let s = max_size.clamp(16, 256) as usize;
            let mut data = vec![0u8; s * s * 4];
            let space = ffi::CGColorSpaceCreateDeviceRGB();
            let ctx = ffi::CGBitmapContextCreate(data.as_mut_ptr().cast(), s, s, 8, s * 4, space, ffi::RGBA_PREMULTIPLIED);
            ffi::CGColorSpaceRelease(space);
            if ctx.is_null() {
                return Err(os("CGBitmapContextCreate", "failed"));
            }
            let rect = ffi::CGRect { origin: ffi::CGPoint { x: 0.0, y: 0.0 }, size: ffi::CGSize { width: s as f64, height: s as f64 } };
            ffi::CGContextDrawImage(ctx, rect, &*image as *const _ as *const std::ffi::c_void);
            ffi::CGContextRelease(ctx);
            unpremultiply(&mut data);
            Ok(RgbaImage { width: s as u32, height: s as u32, data })
        }
    })
}
