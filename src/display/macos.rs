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
    /// Current cursor type for this window (MacOSCursorType as u8).
    cursor_type: u8,
    /// X11 background pixel color (BGRA). Used to fill new areas during resize
    /// instead of white, matching XQuartz's behavior of preserving content with gravity.
    background_pixel: u32,
    /// Deferred show: window is made visible on the first render frame,
    /// so the client's initial drawing is already in the IOSurface before
    /// the window appears. Prevents the background-color flash on startup.
    pending_show: bool,
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
    /// Last polled button state (bitmask from pressedMouseButtons) for edge detection
    static LAST_BUTTONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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
            // setFrameSize: override — immediate IOSurface resize when macOS window is resized
            class_addMethod(raw_cls, objc2::sel!(setFrameSize:),
                set_frame_size as *const std::ffi::c_void, c"v@:{CGSize=dd}".as_ptr() as _);
            // wantsUpdateLayer -> YES: tells AppKit we manage layer contents ourselves
            class_addMethod(raw_cls, objc2::sel!(wantsUpdateLayer),
                wants_update_layer as *const std::ffi::c_void, c"B@:".as_ptr() as _);
            // updateLayer: no-op — we set layer.contents directly from flush_window
            class_addMethod(raw_cls, objc2::sel!(updateLayer),
                update_layer_noop as *const std::ffi::c_void, c"v@:".as_ptr() as _);

            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

// --- PSLXInputView method implementations ---

