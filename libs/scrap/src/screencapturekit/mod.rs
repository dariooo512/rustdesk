//! ScreenCaptureKit video capture backend (raw Objective-C FFI).
//!
//! CGDisplayStream (the legacy `quartz` backend) is deprecated and fails to
//! start on recent macOS (14+/26) and on virtual displays. ScreenCaptureKit is
//! Apple's supported replacement. This module drives it directly via the `objc`
//! runtime so it needs no new crate dependencies.
//!
//! Flow: fetch the shareable content → pick the `SCDisplay` matching a
//! `CGDirectDisplayID` → build an `SCContentFilter` + `SCStreamConfiguration`
//! (BGRA, fixed output size) → start an `SCStream` whose per-frame callback
//! copies each `CVPixelBuffer` into a plain BGRA buffer. `take_frame` hands the
//! latest buffer to the common capturer, which normalises + encodes it exactly
//! like a CGDisplayStream frame.

#![allow(non_snake_case)]

mod ffi;

use self::ffi::*;
use block::ConcreteBlock;
use hbb_common::log;
use objc::declare::ClassDecl;
use objc::rc::autoreleasepool;
use objc::runtime::{Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use std::os::raw::c_void;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

/// One captured frame, already copied out of the `CVPixelBuffer` as BGRA.
pub struct Frame {
    pub data: Vec<u8>,
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

type Slot = Arc<Mutex<Option<Frame>>>;

/// A pointer we knowingly move across the completion-handler thread boundary.
struct SendPtr(id);
unsafe impl Send for SendPtr {}

pub struct Capturer {
    stream: id,
    filter: id,
    config: id,
    output: id,
    queue: *mut c_void, // dispatch_queue_t, intentionally leaked (see Drop)
    slot: Slot,
    slot_raw: *const Mutex<Option<Frame>>,
}

impl Capturer {
    pub fn new(
        display_id: u32,
        width: usize,
        height: usize,
        show_cursor: bool,
        max_fps: u32,
    ) -> Result<Capturer, String> {
        autoreleasepool(|| unsafe { new_impl(display_id, width, height, show_cursor, max_fps) })
    }

    /// Take the most recent frame, if a new one has arrived since last call.
    pub fn take_frame(&mut self) -> Option<Frame> {
        self.slot.lock().ok().and_then(|mut g| g.take())
    }
}

unsafe fn new_impl(
    display_id: u32,
    width: usize,
    height: usize,
    show_cursor: bool,
    max_fps: u32,
) -> Result<Capturer, String> {
    // 1. Shareable content (async). Surfaces permission errors as the NSError.
    let content = get_shareable_content()?;

    // 2. Find the SCDisplay for our CGDirectDisplayID (fall back to the first).
    let displays: id = msg_send![content, displays];
    let count: usize = msg_send![displays, count];
    let mut target: id = std::ptr::null_mut();
    for i in 0..count {
        let d: id = msg_send![displays, objectAtIndex: i];
        let did: u32 = msg_send![d, displayID];
        if did == display_id {
            target = d;
            break;
        }
    }
    if target.is_null() && count > 0 {
        target = msg_send![displays, objectAtIndex: 0usize];
    }
    if target.is_null() {
        let _: () = msg_send![content, release];
        return Err("ScreenCaptureKit: no SCDisplay available".to_owned());
    }
    let _: () = msg_send![target, retain];
    let _: () = msg_send![content, release];

    // 3. Content filter: whole display, no excluded windows.
    let empty_windows: id = msg_send![class!(NSArray), array];
    let filter: id = msg_send![class!(SCContentFilter), alloc];
    let filter: id = msg_send![filter, initWithDisplay: target excludingWindows: empty_windows];
    let _: () = msg_send![target, release];
    if filter.is_null() {
        return Err("ScreenCaptureKit: SCContentFilter init failed".to_owned());
    }

    // 4. Stream configuration: BGRA, fixed output size, capped fps.
    let config: id = msg_send![class!(SCStreamConfiguration), alloc];
    let config: id = msg_send![config, init];
    if config.is_null() {
        let _: () = msg_send![filter, release];
        return Err("ScreenCaptureKit: SCStreamConfiguration init failed".to_owned());
    }
    let _: () = msg_send![config, setWidth: width as NSInteger];
    let _: () = msg_send![config, setHeight: height as NSInteger];
    let _: () = msg_send![config, setPixelFormat: BGRA_PIXEL_FORMAT];
    let _: () = msg_send![config, setQueueDepth: 6 as NSInteger];
    set_shows_cursor(config, show_cursor);
    set_minimum_frame_interval(config, CMTime::frame_interval(max_fps));

    // 5. Shared frame slot + delegate/output object holding a pointer to it.
    let slot: Slot = Arc::new(Mutex::new(None));
    let slot_raw: *const Mutex<Option<Frame>> = Arc::into_raw(slot.clone());
    let output = create_output_object(slot_raw as usize);

    // 6. Stream.
    let stream: id = msg_send![class!(SCStream), alloc];
    let stream: id = msg_send![stream,
        initWithFilter: filter
        configuration: config
        delegate: std::ptr::null_mut::<Object>()];
    if stream.is_null() {
        let _: () = msg_send![output, release];
        let _: () = msg_send![config, release];
        let _: () = msg_send![filter, release];
        let _ = Arc::from_raw(slot_raw);
        return Err("ScreenCaptureKit: SCStream init failed".to_owned());
    }

    // 7. Attach the screen output on a dedicated serial queue.
    let queue = dispatch_queue_create(b"io.rentamac.sck.capture\0".as_ptr() as *const _, std::ptr::null());
    if let Err(e) = add_stream_output(stream, output, queue) {
        let _: () = msg_send![stream, release];
        let _: () = msg_send![output, release];
        let _: () = msg_send![config, release];
        let _: () = msg_send![filter, release];
        let _ = Arc::from_raw(slot_raw);
        return Err(e);
    }

    // 8. Start capturing (async).
    if let Err(e) = start_capture(stream) {
        let _: () = msg_send![stream, release];
        let _: () = msg_send![output, release];
        let _: () = msg_send![config, release];
        let _: () = msg_send![filter, release];
        let _ = Arc::from_raw(slot_raw);
        return Err(e);
    }

    log::info!(
        "ScreenCaptureKit stream started: display={display_id} {width}x{height} cursor={show_cursor} fps<={max_fps}"
    );
    Ok(Capturer {
        stream,
        filter,
        config,
        output,
        queue,
        slot,
        slot_raw,
    })
}

impl Drop for Capturer {
    fn drop(&mut self) {
        unsafe {
            let stopped = stop_capture(self.stream);
            let _: () = msg_send![self.stream, release];
            let _: () = msg_send![self.output, release];
            let _: () = msg_send![self.config, release];
            let _: () = msg_send![self.filter, release];
            // Only reclaim the delegate's Arc reference once we are sure no more
            // callbacks can fire; otherwise leak it to stay memory-safe.
            if stopped && !self.slot_raw.is_null() {
                let _ = Arc::from_raw(self.slot_raw);
            }
            // The dispatch queue is intentionally leaked: releasing dispatch
            // objects across the objc/libdispatch ABI is fragile and a single
            // queue per session is negligible.
            let _ = self.queue;
        }
    }
}

// A few Objective-C calls have argument/return ABIs (`CMTime` by value, a
// `BOOL` whose Rust representation differs across arches, `NSError**`) that are
// awkward through `objc`'s `Encode`-constrained `msg_send!`. For those we
// transmute `objc_msgSend` to the exact C signature. Booleans are marshalled as
// `i8` (0/1), which is ABI-compatible with `_Bool` on Apple targets.

// ---- SCStreamConfiguration.setShowsCursor: (BOOL) ----

unsafe fn set_shows_cursor(config: id, show_cursor: bool) {
    let f: unsafe extern "C" fn(id, Sel, i8) =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(config, sel!(setShowsCursor:), show_cursor as i8);
}

// ---- SCStreamConfiguration.setMinimumFrameInterval: (CMTime by value) ----

unsafe fn set_minimum_frame_interval(config: id, interval: CMTime) {
    let f: unsafe extern "C" fn(id, Sel, CMTime) =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(config, sel!(setMinimumFrameInterval:), interval);
}

// ---- SCStream addStreamOutput:type:sampleHandlerQueue:error: ----

unsafe fn add_stream_output(stream: id, output: id, queue: *mut c_void) -> Result<(), String> {
    let mut err: id = std::ptr::null_mut();
    let f: unsafe extern "C" fn(id, Sel, id, NSInteger, *mut c_void, *mut id) -> i8 =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    let ok = f(
        stream,
        sel!(addStreamOutput:type:sampleHandlerQueue:error:),
        output,
        SC_STREAM_OUTPUT_TYPE_SCREEN,
        queue,
        &mut err,
    );
    if ok == 0 {
        return Err(format!(
            "ScreenCaptureKit: addStreamOutput failed: {}",
            ns_error_string(err)
        ));
    }
    Ok(())
}

// ---- Async: SCShareableContent.getShareableContentWithCompletionHandler: ----

unsafe fn get_shareable_content() -> Result<id, String> {
    let (tx, rx) = channel::<Result<SendPtr, String>>();
    let handler = ConcreteBlock::new(move |content: id, error: id| {
        if !content.is_null() {
            let _: () = msg_send![content, retain];
            let _ = tx.send(Ok(SendPtr(content)));
        } else {
            let _ = tx.send(Err(ns_error_string(error)));
        }
    });
    let handler = handler.copy();
    let cls = class!(SCShareableContent);
    let _: () = msg_send![cls, getShareableContentWithCompletionHandler: &*handler];
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(SendPtr(p))) => Ok(p),
        Ok(Err(e)) => Err(format!("ScreenCaptureKit: getShareableContent error: {e}")),
        Err(_) => Err(
            "ScreenCaptureKit: getShareableContent timed out (screen-recording permission?)"
                .to_owned(),
        ),
    }
}

