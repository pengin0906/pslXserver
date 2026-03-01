// macOS display backend — NSWindow management, IOSurface-backed pixel buffer rendering
#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use crossbeam_channel::{Receiver, Sender};
use log::{debug, info};

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2_app_kit::{NSApplication, NSWindow, NSWindowStyleMask};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRange, NSRect, NSSize, NSString};

use crate::display::{DisplayCommand, DisplayEvent, NativeWindowHandle};
use crate::display::renderer::render_to_buffer;

// --- IOSurface FFI ---
extern "C" {
    fn IOSurfaceCreate(properties: *const c_void) -> *mut c_void;
    fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
}

// --- CoreFoundation FFI ---
extern "C" {
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFAbsoluteTimeGetCurrent() -> f64;
    fn CFRunLoopTimerCreate(
        allocator: *const c_void, fire_date: f64, interval: f64,
        flags: u64, order: i64,
        callout: extern "C" fn(*mut c_void, *mut c_void),
        context: *mut c_void,
    ) -> *mut c_void;
    fn CFRunLoopAddTimer(rl: *mut c_void, timer: *mut c_void, mode: *const c_void);
    static kCFRunLoopCommonModes: *const c_void;
    static kCFRunLoopDefaultMode: *const c_void;

    fn CFDictionaryCreateMutable(
        allocator: *const c_void,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *mut c_void;
    fn CFDictionarySetValue(dict: *mut c_void, key: *const c_void, value: *const c_void);
    fn CFNumberCreate(allocator: *const c_void, the_type: isize, value_ptr: *const c_void) -> *mut c_void;
    fn CFRelease(cf: *const c_void);

    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

// IOSurface property keys
extern "C" {
    static kIOSurfaceWidth: *const c_void;
    static kIOSurfaceHeight: *const c_void;
    static kIOSurfaceBytesPerElement: *const c_void;
    static kIOSurfaceBytesPerRow: *const c_void;
    static kIOSurfacePixelFormat: *const c_void;
}

// CFNumber type: kCFNumberSInt32Type = 3
const CF_NUMBER_SINT32_TYPE: isize = 3;

/// Create an IOSurface with the given dimensions, BGRA format.
fn create_iosurface(width: u16, height: u16) -> *mut c_void {
    let w = width as i32;
    let h = height as i32;
    let bpe: i32 = 4; // bytes per element (BGRA)
    let bpr: i32 = w * 4; // bytes per row
    let pixel_format: i32 = 0x42475241; // 'BGRA' as FourCC

    unsafe {
        let dict = CFDictionaryCreateMutable(
            std::ptr::null(),
            5,
            &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
        );

        let set_int = |key: *const c_void, val: &i32| {
            let num = CFNumberCreate(std::ptr::null(), CF_NUMBER_SINT32_TYPE, val as *const i32 as *const c_void);
            CFDictionarySetValue(dict, key, num as *const c_void);
            CFRelease(num as *const c_void);
        };

        set_int(kIOSurfaceWidth, &w);
        set_int(kIOSurfaceHeight, &h);
        set_int(kIOSurfaceBytesPerElement, &bpe);
        set_int(kIOSurfaceBytesPerRow, &bpr);
        set_int(kIOSurfacePixelFormat, &pixel_format);

        let surface = IOSurfaceCreate(dict as *const c_void);
        CFRelease(dict as *const c_void);
        surface
    }
}

struct WindowInfo {
    window: Retained<NSWindow>,
    /// IOSurface-backed pixel buffer for software rendering.
    surface: *mut c_void,
    width: u16,
    height: u16,
    /// X11 window ID for routing events back to clients.
    x11_id: crate::display::Xid,
    /// Cached X11 screen position (top-left of content area in X11 coords).
    /// Updated every frame to detect window moves.
    x11_x: i16,
    x11_y: i16,
}

impl Drop for WindowInfo {
    fn drop(&mut self) {
        if !self.surface.is_null() {
            unsafe { CFRelease(self.surface as *const c_void); }
        }
    }
}

thread_local! {
    static WINDOWS: RefCell<HashMap<u64, WindowInfo>> = RefCell::new(HashMap::new());
    static CMD_RX: RefCell<Option<Receiver<DisplayCommand>>> = RefCell::new(None);
    static EVT_TX: RefCell<Option<Sender<DisplayEvent>>> = RefCell::new(None);
    static RENDER_MAILBOX: RefCell<Option<crate::display::RenderMailbox>> = RefCell::new(None);
    static NEXT_ID: RefCell<u64> = RefCell::new(1);
    static SCALE_FACTOR: std::cell::Cell<f64> = const { std::cell::Cell::new(2.0) };
    static LAST_POINTER: std::cell::Cell<(i16, i16)> = const { std::cell::Cell::new((0, 0)) };
    /// Cached screen height in points — avoids [NSScreen mainScreen] per event.
    static SCREEN_HEIGHT: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    /// Accumulated trackpad scroll delta (pixels). Emit X11 event when threshold exceeded.
    static SCROLL_ACCUM: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

fn get_scale_factor() -> f64 {
    SCALE_FACTOR.with(|sf| sf.get())
}

fn alloc_id() -> u64 {
    NEXT_ID.with(|id| {
        let v = *id.borrow();
        *id.borrow_mut() = v + 1;
        v
    })
}

/// Register the PSLXInputView class (custom NSView with NSTextInputClient for IME support).
/// Returns the class pointer. Safe to call multiple times (uses Once).
fn get_input_view_class() -> &'static AnyClass {
    use std::sync::Once;
    use std::ffi::CStr;
    static mut CLASS: *const AnyClass = std::ptr::null();
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let superclass = objc2::class!(NSView);
        let mut builder = ClassBuilder::new(c"PSLXInputView", superclass)
            .expect("Failed to create PSLXInputView class");

        // Add ivars
        builder.add_ivar::<u32>(c"x11WindowId");
        builder.add_ivar::<u8>(c"textInserted"); // flag: 1 if insertText was called during interpretKeyEvents

        // Use raw ObjC runtime to add methods (avoids HRTB lifetime issues with objc2)
        extern "C" {
            fn class_addMethod(
                cls: *mut std::ffi::c_void,
                sel: Sel,
                imp: *const std::ffi::c_void,
                types: *const std::ffi::c_char,
            ) -> bool;
            fn class_addProtocol(
                cls: *mut std::ffi::c_void,
                protocol: *const std::ffi::c_void,
            ) -> bool;
            fn objc_getProtocol(
                name: *const std::ffi::c_char,
            ) -> *const std::ffi::c_void;
        }

        let raw_cls = builder.register() as *const AnyClass as *mut std::ffi::c_void;

        unsafe {
            // Formally declare NSTextInputClient protocol conformance
            // so macOS creates an NSTextInputContext for IME support
            let proto = objc_getProtocol(c"NSTextInputClient".as_ptr() as _);
            if !proto.is_null() {
                class_addProtocol(raw_cls, proto);
            }
            // acceptsFirstResponder -> YES
            class_addMethod(raw_cls, objc2::sel!(acceptsFirstResponder),
                accepts_first_responder as *const std::ffi::c_void, c"B@:".as_ptr() as _);
            // keyDown:
            class_addMethod(raw_cls, objc2::sel!(keyDown:),
                view_key_down as *const std::ffi::c_void, c"v@:@".as_ptr() as _);
            // insertText:replacementRange:
            class_addMethod(raw_cls, objc2::sel!(insertText:replacementRange:),
                insert_text_replacement as *const std::ffi::c_void, c"v@:@{_NSRange=QQ}".as_ptr() as _);
            // hasMarkedText
            class_addMethod(raw_cls, objc2::sel!(hasMarkedText),
                has_marked_text as *const std::ffi::c_void, c"B@:".as_ptr() as _);
            // markedRange
            class_addMethod(raw_cls, objc2::sel!(markedRange),
                marked_range as *const std::ffi::c_void, c"{_NSRange=QQ}@:".as_ptr() as _);
            // selectedRange
            class_addMethod(raw_cls, objc2::sel!(selectedRange),
                selected_range as *const std::ffi::c_void, c"{_NSRange=QQ}@:".as_ptr() as _);
            // setMarkedText:selectedRange:replacementRange:
            class_addMethod(raw_cls, objc2::sel!(setMarkedText:selectedRange:replacementRange:),
                set_marked_text as *const std::ffi::c_void, c"v@:@{_NSRange=QQ}{_NSRange=QQ}".as_ptr() as _);
            // unmarkText
            class_addMethod(raw_cls, objc2::sel!(unmarkText),
                unmark_text as *const std::ffi::c_void, c"v@:".as_ptr() as _);
            // validAttributesForMarkedText
            class_addMethod(raw_cls, objc2::sel!(validAttributesForMarkedText),
                valid_attributes as *const std::ffi::c_void, c"@@:".as_ptr() as _);
            // attributedSubstringForProposedRange:actualRange:
            class_addMethod(raw_cls, objc2::sel!(attributedSubstringForProposedRange:actualRange:),
                attributed_substring as *const std::ffi::c_void, c"@@:{_NSRange=QQ}^{_NSRange=QQ}".as_ptr() as _);
            // characterIndexForPoint:
            class_addMethod(raw_cls, objc2::sel!(characterIndexForPoint:),
                char_index_for_point as *const std::ffi::c_void, c"Q@:{CGPoint=dd}".as_ptr() as _);
            // firstRectForCharacterRange:actualRange:
            class_addMethod(raw_cls, objc2::sel!(firstRectForCharacterRange:actualRange:),
                first_rect as *const std::ffi::c_void, c"{CGRect={CGPoint=dd}{CGSize=dd}}@:{_NSRange=QQ}^{_NSRange=QQ}".as_ptr() as _);
            // insertText: (single-arg NSResponder version — fallback when input context not active)
            class_addMethod(raw_cls, objc2::sel!(insertText:),
                insert_text_single as *const std::ffi::c_void, c"v@:@".as_ptr() as _);
            // doCommandBySelector: (non-text keys like arrows, delete)
            class_addMethod(raw_cls, objc2::sel!(doCommandBySelector:),
                do_command_by_selector as *const std::ffi::c_void, c"v@::".as_ptr() as _);

            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

// --- PSLXInputView method implementations ---

unsafe extern "C" fn accepts_first_responder(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

unsafe extern "C" fn view_key_down(this: *mut AnyObject, _sel: Sel, event: *mut AnyObject) {
    if this.is_null() || event.is_null() { return; }
    info!("PSLXInputView keyDown: called");

    // Clear the textInserted flag
    let flag_ivar = (*this).class().instance_variable(c"textInserted").unwrap();
    *flag_ivar.load_mut::<u8>(&mut *this) = 0;

    // Route key event through input method system
    let array: *mut AnyObject = msg_send![objc2::class!(NSArray), arrayWithObject: event];
    let _: () = msg_send![&*this, interpretKeyEvents: array];

    // If insertText: was NOT called, this is a non-text key (arrows, backspace, etc.)
    // Send it as a raw KeyPress event
    let inserted = *flag_ivar.load::<u8>(&*this);
    if inserted == 0 {
        let keycode: u16 = msg_send![&*event, keyCode];
        let state = get_modifier_state(&*event);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;

        let x11_id_ivar = (*this).class().instance_variable(c"x11WindowId").unwrap();
        let x11_id = *x11_id_ivar.load::<u32>(&*this) as crate::display::Xid;

        send_display_event(DisplayEvent::KeyPress {
            window: x11_id,
            keycode: (keycode as u8).wrapping_add(8),
            state,
            time,
        });
    }
}

unsafe extern "C" fn insert_text_replacement(this: *mut AnyObject, _sel: Sel, text: *mut AnyObject, _range: NSRange) {
    if this.is_null() { return; }

    // Set flag so view_key_down knows text was inserted
    let flag_ivar = (*this).class().instance_variable(c"textInserted").unwrap();
    *flag_ivar.load_mut::<u8>(&mut *this) = 1;
    // If we were composing (IME active), suppress the keyUp for the confirmation key
    let was_composing = crate::display::IME_COMPOSING.load(std::sync::atomic::Ordering::Relaxed);
    crate::display::IME_COMPOSING.store(false, std::sync::atomic::Ordering::Relaxed);
    if was_composing {
        crate::display::SUPPRESS_NEXT_KEYUP.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    if text.is_null() { return; }

    // text may be NSString or NSAttributedString
    let is_attr_str: bool = msg_send![&*text, isKindOfClass: objc2::class!(NSAttributedString)];
    let ns_string: *mut AnyObject = if is_attr_str {
        msg_send![&*text, string]
    } else {
        text
    };

    if ns_string.is_null() { return; }

    // Convert NSString to Rust String
    let utf8: *const std::os::raw::c_char = msg_send![&*ns_string, UTF8String];
    if utf8.is_null() { return; }
    let rust_str = match std::ffi::CStr::from_ptr(utf8).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return,
    };

    if rust_str.is_empty() { return; }

    // Get the X11 window ID from the ivar
    let ivar = (*this).class().instance_variable(c"x11WindowId").unwrap();
    let x11_id = *ivar.load::<u32>(&*this) as crate::display::Xid;

    debug!("IME insertText: '{}' for window 0x{:08x}", rust_str, x11_id);
    send_display_event(DisplayEvent::ImeCommit {
        window: x11_id,
        text: rust_str,
    });
}

/// insertText: (single-arg, NSResponder version) — called by interpretKeyEvents: when no
/// input context is active. Delegates to the 2-arg NSTextInputClient version.
unsafe extern "C" fn insert_text_single(this: *mut AnyObject, sel: Sel, text: *mut AnyObject) {
    info!("PSLXInputView insertText: (single-arg) called");
    insert_text_replacement(this, sel, text, NSRange { location: usize::MAX, length: 0 });
}

/// doCommandBySelector: — called by interpretKeyEvents: for non-text keys (arrows, delete, etc.)
/// We let these fall through; the KeyPress fallback in view_key_down handles them.
unsafe extern "C" fn do_command_by_selector(_this: *mut AnyObject, _sel: Sel, _a_selector: Sel) {
    // No-op: non-text keys handled by KeyPress fallback in view_key_down
}

unsafe extern "C" fn has_marked_text(_this: *mut AnyObject, _sel: Sel) -> Bool {
    // Always return NO — we don't render marked text ourselves.
    // macOS IME still shows the candidate window and calls insertText on commit.
    Bool::NO
}

unsafe extern "C" fn marked_range(_this: *mut AnyObject, _sel: Sel) -> NSRange {
    NSRange { location: usize::MAX, length: 0 } // NSNotFound
}

unsafe extern "C" fn selected_range(_this: *mut AnyObject, _sel: Sel) -> NSRange {
    NSRange { location: 0, length: 0 }
}

unsafe extern "C" fn set_marked_text(this: *mut AnyObject, _sel: Sel, _text: *mut AnyObject, _sel_range: NSRange, _repl_range: NSRange) {
    if this.is_null() { return; }
    let flag_ivar = (*this).class().instance_variable(c"textInserted").unwrap();
    *flag_ivar.load_mut::<u8>(&mut *this) = 1;
    crate::display::IME_COMPOSING.store(true, std::sync::atomic::Ordering::Relaxed);
    debug!("IME setMarkedText: preedit active, suppressing raw KeyPress+KeyRelease");
}

unsafe extern "C" fn unmark_text(_this: *mut AnyObject, _sel: Sel) {
    crate::display::IME_COMPOSING.store(false, std::sync::atomic::Ordering::Relaxed);
}

unsafe extern "C" fn valid_attributes(_this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    // Return empty NSArray
    msg_send![objc2::class!(NSArray), array]
}

unsafe extern "C" fn attributed_substring(_this: *mut AnyObject, _sel: Sel, _range: NSRange, _actual: *mut NSRange) -> *mut AnyObject {
    std::ptr::null_mut()
}

unsafe extern "C" fn char_index_for_point(_this: *mut AnyObject, _sel: Sel, _point: NSPoint) -> usize {
    0
}

unsafe extern "C" fn first_rect(this: *mut AnyObject, _sel: Sel, _range: NSRange, _actual: *mut NSRange) -> NSRect {
    if this.is_null() {
        return NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
    }

    // Read global IME cursor position (X11 pixel coords in native window space)
    // X11 pixels map 1:1 to points (no HiDPI scaling for X11 coordinates)
    let spot_x = crate::display::IME_SPOT_X.load(std::sync::atomic::Ordering::Relaxed) as f64;
    let spot_y = crate::display::IME_SPOT_Y.load(std::sync::atomic::Ordering::Relaxed) as f64;
    let line_h = crate::display::IME_SPOT_LINE_H.load(std::sync::atomic::Ordering::Relaxed) as f64;

    let window: *mut AnyObject = msg_send![&*this, window];
    if window.is_null() {
        return NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
    }

    // Convert X11 coords (top-left origin, Y↓) → NSView coords (bottom-left origin, Y↑)
    // spot_y is the TOP of the text line in X11 coords (from PutImage dst_y or ImageText y-ascent).
    // The text line occupies spot_y .. spot_y + line_h in X11 coords.
    let bounds: NSRect = msg_send![&*this, bounds];
    let view_x = spot_x;
    // Bottom of text line in NSView = bounds.height - (spot_y + line_h)
    let view_y = bounds.size.height - spot_y - line_h;

    // Rect origin is bottom-left corner in NSView. Candidate window appears below this rect.
    let view_rect = NSRect::new(
        NSPoint::new(view_x, view_y),
        NSSize::new(1.0, line_h),
    );

    // View coords → window coords → screen coords (proper macOS conversion)
    let win_rect: NSRect = msg_send![&*this, convertRect: view_rect toView: std::ptr::null::<AnyObject>()];
    let screen_rect: NSRect = msg_send![window, convertRectToScreen: win_rect];

    log::info!("firstRect: X11({},{}) → view({},{}) h={} → screen({},{})",
        spot_x as i32, spot_y as i32, view_x as i32, view_y as i32, bounds.size.height as i32,
        screen_rect.origin.x as i32, screen_rect.origin.y as i32);

    screen_rect
}

extern "C" fn timer_callback(_timer: *mut c_void, _info: *mut c_void) {
    // Timer fires even during macOS live resize (via kCFRunLoopCommonModes).
    // Process commands here so windows update in real-time during resize drag.
    process_commands();
}

/// Convert a macOS NSWindow position to X11 screen coordinates.
/// macOS: origin at bottom-left, Y up.  X11: origin at top-left, Y down.
/// Returns (x11_x, x11_y) of the content area's top-left corner.
fn macos_frame_to_x11_pos(window: &NSWindow) -> (i16, i16) {
    unsafe {
        let frame: NSRect = msg_send![&**window, frame];
        let content_h = if let Some(view) = window.contentView() {
            let bounds: NSRect = msg_send![&*view, bounds];
            bounds.size.height
        } else {
            frame.size.height
        };
        let screen: *mut AnyObject = msg_send![objc2::class!(NSScreen), mainScreen];
        let screen_h = if !screen.is_null() {
            let sf: NSRect = msg_send![screen, frame];
            sf.size.height
        } else {
            956.0
        };
        // Top-left of content area in X11 coords:
        // x = frame.origin.x
        // y = screen_height - (frame.origin.y + frame.size.height - title_bar) - ...
        // title_bar_h = frame.size.height - content_h
        // content top in macOS Y = frame.origin.y + frame.size.height - title_bar_h
        //                        = frame.origin.y + content_h
        // In X11 Y (top-down) = screen_h - (frame.origin.y + content_h)
        // BUT: frame.origin.y + content_h = bottom of frame + content height
        //      = top of content area in macOS coords
        // Wait: frame.origin.y is the bottom of the window.
        // Bottom of content = frame.origin.y
        // Top of content = frame.origin.y + content_h
        // In X11 (top-down): y = screen_h - top_of_content = screen_h - (frame.origin.y + content_h)
        let x11_x = frame.origin.x as i16;
        let x11_y = (screen_h - frame.origin.y - content_h) as i16;
        (x11_x, x11_y)
    }
}

/// Check all managed windows for size/position changes.
/// Size change: create new IOSurface, copy old content, send ConfigureNotify + Expose.
/// Position change: send ConfigureNotify with new position.
fn check_window_resizes() {
    // Collect changes without holding WINDOWS borrow across send_display_event
    struct WindowChange {
        win_id: u64,
        x11_id: crate::display::Xid,
        new_w: u16,
        new_h: u16,
        new_x: i16,
        new_y: i16,
        resized: bool,
        moved: bool,
    }

    let changes: Vec<WindowChange> = WINDOWS.with(|w| {
        let ws = w.borrow();
        let mut result = Vec::new();
        for (id, info) in ws.iter() {
            // Get current content view size in points (= X11 pixels)
            let (cur_w, cur_h) = unsafe {
                if let Some(view) = info.window.contentView() {
                    let bounds: NSRect = msg_send![&*view, bounds];
                    (bounds.size.width as u16, bounds.size.height as u16)
                } else {
                    continue;
                }
            };
            // Get current X11 position from macOS window frame
            let (cur_x, cur_y) = macos_frame_to_x11_pos(&info.window);

            let resized = (cur_w != info.width || cur_h != info.height) && cur_w > 0 && cur_h > 0;
            let moved = cur_x != info.x11_x || cur_y != info.x11_y;

            if resized || moved {
                result.push(WindowChange {
                    win_id: *id,
                    x11_id: info.x11_id,
                    new_w: if resized { cur_w } else { info.width },
                    new_h: if resized { cur_h } else { info.height },
                    new_x: cur_x,
                    new_y: cur_y,
                    resized,
                    moved,
                });
            }
        }
        result
    });

    for change in changes {
        if change.resized {
            info!("Window {} resize detected: -> {}x{}", change.win_id, change.new_w, change.new_h);

            WINDOWS.with(|w| {
                let mut ws = w.borrow_mut();
                if let Some(info) = ws.get_mut(&change.win_id) {
                    let old_w = info.width;
                    let old_h = info.height;

                    // Create new IOSurface
                    let new_surface = create_iosurface(change.new_w, change.new_h);
                    if new_surface.is_null() {
                        log::error!("Failed to create new IOSurface for resize");
                        return;
                    }

                    // Copy old content to new surface, clearing new area
                    unsafe {
                        IOSurfaceLock(info.surface, 0, std::ptr::null_mut());
                        IOSurfaceLock(new_surface, 0, std::ptr::null_mut());

                        let old_base = IOSurfaceGetBaseAddress(info.surface) as *const u8;
                        let old_stride = IOSurfaceGetBytesPerRow(info.surface);
                        let new_base = IOSurfaceGetBaseAddress(new_surface) as *mut u8;
                        let new_stride = IOSurfaceGetBytesPerRow(new_surface);

                        // Clear entire new surface to white (xterm background)
                        let total_new = new_stride * change.new_h as usize;
                        std::ptr::write_bytes(new_base, 0xFF, total_new);

                        let copy_rows = (old_h as usize).min(change.new_h as usize);
                        let copy_bytes_per_row = (old_w as usize * 4).min(change.new_w as usize * 4)
                            .min(old_stride).min(new_stride);

                        for row in 0..copy_rows {
                            std::ptr::copy_nonoverlapping(
                                old_base.add(row * old_stride),
                                new_base.add(row * new_stride),
                                copy_bytes_per_row,
                            );
                        }

                        IOSurfaceUnlock(new_surface, 0, std::ptr::null_mut());
                        IOSurfaceUnlock(info.surface, 0, std::ptr::null_mut());
                    }

                    // Replace surface and update dimensions
                    let old_surface = info.surface;
                    info.surface = new_surface;
                    info.width = change.new_w;
                    info.height = change.new_h;
                    unsafe { CFRelease(old_surface as *const c_void); }

                    // Flush CGImage to layer so resize is visible immediately
                    flush_window(info);
                    ca_transaction_flush();

                    debug!("Window {} resized: {}x{} -> {}x{}", change.win_id, old_w, old_h, change.new_w, change.new_h);
                }
            });

            // Send Expose for resize (content needs redraw)
            send_display_event(DisplayEvent::Expose {
                window: change.x11_id,
                x: 0,
                y: 0,
                width: change.new_w,
                height: change.new_h,
                count: 0,
            });
        }

        // Update cached position in WindowInfo
        if change.moved || change.resized {
            WINDOWS.with(|w| {
                let mut ws = w.borrow_mut();
                if let Some(info) = ws.get_mut(&change.win_id) {
                    info.x11_x = change.new_x;
                    info.x11_y = change.new_y;
                }
            });

            // Send ConfigureNotify with actual screen position
            send_display_event(DisplayEvent::ConfigureNotify {
                window: change.x11_id,
                x: change.new_x,
                y: change.new_y,
                width: change.new_w,
                height: change.new_h,
            });
        }
    }
}

fn process_commands() {
    // 1. Process non-render commands from channel (CreateWindow, ShowWindow, etc.)
    let cmds: Vec<DisplayCommand> = CMD_RX.with(|rx| {
        rx.borrow().as_ref().map_or_else(Vec::new, |rx| rx.try_iter().collect())
    });
    for cmd in cmds {
        handle_command(cmd);
    }

    // 1.5. Send global pointer + per-window MotionNotify when cursor moves
    {
        let (rx, ry) = get_screen_mouse_location();
        let last = LAST_POINTER.with(|lp| lp.get());
        if rx != last.0 || ry != last.1 {
            LAST_POINTER.with(|lp| lp.set((rx, ry)));
            send_display_event(DisplayEvent::GlobalPointerUpdate { root_x: rx, root_y: ry });

            // Send MotionNotify to each window with correct window-relative coords.
            // macOS knows each window's actual screen position, so coords are accurate.
            let mouse_loc: NSPoint = unsafe { msg_send![objc2::class!(NSEvent), mouseLocation] };
            // Get timestamp once (not per-window)
            let time = unsafe {
                let pi: *mut AnyObject = msg_send![objc2::class!(NSProcessInfo), processInfo];
                let uptime: f64 = msg_send![pi, systemUptime];
                (uptime * 1000.0) as u32
            };
            WINDOWS.with(|w| {
                let ws = w.borrow();
                for (_id, info) in ws.iter() {
                    let frame: NSRect = unsafe { msg_send![&*info.window, frame] };
                    let content_h = if let Some(view) = info.window.contentView() {
                        let bounds: NSRect = unsafe { msg_send![&*view, bounds] };
                        bounds.size.height
                    } else {
                        frame.size.height
                    };
                    let content_origin_x = frame.origin.x;
                    let content_origin_y = frame.origin.y;
                    let win_x = (mouse_loc.x - content_origin_x) as i16;
                    let win_y = (content_origin_y + content_h - mouse_loc.y) as i16;

                    send_display_event(DisplayEvent::MotionNotify {
                        window: info.x11_id,
                        x: win_x,
                        y: win_y,
                        root_x: rx,
                        root_y: ry,
                        state: 0,
                        time,
                    });
                }
            });
        }
    }

    // 1.6. Detect window resizes — compare content view size to stored IOSurface size
    check_window_resizes();

    // 2. Drain render mailbox — atomically take all pending commands per window
    let render_batches: Vec<(u64, Vec<crate::display::RenderCommand>)> = RENDER_MAILBOX.with(|mb| {
        let mb = mb.borrow();
        if let Some(ref mailbox) = *mb {
            // Take all entries, leaving empty vecs
            let mut batches = Vec::new();
            for mut entry in mailbox.iter_mut() {
                if !entry.value().is_empty() {
                    let commands = std::mem::take(entry.value_mut());
                    batches.push((*entry.key(), commands));
                }
            }
            batches
        } else {
            Vec::new()
        }
    });

    // 3. Render merged batches — one lock/render/flush per window
    if !render_batches.is_empty() {
        WINDOWS.with(|w| {
            let ws = w.borrow();
            for (win_id, commands) in render_batches {
                if let Some(info) = ws.get(&win_id) {
                    // Process all commands — no frame coalescing.
                    // Each command is a simple buffer operation (memcpy/fill),
                    // fast enough even for thousands of commands per frame.

                    #[cfg(debug_assertions)]
                    if commands.len() > 5000 {
                        debug!("win={} coalesce to {}", win_id, commands.len());
                    }

                    let lock_result = unsafe { IOSurfaceLock(info.surface, 0, std::ptr::null_mut()) };
                    if lock_result != 0 { continue; }

                    let base = unsafe { IOSurfaceGetBaseAddress(info.surface) };
                    let bytes_per_row = unsafe { IOSurfaceGetBytesPerRow(info.surface) };
                    let buf_len = bytes_per_row * info.height as usize;
                    let buffer = unsafe {
                        std::slice::from_raw_parts_mut(base as *mut u8, buf_len)
                    };
                    let width = info.width as u32;
                    let height = info.height as u32;
                    let stride = bytes_per_row as u32;
                    for c in &commands {
                        render_to_buffer(buffer, width, height, stride, c);
                    }

                    unsafe { IOSurfaceUnlock(info.surface, 0, std::ptr::null_mut()); }
                    flush_window(info);
                }
            }
            // Force immediate compositing so resize updates are visible in real-time
            ca_transaction_flush();
        });
    }
}

fn handle_command(cmd: DisplayCommand) {
    match cmd {
        DisplayCommand::CreateWindow {
            x11_id, x: _, y: _, width, height, title, override_redirect, reply,
        } => {
            let id = alloc_id();
            let mtm = MainThreadMarker::new().unwrap();

            let style = if override_redirect {
                NSWindowStyleMask::Borderless
            } else {
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable
            };

            // X11 dimensions treated as points (not physical pixels) so text remains legible.
            // On Retina displays, the IOSurface is pixel-doubled by CALayer automatically.
            let mut pt_w = width as f64;
            let mut pt_h = height as f64;
            unsafe {
                let screen: *mut AnyObject = msg_send![objc2::class!(NSScreen), mainScreen];
                if !screen.is_null() {
                    let visible: NSRect = msg_send![screen, visibleFrame];
                    let max_w = visible.size.width;
                    let max_h = visible.size.height - 30.0; // title bar
                    if pt_w > max_w || pt_h > max_h {
                        let fit = (max_w / pt_w).min(max_h / pt_h);
                        pt_w *= fit;
                        pt_h *= fit;
                    }
                }
            }
            // Center window on screen
            let origin = unsafe {
                let screen: *mut AnyObject = msg_send![objc2::class!(NSScreen), mainScreen];
                if !screen.is_null() {
                    let visible: NSRect = msg_send![screen, visibleFrame];
                    NSPoint::new(
                        visible.origin.x + (visible.size.width - pt_w) / 2.0,
                        visible.origin.y + (visible.size.height - pt_h) / 2.0,
                    )
                } else {
                    NSPoint::new(100.0, 100.0)
                }
            };
            let rect = NSRect::new(origin, NSSize::new(pt_w, pt_h));

            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    mtm.alloc(),
                    rect,
                    style,
                    objc2_app_kit::NSBackingStoreType(2),
                    false,
                )
            };

            window.setTitle(&NSString::from_str(&title));

            // Accept mouse moved events and make key window
            unsafe {
                let _: () = msg_send![&*window, setAcceptsMouseMovedEvents: true];
            }

            // IOSurface for zero-copy compositing via CoreAnimation
            let surface = create_iosurface(width, height);
            if surface.is_null() {
                log::error!("Failed to create IOSurface for window {}", id);
                return;
            }

            // Calculate initial X11 screen position from actual macOS window position
            let (x11_x, x11_y) = macos_frame_to_x11_pos(&window);

            // Replace contentView with our custom PSLXInputView for IME support,
            // then enable layer-backing for CGImage-based rendering.
            unsafe {
                let input_cls = get_input_view_class();
                let content_rect: NSRect = if let Some(v) = window.contentView() {
                    msg_send![&*v, bounds]
                } else {
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(pt_w, pt_h))
                };
                let alloc_obj: *mut AnyObject = msg_send![input_cls, alloc];
                let input_view: *mut AnyObject = msg_send![alloc_obj, initWithFrame: content_rect];
                if !input_view.is_null() {
                    // Store the X11 window ID in the ivar
                    let ivar = (*input_view).class().instance_variable(c"x11WindowId").unwrap();
                    *ivar.load_mut::<u32>(&mut *input_view) = x11_id as u32;
                    let _: () = msg_send![&*window, setContentView: input_view];
                }

                if let Some(view) = window.contentView() {
                    let _: () = msg_send![&*view, setWantsLayer: true];
                    let layer: *mut AnyObject = msg_send![&*view, layer];
                    if !layer.is_null() {
                        // Pin content to top-left during resize
                        let gravity = NSString::from_str("topLeft");
                        let _: () = msg_send![layer, setContentsGravity: &*gravity];
                        // 1:1 pixel mapping (no HiDPI scaling)
                        let _: () = msg_send![layer, setContentsScale: 1.0_f64];
                        // Clip layer contents to view bounds
                        let _: () = msg_send![layer, setMasksToBounds: true];
                    }
                    // Prevent AppKit from overwriting layer contents during redraw.
                    // NSViewLayerContentsRedrawNever = 0
                    let _: () = msg_send![&*view, setLayerContentsRedrawPolicy: 0_isize];

                    // Make the custom view the first responder for keyboard events
                    let _: () = msg_send![&*window, makeFirstResponder: &*view];
                }
            }

            WINDOWS.with(|w| {
                w.borrow_mut().insert(id, WindowInfo { window, surface, width, height, x11_id, x11_x, x11_y });
            });

            info!("Created window {} for X11 0x{:08X} ({}x{}) [IOSurface]", id, x11_id, width, height);
            let _ = reply.send(NativeWindowHandle { id });
        }

        DisplayCommand::ShowWindow { handle } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow().get(&handle.id) {
                    // Activate app and bring window to front
                    let mtm = MainThreadMarker::new().unwrap();
                    let app = NSApplication::sharedApplication(mtm);
                    unsafe { let _: () = msg_send![&*app, activateIgnoringOtherApps: true]; }
                    info.window.makeKeyAndOrderFront(None);
                }
            });
        }

        DisplayCommand::HideWindow { handle } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow().get(&handle.id) {
                    info.window.orderOut(None);
                }
            });
        }

        DisplayCommand::RenderBatch { .. } => {
            // Handled in process_commands() batch path — should not reach here
        }

        DisplayCommand::DestroyWindow { handle } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().remove(&handle.id) {
                    // surface released by Drop impl
                    info.window.close();
                }
            });
        }

        DisplayCommand::SetWindowTitle { handle, title } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow().get(&handle.id) {
                    info.window.setTitle(&NSString::from_str(&title));
                }
            });
        }

        DisplayCommand::MoveResizeWindow { handle, x, y, width, height } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow().get(&handle.id) {
                    let screen_h = info.window.screen()
                        .map(|s| s.frame().size.height)
                        .unwrap_or(956.0);
                    let pt_w = width as f64;
                    let pt_h = height as f64;
                    // Convert X11 top-left to macOS bottom-left coordinates
                    let mac_y = screen_h - y as f64 - pt_h;
                    let frame = NSRect::new(
                        NSPoint::new(x as f64, mac_y),
                        NSSize::new(pt_w, pt_h),
                    );
                    info.window.setFrame_display(frame, true);
                }
            });
        }

        DisplayCommand::Shutdown => {
            let mtm = MainThreadMarker::new().unwrap();
            NSApplication::sharedApplication(mtm).terminate(None);
        }

        _ => {
            debug!("Unhandled display command");
        }
    }
}