unsafe extern "C" fn accepts_first_responder(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

/// Tell AppKit we manage layer contents ourselves — prevents drawRect from clearing layer.
unsafe extern "C" fn wants_update_layer(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

/// No-op updateLayer — layer contents set directly by flush_window via IOSurface.
unsafe extern "C" fn update_layer_noop(_this: *mut AnyObject, _sel: Sel) {
}

unsafe extern "C" fn view_key_down(this: *mut AnyObject, _sel: Sel, event: *mut AnyObject) {
    if this.is_null() || event.is_null() { return; }
    info!("PSLXInputView keyDown: called");

    // Check for Cmd+V (paste) and Cmd+C (copy)
    let modifier_flags: u64 = msg_send![&*event, modifierFlags];
    let keycode: u16 = msg_send![&*event, keyCode];
    let cmd_pressed = (modifier_flags & (1 << 20)) != 0; // Command key

    if cmd_pressed && keycode == 9 { // Cmd+V (keycode 9 = 'v')
        // Read macOS pasteboard and send as ImeCommit
        let pb: *mut AnyObject = msg_send![objc2::class!(NSPasteboard), generalPasteboard];
        let ns_string_type: *mut AnyObject = msg_send![objc2::class!(NSString), stringWithUTF8String: c"public.utf8-plain-text".as_ptr()];
        let text: *mut AnyObject = msg_send![&*pb, stringForType: ns_string_type];
        if !text.is_null() {
            let utf8: *const std::os::raw::c_char = msg_send![&*text, UTF8String];
            if !utf8.is_null() {
                if let Ok(s) = std::ffi::CStr::from_ptr(utf8).to_str() {
                    if !s.is_empty() {
                        let ivar = (*this).class().instance_variable(c"x11WindowId").unwrap();
                        let x11_id = *ivar.load::<u32>(&*this) as crate::display::Xid;
                        info!("Cmd+V paste: '{}' to window 0x{:08x}", s, x11_id);
                        send_display_event(DisplayEvent::ImeCommit {
                            window: x11_id,
                            text: s.to_string(),
                        });
                    }
                }
            }
        }
        return; // Don't process further
    }

    if cmd_pressed && keycode == 8 { // Cmd+C (keycode 8 = 'c')
        // Copy X11 selection to macOS pasteboard
        // Run pbcopy with the selection data (request via pbpaste-like mechanism)
        info!("Cmd+C: requesting X11 selection for macOS clipboard");
        let ivar = (*this).class().instance_variable(c"x11WindowId").unwrap();
        let x11_id = *ivar.load::<u32>(&*this) as crate::display::Xid;
        send_display_event(DisplayEvent::ClipboardCopyRequest { window: x11_id });
        return;
    }

    // Clear the textInserted flag
    let flag_ivar = (*this).class().instance_variable(c"textInserted").unwrap();
    *flag_ivar.load_mut::<u8>(&mut *this) = 0;

    // Detect space press during IME composition → start showing preedit inline
    let composing = crate::display::IME_COMPOSING.load(std::sync::atomic::Ordering::Relaxed);
    info!("view_key_down: keycode={} composing={}", keycode, composing);
    if composing && keycode == 49 {
        crate::display::IME_CONVERTING.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // Route key event through input method system
    let array: *mut AnyObject = msg_send![objc2::class!(NSArray), arrayWithObject: event];
    let _: () = msg_send![&*this, interpretKeyEvents: array];

    // If insertText:/setMarkedText: was NOT called, this is a non-text key (arrows, backspace, etc.)
    // Send it as a raw KeyPress event — but NOT during IME composition
    let inserted = *flag_ivar.load::<u8>(&*this);
    if inserted == 0 && !crate::display::IME_COMPOSING.load(std::sync::atomic::Ordering::Relaxed) {
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
    // Always suppress the next keyUp — ImeCommit already sends KeyPress+KeyRelease
    // via send_ime_text, so the NS_KEY_UP handler's KeyRelease would be a duplicate.
    crate::display::IME_COMPOSING.store(false, std::sync::atomic::Ordering::Relaxed);
    crate::display::IME_CONVERTING.store(false, std::sync::atomic::Ordering::Relaxed);
    crate::display::SUPPRESS_NEXT_KEYUP.store(true, std::sync::atomic::Ordering::Relaxed);

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

unsafe extern "C" fn set_marked_text(this: *mut AnyObject, _sel: Sel, text: *mut AnyObject, _sel_range: NSRange, _repl_range: NSRange) {
    if this.is_null() { return; }
    let flag_ivar = (*this).class().instance_variable(c"textInserted").unwrap();
    *flag_ivar.load_mut::<u8>(&mut *this) = 1;
    crate::display::IME_COMPOSING.store(true, std::sync::atomic::Ordering::Relaxed);

    // Only send preedit to X11 after space is pressed (conversion started).
    // Before space, macOS candidate window handles display alone.
    if !text.is_null() {
        let is_attr_str: bool = msg_send![&*text, isKindOfClass: objc2::class!(NSAttributedString)];
        let ns_string: *mut AnyObject = if is_attr_str {
            msg_send![&*text, string]
        } else {
            text
        };
        if !ns_string.is_null() {
            let utf8: *const std::os::raw::c_char = msg_send![&*ns_string, UTF8String];
            if !utf8.is_null() {
                if let Ok(s) = std::ffi::CStr::from_ptr(utf8).to_str() {
                    let x11_id_ivar = (*this).class().instance_variable(c"x11WindowId").unwrap();
                    let x11_id = *x11_id_ivar.load::<u32>(&*this) as crate::display::Xid;
                    info!("IME setMarkedText: preedit='{}' (sent to X11)", s);
                    send_display_event(DisplayEvent::ImePreeditDraw {
                        window: x11_id,
                        text: s.to_string(),
                        cursor_pos: s.chars().count() as u32,
                    });
                }
            }
        }
    }
}

unsafe extern "C" fn unmark_text(_this: *mut AnyObject, _sel: Sel) {
    crate::display::IME_COMPOSING.store(false, std::sync::atomic::Ordering::Relaxed);
    crate::display::IME_CONVERTING.store(false, std::sync::atomic::Ordering::Relaxed);
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

/// setFrameSize: override — called by AppKit immediately when the content view resizes
/// (e.g. user drags the window edge). Creates a new IOSurface at the new size so content
/// is properly clipped, then sends ConfigureNotify + Expose to X11 clients.
///
/// Throttled: IOSurface recreation + event sending happens at most once per 50ms during
/// rapid drag resize to avoid overwhelming xterm. check_window_resizes() ensures the
/// final size is always processed.
unsafe extern "C" fn set_frame_size(this: *mut AnyObject, _sel: Sel, new_size: NSSize) {
    // Call super's setFrameSize: via objc_msgSendSuper
    #[repr(C)]
    struct ObjcSuper {
        receiver: *mut AnyObject,
        super_class: *const AnyClass,
    }
    extern "C" {
        fn objc_msgSendSuper(sup: *const ObjcSuper, sel: Sel, size: NSSize);
    }
    let sup = ObjcSuper {
        receiver: this,
        super_class: objc2::class!(NSView),
    };
    objc_msgSendSuper(&sup, objc2::sel!(setFrameSize:), new_size);

    if this.is_null() { return; }

    // Read x11WindowId ivar
    let ivar = match (*this).class().instance_variable(c"x11WindowId") {
        Some(v) => v,
        None => return,
    };
    let x11_id = *ivar.load::<u32>(&*this);
    if x11_id == 0 { return; }

    let new_w = new_size.width as u16;
    let new_h = new_size.height as u16;
    if new_w == 0 || new_h == 0 { return; }

    extern "C" {
        fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
        fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
    }

    // Resize IOSurface and update WindowInfo; collect event info for after borrow ends.
    // Optimization: only recreate IOSurface when GROWING beyond current surface size.
    // When shrinking, masksToBounds clips the existing surface — no allocation needed.
    // Use try_borrow_mut to gracefully handle re-entrant calls.
    let event_info: Option<(i16, i16, u16, u16, u16, u16)> = WINDOWS.with(|w| {
        let mut ws = match w.try_borrow_mut() {
            Ok(ws) => ws,
            Err(_) => {
                return None;
            }
        };
        let (win_id, info) = match ws.iter_mut().find(|(_, info)| info.x11_id == x11_id) {
            Some(e) => e,
            None => return None,
        };

        if info.width == new_w && info.height == new_h {
            return None; // No change
        }

        let old_w = info.width;
        let old_h = info.height;

        // Check if we need a new IOSurface (only when growing beyond allocated size)
        let surface_w = IOSurfaceGetWidth(info.surface) as u16;
        let surface_h = IOSurfaceGetHeight(info.surface) as u16;
        let need_new_surface = new_w > surface_w || new_h > surface_h;

        debug!("setFrameSize: window {} (x11 0x{:08X}) resize {}x{} -> {}x{} (surface={}x{} need_new={})",
              win_id, x11_id, old_w, old_h, new_w, new_h, surface_w, surface_h, need_new_surface);

        // Always update logical dimensions
        info.width = new_w;
        info.height = new_h;

        if need_new_surface {
            // Flush any pending render commands to old surface BEFORE replacing it.
            // This ensures the old surface has the latest app content before copy.
            let win_id_u64 = *win_id;
            RENDER_MAILBOX.with(|mb| {
                let mb = mb.borrow();
                if let Some(ref mailbox) = *mb {
                    if let Some((_k, commands)) = mailbox.remove(&win_id_u64) {
                        if !commands.is_empty() {
                            let lock_result = IOSurfaceLock(info.surface, 0, std::ptr::null_mut());
                            if lock_result == 0 {
                                let base = IOSurfaceGetBaseAddress(info.surface);
                                let stride = IOSurfaceGetBytesPerRow(info.surface);
                                let buf_len = stride * old_h as usize;
                                let buffer = std::slice::from_raw_parts_mut(
                                    base as *mut u8, buf_len);
                                let w = old_w as u32;
                                let h = old_h as u32;
                                let s = stride as u32;
                                for c in &commands {
                                    render_to_buffer(buffer, w, h, s, c);
                                }
                                IOSurfaceUnlock(info.surface, 0, std::ptr::null_mut());
                            }
                        }
                    }
                }
            });

            // Growing: create new IOSurface with headroom to reduce re-allocations
            let alloc_w = ((new_w as usize + 127) & !127).max(new_w as usize) as u16;
            let alloc_h = ((new_h as usize + 127) & !127).max(new_h as usize) as u16;
            let new_surface = create_iosurface(alloc_w, alloc_h);
            if new_surface.is_null() {
                log::error!("Failed to create IOSurface in setFrameSize");
                return None;
            }

            // Clear ENTIRE new surface to background_pixel.
            // X11 servers clear the window background before sending Expose.
            // Apps (xclock etc.) draw on top without clearing first.
            IOSurfaceLock(new_surface, 0, std::ptr::null_mut());
            let new_base = IOSurfaceGetBaseAddress(new_surface) as *mut u8;
            let new_stride = IOSurfaceGetBytesPerRow(new_surface);
            let bg = info.background_pixel;
            let bg_bytes: [u8; 4] = [
                (bg & 0xFF) as u8,
                ((bg >> 8) & 0xFF) as u8,
                ((bg >> 16) & 0xFF) as u8,
                0xFF,
            ];
            for row in 0..(new_h as usize) {
                let row_base = new_base.add(row * new_stride);
                for col in 0..(new_w as usize) {
                    let off = col * 4;
                    std::ptr::copy_nonoverlapping(bg_bytes.as_ptr(), row_base.add(off), 4);
                }
            }
            IOSurfaceUnlock(new_surface, 0, std::ptr::null_mut());

            let old_surface = info.surface;
            info.surface = new_surface;
            CFRelease(old_surface as *const c_void);
        } else {
            // Shrinking or same-surface: clear visible area to background_pixel.
            // X11 servers clear the window background before sending Expose.
            let bg = info.background_pixel;
            let bg_bytes: [u8; 4] = [
                (bg & 0xFF) as u8,
                ((bg >> 8) & 0xFF) as u8,
                ((bg >> 16) & 0xFF) as u8,
                0xFF,
            ];
            IOSurfaceLock(info.surface, 0, std::ptr::null_mut());
            let base = IOSurfaceGetBaseAddress(info.surface) as *mut u8;
            let stride = IOSurfaceGetBytesPerRow(info.surface);
            for row in 0..(new_h as usize) {
                let row_base = base.add(row * stride);
                for col in 0..(new_w as usize) {
                    let off = col * 4;
                    std::ptr::copy_nonoverlapping(bg_bytes.as_ptr(), row_base.add(off), 4);
                }
            }
            IOSurfaceUnlock(info.surface, 0, std::ptr::null_mut());
        }

        // Flush to CALayer (clipped by masksToBounds when shrinking)
        flush_window(info);
        ca_transaction_flush();

        // Update position cache
        let (new_x, new_y) = macos_frame_to_x11_pos(&info.window);
        info.x11_x = new_x;
        info.x11_y = new_y;

        Some((new_x, new_y, old_w, old_h, new_w, new_h))
    });

    // Send ConfigureNotify + Expose outside the WINDOWS borrow.
    // Skip if SUPPRESS_RESIZE_EVENTS is set (X11 ConfigureWindow already sent these).
    let suppressed = crate::display::SUPPRESS_RESIZE_EVENTS.swap(false, std::sync::atomic::Ordering::Relaxed);
    if let Some((x, y, _old_w, _old_h, w, h)) = event_info {
        if !suppressed {
            send_display_event(DisplayEvent::ConfigureNotify {
                window: x11_id,
                x,
                y,
                width: w,
                height: h,
            });
            // Full Expose so app redraws at new size
            send_display_event(DisplayEvent::Expose {
                window: x11_id,
                x: 0,
                y: 0,
                width: w,
                height: h,
                count: 0,
            });
        }
    }
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

            let old_dims: Option<(u16, u16)> = WINDOWS.with(|w| {
                let mut ws = w.borrow_mut();
                if let Some(info) = ws.get_mut(&change.win_id) {
                    let old_w = info.width;
                    let old_h = info.height;

                    // Flush pending render commands to old surface before replacing
                    unsafe {
                        RENDER_MAILBOX.with(|mb| {
                            let mb = mb.borrow();
                            if let Some(ref mailbox) = *mb {
                                if let Some((_k, commands)) = mailbox.remove(&change.win_id) {
                                    if !commands.is_empty() {
                                        let lock_result = IOSurfaceLock(info.surface, 0, std::ptr::null_mut());
                                        if lock_result == 0 {
                                            let base = IOSurfaceGetBaseAddress(info.surface);
                                            let stride = IOSurfaceGetBytesPerRow(info.surface);
                                            let buf_len = stride * old_h as usize;
                                            let buffer = std::slice::from_raw_parts_mut(
                                                base as *mut u8, buf_len);
                                            let bw = old_w as u32;
                                            let bh = old_h as u32;
                                            let bs = stride as u32;
                                            for c in &commands {
                                                render_to_buffer(buffer, bw, bh, bs, c);
                                            }
                                            IOSurfaceUnlock(info.surface, 0, std::ptr::null_mut());
                                        }
                                    }
                                }
                            }
                        });
                    }

                    // Create new IOSurface with headroom
                    let alloc_w = ((change.new_w as usize + 127) & !127).max(change.new_w as usize) as u16;
                    let alloc_h = ((change.new_h as usize + 127) & !127).max(change.new_h as usize) as u16;
                    let new_surface = create_iosurface(alloc_w, alloc_h);
                    if new_surface.is_null() {
                        log::error!("Failed to create new IOSurface for resize");
                        return None;
                    }

                    // Copy old content to new surface using gravity approach (XQuartz):
                    // preserve old content at top-left, fill only new strips with background_pixel
                    unsafe {
                        IOSurfaceLock(info.surface, 0, std::ptr::null_mut());
                        IOSurfaceLock(new_surface, 0, std::ptr::null_mut());

                        let old_base = IOSurfaceGetBaseAddress(info.surface) as *const u8;
                        let old_stride = IOSurfaceGetBytesPerRow(info.surface);
                        let new_base = IOSurfaceGetBaseAddress(new_surface) as *mut u8;
                        let new_stride = IOSurfaceGetBytesPerRow(new_surface);

                        // Step 1: Copy old content to top-left (NorthWest gravity)
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

                        // Step 2: Fill only new strips with background_pixel
                        let bg = info.background_pixel;
                        let bg_bytes = bg.to_ne_bytes();

                        // Right strip: columns old_w..new_w for rows 0..min(old_h, new_h)
                        if change.new_w > old_w {
                            for row in 0..copy_rows {
                                let row_start = new_base.add(row * new_stride + old_w as usize * 4);
                                let fill_pixels = (change.new_w - old_w) as usize;
                                for px in 0..fill_pixels {
                                    std::ptr::copy_nonoverlapping(bg_bytes.as_ptr(), row_start.add(px * 4), 4);
                                }
                            }
                        }
                        // Bottom strip: rows old_h..new_h for full width
                        if change.new_h > old_h {
                            for row in old_h as usize..change.new_h as usize {
                                let row_start = new_base.add(row * new_stride);
                                for px in 0..change.new_w as usize {
                                    std::ptr::copy_nonoverlapping(bg_bytes.as_ptr(), row_start.add(px * 4), 4);
                                }
                            }
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
                    return Some((old_w, old_h));
                }
                None
            });

            // Full Expose so app redraws at new size
            if old_dims.is_some() {
                send_display_event(DisplayEvent::Expose {
                    window: change.x11_id,
                    x: 0, y: 0,
                    width: change.new_w,
                    height: change.new_h,
                    count: 0,
                });
            }
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

/// Drop redundant full-window PutImage commands. When Electron sends many
/// full-screen frames in a single batch, only the last full-screen PutImage
/// matters since it overwrites the entire surface. Non-PutImage commands and
/// partial PutImage commands are preserved.
fn coalesce_putimage(commands: Vec<crate::display::RenderCommand>, win_w: u32, win_h: u32) -> Vec<crate::display::RenderCommand> {
    if commands.len() < 2 { return commands; }

    // Find the index of the last full-window PutImage
    let mut last_full_idx: Option<usize> = None;
    for (i, cmd) in commands.iter().enumerate() {
        if let crate::display::RenderCommand::PutImage { x, y, width, height, .. } = cmd {
            if *x == 0 && *y == 0 && *width as u32 >= win_w && *height as u32 >= win_h {
                last_full_idx = Some(i);
            }
        }
    }

    if let Some(last_idx) = last_full_idx {
        // Count how many full PutImages we're dropping
        let mut dropped = 0usize;
        let result: Vec<_> = commands.into_iter().enumerate().filter(|(i, cmd)| {
            if *i == last_idx { return true; } // keep the last one
            if let crate::display::RenderCommand::PutImage { x, y, width, height, .. } = cmd {
                if *x == 0 && *y == 0 && *width as u32 >= win_w && *height as u32 >= win_h {
                    dropped += 1;
                    return false; // drop earlier full-screen PutImage
                }
            }
            true // keep everything else
        }).map(|(_, cmd)| cmd).collect();
        if dropped > 0 {
            log::debug!("coalesce_putimage: dropped {} redundant full-screen frames", dropped);
        }
        result
    } else {
        commands
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

    // 1.5. Detect button press/release edges + send MotionNotify when cursor moves
    {
        let (rx, ry) = get_screen_mouse_location();
        let cur_buttons: u64 = unsafe { msg_send![objc2::class!(NSEvent), pressedMouseButtons] };
        let prev_buttons = LAST_BUTTONS.with(|lb| lb.get());

        // Detect button edges and send ButtonPress/ButtonRelease
        if cur_buttons != prev_buttons {
            LAST_BUTTONS.with(|lb| lb.set(cur_buttons));
            let mouse_loc: NSPoint = unsafe { msg_send![objc2::class!(NSEvent), mouseLocation] };
            let time = unsafe {
                let pi: *mut AnyObject = msg_send![objc2::class!(NSProcessInfo), processInfo];
                let uptime: f64 = msg_send![pi, systemUptime];
                (uptime * 1000.0) as u32
            };
            // Check each of the first 3 buttons (left, right, middle)
            for btn_idx in 0u64..3 {
                let was = (prev_buttons >> btn_idx) & 1;
                let now = (cur_buttons >> btn_idx) & 1;
                if was == now { continue; }
                let x11_button: u8 = match btn_idx { 0 => 1, 1 => 3, 2 => 2, _ => 0 };
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
                        let win_x = (mouse_loc.x - frame.origin.x) as i16;
                        let win_y = (frame.origin.y + content_h - mouse_loc.y) as i16;
                        let in_window = win_x >= 0 && win_y >= 0
                            && (win_x as f64) < frame.size.width
                            && (win_y as f64) < content_h;
                        let state = get_mouse_button_state();
                        if now == 1 {
                            // ButtonPress: only for window under cursor
                            if !in_window { continue; }
                            let btn_mask: u16 = match x11_button { 1=>0x100, 2=>0x200, 3=>0x400, _=>0 };
                            log::debug!("Polling ButtonPress: btn={} win=0x{:08x} ({},{}) state=0x{:04x}", x11_button, info.x11_id, win_x, win_y, state & !btn_mask);
                            send_display_event(DisplayEvent::ButtonPress {
                                window: info.x11_id, button: x11_button,
                                x: win_x, y: win_y, root_x: rx, root_y: ry,
                                state: state & !btn_mask, time,
                            });
                        } else {
                            // ButtonRelease: send even when outside window (drag may end outside)
                            log::debug!("Polling ButtonRelease: btn={} win=0x{:08x} ({},{}) state=0x{:04x}", x11_button, info.x11_id, win_x, win_y, state);
                            send_display_event(DisplayEvent::ButtonRelease {
                                window: info.x11_id, button: x11_button,
                                x: win_x, y: win_y, root_x: rx, root_y: ry,
                                state, time,
                            });
                        }
                    }
                });
            }
        }

        let last = LAST_POINTER.with(|lp| lp.get());
        if rx != last.0 || ry != last.1 {
            let btn_state = get_mouse_button_state();
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
                        state: get_mouse_button_state(),
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
            let mut ws = w.borrow_mut();
            for (win_id, commands) in render_batches {
                if let Some(info) = ws.get_mut(&win_id) {
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

                    // Drop redundant full-window PutImage frames — keep only the last one.
                    // Electron/Chromium sends many full-screen PutImage per frame; only the
                    // final one matters since each overwrites the entire surface.
                    let commands = coalesce_putimage(commands, width, height);

                    for c in &commands {
                        render_to_buffer(buffer, width, height, stride, c);
                    }

                    unsafe { IOSurfaceUnlock(info.surface, 0, std::ptr::null_mut()); }
                    flush_window(info);

                    // Deferred show: make window visible after the first render
                    // so the client's drawing is already in the surface.
                    if info.pending_show {
                        info.pending_show = false;
                        let mtm = MainThreadMarker::new().unwrap();
                        let app = NSApplication::sharedApplication(mtm);
                        unsafe { let _: () = msg_send![&*app, activateIgnoringOtherApps: true]; }
                        info.window.makeKeyAndOrderFront(None);
                    }
                }
            }
            // Force immediate compositing
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

            // Accept mouse moved events
            unsafe {
                let _: () = msg_send![&*window, setAcceptsMouseMovedEvents: true];
            }

            // IOSurface for zero-copy compositing via CoreAnimation
            let surface = create_iosurface(width, height);
            if surface.is_null() {
                log::error!("Failed to create IOSurface for window {}", id);
                return;
            }

            // Fill IOSurface with opaque black (0xFF000000 BGRA) before attaching to CALayer.
            // Without this, the IOSurface is transparent and the NSWindow's white background
            // shows through until the client draws, causing a visible white flash on startup.
            unsafe {
                IOSurfaceLock(surface, 0, std::ptr::null_mut());
                let base = IOSurfaceGetBaseAddress(surface) as *mut u8;
                let stride = IOSurfaceGetBytesPerRow(surface);
                let h = height as usize;
                let w = width as usize;
                for row in 0..h {
                    let row_ptr = base.add(row * stride) as *mut u32;
                    for col in 0..w {
                        *row_ptr.add(col) = 0xFF000000; // opaque black (BGRA)
                    }
                }
                IOSurfaceUnlock(surface, 0, std::ptr::null_mut());
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
                        // Set IOSurface directly as initial layer contents (zero-copy)
                        let _: () = msg_send![layer, setContents: surface as *mut AnyObject];
                    }
                    // Prevent AppKit from overwriting layer contents during redraw.
                    // NSViewLayerContentsRedrawNever = 0
                    let _: () = msg_send![&*view, setLayerContentsRedrawPolicy: 0_isize];
                    // Pin content to top-left during live resize — prevents macOS from
                    // stretching the layer contents between setFrameSize callbacks.
                    // NSViewLayerContentsPlacementTopLeft = 11
                    let _: () = msg_send![&*view, setLayerContentsPlacement: 11_isize];

                    // Make the custom view the first responder for keyboard events
                    let _: () = msg_send![&*window, makeFirstResponder: &*view];
                }
            }

            WINDOWS.with(|w| {
                w.borrow_mut().insert(id, WindowInfo { window, surface, width, height, x11_id, x11_x, x11_y, cursor_type: 0, background_pixel: 0xFFFFFFFF, pending_show: false });
            });

            info!("Created window {} for X11 0x{:08X} ({}x{}) [IOSurface]", id, x11_id, width, height);
            let _ = reply.send(NativeWindowHandle { id });
        }

        DisplayCommand::ShowWindow { handle } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().get_mut(&handle.id) {
                    // Defer actual show until first render frame so the client's
                    // initial drawing is already in the IOSurface before the window
                    // becomes visible. This eliminates the background flash on startup.
                    info.pending_show = true;
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
            // Extract window ref and computed frame BEFORE dropping the WINDOWS borrow.
            // setFrame_display triggers setFrameSize: override which needs borrow_mut.
            let window_and_frame = WINDOWS.with(|w| {
                let ws = w.borrow();
                if let Some(info) = ws.get(&handle.id) {
                    let screen_h = info.window.screen()
                        .map(|s| s.frame().size.height)
                        .unwrap_or(956.0);
                    let pt_w = width as f64;
                    let pt_h = height as f64;
                    // X11 size = content area size (not including title bar)
                    // Use frameRectForContentRect to compute the full frame
                    let content_rect = NSRect::new(
                        NSPoint::new(0.0, 0.0),
                        NSSize::new(pt_w, pt_h),
                    );
                    let frame_for_content: NSRect = unsafe {
                        msg_send![&*info.window, frameRectForContentRect: content_rect]
                    };
                    let title_bar_h = frame_for_content.size.height - pt_h;
                    // Convert X11 content top-left to macOS frame bottom-left.
                    // Content bottom in macOS Y = screen_h - y - pt_h.
                    // Frame bottom = content bottom (no bottom border).
                    let mac_y = screen_h - y as f64 - pt_h;
                    let frame = NSRect::new(
                        NSPoint::new(x as f64, mac_y),
                        NSSize::new(pt_w, pt_h + title_bar_h),
                    );
                    info!("MoveResizeWindow: content {}x{} frame {}x{} title_h={}", pt_w, pt_h, frame.size.width, frame.size.height, title_bar_h);
                    Some((info.window.clone(), frame))
                } else {
                    None
                }
            });
            // Call setFrame_display outside the borrow so setFrameSize: can borrow_mut
            if let Some((window, frame)) = window_and_frame {
                window.setFrame_display(frame, true);
            }
        }

        DisplayCommand::SetWindowCursor { handle, cursor_type } => {
            WINDOWS.with(|w| {
                let mut ws = w.borrow_mut();
                if let Some(info) = ws.get_mut(&handle.id) {
                    info.cursor_type = cursor_type;
                    // Invalidate cursor rects to trigger resetCursorRects
                    if let Some(view) = info.window.contentView() {
                        unsafe {
                            let _: () = msg_send![&*view, discardCursorRects];
                            let bounds: NSRect = msg_send![&*view, bounds];
                            let ns_cursor = get_ns_cursor(cursor_type);
                            let _: () = msg_send![&*view, addCursorRect: bounds cursor: ns_cursor];
                        }
                    }
                    debug!("SetWindowCursor: win={} cursor_type={}", handle.id, cursor_type);
                }
            });
        }

        DisplayCommand::SetWindowBackgroundPixel { handle, pixel } => {
            WINDOWS.with(|w| {
                let mut ws = w.borrow_mut();
                if let Some(info) = ws.get_mut(&handle.id) {
                    info.background_pixel = pixel;
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

/// Get an NSCursor pointer for the given MacOSCursorType value.
fn get_ns_cursor(cursor_type: u8) -> *mut AnyObject {
    unsafe {
        let cls = objc2::class!(NSCursor);
        match cursor_type {
            0 => msg_send![cls, arrowCursor],         // Arrow
            1 => msg_send![cls, IBeamCursor],          // IBeam
            2 => msg_send![cls, crosshairCursor],      // Crosshair
            3 => msg_send![cls, pointingHandCursor],   // PointingHand
            4 => msg_send![cls, openHandCursor],       // OpenHand
            5 => msg_send![cls, closedHandCursor],     // ClosedHand
            6 => msg_send![cls, resizeLeftRightCursor], // ResizeLeftRight
            7 => msg_send![cls, resizeUpDownCursor],   // ResizeUpDown
            8 => msg_send![cls, resizeLeftCursor],     // ResizeLeft
            9 => msg_send![cls, resizeRightCursor],    // ResizeRight
            10 => msg_send![cls, resizeUpCursor],      // ResizeUp
            11 => msg_send![cls, resizeDownCursor],    // ResizeDown
            12 => msg_send![cls, operationNotAllowedCursor], // OperationNotAllowed
            _ => msg_send![cls, arrowCursor],          // Default
        }
    }
}

/// Inner flush: update CALayer contents without managing CATransaction.
/// Set IOSurface directly as CALayer contents — zero-copy, no color space conversion.
/// nil→surface cycle forces CALayer to re-read the IOSurface backing store.
fn flush_window(info: &WindowInfo) {
    unsafe {
        if info.width == 0 || info.height == 0 || info.surface.is_null() { return; }
        if let Some(view) = info.window.contentView() {
            let layer: *mut AnyObject = msg_send![&*view, layer];
            if !layer.is_null() {
                let ca_cls = objc_getClass(b"CATransaction\0".as_ptr());
                let _: () = msg_send![ca_cls, begin];
                let _: () = msg_send![ca_cls, setDisableActions: true];
                let null_obj: *mut AnyObject = std::ptr::null_mut();
                let _: () = msg_send![layer, setContents: null_obj];
                let _: () = msg_send![layer, setContents: info.surface as *mut AnyObject];
                let _: () = msg_send![ca_cls, commit];
            }
        }
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

            // Resizable style removed — sendEvent won't trigger macOS resize.
            // Forward all events so title bar drag/close/minimize work.
            unsafe {
                let _: () = msg_send![&*app, sendEvent: event_ref];
            }
        }
    }
}

/// Convert NSEvent to DisplayEvent and send to the X11 server thread.
fn handle_ns_event(event: &AnyObject, event_type: usize) {
    // Log mouse events at INFO level for debugging
    if event_type >= 1 && event_type <= 7 || event_type == 25 || event_type == 26 || event_type == 27 {
        log::debug!("handle_ns_event: type={} (mouse)", event_type);
    }
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
            let button: u8 = match event_type {
                NS_LEFT_MOUSE_DOWN => 1,
                NS_RIGHT_MOUSE_DOWN => 3,
                _ => 2,
            };
            // Check if polling already detected this press (avoid duplicate ButtonPress)
            let btn_bit: u64 = match button { 1 => 1, 3 => 2, 2 => 4, _ => 0 };
            let prev = LAST_BUTTONS.with(|lb| lb.get());
            if (prev & btn_bit) == 0 {
                let (x, y) = get_event_location(event, x11_id, win_width, win_height);
                let (root_x, root_y) = get_screen_mouse_location();
                // For ButtonPress, state should reflect state BEFORE this press.
                // macOS pressedMouseButtons already includes the button being pressed,
                // so we subtract it.
                let button_mask: u16 = match button {
                    1 => 0x100, 2 => 0x200, 3 => 0x400, _ => 0,
                };
                let state = get_modifier_state(event) & !button_mask;
                debug!("ButtonPress: window=0x{:08x} button={} x={} y={} root=({},{})", x11_id, button, x, y, root_x, root_y);
                send_display_event(DisplayEvent::ButtonPress {
                    window: x11_id, button, x, y, root_x, root_y, state, time,
                });
            } else {
                log::debug!("ButtonPress: skipped (polling already sent) btn={}", button);
            }
            // Always sync LAST_BUTTONS so the polling path doesn't re-fire
            let cur: u64 = unsafe { msg_send![objc2::class!(NSEvent), pressedMouseButtons] };
            LAST_BUTTONS.with(|lb| lb.set(cur));
        }
        NS_LEFT_MOUSE_UP | NS_RIGHT_MOUSE_UP | NS_OTHER_MOUSE_UP => {
            let button: u8 = match event_type {
                NS_LEFT_MOUSE_UP => 1,
                NS_RIGHT_MOUSE_UP => 3,
                _ => 2,
            };
            // Check if polling already detected this release (avoid duplicate ButtonRelease)
            let btn_bit: u64 = match button { 1 => 1, 3 => 2, 2 => 4, _ => 0 };
            let prev = LAST_BUTTONS.with(|lb| lb.get());
            if (prev & btn_bit) != 0 {
                let (x, y) = get_event_location(event, x11_id, win_width, win_height);
                let (root_x, root_y) = get_screen_mouse_location();
                let state = get_modifier_state(event);
                send_display_event(DisplayEvent::ButtonRelease {
                    window: x11_id, button, x, y, root_x, root_y, state, time,
                });
            } else {
                log::debug!("ButtonRelease: skipped (polling already sent) btn={}", button);
            }
            // Always sync LAST_BUTTONS so the polling path doesn't re-fire
            let cur: u64 = unsafe { msg_send![objc2::class!(NSEvent), pressedMouseButtons] };
            LAST_BUTTONS.with(|lb| lb.set(cur));
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
            // Don't clamp: allow negative/out-of-bounds coords for drag scroll-back
            let x = point.x as i16;
            let y = (vh - point.y) as i16;
            return (x, y);
        }
        // Fallback — don't clamp for drag scroll-back
        let x = point.x as i16;
        let y = (win_height as f64 - point.y) as i16;
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
    // Add mouse button masks from macOS pressed buttons
    let pressed: u64 = unsafe { msg_send![objc2::class!(NSEvent), pressedMouseButtons] };
    if pressed & (1 << 0) != 0 { state |= 0x100; }  // Button1Mask (left)
    if pressed & (1 << 1) != 0 { state |= 0x400; }  // Button3Mask (right, macOS bit 1 = X11 button 3)
    if pressed & (1 << 2) != 0 { state |= 0x200; }  // Button2Mask (middle, macOS bit 2 = X11 button 2)
    state
}

/// Get current mouse button state without an event (for polling path).
fn get_mouse_button_state() -> u16 {
    let pressed: u64 = unsafe { msg_send![objc2::class!(NSEvent), pressedMouseButtons] };
    let mut state = 0u16;
    if pressed & (1 << 0) != 0 { state |= 0x100; }  // Button1Mask
    if pressed & (1 << 1) != 0 { state |= 0x400; }  // Button3Mask
    if pressed & (1 << 2) != 0 { state |= 0x200; }  // Button2Mask
    state
}

fn send_display_event(evt: DisplayEvent) {
    EVT_TX.with(|tx| {
        if let Some(ref tx) = *tx.borrow() {
            let _ = tx.send(evt);
        }
    });
}