// ---- Async: SCStream startCaptureWithCompletionHandler: ----

unsafe fn start_capture(stream: id) -> Result<(), String> {
    let (tx, rx) = channel::<Result<(), String>>();
    let handler = ConcreteBlock::new(move |error: id| {
        if error.is_null() {
            let _ = tx.send(Ok(()));
        } else {
            let _ = tx.send(Err(ns_error_string(error)));
        }
    });
    let handler = handler.copy();
    let _: () = msg_send![stream, startCaptureWithCompletionHandler: &*handler];
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("ScreenCaptureKit: startCapture error: {e}")),
        Err(_) => Err("ScreenCaptureKit: startCapture timed out".to_owned()),
    }
}

unsafe fn stop_capture(stream: id) -> bool {
    if stream.is_null() {
        return true;
    }
    let (tx, rx) = channel::<()>();
    let handler = ConcreteBlock::new(move |_error: id| {
        let _ = tx.send(());
    });
    let handler = handler.copy();
    let _: () = msg_send![stream, stopCaptureWithCompletionHandler: &*handler];
    rx.recv_timeout(Duration::from_secs(3)).is_ok()
}

// ---- NSError -> String ----

unsafe fn ns_error_string(err: id) -> String {
    if err.is_null() {
        return "nil".to_owned();
    }
    let desc: id = msg_send![err, localizedDescription];
    ns_string_to_rust(desc)
}

