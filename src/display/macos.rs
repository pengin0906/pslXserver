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
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
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

// --- CoreGraphics CGEventTap FFI ---
extern "C" {
    // CGEventTapCreate: returns CFMachPortRef (opaque), NULL on failure.
    // kCGSessionEventTap=1, kCGHeadInsertEventTap=0, kCGEventTapOptionListenOnly=1
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void,
        user_info: *mut c_void,
    ) -> *mut c_void;
    // CFMachPortCreateRunLoopSource: wraps a CFMachPortRef (CGEventTap) as a CFRunLoopSource.
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: *mut c_void,
        order: i32,
    ) -> *mut c_void;
}

// --- IOSurface property keys
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
    /// IOSurface-backed pixel buffer for software rendering (render target).
    surface: *mut c_void,
    /// Second IOSurface for double-buffering display.
    /// Alternating pointers forces CALayer to re-read pixel data each frame.
    display_surface: *mut c_void,
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
    /// Whether this window is visible on screen (vs hidden render-only buffer).
    visible: bool,
}

impl Drop for WindowInfo {
    fn drop(&mut self) {
        if !self.surface.is_null() {
            unsafe { CFRelease(self.surface as *const c_void); }
        }
        if !self.display_surface.is_null() {
            unsafe { CFRelease(self.display_surface as *const c_void); }
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
    /// Accumulated horizontal scroll delta (pixels).
    static SCROLL_ACCUM_X: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    /// Accumulated pinch-to-zoom magnification delta.
    static MAGNIFY_ACCUM: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    /// Implicit pointer grab: window that received the most recent ButtonPress.
    /// While any button is down, MotionNotify goes here (not to cursor-under window).
    /// Cleared when all buttons are released.
    static GRAB_WINDOW: std::cell::Cell<Option<crate::display::Xid>> = const { std::cell::Cell::new(None) };
    /// X11 window id of the last NSWindow that was the macOS keyWindow.
    /// Used to detect key window changes (e.g. after title bar drag) and re-send FocusIn.
    static LAST_KEY_WINDOW_X11: std::cell::Cell<crate::display::Xid> = const { std::cell::Cell::new(0) };
    /// X11 window id of the window the mouse cursor is currently inside.
    /// Used for proactive FocusIn on EnterNotify — sent every frame even when
    /// pslXserver is not the active macOS app.
    static LAST_ENTER_WINDOW_X11: std::cell::Cell<crate::display::Xid> = const { std::cell::Cell::new(0) };
    /// Frames remaining to suppress button polling after app activation.
    /// activateIgnoringOtherApps:YES causes macOS to synthesize button events;
    /// suppressing button detection for a few frames prevents spurious ButtonPress.
    static SUPPRESS_BUTTON_FRAMES: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
    /// NSWindow pointer to call makeKeyAndOrderFront: on the next timer tick.
    /// Set by activate_app() after calling activateIgnoringOtherApps:YES.
    /// Consumed by process_commands() so makeKeyAndOrderFront runs after activation settles.
    static PENDING_KEY_WINDOW: std::cell::Cell<*mut AnyObject> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    /// Cached mouse location for check_enter_notify — skip windowNumberAtPoint when unchanged.
    static LAST_MOUSE_LOC: std::cell::Cell<(i64, i64)> = const { std::cell::Cell::new((i64::MIN, i64::MIN)) };
}

/// Convert macOS keycode to X11 keycode (evdev keycode + 8).
/// Chrome/Electron interpret X11 keycodes as evdev codes, so we must use the
/// Linux evdev layout rather than macOS keycode + 8.
/// evdev codes from linux/input-event-codes.h, X11 keycode = evdev + 8.
fn macos_keycode_to_x11(mac: u16) -> u8 {
    let evdev: u16 = match mac {
        // Letters (macOS → evdev)
        0  => 30,  // a → KEY_A
        1  => 31,  // s → KEY_S
        2  => 32,  // d → KEY_D
        3  => 33,  // f → KEY_F
        4  => 35,  // h → KEY_H
        5  => 34,  // g → KEY_G
        6  => 44,  // z → KEY_Z
        7  => 45,  // x → KEY_X
        8  => 46,  // c → KEY_C
        9  => 47,  // v → KEY_V
        11 => 48,  // b → KEY_B
        12 => 16,  // q → KEY_Q
        13 => 17,  // w → KEY_W
        14 => 18,  // e → KEY_E
        15 => 19,  // r → KEY_R
        16 => 21,  // y → KEY_Y
        17 => 20,  // t → KEY_T
        // Numbers
        18 => 2,   // 1 → KEY_1
        19 => 3,   // 2 → KEY_2
        20 => 4,   // 3 → KEY_3
        21 => 5,   // 4 → KEY_4
        22 => 7,   // 6 → KEY_6
        23 => 6,   // 5 → KEY_5
        24 => 13,  // = → KEY_EQUAL
        25 => 10,  // 9 → KEY_9
        26 => 8,   // 7 → KEY_7
        27 => 12,  // - → KEY_MINUS
        28 => 9,   // 8 → KEY_8
        29 => 11,  // 0 → KEY_0
        // Punctuation
        30 => 27,  // ] → KEY_RIGHTBRACE
        31 => 24,  // o → KEY_O
        32 => 22,  // u → KEY_U
        33 => 26,  // [ → KEY_LEFTBRACE
        34 => 23,  // i → KEY_I
        35 => 25,  // p → KEY_P
        37 => 38,  // l → KEY_L
        38 => 36,  // j → KEY_J
        39 => 40,  // ' → KEY_APOSTROPHE
        40 => 37,  // k → KEY_K
        41 => 39,  // ; → KEY_SEMICOLON
        42 => 43,  // \ → KEY_BACKSLASH
        43 => 51,  // , → KEY_COMMA
        44 => 53,  // / → KEY_SLASH
        45 => 49,  // n → KEY_N
        46 => 50,  // m → KEY_M
        47 => 52,  // . → KEY_DOT
        // Special keys
        36 => 28,  // Return → KEY_ENTER
        48 => 15,  // Tab → KEY_TAB
        49 => 57,  // Space → KEY_SPACE
        50 => 41,  // ` → KEY_GRAVE
        51 => 14,  // Backspace → KEY_BACKSPACE
        53 => 1,   // Escape → KEY_ESC
        // Modifier keys
        54 => 126, // Right Command → KEY_RIGHTMETA
        55 => 125, // Left Command → KEY_LEFTMETA
        56 => 42,  // Left Shift → KEY_LEFTSHIFT
        57 => 58,  // Caps Lock → KEY_CAPSLOCK
        58 => 56,  // Left Option → KEY_LEFTALT
        59 => 29,  // Left Control → KEY_LEFTCTRL
        60 => 54,  // Right Shift → KEY_RIGHTSHIFT
        61 => 100, // Right Option → KEY_RIGHTALT
        62 => 97,  // Right Control → KEY_RIGHTCTRL
        // Function keys
        122 => 59, // F1
        120 => 60, // F2
        99  => 61, // F3
        118 => 62, // F4
        96  => 63, // F5
        97  => 64, // F6
        98  => 65, // F7
        100 => 66, // F8
        101 => 67, // F9
        109 => 68, // F10
        103 => 87, // F11
        111 => 88, // F12
        // Arrow keys
        123 => 105, // Left → KEY_LEFT
        124 => 106, // Right → KEY_RIGHT
        125 => 108, // Down → KEY_DOWN
        126 => 103, // Up → KEY_UP
        // Navigation keys
        115 => 102, // Home → KEY_HOME
        119 => 107, // End → KEY_END (Delete/Forward Delete on some Mac keyboards)
        116 => 104, // PageUp → KEY_PAGEUP
        121 => 109, // PageDown → KEY_PAGEDOWN
        117 => 111, // Delete (Forward) → KEY_DELETE
        // JIS keys
        102 => 100, // Eisu (英数)
        104 => 92,  // Kana (かな) → KEY_HENKAN
        // Keypad
        65 => 83,   // Keypad . → KEY_KPDOT
        67 => 55,   // Keypad * → KEY_KPASTERISK
        69 => 78,   // Keypad + → KEY_KPPLUS
        75 => 98,   // Keypad / → KEY_KPSLASH
        76 => 96,   // Keypad Enter → KEY_KPENTER
        78 => 74,   // Keypad - → KEY_KPMINUS
        82 => 82,   // Keypad 0 → KEY_KP0
        83 => 79,   // Keypad 1 → KEY_KP1
        84 => 80,   // Keypad 2 → KEY_KP2
        85 => 81,   // Keypad 3 → KEY_KP3
        86 => 75,   // Keypad 4 → KEY_KP4
        87 => 76,   // Keypad 5 → KEY_KP5
        88 => 77,   // Keypad 6 → KEY_KP6
        89 => 71,   // Keypad 7 → KEY_KP7
        91 => 72,   // Keypad 8 → KEY_KP8
        92 => 73,   // Keypad 9 → KEY_KP9
        // Fallback
        _ => return (mac as u8).wrapping_add(8),
    };
    (evdev + 8) as u8
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
            // acceptsFirstMouse: -> YES (first click on inactive window is delivered, not consumed)
            class_addMethod(raw_cls, objc2::sel!(acceptsFirstMouse:),
                accepts_first_mouse as *const std::ffi::c_void, c"B@:@".as_ptr() as _);
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
            // updateTrackingAreas: refresh NSTrackingArea on resize
            class_addMethod(raw_cls, objc2::sel!(updateTrackingAreas),
                update_tracking_areas as *const std::ffi::c_void, c"v@:".as_ptr() as _);
            // mouseEntered: — called by NSTrackingArea (NSTrackingActiveAlways) even
            // when pslXserver is backgrounded; activates window without spurious clicks.
            class_addMethod(raw_cls, objc2::sel!(mouseEntered:),
                mouse_entered as *const std::ffi::c_void, c"v@:@".as_ptr() as _);

            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

// --- PSLXInputView method implementations ---

/// resetCursorRects — set up macOS cursor rects for window edge resize zones.
/// macOS automatically changes the cursor when the mouse enters these rects.

unsafe extern "C" fn accepts_first_responder(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

/// acceptsFirstMouse: — return YES so the first click on an inactive window is delivered
/// to this view (not consumed by macOS just to activate the app).
/// Without this, clicking xterm content only activates pslXserver but the click itself
/// is swallowed by macOS → our mouseDown handler is never called → no ButtonPress/FocusIn.
unsafe extern "C" fn accepts_first_mouse(_this: *mut AnyObject, _sel: Sel, _event: *mut AnyObject) -> Bool {
    Bool::YES
}

/// updateTrackingAreas — replace the NSTrackingArea each time the view is resized.
/// NSTrackingActiveAlways: tracking fires even when pslXserver is NOT the active app.
/// NSTrackingMouseEnteredAndExited (0x01) + NSTrackingActiveAlways (0x80) + NSTrackingInVisibleRect (0x200)
unsafe extern "C" fn update_tracking_areas(this: *mut AnyObject, _sel: Sel) {
    // Remove all existing tracking areas
    let areas: *mut AnyObject = msg_send![this, trackingAreas];
    let count: usize = msg_send![areas, count];
    // Iterate in reverse to avoid index shifting
    let old: Vec<*mut AnyObject> = (0..count)
        .map(|i| { let a: *mut AnyObject = msg_send![areas, objectAtIndex: i]; a })
        .collect();
    for area in old {
        let _: () = msg_send![this, removeTrackingArea: area];
    }
    // Add a new tracking area covering the entire visible rect
    let bounds: NSRect = msg_send![this, bounds];
    let options: u32 = 0x01 | 0x80 | 0x200; // MouseEnteredAndExited | ActiveAlways | InVisibleRect
    let ta_cls = objc2::class!(NSTrackingArea);
    let ta: *mut AnyObject = msg_send![ta_cls, alloc];
    let ta: *mut AnyObject = msg_send![ta, initWithRect: bounds
                                                options: options
                                                  owner: this
                                               userInfo: std::ptr::null::<AnyObject>()];
    let _: () = msg_send![this, addTrackingArea: ta];
}

/// mouseEntered: — NSTrackingArea callback (NSTrackingActiveAlways).
/// When pslXserver IS active: handle normal focus-follows-mouse between X11 windows.
/// When pslXserver is NOT active: do NOT call activate_app and do NOT set
/// LAST_ENTER_WINDOW_X11 — let the NSEvent global monitor handle activation.
/// Reason: activateIgnoringOtherApps is ignored by Sequoia from NSTrackingArea context,
/// and setting LAST_ENTER_WINDOW_X11 here would prevent the global monitor from firing.
unsafe extern "C" fn mouse_entered(this: *mut AnyObject, _sel: Sel, _event: *mut AnyObject) {
    let ns_window: *mut AnyObject = msg_send![this, window];
    if ns_window.is_null() { return; }

    let x11_id = WINDOWS.with(|w| {
        let ws = w.borrow();
        for (_id, info) in ws.iter() {
            let win_ptr = &*info.window as *const NSWindow as *const AnyObject as *mut AnyObject;
            if win_ptr == ns_window { return info.x11_id; }
        }
        0
    });
    if x11_id == 0 { return; }

    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    let is_active: bool = msg_send![&*app, isActive];

    if is_active {
        // Normal case: pslXserver already has focus, user moved to a different X11 window.
        LAST_ENTER_WINDOW_X11.with(|le| le.set(x11_id));
        info!("mouseEntered (active) → FocusIn x11=0x{:08x}", x11_id);
        send_display_event(DisplayEvent::FocusIn { window: x11_id });
    } else {
        // Backgrounded: DON'T set LAST_ENTER_WINDOW_X11 here.
        // The global monitor will see the next mouse-moved event, find LAST=0 ≠ entered_xid,
        // and call activate_app in a user-event context that Sequoia actually respects.
        SUPPRESS_BUTTON_FRAMES.with(|s| s.set(8));
        info!("mouseEntered (backgrounded) x11=0x{:08x} — deferring to global monitor", x11_id);
    }
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

    // JIS IME toggle keys (英数=102, かな=104): bypass interpretKeyEvents so macOS IME
    // state is not disturbed. Send raw X11 KeyPress and let macOS handle input source switch.
    if keycode == 102 || keycode == 104 {
        let state = get_modifier_state(&*event);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;
        let x11_id_ivar = (*this).class().instance_variable(c"x11WindowId").unwrap();
        let x11_id = *x11_id_ivar.load::<u32>(&*this) as crate::display::Xid;
        send_display_event(DisplayEvent::KeyPress { window: x11_id, keycode: (keycode as u32 + 8) as u8, state, time });
        return;
    }

    // Route key event through input method system
    let array: *mut AnyObject = msg_send![objc2::class!(NSArray), arrayWithObject: event];
    let _: () = msg_send![&*this, interpretKeyEvents: array];

    // If insertText:/setMarkedText: was NOT called, this is a non-text key (arrows, backspace, etc.)
    // Send it as a raw KeyPress event using the physical keycode — but NOT during IME composition.
    // GetKeyboardMapping uses UCKeyTranslate-derived mapping so all layouts (JIS etc.) work correctly.
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
            keycode: macos_keycode_to_x11(keycode),
            state,
            time,
        });
    }
}

unsafe extern "C" fn insert_text_replacement(this: *mut AnyObject, _sel: Sel, text: *mut AnyObject, repl_range: NSRange) {
    if this.is_null() { return; }

    crate::display::IME_COMPOSING.store(false, std::sync::atomic::Ordering::Relaxed);
    crate::display::IME_CONVERTING.store(false, std::sync::atomic::Ordering::Relaxed);

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

    // For single ASCII chars, let the raw KeyPress path in view_key_down handle it.
    // UCKeyTranslate-based GetKeyboardMapping ensures correct keysyms for physical keycodes.
    let is_single_ascii = rust_str.len() == 1 && rust_str.as_bytes()[0] < 0x80;
    if is_single_ascii {
        debug!("insertText: single ASCII '{}' — letting raw KeyPress handle it", rust_str);
        return;
    }

    // Non-ASCII or multi-char: send via ImeCommit / ImeReplace
    let flag_ivar = (*this).class().instance_variable(c"textInserted").unwrap();
    *flag_ivar.load_mut::<u8>(&mut *this) = 1;
    // Suppress next keyUp — ImeCommit already sends KeyPress+KeyRelease
    crate::display::SUPPRESS_NEXT_KEYUP.store(true, std::sync::atomic::Ordering::Relaxed);

    let ivar = (*this).class().instance_variable(c"x11WindowId").unwrap();
    let x11_id = *ivar.load::<u32>(&*this) as crate::display::Xid;

    let reconverting = crate::display::RECONVERTING.swap(false, std::sync::atomic::Ordering::Relaxed);

    if reconverting && repl_range.length > 0 {
        // Reconversion: erase original text and insert converted text
        let erase_chars = repl_range.length;
        info!("IME reconversion: erase {} chars, insert '{}'", erase_chars, rust_str);
        // Update last commit for potential chained reconversion
        {
            let mut lct = crate::display::LAST_COMMIT_TEXT.lock().unwrap();
            *lct = rust_str.clone();
        }
        crate::display::LAST_COMMIT_CHAR_COUNT.store(rust_str.chars().count(), std::sync::atomic::Ordering::Relaxed);
        send_display_event(DisplayEvent::ImeReplace {
            window: x11_id,
            erase_chars,
            text: rust_str,
        });
    } else {
        // Normal commit: save meaningful multi-char CJK text for potential reconversion.
        // Skip ASCII-only or single-char (spaces, punctuation) — not useful to reconvert.
        let has_cjk = rust_str.chars().any(|c| c > '\u{2E7F}');
        if has_cjk {
            let char_count = rust_str.chars().count();
            let mut lct = crate::display::LAST_COMMIT_TEXT.lock().unwrap();
            *lct = rust_str.clone();
            drop(lct);
            crate::display::LAST_COMMIT_CHAR_COUNT.store(char_count, std::sync::atomic::Ordering::Relaxed);
        }
        debug!("IME insertText: '{}' for window 0x{:08x}", rust_str, x11_id);
        send_display_event(DisplayEvent::ImeCommit {
            window: x11_id,
            text: rust_str,
        });
    }
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
    // Return YES during IME composition so macOS properly shows the candidate window.
    // Preedit is not sent to X11 (ibus on remote Linux re-processes it), so macOS
    // candidate window is the only preedit display.
    if crate::display::IME_COMPOSING.load(std::sync::atomic::Ordering::Relaxed) {
        Bool::YES
    } else {
        Bool::NO
    }
}

unsafe extern "C" fn marked_range(_this: *mut AnyObject, _sel: Sel) -> NSRange {
    NSRange { location: usize::MAX, length: 0 } // NSNotFound
}

unsafe extern "C" fn selected_range(_this: *mut AnyObject, _sel: Sel) -> NSRange {
    // Return last commit range so macOS IME can offer reconversion.
    // Only when not currently composing (reconversion applies to already-committed text).
    if !crate::display::IME_COMPOSING.load(std::sync::atomic::Ordering::Relaxed) {
        let n = crate::display::LAST_COMMIT_CHAR_COUNT.load(std::sync::atomic::Ordering::Relaxed);
        if n > 0 {
            return NSRange { location: 0, length: n };
        }
    }
    NSRange { location: 0, length: 0 }
}

unsafe extern "C" fn set_marked_text(this: *mut AnyObject, _sel: Sel, text: *mut AnyObject, _sel_range: NSRange, repl_range: NSRange) {
    if this.is_null() { return; }
    let flag_ivar = (*this).class().instance_variable(c"textInserted").unwrap();
    *flag_ivar.load_mut::<u8>(&mut *this) = 1;

    // Extract preedit text
    let preedit_str = if !text.is_null() {
        let is_attr_str: bool = msg_send![&*text, isKindOfClass: objc2::class!(NSAttributedString)];
        let ns_string: *mut AnyObject = if is_attr_str {
            msg_send![&*text, string]
        } else {
            text
        };
        if !ns_string.is_null() {
            let utf8: *const std::os::raw::c_char = msg_send![&*ns_string, UTF8String];
            if !utf8.is_null() {
                std::ffi::CStr::from_ptr(utf8).to_str().ok().map(|s| s.to_string())
            } else { None }
        } else { None }
    } else { None };

    let preedit_empty = preedit_str.as_ref().map_or(true, |s| s.is_empty());

    if preedit_empty {
        // Preedit cleared (e.g., BS deleted all preedit chars) — end composition
        crate::display::IME_COMPOSING.store(false, std::sync::atomic::Ordering::Relaxed);
        crate::display::IME_CONVERTING.store(false, std::sync::atomic::Ordering::Relaxed);
    } else {
        crate::display::IME_COMPOSING.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // replacementRange.length > 0 means macOS IME is reconverting existing committed text.
    // Suppress preedit injection so the original text in xterm isn't disturbed.
    if repl_range.length > 0 {
        crate::display::RECONVERTING.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    if let Some(s) = preedit_str {
        if !s.is_empty() {
            let x11_id_ivar = (*this).class().instance_variable(c"x11WindowId").unwrap();
            let x11_id = *x11_id_ivar.load::<u32>(&*this) as crate::display::Xid;
            if crate::display::RECONVERTING.load(std::sync::atomic::Ordering::Relaxed) {
                // During reconversion the original text stays in xterm; floating window only.
                info!("IME setMarkedText (reconversion): preedit='{}' — floating only", s);
            } else {
                info!("IME setMarkedText: preedit='{}' (sent to X11)", s);
                send_display_event(DisplayEvent::ImePreeditDraw {
                    window: x11_id,
                    text: s,
                    cursor_pos: 0,
                });
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

unsafe extern "C" fn attributed_substring(_this: *mut AnyObject, _sel: Sel, _range: NSRange, actual: *mut NSRange) -> *mut AnyObject {
    let text = crate::display::LAST_COMMIT_TEXT.lock().unwrap().clone();
    if text.is_empty() {
        return std::ptr::null_mut();
    }
    // Just return the text; reconversion detection happens in setMarkedText: (repl_range.length > 0)
    let c_str = match std::ffi::CString::new(text.as_str()) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let ns_str: *mut AnyObject = msg_send![objc2::class!(NSString), stringWithUTF8String: c_str.as_ptr()];
    if ns_str.is_null() { return std::ptr::null_mut(); }
    if !actual.is_null() {
        *actual = NSRange { location: 0, length: crate::display::LAST_COMMIT_CHAR_COUNT.load(std::sync::atomic::Ordering::Relaxed) };
    }
    let attr_str: *mut AnyObject = msg_send![objc2::class!(NSAttributedString), alloc];
    let attr_str: *mut AnyObject = msg_send![attr_str, initWithString: ns_str];
    attr_str
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

            // Growing: create new IOSurface pair with headroom to reduce re-allocations
            let alloc_w = ((new_w as usize + 127) & !127).max(new_w as usize) as u16;
            let alloc_h = ((new_h as usize + 127) & !127).max(new_h as usize) as u16;
            let new_surface = create_iosurface(alloc_w, alloc_h);
            let new_display = create_iosurface(alloc_w, alloc_h);
            if new_surface.is_null() || new_display.is_null() {
                log::error!("Failed to create IOSurface in setFrameSize");
                return None;
            }

            // Clear ENTIRE new surfaces to background_pixel.
            // X11 servers clear the window background before sending Expose.
            // Apps (xclock etc.) draw on top without clearing first.
            let bg = info.background_pixel;
            let bg_bytes: [u8; 4] = [
                (bg & 0xFF) as u8,
                ((bg >> 8) & 0xFF) as u8,
                ((bg >> 16) & 0xFF) as u8,
                0xFF,
            ];
            for s in [new_surface, new_display] {
                IOSurfaceLock(s, 0, std::ptr::null_mut());
                let base = IOSurfaceGetBaseAddress(s) as *mut u8;
                let stride = IOSurfaceGetBytesPerRow(s);
                for row in 0..(new_h as usize) {
                    let row_base = base.add(row * stride);
                    for col in 0..(new_w as usize) {
                        let off = col * 4;
                        std::ptr::copy_nonoverlapping(bg_bytes.as_ptr(), row_base.add(off), 4);
                    }
                }
                IOSurfaceUnlock(s, 0, std::ptr::null_mut());
            }

            let old_surface = info.surface;
            let old_display = info.display_surface;
            info.surface = new_surface;
            info.display_surface = new_display;
            CFRelease(old_surface as *const c_void);
            CFRelease(old_display as *const c_void);
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

        // Swap + flush: cleared surface becomes display surface for immediate visual update.
        std::mem::swap(&mut info.surface, &mut info.display_surface);
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

    // Check if AudioQueue needs initialization (triggered by PA server)
    unsafe { crate::audio::check_audio_init(); }
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
        // Use cached screen height — avoids [NSScreen mainScreen] ObjC call per frame
        let screen_h = SCREEN_HEIGHT.with(|sh| sh.get());
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

                    // Replace both surfaces and update dimensions.
                    // Both must use the same alloc size to ensure matching stride.
                    let old_surface = info.surface;
                    let old_display = info.display_surface;
                    let new_display = create_iosurface(alloc_w, alloc_h);
                    // Copy new_surface → new_display so both have identical content.
                    unsafe {
                        IOSurfaceLock(new_surface, 1, std::ptr::null_mut());
                        IOSurfaceLock(new_display, 0, std::ptr::null_mut());
                        let src = IOSurfaceGetBaseAddress(new_surface) as *const u8;
                        let dst = IOSurfaceGetBaseAddress(new_display) as *mut u8;
                        let stride = IOSurfaceGetBytesPerRow(new_surface);
                        std::ptr::copy_nonoverlapping(src, dst, stride * change.new_h as usize);
                        IOSurfaceUnlock(new_display, 0, std::ptr::null_mut());
                        IOSurfaceUnlock(new_surface, 1, std::ptr::null_mut());
                    }
                    info.surface = new_surface;
                    info.display_surface = new_display;
                    info.width = change.new_w;
                    info.height = change.new_h;
                    unsafe {
                        CFRelease(old_surface as *const c_void);
                        CFRelease(old_display as *const c_void);
                    }

                    // Swap + flush so resize is visible immediately
                    std::mem::swap(&mut info.surface, &mut info.display_surface);
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

/// Activate pslXserver so PSLXInputView receives keyDown: events.
/// Activate pslXserver and bring the given window to front.
/// MUST be called from a real user-event handler (mouseEntered:, mouseDown:, etc.)
/// macOS Sequoia ignores activation calls from timer/background contexts.
/// CGEventTap callback — fires on main thread for every mouse event at WindowServer level.
/// Unlike NSEvent global monitor, this fires even when Finder (or any app that doesn't
/// use acceptsMouseMovedEvents) is the active app.
unsafe extern "C" fn cg_mouse_tap_callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: *mut c_void,
    _user_info: *mut c_void,
) -> *mut c_void {
    // Only act when we're in the background.
    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return event,
    };
    let app = NSApplication::sharedApplication(mtm);
    let is_active: bool = msg_send![&*app, isActive];
    if is_active { return event; }

    let mouse_loc: NSPoint = msg_send![objc2::class!(NSEvent), mouseLocation];
    // Use windowNumberAtPoint to find the TOPMOST window at the cursor,
    // including windows from OTHER apps. Only activate if the topmost
    // window is ours — prevents raising windows hidden behind other apps.
    let topmost_win_num: isize = msg_send![objc2::class!(NSWindow),
        windowNumberAtPoint: mouse_loc
        belowWindowWithWindowNumber: 0isize];
    let (entered_xid, entered_nswin): (crate::display::Xid, *mut AnyObject) = WINDOWS.with(|w| {
        let ws = w.borrow();
        for (_id, info) in ws.iter() {
            if !info.visible { continue; }
            let is_mini: bool = msg_send![&*info.window, isMiniaturized];
            if is_mini { continue; }
            let win_num: isize = msg_send![&*info.window, windowNumber];
            if win_num == topmost_win_num {
                let ptr = &*info.window as *const NSWindow as *mut AnyObject;
                return (info.x11_id, ptr);
            }
        }
        (0, std::ptr::null_mut())
    });

    // kCGEventLeftMouseDown=1, kCGEventRightMouseDown=3: activation click.
    // Suppress the resulting ButtonPress so it doesn't cause a spurious newline in xterm.
    let is_mouse_down = event_type == 1 || event_type == 3;
    if is_mouse_down {
        if !entered_nswin.is_null() {
            info!("CGEventTap: mouseDown on x11=0x{:08x} while backgrounded — suppressing ButtonPress", entered_xid);
            SUPPRESS_BUTTON_FRAMES.with(|s| s.set(8));
            LAST_ENTER_WINDOW_X11.with(|le| le.set(entered_xid));
            activate_app(entered_nswin);
            send_display_event(DisplayEvent::FocusIn { window: entered_xid });
        }
        return event;
    }

    // Mouse moved/dragged: focus-follows-mouse activation.
    LAST_ENTER_WINDOW_X11.with(|le| {
        if entered_nswin.is_null() {
            // Mouse left all our windows — reset so next entry fires again.
            if le.get() != 0 { le.set(0); }
            return;
        }
        if le.get() == entered_xid { return; }
        le.set(entered_xid);
        info!("CGEventTap: cursor entered x11=0x{:08x} while backgrounded — activating", entered_xid);
        SUPPRESS_BUTTON_FRAMES.with(|s| s.set(8));
        activate_app(entered_nswin);
        send_display_event(DisplayEvent::FocusIn { window: entered_xid });
    });

    event // passive listener: return event unchanged
}

/// Activate pslXserver and bring the given NSWindow to key.
/// Must be called from a user-event context (CGEventTap or NSEvent global monitor handler).
/// activateIgnoringOtherApps:YES is deprecated but still respected by Sequoia in user-event context.
unsafe fn activate_app(ns_window: *mut AnyObject) {
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    // Re-assert Regular policy each time — CLI-launched processes may have it reset.
    let _: bool = msg_send![&*app, setActivationPolicy: 0i64];
    let _: () = msg_send![&*app, activateIgnoringOtherApps: objc2::runtime::Bool::YES];
    // Schedule makeKeyAndOrderFront: for the NEXT timer tick so activation settles first.
    if !ns_window.is_null() {
        PENDING_KEY_WINDOW.with(|p| p.set(ns_window));
    }
}

/// Poll mouse position each frame; implement X11 focus-follows-pointer in macOS terms.
/// When cursor enters one of our windows:
///   1. Make the NSWindow key + activate pslXserver (macOS side: delivers KeyPress to PSLXInputView)
///   2. Send X11 FocusIn to the X11 client (X11 side: xterm sets ICFocus and accepts keyboard)
/// This harmonizes X11's focus-follows-mouse with macOS's click-to-focus model.
fn check_enter_notify() {
    let mouse_loc: NSPoint = unsafe { msg_send![objc2::class!(NSEvent), mouseLocation] };

    // Skip expensive windowNumberAtPoint IPC if mouse hasn't moved (saves ~89% CPU).
    let loc_key = (mouse_loc.x.to_bits() as i64, mouse_loc.y.to_bits() as i64);
    let prev = LAST_MOUSE_LOC.with(|c| c.get());
    if loc_key == prev {
        return;
    }
    LAST_MOUSE_LOC.with(|c| c.set(loc_key));

    // Use windowNumberAtPoint to find the TOPMOST window at the cursor.
    // Only match if it's one of ours — prevents stealing focus from other apps.
    let topmost_win_num: isize = unsafe {
        msg_send![objc2::class!(NSWindow),
            windowNumberAtPoint: mouse_loc
            belowWindowWithWindowNumber: 0isize]
    };
    let entered_xid = WINDOWS.with(|w| {
        let ws = w.borrow();
        for (_id, info) in ws.iter() {
            if !info.visible { continue; }
            let is_mini: bool = unsafe { msg_send![&*info.window, isMiniaturized] };
            if is_mini { continue; }
            let win_num: isize = unsafe { msg_send![&*info.window, windowNumber] };
            if win_num == topmost_win_num {
                return info.x11_id;
            }
        }
        0
    });

    LAST_ENTER_WINDOW_X11.with(|le| {
        if le.get() == entered_xid { return; }
        le.set(entered_xid);
        if entered_xid == 0 { return; }

        // Only send X11 FocusIn from timer — macOS activation must come from
        // a real user-event handler (mouseEntered:), not a 60fps timer loop.
        // macOS Sequoia ignores activateIgnoringOtherApps from timer contexts.
        info!("EnterNotify → FocusIn x11=0x{:08x}", entered_xid);
        send_display_event(DisplayEvent::FocusIn { window: entered_xid });
    });
}

/// Poll NSApp.keyWindow each frame; send FocusIn when it switches to one of our windows.
/// This catches focus gained via title bar drag or other system-level window activation
/// that bypasses our NSEvent mouse-down handler.
fn check_key_window() {
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    let key_win: *mut AnyObject = unsafe { msg_send![&*app, keyWindow] };
    if key_win.is_null() {
        // Backgrounded — no key window. Reset tracker so next click/activation fires FocusIn.
        LAST_KEY_WINDOW_X11.with(|lkw| lkw.set(0));
        return;
    }

    // Find the X11 window id for the current key NSWindow.
    let x11_id: crate::display::Xid = WINDOWS.with(|w| {
        let ws = w.borrow();
        for info in ws.values() {
            // Compare raw NSWindow pointer identity.
            let win_ptr = &*info.window as *const NSWindow as *const AnyObject as *mut AnyObject;
            if win_ptr == key_win {
                return info.x11_id;
            }
        }
        0
    });

    if x11_id == 0 {
        return;
    }

    LAST_KEY_WINDOW_X11.with(|lkw| {
        if lkw.get() != x11_id {
            lkw.set(x11_id);
            info!("Key window changed → FocusIn x11=0x{:08x}", x11_id);
            send_display_event(DisplayEvent::FocusIn { window: x11_id });
        }
    });
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
        // Cache mouse location and button state once per tick (avoid repeated ObjC calls)
        let mouse_loc: NSPoint = unsafe { msg_send![objc2::class!(NSEvent), mouseLocation] };
        let sh = SCREEN_HEIGHT.with(|sh| sh.get());
        let rx = mouse_loc.x as i16;
        let ry = (sh - mouse_loc.y) as i16;
        let cur_buttons: u64 = unsafe { msg_send![objc2::class!(NSEvent), pressedMouseButtons] };
        let prev_buttons = LAST_BUTTONS.with(|lb| lb.get());

        // Detect button edges and send ButtonPress/ButtonRelease.
        // Skip for a few frames after activateIgnoringOtherApps: to avoid spurious clicks.
        if SUPPRESS_BUTTON_FRAMES.with(|s| {
            let v = s.get();
            if v > 0 { s.set(v - 1); }
            v > 0
        }) {
            LAST_BUTTONS.with(|lb| lb.set(cur_buttons)); // sync state silently
        } else if cur_buttons != prev_buttons {
            LAST_BUTTONS.with(|lb| lb.set(cur_buttons));
            let time = unsafe {
                let pi: *mut AnyObject = msg_send![objc2::class!(NSProcessInfo), processInfo];
                let uptime: f64 = msg_send![pi, systemUptime];
                (uptime * 1000.0) as u32
            };
            // Compute button state once from cur_buttons + current keyboard modifiers
            let btn_state = buttons_to_x11_state(cur_buttons) | get_keyboard_modifiers();
            // Check each of the first 3 buttons (left, right, middle)
            for btn_idx in 0u64..3 {
                let was = (prev_buttons >> btn_idx) & 1;
                let now = (cur_buttons >> btn_idx) & 1;
                if was == now { continue; }
                let x11_button: u8 = match btn_idx { 0 => 1, 1 => 3, 2 => 2, _ => 0 };
                // Find the SINGLE best (frontmost/largest) window under cursor
                // and send only ONE event. Sending to multiple windows causes
                // duplicate ButtonPress/Release which breaks implicit grab.
                WINDOWS.with(|w| {
                    let ws = w.borrow();
                    let mut best: Option<(crate::display::Xid, i16, i16, isize)> = None;
                    for (_id, info) in ws.iter() {
                        if !info.visible { continue; } // skip hidden render-only windows
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
                        if !in_window { continue; }
                        // Pick frontmost window (highest windowNumber)
                        let win_num: isize = unsafe { msg_send![&*info.window, windowNumber] };
                        if best.is_none() || win_num > best.unwrap().3 {
                            best = Some((info.x11_id, win_x, win_y, win_num));
                        }
                    }
                    if let Some((x11_id, win_x, win_y, _)) = best {
                        if now == 1 {
                            let btn_mask: u16 = match x11_button { 1=>0x100, 2=>0x200, 3=>0x400, _=>0 };
                            // Implicit grab: record window for this press
                            GRAB_WINDOW.with(|gw| gw.set(Some(x11_id)));
                            send_display_event(DisplayEvent::ButtonPress {
                                window: x11_id, button: x11_button,
                                x: win_x, y: win_y, root_x: rx, root_y: ry,
                                state: btn_state & !btn_mask, time,
                            });
                        } else {
                            send_display_event(DisplayEvent::ButtonRelease {
                                window: x11_id, button: x11_button,
                                x: win_x, y: win_y, root_x: rx, root_y: ry,
                                state: btn_state, time,
                            });
                            // Release grab when all buttons are up
                            if cur_buttons == 0 {
                                GRAB_WINDOW.with(|gw| gw.set(None));
                            }
                        }
                    }
                });
            }
        }

        let last = LAST_POINTER.with(|lp| lp.get());
        if rx != last.0 || ry != last.1 {
            let btn_state = buttons_to_x11_state(cur_buttons);
            LAST_POINTER.with(|lp| lp.set((rx, ry)));
            send_display_event(DisplayEvent::GlobalPointerUpdate { root_x: rx, root_y: ry });

            // Get timestamp once
            let time = unsafe {
                let pi: *mut AnyObject = msg_send![objc2::class!(NSProcessInfo), processInfo];
                let uptime: f64 = msg_send![pi, systemUptime];
                (uptime * 1000.0) as u32
            };
            // Send MotionNotify: during implicit grab (button held), send to grab window.
            // Otherwise send to the frontmost visible window under cursor.
            let grab_xid = GRAB_WINDOW.with(|gw| gw.get());
            WINDOWS.with(|w| {
                let ws = w.borrow();
                // If grab is active, find the grabbed window by x11_id and compute coords
                if let Some(gxid) = grab_xid {
                    if let Some((_wid, info)) = ws.iter().find(|(_, i)| i.x11_id == gxid) {
                        let frame: NSRect = unsafe { msg_send![&*info.window, frame] };
                        let content_h = if let Some(view) = info.window.contentView() {
                            let bounds: NSRect = unsafe { msg_send![&*view, bounds] };
                            bounds.size.height
                        } else { frame.size.height };
                        let win_x = (mouse_loc.x - frame.origin.x) as i16;
                        let win_y = (frame.origin.y + content_h - mouse_loc.y) as i16;
                        send_display_event(DisplayEvent::MotionNotify {
                            window: gxid, x: win_x, y: win_y,
                            root_x: rx, root_y: ry, state: btn_state, time,
                        });
                        return;
                    }
                }
                let mut best: Option<(crate::display::Xid, i16, i16, isize)> = None;
                for (_id, info) in ws.iter() {
                    if !info.visible { continue; }
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
                    if !in_window { continue; }
                    let win_num: isize = unsafe { msg_send![&*info.window, windowNumber] };
                    if best.is_none() || win_num > best.unwrap().3 {
                        best = Some((info.x11_id, win_x, win_y, win_num));
                    }
                }
                if let Some((x11_id, win_x, win_y, _)) = best {
                    send_display_event(DisplayEvent::MotionNotify {
                        window: x11_id,
                        x: win_x,
                        y: win_y,
                        root_x: rx,
                        root_y: ry,
                        state: btn_state,
                        time,
                    });
                }
            });
        }

        // 1.55. macOS-native resize cursor at window edges using private API
        // _windowResizeEastWestCursor = the "pinched vertical bar" cursor (same as macOS window edges)
        {
            const EDGE_PX: f64 = 6.0;
            let mut edge_type: u8 = 0; // 0=none, 1=left/right, 2=top/bottom, 3=NW-SE, 4=NE-SW
            WINDOWS.with(|w| {
                let ws = w.borrow();
                for (_id, info) in ws.iter() {
                    if !info.visible { continue; }
                    let frame: NSRect = unsafe { msg_send![&*info.window, frame] };
                    let mx = mouse_loc.x - frame.origin.x;
                    let my = mouse_loc.y - frame.origin.y;
                    if mx < -EDGE_PX || mx > frame.size.width + EDGE_PX
                        || my < -EDGE_PX || my > frame.size.height + EDGE_PX { continue; }
                    let near_left = mx < EDGE_PX;
                    let near_right = mx >= frame.size.width - EDGE_PX;
                    let near_bottom = my < EDGE_PX;
                    let near_top = my >= frame.size.height - EDGE_PX;
                    if (near_left && near_top) || (near_right && near_bottom) {
                        edge_type = 3; // NW-SE diagonal
                    } else if (near_right && near_top) || (near_left && near_bottom) {
                        edge_type = 4; // NE-SW diagonal
                    } else if near_left || near_right {
                        edge_type = 1; // East-West
                    } else if near_top || near_bottom {
                        edge_type = 2; // North-South
                    }
                    break;
                }
            });
            // Apply resize cursor every tick (macOS resets cursor each run loop iteration)
            if edge_type != 0 {
                unsafe {
                    let cls = objc2::class!(NSCursor);
                    let sel_name = match edge_type {
                        1 => c"_windowResizeEastWestCursor",
                        2 => c"_windowResizeNorthSouthCursor",
                        3 => c"_windowResizeNorthWestSouthEastCursor",
                        _ => c"_windowResizeNorthEastSouthWestCursor",
                    };
                    let sel = objc2::runtime::Sel::register(sel_name);
                    let cursor: *mut AnyObject = msg_send![cls, performSelector: sel];
                    if !cursor.is_null() {
                        let _: () = msg_send![&*cursor, set];
                    }
                }
            }
        }
    }

    // 1.6. Detect window resizes — compare content view size to stored IOSurface size
    check_window_resizes();

    // 1.6b. Execute deferred makeKeyAndOrderFront: from previous activate_app() call.
    // activate_app() calls activateIgnoringOtherApps (async WindowServer RPC), then sets
    // PENDING_KEY_WINDOW. We call makeKeyAndOrderFront one timer tick later so activation
    // has settled before we attempt to make the window key.
    PENDING_KEY_WINDOW.with(|p| {
        let win = p.get();
        if !win.is_null() {
            p.set(std::ptr::null_mut());
            unsafe {
                let _: () = msg_send![win, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
            }
        }
    });

    // 1.7. Send FocusIn when macOS key window changes (e.g. after title bar drag)
    check_key_window();

    // 1.8. Send FocusIn when mouse enters one of our windows (proactive EnterNotify).
    // Runs every frame so xterm gets FocusIn as soon as cursor enters, without
    // needing to click or drag — works even when pslXserver is not the active app.
    check_enter_notify();

    // 2. Drain render mailbox + render — targeted get_mut per window (avoids iter_mut's 16-shard scan)
    RENDER_MAILBOX.with(|mb| {
        let mb = mb.borrow();
        if let Some(ref mailbox) = *mb {
            // Note: do NOT call mailbox.is_empty() — it locks all 16 DashMap shards,
            // causing heavy contention with the protocol thread's write lock.
            // Instead, just iterate known window IDs and check each shard individually.
            WINDOWS.with(|w| {
                let mut ws = w.borrow_mut();
                let win_ids: Vec<u64> = ws.keys().copied().collect();
                for win_id in win_ids {
                    // get_mut locks only 1 shard (not all 16)
                    let commands = if let Some(mut entry) = mailbox.get_mut(&win_id) {
                        if entry.is_empty() { continue; }
                        std::mem::take(entry.value_mut())
                    } else {
                        continue;
                    };
                    // entry guard dropped here — shard unlocked before rendering

                    if let Some(info) = ws.get_mut(&win_id) {
                        let width = info.width as u32;
                        let height = info.height as u32;

                        // Drop redundant full-window PutImage frames — keep only the last one.
                        let commands = coalesce_putimage(commands, width, height);

                        // Skip expensive display→render surface copy when first command covers all.
                        let first_covers_all = matches!(commands.first(),
                            Some(crate::display::RenderCommand::PutImage { x, y, width: w, height: h, .. })
                            if *x == 0 && *y == 0 && *w as u32 >= width && *h as u32 >= height
                        );

                        unsafe {
                            IOSurfaceLock(info.surface, 0, std::ptr::null_mut());
                            if !first_covers_all {
                                IOSurfaceLock(info.display_surface, 1, std::ptr::null_mut());
                                let src = IOSurfaceGetBaseAddress(info.display_surface) as *const u8;
                                let dst = IOSurfaceGetBaseAddress(info.surface) as *mut u8;
                                let src_stride = IOSurfaceGetBytesPerRow(info.display_surface);
                                let dst_stride = IOSurfaceGetBytesPerRow(info.surface);
                                let h = info.height as usize;
                                if src_stride == dst_stride {
                                    std::ptr::copy_nonoverlapping(src, dst, src_stride * h);
                                } else {
                                    let row_bytes = (info.width as usize) * 4;
                                    for row in 0..h {
                                        std::ptr::copy_nonoverlapping(
                                            src.add(row * src_stride),
                                            dst.add(row * dst_stride),
                                            row_bytes,
                                        );
                                    }
                                }
                                IOSurfaceUnlock(info.display_surface, 1, std::ptr::null_mut());
                            }
                        }

                        let base = unsafe { IOSurfaceGetBaseAddress(info.surface) };
                        let bytes_per_row = unsafe { IOSurfaceGetBytesPerRow(info.surface) };
                        let buf_len = bytes_per_row * info.height as usize;
                        let buffer = unsafe {
                            std::slice::from_raw_parts_mut(base as *mut u8, buf_len)
                        };
                        let stride = bytes_per_row as u32;

                        for c in &commands {
                            render_to_buffer(buffer, width, height, stride, c);
                        }

                        unsafe { IOSurfaceUnlock(info.surface, 0, std::ptr::null_mut()); }

                        std::mem::swap(&mut info.surface, &mut info.display_surface);
                        flush_window(info);

                    }
                }
            });
        }
    });

    // Force immediate compositing after rendering (if any windows were drawn)
    // ca_transaction_flush is cheap when no CATransaction was started
    ca_transaction_flush();
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

            // Double-buffered IOSurfaces: render target + display target.
            // Alternating pointers forces CALayer to re-read pixel data each frame.
            let surface = create_iosurface(width, height);
            let display_surface = create_iosurface(width, height);
            if surface.is_null() || display_surface.is_null() {
                log::error!("Failed to create IOSurface for window {}", id);
                return;
            }

            // Fill both IOSurfaces with opaque black (0xFF000000 BGRA).
            // Without this, the IOSurface is transparent and the NSWindow's white background
            // shows through until the client draws, causing a visible white flash on startup.
            unsafe {
                for s in [surface, display_surface] {
                    IOSurfaceLock(s, 0, std::ptr::null_mut());
                    let base = IOSurfaceGetBaseAddress(s) as *mut u8;
                    let stride = IOSurfaceGetBytesPerRow(s);
                    let h = height as usize;
                    let w = width as usize;
                    for row in 0..h {
                        let row_ptr = base.add(row * stride) as *mut u32;
                        for col in 0..w {
                            *row_ptr.add(col) = 0xFF000000; // opaque black (BGRA)
                        }
                    }
                    IOSurfaceUnlock(s, 0, std::ptr::null_mut());
                }
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
                    // Ensure tracking area is set up now (AppKit may call updateTrackingAreas
                    // lazily; we call it explicitly so mouseEntered: fires from the start)
                    let _: () = msg_send![&*view, updateTrackingAreas];
                }
            }

            WINDOWS.with(|w| {
                w.borrow_mut().insert(id, WindowInfo { window, surface, display_surface, width, height, x11_id, x11_x, x11_y, cursor_type: 0, background_pixel: 0xFFFFFFFF, visible: false });
            });

            info!("Created window {} for X11 0x{:08X} ({}x{}) [IOSurface]", id, x11_id, width, height);
            let _ = reply.send(NativeWindowHandle { id });
        }

        DisplayCommand::ShowWindow { handle, visible } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().get_mut(&handle.id) {
                    info.visible = visible;
                    if visible {
                        info!("ShowWindow: id={} x11=0x{:08X} - makeKeyAndOrderFront", handle.id, info.x11_id);
                        let mtm = MainThreadMarker::new().unwrap();
                        let app = NSApplication::sharedApplication(mtm);
                        unsafe { let _: () = msg_send![&*app, activateIgnoringOtherApps: true]; }
                        info.window.makeKeyAndOrderFront(None);
                        // Update X11 focus to match macOS key window
                        let x11_id = info.x11_id;
                        send_display_event(DisplayEvent::FocusIn { window: x11_id });
                        flush_window(info);
                    } else {
                        info!("ShowWindow: id={} x11=0x{:08X} - hidden (render-only)", handle.id, info.x11_id);
                    }
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

        DisplayCommand::SetWindowIcon { handle, width, height, argb_data } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow().get(&handle.id) {
                    // Convert ARGB (network byte order) to RGBA for NSBitmapImageRep
                    let pixel_count = (width * height) as usize;
                    let mut rgba = vec![0u8; pixel_count * 4];
                    for i in 0..pixel_count {
                        let argb = argb_data[i];
                        let a = ((argb >> 24) & 0xFF) as u8;
                        let r = ((argb >> 16) & 0xFF) as u8;
                        let g = ((argb >> 8) & 0xFF) as u8;
                        let b = (argb & 0xFF) as u8;
                        rgba[i * 4]     = r;
                        rgba[i * 4 + 1] = g;
                        rgba[i * 4 + 2] = b;
                        rgba[i * 4 + 3] = a;
                    }
                    unsafe {
                        use objc2::msg_send;
                        use objc2::runtime::AnyObject;
                        // Create NSBitmapImageRep from RGBA data
                        let rep: *mut AnyObject = msg_send![objc2::class!(NSBitmapImageRep), alloc];
                        let rep: *mut AnyObject = msg_send![rep,
                            initWithBitmapDataPlanes: &rgba.as_ptr() as *const *const u8
                            pixelsWide: width as isize
                            pixelsHigh: height as isize
                            bitsPerSample: 8isize
                            samplesPerPixel: 4isize
                            hasAlpha: true
                            isPlanar: false
                            colorSpaceName: &*NSString::from_str("NSDeviceRGBColorSpace")
                            bytesPerRow: (width * 4) as isize
                            bitsPerPixel: 32isize
                        ];
                        if !rep.is_null() {
                            let size = NSSize { width: width as f64, height: height as f64 };
                            let image: *mut AnyObject = msg_send![objc2::class!(NSImage), alloc];
                            let image: *mut AnyObject = msg_send![image, initWithSize: size];
                            if !image.is_null() {
                                let _: () = msg_send![image, addRepresentation: rep];

                                // Set as Dock icon
                                let dock_tile: *mut AnyObject = msg_send![&*info.window, dockTile];
                                if !dock_tile.is_null() {
                                    // Reset image to original size for Dock
                                    let _: () = msg_send![image, setSize: size];
                                    let image_view: *mut AnyObject = msg_send![objc2::class!(NSImageView), alloc];
                                    let frame = NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size };
                                    let image_view: *mut AnyObject = msg_send![image_view, initWithFrame: frame];
                                    let _: () = msg_send![image_view, setImage: image];
                                    let _: () = msg_send![dock_tile, setContentView: image_view];
                                    let _: () = msg_send![dock_tile, display];
                                    let _: () = msg_send![image_view, release];
                                }
                                // Set as miniwindow + app Dock icon
                                let _: () = msg_send![image, setSize: size];
                                let _: () = msg_send![&*info.window, setMiniwindowImage: image];
                                let mtm = MainThreadMarker::new().unwrap();
                                let app = NSApplication::sharedApplication(mtm);
                                let _: () = msg_send![&*app, setApplicationIconImage: image];
                                let _: () = msg_send![image, release];
                            }
                            let _: () = msg_send![rep, release];
                        }
                    }
                    log::info!("SetWindowIcon: {}x{} icon applied to window {}", width, height, handle.id);
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

        DisplayCommand::ReadPixels { handle, x, y, width, height, reply } => {
            let result = WINDOWS.with(|w| {
                let ws = w.borrow();
                if let Some(info) = ws.get(&handle.id) {
                    unsafe {
                        let lock_result = IOSurfaceLock(info.surface, 1, std::ptr::null_mut()); // 1=read-only
                        if lock_result != 0 {
                            return None;
                        }
                        let base = IOSurfaceGetBaseAddress(info.surface) as *const u8;
                        let stride = IOSurfaceGetBytesPerRow(info.surface);
                        let surf_h = info.height as usize;
                        let w = width as usize;
                        let h = height as usize;
                        let mut pixels = vec![0u8; w * h * 4];
                        for row in 0..h {
                            let sy = y as usize + row;
                            if sy >= surf_h { break; }
                            let src_row = base.add(sy * stride);
                            let sx_start = (x.max(0) as usize) * 4;
                            let dst_off = row * w * 4;
                            let copy_bytes = (w * 4).min(stride - sx_start);
                            std::ptr::copy_nonoverlapping(
                                src_row.add(sx_start),
                                pixels.as_mut_ptr().add(dst_off),
                                copy_bytes,
                            );
                        }
                        IOSurfaceUnlock(info.surface, 1, std::ptr::null_mut());
                        Some(pixels)
                    }
                } else {
                    None
                }
            });
            let _ = reply.send(result);
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

/// Inner flush: set display_surface as CALayer contents.
/// Double-buffering ensures the pointer alternates each frame, forcing CALayer
/// to re-read pixel data. No CGImage creation needed — zero color-space conversion overhead.
fn flush_window(info: &WindowInfo) {
    unsafe {
        if info.width == 0 || info.height == 0 || info.display_surface.is_null() { return; }
        if let Some(view) = info.window.contentView() {
            let layer: *mut AnyObject = msg_send![&*view, layer];
            if !layer.is_null() {
                let ca_cls = objc_getClass(b"CATransaction\0".as_ptr());
                let _: () = msg_send![ca_cls, begin];
                let _: () = msg_send![ca_cls, setDisableActions: true];
                let _: () = msg_send![layer, setContents: info.display_surface as *mut AnyObject];
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
const NS_MAGNIFY: usize = 30;

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

    // Disable macOS autocorrect, inline predictions, and spellcheck
    // (these interfere with X11 key event delivery)
    unsafe {
        let defaults: *mut AnyObject = msg_send![objc2::class!(NSUserDefaults), standardUserDefaults];
        let key1: *mut AnyObject = msg_send![objc2::class!(NSString), stringWithUTF8String: c"NSAutomaticTextCompletionEnabled".as_ptr()];
        let key2: *mut AnyObject = msg_send![objc2::class!(NSString), stringWithUTF8String: c"NSAutomaticSpellingCorrectionEnabled".as_ptr()];
        let key3: *mut AnyObject = msg_send![objc2::class!(NSString), stringWithUTF8String: c"NSAutomaticTextReplacementEnabled".as_ptr()];
        let _: () = msg_send![&*defaults, setBool: false forKey: &*key1];
        let _: () = msg_send![&*defaults, setBool: false forKey: &*key2];
        let _: () = msg_send![&*defaults, setBool: false forKey: &*key3];
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

    // Install NSEvent global monitor for mouse-moved events.
    // NSTrackingArea (NSTrackingActiveAlways) does NOT fire mouseEntered: for
    // background apps on macOS Sequoia. Global monitors fire even when backgrounded
    // because they receive copies of events sent to OTHER apps.
    // We use this to detect cursor entering our windows and activate.
    // NSEventMaskMouseMoved = 1 << 5 = 32, NSEventMaskLeftMouseDragged = 1<<6
    unsafe {
        use block2::RcBlock;
        let handler = RcBlock::new(move |_event: *mut AnyObject| {
            // Only fire when pslXserver is NOT the active app.
            let mtm = MainThreadMarker::new().unwrap();
            let app = NSApplication::sharedApplication(mtm);
            let is_active: bool = msg_send![&*app, isActive];
            if is_active { return; } // Already active — nothing to do

            // Check if cursor is on the TOPMOST window and it's ours.
            let mouse_loc: NSPoint = msg_send![objc2::class!(NSEvent), mouseLocation];
            let topmost_win_num: isize = msg_send![objc2::class!(NSWindow),
                windowNumberAtPoint: mouse_loc
                belowWindowWithWindowNumber: 0isize];
            let (entered_xid, entered_nswin): (crate::display::Xid, *mut AnyObject) = WINDOWS.with(|w| {
                let ws = w.borrow();
                for (_id, info) in ws.iter() {
                    if !info.visible { continue; }
                    let is_mini: bool = msg_send![&*info.window, isMiniaturized];
                    if is_mini { continue; }
                    let win_num: isize = msg_send![&*info.window, windowNumber];
                    if win_num == topmost_win_num {
                        let ptr = &*info.window as *const NSWindow as *mut AnyObject;
                        return (info.x11_id, ptr);
                    }
                }
                (0, std::ptr::null_mut())
            });
            if entered_nswin.is_null() { return; }

            // Cursor is over one of our windows while backgrounded — activate.
            // No dedup on xid: user may cmd-tab while mouse is over our window,
            // leaving LAST_ENTER_WINDOW_X11 already set to entered_xid.
            // The is_active guard above prevents redundant calls once we're active.
            info!("GlobalMonitor: cursor over x11=0x{:08x} while backgrounded — activating", entered_xid);
            SUPPRESS_BUTTON_FRAMES.with(|s| s.set(8));
            LAST_ENTER_WINDOW_X11.with(|le| le.set(entered_xid));
            activate_app(entered_nswin);
            send_display_event(DisplayEvent::FocusIn { window: entered_xid });
        });
        let ns_event_cls = objc2::class!(NSEvent);
        // NSEventMaskMouseMoved(32) | NSEventMaskLeftMouseDragged(64) | NSEventMaskRightMouseDragged(128)
        // NSEventTypeOtherMouseDragged = 27 → 1<<27 = 134217728
        // NOTE: no keyboard masks — those require Accessibility permission
        let mask: u64 = (1 << 5) | (1 << 6) | (1 << 7) | (1 << 27);
        let block_ptr = block2::RcBlock::as_ptr(&handler);
        let _monitor: *mut AnyObject = msg_send![
            ns_event_cls,
            addGlobalMonitorForEventsMatchingMask: mask
            handler: block_ptr
        ];
        info!("Installed NSEvent global monitor for mouse-enter focus (monitor={:?})", _monitor);
        // Leak the block — the monitor runs forever (app lifetime)
        std::mem::forget(handler);
    }

    // CGEventTap: WindowServer-level mouse monitor.
    // Fires for ALL mouse events regardless of which app is active (including Finder).
    // Added to the main run loop so the callback executes on the main thread —
    // this is a user-event context that macOS Sequoia respects for activation.
    // kCGSessionEventTap=1, kCGHeadInsertEventTap=0, kCGEventTapOptionListenOnly=1
    // Without Accessibility permission, creates a passive (observe-only) tap — still works.
    unsafe {
        // bit 1=LeftMouseDown, 3=RightMouseDown (catch activation click to suppress ButtonPress),
        // 5=MouseMoved, 6=LeftMouseDragged, 7=RightMouseDragged, 27=OtherMouseDragged
        let mouse_mask: u64 = (1u64 << 1) | (1u64 << 3) | (1u64 << 5) | (1u64 << 6) | (1u64 << 7) | (1u64 << 27);
        let tap = CGEventTapCreate(
            1, // kCGSessionEventTap
            0, // kCGHeadInsertEventTap
            1, // kCGEventTapOptionListenOnly
            mouse_mask,
            cg_mouse_tap_callback,
            std::ptr::null_mut(),
        );
        if !tap.is_null() {
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
            if !source.is_null() {
                CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
                info!("Installed CGEventTap for focus-follows-mouse (tap={:?})", tap);
            }
        } else {
            info!("CGEventTap creation failed (Accessibility not granted) — relying on NSEvent global monitor");
        }
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

    // Set "X" application icon for Dock
    unsafe {
        extern "C" {
            fn CGColorSpaceCreateDeviceRGB() -> *mut std::ffi::c_void;
            fn CGBitmapContextCreate(
                data: *mut std::ffi::c_void, width: usize, height: usize,
                bits_per_component: usize, bytes_per_row: usize,
                colorspace: *mut std::ffi::c_void, bitmap_info: u32,
            ) -> *mut std::ffi::c_void;
            fn CGContextSetRGBFillColor(ctx: *mut std::ffi::c_void, r: f64, g: f64, b: f64, a: f64);
            fn CGContextFillRect(ctx: *mut std::ffi::c_void, rect: CGRect);
            fn CGContextRelease(ctx: *mut std::ffi::c_void);
            fn CGColorSpaceRelease(cs: *mut std::ffi::c_void);
            fn CTFontCreateWithName(name: *const std::ffi::c_void, size: f64, matrix: *const std::ffi::c_void) -> *mut std::ffi::c_void;
            fn CTLineCreateWithAttributedString(attr_str: *const std::ffi::c_void) -> *mut std::ffi::c_void;
            fn CTLineDraw(line: *const std::ffi::c_void, ctx: *mut std::ffi::c_void);
            fn CGContextSetTextPosition(ctx: *mut std::ffi::c_void, x: f64, y: f64);
            fn CFRelease(cf: *mut std::ffi::c_void);
        }
        #[repr(C)]
        struct CGRect { x: f64, y: f64, w: f64, h: f64 }
        let icon_size: usize = 128;
        let stride = icon_size * 4;
        let mut buf = vec![0u8; stride * icon_size];
        let cs = CGColorSpaceCreateDeviceRGB();
        let ctx = CGBitmapContextCreate(
            buf.as_mut_ptr() as *mut _, icon_size, icon_size, 8, stride, cs, 0x2002,
        );
        if !ctx.is_null() {
            // Black background with rounded feel
            CGContextSetRGBFillColor(ctx, 0.1, 0.1, 0.1, 1.0);
            CGContextFillRect(ctx, CGRect { x: 0.0, y: 0.0, w: 128.0, h: 128.0 });
            // White "X" text
            CGContextSetRGBFillColor(ctx, 1.0, 1.0, 1.0, 1.0);
            let font_name: *mut AnyObject = msg_send![
                objc2::class!(NSString), stringWithUTF8String: c"Helvetica-Bold".as_ptr()
            ];
            let ct_font = CTFontCreateWithName(font_name as *const _, 100.0, std::ptr::null());
            if !ct_font.is_null() {
                extern "C" {
                    static kCTFontAttributeName: *const std::ffi::c_void;
                    static kCTForegroundColorFromContextAttributeName: *const std::ffi::c_void;
                }
                let yes: *mut AnyObject = msg_send![objc2::class!(NSNumber), numberWithBool: true];
                let keys = [kCTFontAttributeName as *const AnyObject, kCTForegroundColorFromContextAttributeName as *const AnyObject];
                let vals = [ct_font as *const AnyObject, yes as *const AnyObject];
                let attrs: *mut AnyObject = msg_send![
                    objc2::class!(NSDictionary), dictionaryWithObjects: vals.as_ptr() forKeys: keys.as_ptr() count: 2usize
                ];
                let x_str: *mut AnyObject = msg_send![
                    objc2::class!(NSString), stringWithUTF8String: c"X".as_ptr()
                ];
                let attr_str: *mut AnyObject = msg_send![objc2::class!(NSAttributedString), alloc];
                let attr_str: *mut AnyObject = msg_send![attr_str, initWithString: x_str attributes: attrs];
                if !attr_str.is_null() {
                    let ct_line = CTLineCreateWithAttributedString(attr_str as *const _);
                    if !ct_line.is_null() {
                        CGContextSetTextPosition(ctx, 28.0, 18.0);
                        CTLineDraw(ct_line as *const _, ctx);
                        CFRelease(ct_line);
                    }
                    let _: () = msg_send![&*attr_str, release];
                }
                CFRelease(ct_font);
            }
            CGContextRelease(ctx);

            // Create NSImage from bitmap
            let rep: *mut AnyObject = msg_send![objc2::class!(NSBitmapImageRep), alloc];
            let rep: *mut AnyObject = msg_send![rep,
                initWithBitmapDataPlanes: &buf.as_ptr() as *const *const u8
                pixelsWide: 128isize pixelsHigh: 128isize
                bitsPerSample: 8isize samplesPerPixel: 4isize
                hasAlpha: true isPlanar: false
                colorSpaceName: &*NSString::from_str("NSDeviceRGBColorSpace")
                bytesPerRow: stride as isize bitsPerPixel: 32isize
            ];
            if !rep.is_null() {
                let sz = NSSize { width: 128.0, height: 128.0 };
                let image: *mut AnyObject = msg_send![objc2::class!(NSImage), alloc];
                let image: *mut AnyObject = msg_send![image, initWithSize: sz];
                if !image.is_null() {
                    let _: () = msg_send![image, addRepresentation: rep];
                    let _: () = msg_send![&*app, setApplicationIconImage: image];
                    let _: () = msg_send![image, release];
                }
                let _: () = msg_send![rep, release];
            }
        }
        CGColorSpaceRelease(cs);
    }

    // Build keyboard layout map from macOS UCKeyTranslate (supports JIS, US, UK, etc.)
    let kmap = build_keyboard_map();
    let _ = crate::display::KEYBOARD_MAP.set(kmap);
    info!("Keyboard layout map built via UCKeyTranslate");

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

    // When user clicks on a window, update X11 focus to that window
    if event_type == NS_LEFT_MOUSE_DOWN || event_type == NS_RIGHT_MOUSE_DOWN || event_type == NS_OTHER_MOUSE_DOWN {
        send_display_event(DisplayEvent::FocusIn { window: x11_id });
    }

    match event_type {
        NS_LEFT_MOUSE_DOWN | NS_RIGHT_MOUSE_DOWN | NS_OTHER_MOUSE_DOWN => {
            let button: u8 = match event_type {
                NS_LEFT_MOUSE_DOWN => 1,
                NS_RIGHT_MOUSE_DOWN => 3,
                _ => 2,
            };
            // Suppress spurious button events from app activation (SUPPRESS_BUTTON_FRAMES set
            // by mouseEntered: before calling activateIgnoringOtherApps:).
            if SUPPRESS_BUTTON_FRAMES.with(|s| s.get()) > 0 {
                let cur: u64 = unsafe { msg_send![objc2::class!(NSEvent), pressedMouseButtons] };
                LAST_BUTTONS.with(|lb| lb.set(cur));
                // fall through to sync LAST_BUTTONS at end of arm
            } else {
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
            } // end else (not suppressed)
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
                // X11 spec: ButtonRelease state includes the releasing button mask
                // (held "just before" the event). macOS pressedMouseButtons is already 0 at this point.
                let btn_mask: u16 = match button { 1 => 0x100, 2 => 0x200, 3 => 0x400, _ => 0 };
                let state = get_modifier_state(event) | btn_mask;
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
                    window: x11_id, keycode: macos_keycode_to_x11(keycode), state, time,
                });
            }
        }
        NS_SCROLL_WHEEL => {
            let (x, y) = get_event_location(event, x11_id, win_width, win_height);
            let (root_x, root_y) = get_screen_mouse_location();
            let state = get_modifier_state(event);
            let has_precise: bool = unsafe { msg_send![event, hasPreciseScrollingDeltas] };
            if has_precise {
                // Ignore momentum-phase events (after finger is lifted).
                // Momentum scroll causes unintended extra scrolling ("bounce").
                // NSEventPhaseMomentum = Began(0x01)|Changed(0x04)|Ended(0x08)|Cancelled(0x10)
                let momentum_phase: u64 = unsafe { msg_send![event, momentumPhase] };
                if momentum_phase != 0 {
                    // Reset accumulator so residual doesn't trigger on next real scroll
                    SCROLL_ACCUM.with(|a| a.set(0.0));
                    SCROLL_ACCUM_X.with(|a| a.set(0.0));
                    // skip this event
                }
                // Only process user-driven (non-momentum) scroll events
                if momentum_phase == 0 {

                // Trackpad: accumulate pixel deltas, emit X11 scroll when threshold hit.
                // macOS sends many small deltas (2-5px each); X11 button 4/5 = ~3 lines.
                let threshold = 40.0; // pixels per X11 scroll click

                // Vertical scroll (Button 4=up, 5=down)
                let delta_y: f64 = unsafe { msg_send![event, scrollingDeltaY] };
                if delta_y.abs() > 0.1 {
                    let accum = SCROLL_ACCUM.with(|a| {
                        let mut v = a.get() + delta_y;
                        if (v > 0.0) != (delta_y > 0.0) && delta_y != 0.0 { v = delta_y; }
                        a.set(v);
                        v
                    });
                    let clicks = (accum.abs() / threshold) as u32;
                    if clicks > 0 {
                        let button = if accum > 0.0 { 4u8 } else { 5u8 };
                        SCROLL_ACCUM.with(|a| {
                            a.set(accum - accum.signum() * (clicks as f64 * threshold));
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
                }

                // Horizontal scroll (Button 6=left, 7=right)
                let delta_x: f64 = unsafe { msg_send![event, scrollingDeltaX] };
                if delta_x.abs() > 0.1 {
                    let accum_x = SCROLL_ACCUM_X.with(|a| {
                        let mut v = a.get() + delta_x;
                        if (v > 0.0) != (delta_x > 0.0) && delta_x != 0.0 { v = delta_x; }
                        a.set(v);
                        v
                    });
                    let clicks_x = (accum_x.abs() / threshold) as u32;
                    if clicks_x > 0 {
                        let button = if accum_x > 0.0 { 6u8 } else { 7u8 };
                        SCROLL_ACCUM_X.with(|a| {
                            a.set(accum_x - accum_x.signum() * (clicks_x as f64 * threshold));
                        });
                        for _ in 0..clicks_x.min(3) {
                            send_display_event(DisplayEvent::ButtonPress {
                                window: x11_id, button, x, y, root_x, root_y, state, time,
                            });
                            send_display_event(DisplayEvent::ButtonRelease {
                                window: x11_id, button, x, y, root_x, root_y, state, time,
                            });
                        }
                    }
                }

                } // end if momentum_phase == 0
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
        NS_MAGNIFY => {
            // Pinch-to-zoom → Ctrl + scroll (Button 4/5).
            // Chrome/Firefox interpret Ctrl+scroll as page zoom.
            let (x, y) = get_event_location(event, x11_id, win_width, win_height);
            let (root_x, root_y) = get_screen_mouse_location();
            let state = get_modifier_state(event) | 0x0004; // Add ControlMask
            let magnification: f64 = unsafe { msg_send![event, magnification] };
            let accum = MAGNIFY_ACCUM.with(|a| {
                let v = a.get() + magnification;
                a.set(v);
                v
            });
            let threshold = 0.1; // magnification units per scroll click
            let clicks = (accum.abs() / threshold) as u32;
            if clicks > 0 {
                let button = if accum > 0.0 { 4u8 } else { 5u8 }; // zoom in = scroll up
                MAGNIFY_ACCUM.with(|a| {
                    a.set(accum - accum.signum() * (clicks as f64 * threshold));
                });
                for _ in 0..clicks.min(5) {
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

/// Build a keyboard map from the current macOS keyboard layout using UCKeyTranslate.
/// Returns an array of 128 entries (indexed by macOS keycode):
/// each entry is (normal_keysym, shifted_keysym) in X11 keysym encoding.
/// This correctly handles JIS, US, UK, AZERTY and all other layouts.
pub fn build_keyboard_map() -> Box<[(u32, u32); 128]> {
    use std::ffi::c_void;
    extern "C" {
        fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *mut c_void;
        fn TISGetInputSourceProperty(source: *mut c_void, property_key: *const c_void) -> *mut c_void;
        fn CFDataGetBytePtr(data: *mut c_void) -> *const u8;
        fn CFRelease(cf: *const c_void);
        fn UCKeyTranslate(
            key_layout_ptr: *const u8,
            virtual_key_code: u16,
            key_action: u16,
            modifier_key_state: u32,
            keyboard_type: u32,
            key_translate_options: u32,
            dead_key_state: *mut u32,
            max_string_length: usize,
            actual_string_length: *mut usize,
            unicode_string: *mut u16,
        ) -> i32;
        fn LMGetKbdType() -> u8;
        static kTISPropertyUnicodeKeyLayoutData: *const c_void;
    }

    let mut map = Box::new([(0u32, 0u32); 128]);

    unsafe {
        let source = TISCopyCurrentASCIICapableKeyboardLayoutInputSource();
        if source.is_null() {
            log::warn!("build_keyboard_map: TISCopyCurrentASCIICapableKeyboardLayoutInputSource returned null");
            return map;
        }

        let layout_data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
        if layout_data.is_null() {
            log::warn!("build_keyboard_map: no UnicodeKeyLayoutData (maybe input source is not a keyboard layout)");
            CFRelease(source);
            return map;
        }

        let layout_ptr = CFDataGetBytePtr(layout_data as *mut c_void);
        if layout_ptr.is_null() {
            CFRelease(source);
            return map;
        }

        let kbd_type = LMGetKbdType() as u32;
        // kUCKeyActionDisplay = 3: returns the character displayed on the key cap, avoids dead-key composition
        let key_action = 3u16;

        for keycode in 0u16..128 {
            let translate = |modifier: u32| -> u32 {
                let mut dead: u32 = 0;
                let mut buf = [0u16; 4];
                let mut actual_len: usize = 0;
                let status = UCKeyTranslate(
                    layout_ptr, keycode, key_action, modifier,
                    kbd_type, 0, &mut dead, 4, &mut actual_len, buf.as_mut_ptr(),
                );
                if status != 0 || actual_len == 0 { return 0; }
                unicode_to_x11_keysym(buf[0] as u32)
            };

            let normal  = translate(0);          // no modifier
            let shifted  = translate(2);         // shiftKey >> 8 = 512 >> 8 = 2
            map[keycode as usize] = (normal, shifted);
        }

        CFRelease(source);
    }

    info!("build_keyboard_map: built layout for {} keycodes", 128);
    map
}

/// Convert a Unicode codepoint returned by UCKeyTranslate to an X11 keysym.
fn unicode_to_x11_keysym(cp: u32) -> u32 {
    if cp == 0 { return 0; }
    // Skip control characters and non-printable ranges
    if cp < 0x20 { return 0; }
    // Skip DEL and C1 control characters
    if cp >= 0x7F && cp < 0xA0 { return 0; }
    // Skip Unicode private-use area (returned by UCKeyTranslate for special keys like arrows)
    if cp >= 0xE000 && cp <= 0xF8FF { return 0; }
    // Latin-1 (0x20-0xFF): X11 keysym == Unicode codepoint
    if cp <= 0xFF { return cp; }
    // Unicode BMP and beyond: X11 keysym = 0x01000000 | codepoint
    0x01000000 | cp
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
/// Convert macOS pressedMouseButtons bitmask to X11 button state.
/// Call once and reuse, avoiding repeated ObjC calls.
fn buttons_to_x11_state(pressed: u64) -> u16 {
    let mut state = 0u16;
    if pressed & (1 << 0) != 0 { state |= 0x100; }  // Button1Mask
    if pressed & (1 << 1) != 0 { state |= 0x400; }  // Button3Mask
    if pressed & (1 << 2) != 0 { state |= 0x200; }  // Button2Mask
    state
}

fn get_mouse_button_state() -> u16 {
    let pressed: u64 = unsafe { msg_send![objc2::class!(NSEvent), pressedMouseButtons] };
    buttons_to_x11_state(pressed)
}

/// Get keyboard modifier state without an NSEvent (for polling path).
fn get_keyboard_modifiers() -> u16 {
    let flags: u64 = unsafe { msg_send![objc2::class!(NSEvent), modifierFlags] };
    let mut state = 0u16;
    if flags & (1 << 16) != 0 { state |= 2; }   // CapsLock → LockMask
    if flags & (1 << 17) != 0 { state |= 1; }   // Shift → ShiftMask
    if flags & (1 << 18) != 0 { state |= 4; }   // Control → ControlMask
    if flags & (1 << 19) != 0 { state |= 8; }   // Option → Mod1Mask
    if flags & (1 << 20) != 0 { state |= 64; }  // Command → Mod4Mask
    state
}

fn send_display_event(evt: DisplayEvent) {
    EVT_TX.with(|tx| {
        if let Some(ref tx) = *tx.borrow() {
            let _ = tx.send(evt);
        }
    });
}