// CoreAnimation FFI
extern "C" {
    // CATransaction: force immediate layer compositing
    fn objc_getClass(name: *const u8) -> *mut AnyObject;
}

fn ca_transaction_flush() {
    unsafe {
        let cls = objc_getClass(b"CATransaction\0".as_ptr());
        if !cls.is_null() {
            let _: () = msg_send![cls, flush];
        }
    }
}

// CGImage-based flush removed — IOSurface is used directly as layer.contents.
// Eliminates per-frame buffer copy (previously ~3.2MB memcpy + CGImage alloc per window).

/// Create a CGImage from the IOSurface buffer and set it as the layer's contents.
/// IOSurface direct as layer.contents is unreliable (AppKit overwrites it),
/// so we create a CGImage snapshot each frame instead.
fn flush_window(info: &WindowInfo) {
    unsafe {
        let w = info.width as usize;
        let h = info.height as usize;
        if w == 0 || h == 0 { return; }

        // Lock IOSurface to read pixels
        IOSurfaceLock(info.surface, 1, std::ptr::null_mut()); // read-only lock
        let base = IOSurfaceGetBaseAddress(info.surface);
        let bpr = IOSurfaceGetBytesPerRow(info.surface);

        // Create CGImage from IOSurface pixel data
        extern "C" {
            fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
            fn CGColorSpaceRelease(cs: *mut c_void);
            fn CGDataProviderCreateWithData(
                info: *mut c_void,
                data: *const c_void,
                size: usize,
                releaseData: *const c_void,
            ) -> *mut c_void;
            fn CGDataProviderRelease(provider: *mut c_void);
            fn CGImageCreate(
                width: usize, height: usize,
                bitsPerComponent: usize, bitsPerPixel: usize,
                bytesPerRow: usize,
                space: *mut c_void,
                bitmapInfo: u32,
                provider: *mut c_void,
                decode: *const f64,
                shouldInterpolate: bool,
                intent: i32,
            ) -> *mut c_void;
            fn CGImageRelease(image: *mut c_void);
        }

        let cs = CGColorSpaceCreateDeviceRGB();
        let data_len = bpr * h;
        let provider = CGDataProviderCreateWithData(
            std::ptr::null_mut(),
            base,
            data_len,
            std::ptr::null(),
        );

        // kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst = 0x2002
        // This matches BGRA with premultiplied alpha (our pixel format)
        let bitmap_info: u32 = (2 << 12) | 2; // kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst
        let image = CGImageCreate(
            w, h, 8, 32, bpr,
            cs,
            bitmap_info,
            provider,
            std::ptr::null(),
            false,
            0, // kCGRenderingIntentDefault
        );

        IOSurfaceUnlock(info.surface, 1, std::ptr::null_mut());

        if !image.is_null() {
            if let Some(view) = info.window.contentView() {
                let layer: *mut AnyObject = msg_send![&*view, layer];
                if !layer.is_null() {
                    let _: () = msg_send![layer, setContents: image as *mut AnyObject];
                }
            }
        }

        CGImageRelease(image);
        CGDataProviderRelease(provider);
        CGColorSpaceRelease(cs);
    }
}

