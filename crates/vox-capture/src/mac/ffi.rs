//! The plain C functions of CoreFoundation, CoreVideo, CoreMedia and CoreGraphics this backend
//! calls (the Objective-C classes come from the objc2 crates). Raw pointers only.

#![allow(non_snake_case, non_upper_case_globals, dead_code)]

use std::ffi::c_void;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CVPixelBufferRef = *mut c_void;
pub type CVPixelBufferPoolRef = *mut c_void;
pub type CMSampleBufferRef = *const c_void;
pub type CMFormatDescriptionRef = *const c_void;

/// `CMTime`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

impl CMTime {
    /// Microseconds, when the time is valid and numeric.
    pub fn micros(&self) -> Option<i64> {
        // kCMTimeFlags_Valid, and none of the infinities / indefinite.
        (self.flags & 1 != 0 && self.flags & 0x1C == 0 && self.timescale > 0).then(|| (self.value as i128 * 1_000_000 / self.timescale as i128) as i64)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CMVideoDimensions {
    pub width: i32,
    pub height: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

pub const kAudioFormatLinearPCM: u32 = u32::from_be_bytes(*b"lpcm");
pub const kAudioFormatFlagIsFloat: u32 = 1;
pub const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}

/// `AudioBufferList` with room for up to 8 buffers (one per channel when not interleaved).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioBufferList8 {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

pub const kCVPixelBufferLock_ReadOnly: u64 = 1;
/// '420v': NV12, video range (what ScreenCaptureKit and cameras deliver).
pub const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: u32 = u32::from_be_bytes(*b"420v");
/// '420f': NV12, full range.
pub const kCVPixelFormatType_420YpCbCr8BiPlanarFullRange: u32 = u32::from_be_bytes(*b"420f");
pub const kCVPixelFormatType_32BGRA: u32 = u32::from_be_bytes(*b"BGRA");
pub const kCFNumberSInt64Type: isize = 4;
pub const kCFNumberSInt32Type: isize = 3;
/// `kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big`: R G B A bytes.
pub const RGBA_PREMULTIPLIED: u32 = 1 | (4 << 12);

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFArrayGetCount(a: CFArrayRef) -> isize;
    pub fn CFArrayGetValueAtIndex(a: CFArrayRef, i: isize) -> *const c_void;
    pub fn CFDictionaryGetValue(d: CFDictionaryRef, key: *const c_void) -> *const c_void;
    pub fn CFNumberGetValue(n: *const c_void, kind: isize, out: *mut c_void) -> u8;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    pub fn CVPixelBufferRetain(pb: CVPixelBufferRef) -> CVPixelBufferRef;
    pub fn CVPixelBufferRelease(pb: CVPixelBufferRef);
    pub fn CVPixelBufferLockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> i32;
    pub fn CVPixelBufferUnlockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> i32;
    pub fn CVPixelBufferGetWidth(pb: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetHeight(pb: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetPixelFormatType(pb: CVPixelBufferRef) -> u32;
    pub fn CVPixelBufferGetPlaneCount(pb: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetBaseAddress(pb: CVPixelBufferRef) -> *mut u8;
    pub fn CVPixelBufferGetBytesPerRow(pb: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetBaseAddressOfPlane(pb: CVPixelBufferRef, plane: usize) -> *mut u8;
    pub fn CVPixelBufferGetBytesPerRowOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    pub fn CVPixelBufferGetHeightOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    pub fn CVPixelBufferPoolCreate(allocator: CFTypeRef, pool_attrs: CFDictionaryRef, pb_attrs: CFDictionaryRef, out: *mut CVPixelBufferPoolRef) -> i32;
    pub fn CVPixelBufferPoolCreatePixelBuffer(allocator: CFTypeRef, pool: CVPixelBufferPoolRef, out: *mut CVPixelBufferRef) -> i32;
    pub fn CVPixelBufferPoolRelease(pool: CVPixelBufferPoolRef);
    pub static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    pub static kCVPixelBufferWidthKey: CFStringRef;
    pub static kCVPixelBufferHeightKey: CFStringRef;
    pub static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    pub fn CMSampleBufferGetImageBuffer(sb: CMSampleBufferRef) -> CVPixelBufferRef;
    pub fn CMSampleBufferGetPresentationTimeStamp(sb: CMSampleBufferRef) -> CMTime;
    pub fn CMSampleBufferGetSampleAttachmentsArray(sb: CMSampleBufferRef, create: u8) -> CFArrayRef;
    pub fn CMSampleBufferGetFormatDescription(sb: CMSampleBufferRef) -> CMFormatDescriptionRef;
    pub fn CMSampleBufferGetNumSamples(sb: CMSampleBufferRef) -> isize;
    pub fn CMSampleBufferIsValid(sb: CMSampleBufferRef) -> u8;
    pub fn CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sb: CMSampleBufferRef,
        size_needed_out: *mut usize,
        list_out: *mut AudioBufferList8,
        list_size: usize,
        block_allocator: CFTypeRef,
        memory_allocator: CFTypeRef,
        flags: u32,
        block_buffer_out: *mut CFTypeRef,
    ) -> i32;
    pub fn CMAudioFormatDescriptionGetStreamBasicDescription(desc: CMFormatDescriptionRef) -> *const AudioStreamBasicDescription;
    pub fn CMVideoFormatDescriptionGetDimensions(desc: CMFormatDescriptionRef) -> CMVideoDimensions;
    pub fn CMFormatDescriptionGetMediaSubType(desc: CMFormatDescriptionRef) -> u32;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGPreflightScreenCaptureAccess() -> bool;
    pub fn CGRequestScreenCaptureAccess() -> bool;
    pub fn CGMainDisplayID() -> u32;
    pub fn CGDisplayIsBuiltin(display: u32) -> u32;
    pub fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
    pub fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
    pub fn CGDisplayModeGetPixelHeight(mode: *mut c_void) -> usize;
    pub fn CGDisplayModeRelease(mode: *mut c_void);
    pub fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    pub fn CGColorSpaceRelease(space: *mut c_void);
    pub fn CGBitmapContextCreate(data: *mut c_void, w: usize, h: usize, bits_per_component: usize, bytes_per_row: usize, space: *mut c_void, info: u32) -> *mut c_void;
    pub fn CGContextDrawImage(ctx: *mut c_void, rect: CGRect, image: *const c_void);
    pub fn CGContextRelease(ctx: *mut c_void);
    pub fn CGImageGetWidth(image: *const c_void) -> usize;
    pub fn CGImageGetHeight(image: *const c_void) -> usize;
    pub static kCGDisplayStreamYCbCrMatrix_ITU_R_709_2: CFStringRef;
    pub fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect) -> bool;
}

#[repr(C)]
#[derive(Default)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    fn sysctlbyname(name: *const std::ffi::c_char, oldp: *mut c_void, oldlenp: *mut usize, newp: *mut c_void, newlen: usize) -> i32;
}

/// Host time (the clock of capture timestamps: `mach_absolute_time`) in microseconds.
pub fn host_time_us() -> i64 {
    use std::sync::OnceLock;
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    let (n, d) = *TB.get_or_init(|| {
        let mut i = MachTimebaseInfo::default();
        // SAFETY: plain out-parameter call.
        unsafe { mach_timebase_info(&mut i) };
        (i.numer.max(1) as u64, i.denom.max(1) as u64)
    });
    // SAFETY: no arguments.
    let t = unsafe { mach_absolute_time() } as u128;
    (t * n as u128 / d as u128 / 1000) as i64
}

/// A string sysctl ("machdep.cpu.brand_string" -> "Apple M1").
pub fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut len = 0usize;
    // SAFETY: size query, then a read into a buffer of that size.
    unsafe {
        if sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) != 0 || len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len];
        if sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
        buf.truncate(len);
        let s = String::from_utf8_lossy(&buf).trim_end_matches('\0').trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// The value of a CFNumber attachment (an NSNumber) as f64.
///
/// # Safety
/// `n` is null or a CFNumber.
pub unsafe fn cf_number_f64(n: *const c_void) -> Option<f64> {
    if n.is_null() {
        return None;
    }
    let mut v = 0f64;
    // kCFNumberFloat64Type
    (CFNumberGetValue(n, 6, (&mut v as *mut f64).cast()) != 0).then_some(v)
}

/// The value of a CFNumber attachment (an NSNumber) as i64.
///
/// # Safety
/// `n` is null or a CFNumber.
pub unsafe fn cf_number_i64(n: *const c_void) -> Option<i64> {
    if n.is_null() {
        return None;
    }
    let mut v = 0i64;
    (CFNumberGetValue(n, kCFNumberSInt64Type, (&mut v as *mut i64).cast()) != 0).then_some(v)
}