unsafe fn ns_string_to_rust(s: id) -> String {
    if s.is_null() {
        return String::new();
    }
    let utf8: *const std::os::raw::c_char = msg_send![s, UTF8String];
    if utf8.is_null() {
        return String::new();
    }
    std::ffi::CStr::from_ptr(utf8)
        .to_string_lossy()
        .into_owned()
}

// ---- SCStreamOutput delegate class (created once at runtime) ----

const IVAR_SLOT: &str = "rentamac_slot";
static REGISTER_CLASS: Once = Once::new();
static mut OUTPUT_CLASS: *const objc::runtime::Class = std::ptr::null();

unsafe fn create_output_object(slot_ptr: usize) -> id {
    REGISTER_CLASS.call_once(|| {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("RentaMacSCKOutput", superclass)
            .expect("failed to declare RentaMacSCKOutput");
        decl.add_ivar::<usize>(IVAR_SLOT);
        decl.add_method(
            sel!(stream:didOutputSampleBuffer:ofType:),
            on_output as extern "C" fn(&Object, Sel, id, id, NSInteger),
        );
        if let Some(p) = objc::runtime::Protocol::get("SCStreamOutput") {
            decl.add_protocol(p);
        }
        if let Some(p) = objc::runtime::Protocol::get("SCStreamDelegate") {
            decl.add_protocol(p);
        }
        let cls = decl.register() as *const objc::runtime::Class;
        unsafe {
            OUTPUT_CLASS = cls;
        }
    });
    let cls: &objc::runtime::Class = &*OUTPUT_CLASS;
    let obj: id = msg_send![cls, alloc];
    let obj: id = msg_send![obj, init];
    let obj_ref: &mut Object = &mut *obj;
    obj_ref.set_ivar::<usize>(IVAR_SLOT, slot_ptr);
    obj
}

extern "C" fn on_output(
    this: &Object,
    _cmd: Sel,
    _stream: id,
    sample_buffer: id,
    _of_type: NSInteger,
) {
    unsafe {
        let sb = sample_buffer as *mut c_void;
        if sb.is_null() {
            return;
        }
        let image_buffer = CMSampleBufferGetImageBuffer(sb);
        if image_buffer.is_null() {
            // Non-complete frames (idle/blank) often carry no image buffer.
            return;
        }
        if CVPixelBufferLockBaseAddress(image_buffer, kCVPixelBufferLock_ReadOnly) != 0 {
            return;
        }
        let base = CVPixelBufferGetBaseAddress(image_buffer);
        let stride = CVPixelBufferGetBytesPerRow(image_buffer);
        let width = CVPixelBufferGetWidth(image_buffer);
        let height = CVPixelBufferGetHeight(image_buffer);
        if !base.is_null() && stride > 0 && height > 0 {
            let len = stride * height;
            let mut data = vec![0u8; len];
            std::ptr::copy_nonoverlapping(base as *const u8, data.as_mut_ptr(), len);
            let slot_ptr = *this.get_ivar::<usize>(IVAR_SLOT) as *const Mutex<Option<Frame>>;
            if !slot_ptr.is_null() {
                if let Ok(mut g) = (*slot_ptr).lock() {
                    *g = Some(Frame {
                        data,
                        stride,
                        width,
                        height,
                    });
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(image_buffer, kCVPixelBufferLock_ReadOnly);
    }
}