// NSEvent type constants
const NS_LEFT_MOUSE_DOWN: usize = 1;
const NS_LEFT_MOUSE_UP: usize = 2;
const NS_RIGHT_MOUSE_DOWN: usize = 3;
const NS_RIGHT_MOUSE_UP: usize = 4;
const NS_MOUSE_MOVED: usize = 5;
const NS_LEFT_MOUSE_DRAGGED: usize = 6;
const NS_RIGHT_MOUSE_DRAGGED: usize = 7;
const NS_KEY_DOWN: usize = 10;
const NS_KEY_UP: usize = 11;
const NS_SCROLL_WHEEL: usize = 22;
const NS_OTHER_MOUSE_DOWN: usize = 25;
const NS_OTHER_MOUSE_UP: usize = 26;
const NS_OTHER_MOUSE_DRAGGED: usize = 27;

/// Run the Cocoa application on the main thread.
/// This function blocks — it IS the main run loop.
pub fn run_cocoa_app(
    cmd_rx: Receiver<DisplayCommand>,
    evt_tx: Sender<DisplayEvent>,
    render_mailbox: crate::display::RenderMailbox,
) {
    let mtm = MainThreadMarker::new()
        .expect("Must be called from the main thread");

    let app = NSApplication::sharedApplication(mtm);

    // Make the app a regular app (shows in dock, can own windows)
    unsafe {
        let _: bool = msg_send![&*app, setActivationPolicy: 0i64];
    }

    // Store channels and render mailbox in thread-local storage
    CMD_RX.with(|rx| *rx.borrow_mut() = Some(cmd_rx));
    EVT_TX.with(|tx| *tx.borrow_mut() = Some(evt_tx));
    RENDER_MAILBOX.with(|mb| *mb.borrow_mut() = Some(render_mailbox));

    // Create a polling timer at ~60fps to drain the command channel
    unsafe {
        let now = CFAbsoluteTimeGetCurrent();
        let timer = CFRunLoopTimerCreate(
            std::ptr::null(), now + 0.016, 0.016,
            0, 0, timer_callback, std::ptr::null_mut(),
        );
        CFRunLoopAddTimer(CFRunLoopGetCurrent(), timer, kCFRunLoopCommonModes);
    }

    // Detect backing scale factor and cache screen height
    unsafe {
        let screen: *mut AnyObject = msg_send![objc2::class!(NSScreen), mainScreen];
        if !screen.is_null() {
            let scale: f64 = msg_send![screen, backingScaleFactor];
            SCALE_FACTOR.with(|sf| sf.set(scale));
            let frame: NSRect = msg_send![screen, frame];
            SCREEN_HEIGHT.with(|sh| sh.set(frame.size.height));
            info!("Backing scale factor: {}, screen height: {}", scale, frame.size.height);
        }
    }

    info!("Cocoa application initialized with display command polling");

    unsafe {
        let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
        let _: () = msg_send![&*app, finishLaunching];
    }

    // Manual event loop — intercept mouse/key events before dispatch
    // Process display commands (rendering) alongside NSEvents
    // Note: timer_callback also calls process_commands() during live resize
    loop {
        process_commands();

        // Drain available NSEvents; wait up to 16ms if no commands pending
        let event: Option<Retained<AnyObject>> = unsafe {
            let timeout_date: *mut AnyObject = msg_send![
                objc2::class!(NSDate),
                dateWithTimeIntervalSinceNow: 0.016f64
            ];
            let ns_event: *mut AnyObject = msg_send![
                &*app,
                nextEventMatchingMask: u64::MAX,
                untilDate: timeout_date,
                inMode: kCFRunLoopDefaultMode as *const AnyObject,
                dequeue: true
            ];
            if ns_event.is_null() {
                None
            } else {
                Some(Retained::retain(ns_event).unwrap())
            }
        };

        if let Some(ref event) = event {
            let event_ref: &AnyObject = &**event;
            let event_type: usize = unsafe { msg_send![event_ref, type] };
            if event_type == NS_KEY_DOWN || event_type == NS_KEY_UP {
                debug!("NSEvent type={} (key event)", event_type);
            }
            handle_ns_event(event_ref, event_type);

            // Forward event to NSApp for normal processing
            unsafe {
                let _: () = msg_send![&*app, sendEvent: event_ref];
            }
        }
    }
}

