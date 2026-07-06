//! Minimal C/Objective-C FFI needed for the ScreenCaptureKit video backend.
//!
//! We only declare the handful of CoreMedia / CoreVideo / libdispatch functions
//! we actually use, plus the frameworks that must be linked so the Objective-C
//! classes (`SCStream`, `SCShareableContent`, …) resolve at runtime via `class!`.

#![allow(non_upper_case_globals)]
#![allow(dead_code)]

use objc::runtime::Object;
use std::os::raw::{c_char, c_void};

pub type id = *mut Object;
pub type NSInteger = isize;

/// `kCVPixelBufferLock_ReadOnly`
pub const kCVPixelBufferLock_ReadOnly: u64 = 0x0000_0001;
/// `kCVPixelFormatType_32BGRA` == 'BGRA'
pub const BGRA_PIXEL_FORMAT: u32 = 0x4247_5241;
/// `SCStreamOutputType.screen`
pub const SC_STREAM_OUTPUT_TYPE_SCREEN: NSInteger = 0;

/// `CMTime` (CoreMedia). 24 bytes; passed by value to `setMinimumFrameInterval:`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

/// `kCMTimeFlags_Valid`
pub const kCMTimeFlags_Valid: u32 = 1;

impl CMTime {
    /// A frame interval of `1/fps` seconds.
    pub fn frame_interval(fps: u32) -> CMTime {
        CMTime {
            value: 1,
            timescale: fps.max(1) as i32,
            flags: kCMTimeFlags_Valid,
            epoch: 0,
        }
    }
}

// Force these frameworks onto the link line so their Objective-C classes are
// loaded into the process (needed for `class!(SCStream)` etc.).
#[link(name = "ScreenCaptureKit", kind = "framework")]
extern "C" {}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    pub fn CMSampleBufferGetImageBuffer(sbuf: *mut c_void) -> *mut c_void;
    pub fn CMSampleBufferIsValid(sbuf: *mut c_void) -> u8;
    pub fn CMSampleBufferDataIsReady(sbuf: *mut c_void) -> u8;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    pub fn CVPixelBufferLockBaseAddress(pixel_buffer: *mut c_void, lock_flags: u64) -> i32;
    pub fn CVPixelBufferUnlockBaseAddress(pixel_buffer: *mut c_void, lock_flags: u64) -> i32;
    pub fn CVPixelBufferGetBaseAddress(pixel_buffer: *mut c_void) -> *mut c_void;
    pub fn CVPixelBufferGetBytesPerRow(pixel_buffer: *mut c_void) -> usize;
    pub fn CVPixelBufferGetWidth(pixel_buffer: *mut c_void) -> usize;
    pub fn CVPixelBufferGetHeight(pixel_buffer: *mut c_void) -> usize;
}

#[link(name = "System", kind = "dylib")]
extern "C" {
    pub fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
}

extern "C" {
    /// libobjc's message trampoline. We transmute it to exact signatures for the
    /// few calls whose argument ABI (`CMTime` by value, `NSError**`) is awkward
    /// to express through `objc`'s `msg_send!`/`Encode` machinery.
    pub fn objc_msgSend();
}