/// Convert NSEvent to DisplayEvent and send to the X11 server thread.
fn handle_ns_event(event: &AnyObject, event_type: usize) {
    // Try to get the event's window first
    let ns_window: *mut AnyObject = unsafe { msg_send![event, window] };

    // Find which of our managed windows this event belongs to
    let (x11_id, win_width, win_height) = if !ns_window.is_null() {
        // Direct window match
        WINDOWS.with(|w| {
            let ws = w.borrow();
            for (_id, info) in ws.iter() {
                let our_window: *const AnyObject = &*info.window as *const _ as *const AnyObject;
                if our_window as usize == ns_window as usize {
                    return Some((info.x11_id, info.width, info.height));
                }
            }
            None
        }).unwrap_or_default()
    } else {
        // Window is nil — find the window under the mouse cursor using screen coords
        let mouse_loc: NSPoint = unsafe { msg_send![objc2::class!(NSEvent), mouseLocation] };
        WINDOWS.with(|w| {
            let ws = w.borrow();
            for (_id, info) in ws.iter() {
                let frame: NSRect = unsafe { msg_send![&*info.window, frame] };
                if mouse_loc.x >= frame.origin.x
                    && mouse_loc.x < frame.origin.x + frame.size.width
                    && mouse_loc.y >= frame.origin.y
                    && mouse_loc.y < frame.origin.y + frame.size.height
                {
                    return Some((info.x11_id, info.width, info.height));
                }
            }
            None
        }).unwrap_or_default()
    };

    if x11_id == 0 {
        if event_type == NS_KEY_DOWN || event_type == NS_KEY_UP {
            debug!("Key event with x11_id=0 ns_window={:?}", ns_window);
        }
        return; // Not one of our windows
    }

    let time = unsafe {
        let ts: f64 = msg_send![event, timestamp];
        (ts * 1000.0) as u32
    };

    match event_type {
        NS_LEFT_MOUSE_DOWN | NS_RIGHT_MOUSE_DOWN | NS_OTHER_MOUSE_DOWN => {
            let (x, y) = get_event_location(event, x11_id, win_width, win_height);
            let (root_x, root_y) = get_screen_mouse_location();
            let button = match event_type {
                NS_LEFT_MOUSE_DOWN => 1,
                NS_RIGHT_MOUSE_DOWN => 3,
                _ => 2,
            };
            let state = get_modifier_state(event);
            debug!("ButtonPress: window=0x{:08x} button={} x={} y={} root=({},{})", x11_id, button, x, y, root_x, root_y);
            send_display_event(DisplayEvent::ButtonPress {
                window: x11_id, button, x, y, root_x, root_y, state, time,
            });
        }
        NS_LEFT_MOUSE_UP | NS_RIGHT_MOUSE_UP | NS_OTHER_MOUSE_UP => {
            let (x, y) = get_event_location(event, x11_id, win_width, win_height);
            let (root_x, root_y) = get_screen_mouse_location();
            let button = match event_type {
                NS_LEFT_MOUSE_UP => 1,
                NS_RIGHT_MOUSE_UP => 3,
                _ => 2,
            };
            let state = get_modifier_state(event);
            send_display_event(DisplayEvent::ButtonRelease {
                window: x11_id, button, x, y, root_x, root_y, state, time,
            });
        }
        NS_MOUSE_MOVED | NS_LEFT_MOUSE_DRAGGED | NS_RIGHT_MOUSE_DRAGGED | NS_OTHER_MOUSE_DRAGGED => {
            let (x, y) = get_event_location(event, x11_id, win_width, win_height);
            let (root_x, root_y) = get_screen_mouse_location();
            let state = get_modifier_state(event);
            send_display_event(DisplayEvent::MotionNotify {
                window: x11_id, x, y, root_x, root_y, state, time,
            });
        }
        NS_KEY_DOWN => {
            // Handled by PSLXInputView's keyDown: via sendEvent: dispatch.
            // Do NOT send KeyPress here — the custom view handles both
            // text input (via insertText:) and non-text keys (arrows, etc.).
        }
        NS_KEY_UP => {
            // Suppress KeyRelease while IME is composing OR for the confirmation key after commit
            if crate::display::IME_COMPOSING.load(std::sync::atomic::Ordering::Relaxed)
                || crate::display::SUPPRESS_NEXT_KEYUP.swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                // Swallow this keyUp — either mid-composition or the Enter/Space that committed IME text
            } else {
                let keycode: u16 = unsafe { msg_send![event, keyCode] };
                let state = get_modifier_state(event);
                send_display_event(DisplayEvent::KeyRelease {
                    window: x11_id, keycode: (keycode as u8).wrapping_add(8), state, time,
                });
            }
        }
        NS_SCROLL_WHEEL => {
            let (x, y) = get_event_location(event, x11_id, win_width, win_height);
            let (root_x, root_y) = get_screen_mouse_location();
            let state = get_modifier_state(event);
            let has_precise: bool = unsafe { msg_send![event, hasPreciseScrollingDeltas] };
            if has_precise {
                // Trackpad: accumulate pixel deltas, emit X11 scroll when threshold hit.
                // macOS sends many small deltas (2-5px each); X11 button 4/5 = ~3 lines.
                let delta_y: f64 = unsafe { msg_send![event, scrollingDeltaY] };
                let accum = SCROLL_ACCUM.with(|a| {
                    let mut v = a.get() + delta_y;
                    // Reset accumulator if direction changed
                    if (v > 0.0) != (delta_y > 0.0) && delta_y != 0.0 {
                        v = delta_y;
                    }
                    a.set(v);
                    v
                });
                let threshold = 80.0; // pixels per X11 scroll click
                let clicks = (accum.abs() / threshold) as u32;
                if clicks > 0 {
                    let button = if accum > 0.0 { 4u8 } else { 5u8 };
                    // Subtract consumed delta from accumulator
                    SCROLL_ACCUM.with(|a| {
                        let remaining = accum - accum.signum() * (clicks as f64 * threshold);
                        a.set(remaining);
                    });
                    for _ in 0..clicks.min(3) {
                        send_display_event(DisplayEvent::ButtonPress {
                            window: x11_id, button, x, y, root_x, root_y, state, time,
                        });
                        send_display_event(DisplayEvent::ButtonRelease {
                            window: x11_id, button, x, y, root_x, root_y, state, time,
                        });
                    }
                }
            } else {
                // Discrete mouse wheel: scrollingDeltaY is ±1.0 per click
                let delta_y: f64 = unsafe { msg_send![event, scrollingDeltaY] };
                if delta_y.abs() > 0.1 {
                    let button = if delta_y > 0.0 { 4u8 } else { 5u8 };
                    send_display_event(DisplayEvent::ButtonPress {
                        window: x11_id, button, x, y, root_x, root_y, state, time,
                    });
                    send_display_event(DisplayEvent::ButtonRelease {
                        window: x11_id, button, x, y, root_x, root_y, state, time,
                    });
                }
            }
        }
        _ => {}
    }
}

/// Get the current mouse position in X11 root window (screen) coordinates.
/// Returns logical points (not physical pixels) to match our point-based IOSurface rendering.
fn get_screen_mouse_location() -> (i16, i16) {
    unsafe {
        let mouse_loc: NSPoint = msg_send![objc2::class!(NSEvent), mouseLocation];
        // Use cached screen height — avoids [NSScreen mainScreen] ObjC call per event.
        let sh = SCREEN_HEIGHT.with(|sh| sh.get());
        let x = mouse_loc.x as i16;
        let y = (sh - mouse_loc.y) as i16;
        (x, y)
    }
}

fn get_event_location(event: &AnyObject, x11_id: crate::display::Xid, win_width: u16, win_height: u16) -> (i16, i16) {
    let ns_window: *mut AnyObject = unsafe { msg_send![event, window] };
    if !ns_window.is_null() {
        // locationInWindow returns coords in points (origin at bottom-left of content view)
        let point: NSPoint = unsafe { msg_send![event, locationInWindow] };
        let view: *mut AnyObject = unsafe { msg_send![ns_window, contentView] };
        if !view.is_null() {
            let bounds: NSRect = unsafe { msg_send![view, bounds] };
            let vh = bounds.size.height;
            // With topLeft gravity: IOSurface pixels map 1:1 to points, pinned at top-left
            // macOS Y is bottom-up, X11 Y is top-down
            let x = point.x.clamp(0.0, win_width as f64 - 1.0) as i16;
            let y = (vh - point.y).clamp(0.0, win_height as f64 - 1.0) as i16;
            return (x, y);
        }
        // Fallback
        let x = point.x.clamp(0.0, win_width as f64 - 1.0) as i16;
        let y = (win_height as f64 - point.y).clamp(0.0, win_height as f64 - 1.0) as i16;
        (x, y)
    } else {
        // No window — use screen coords and find the matching window frame
        let screen_point: NSPoint = unsafe { msg_send![event, locationInWindow] };
        let (wx, wy) = WINDOWS.with(|w| {
            let ws = w.borrow();
            for (_id, info) in ws.iter() {
                if info.x11_id == x11_id {
                    let frame: NSRect = unsafe { msg_send![&*info.window, frame] };
                    let fw = frame.size.width;
                    let fh = frame.size.height;
                    if fw > 0.0 && fh > 0.0 {
                        let x = ((screen_point.x - frame.origin.x) / fw * info.width as f64) as i16;
                        let y = ((frame.origin.y + fh - screen_point.y) / fh * info.height as f64) as i16;
                        return Some((x, y));
                    }
                }
            }
            None
        }).unwrap_or((0, 0));
        (wx, wy)
    }
}

fn get_modifier_state(event: &AnyObject) -> u16 {
    let flags: u64 = unsafe { msg_send![event, modifierFlags] };
    let mut state = 0u16;
    if flags & (1 << 16) != 0 { state |= 2; }     // CapsLock → LockMask
    if flags & (1 << 17) != 0 { state |= 1; }     // Shift → ShiftMask
    if flags & (1 << 18) != 0 { state |= 4; }     // Control → ControlMask
    if flags & (1 << 19) != 0 { state |= 8; }     // Option → Mod1Mask
    if flags & (1 << 20) != 0 { state |= 64; }    // Command → Mod4Mask
    state
}

fn send_display_event(evt: DisplayEvent) {
    EVT_TX.with(|tx| {
        if let Some(ref tx) = *tx.borrow() {
            let _ = tx.send(evt);
        }
    });
}
