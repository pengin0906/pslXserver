// iOS display backend — UIWindow/UIView management, IOSurface-backed pixel buffer rendering
// Mirrors macos.rs but uses UIKit instead of AppKit.
// Single fullscreen UIView — all X11 windows rendered to one surface.
#![cfg(target_os = "ios")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use crossbeam_channel::{Receiver, Sender};
use log::{debug, info, warn};

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2_foundation::{MainThreadMarker, NSString};

use crate::display::{DisplayCommand, DisplayEvent, NativeWindowHandle};
use crate::display::renderer::render_to_buffer;

/// Write debug message to /tmp/pslx_ios.log (visible from host for simulator debugging).
fn ios_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/pslx_ios.log") {
        let _ = writeln!(f, "{}", msg);
    }
}

// --- IOSurface FFI (same as macOS) ---
extern "C" {
    fn IOSurfaceCreate(properties: *const c_void) -> *mut c_void;
    fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
    fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
}

// --- CoreFoundation FFI (same as macOS) ---
extern "C" {
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFAbsoluteTimeGetCurrent() -> f64;
    fn CFRunLoopTimerCreate(
        allocator: *const c_void, fire_date: f64, interval: f64,
        flags: u64, order: i64,
        callout: extern "C" fn(*mut c_void, *mut c_void),
        context: *mut c_void,
    ) -> *mut c_void;
    fn CFRunLoopAddTimer(rl: *mut c_void, timer: *mut c_void, mode: *const c_void);
    static kCFRunLoopCommonModes: *const c_void;

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

// --- IOSurface property keys ---
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
    let bpe: i32 = 4;
    let bpr: i32 = w * 4;
    let pixel_format: i32 = 0x42475241; // 'BGRA'

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
    /// IOSurface-backed pixel buffer for software rendering (render target).
    surface: *mut c_void,
    /// Second IOSurface for double-buffering display.
    display_surface: *mut c_void,
    width: u16,
    height: u16,
    /// X11 window ID for routing events back to clients.
    x11_id: crate::display::Xid,
    /// X11 screen position.
    x11_x: i16,
    x11_y: i16,
    background_pixel: u32,
    visible: bool,
    /// Window title from X11 (same as macOS NSWindow.setTitle).
    title: String,
    /// override_redirect flag (same as macOS borderless style).
    override_redirect: bool,
    /// CALayer for rendering this window's content (UIView.layer, like macOS contentView.layer).
    ca_layer: *mut AnyObject,
    /// Per-window UIWindow (like macOS NSWindow). May be null until scene_will_connect.
    ui_window: *mut AnyObject,
    /// Per-window PSLXView (like macOS PSLXInputView/contentView).
    ui_view: *mut AnyObject,
    /// Deferred show: ShowWindow was called before scene_will_connect created the UIWindow.
    pending_show: bool,
}

impl Drop for WindowInfo {
    fn drop(&mut self) {
        if !self.surface.is_null() {
            unsafe { CFRelease(self.surface as *const c_void); }
        }
        if !self.display_surface.is_null() {
            unsafe { CFRelease(self.display_surface as *const c_void); }
        }
        if !self.ca_layer.is_null() {
            unsafe {
                let _: () = msg_send![self.ca_layer, removeFromSuperlayer];
                CFRelease(self.ca_layer as *const c_void);
            }
        }
        // UIWindow/UIView are managed by UIKit, just release our retain
        if !self.ui_window.is_null() {
            unsafe { CFRelease(self.ui_window as *const c_void); }
        }
    }
}

/// Queue of native window IDs waiting for a UIScene to be created.
/// Accessed from main thread only (UIKit callbacks).
static PENDING_SCENE_WINDOWS: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

thread_local! {
    static WINDOWS: RefCell<HashMap<u64, WindowInfo>> = RefCell::new(HashMap::new());
    static CMD_RX: RefCell<Option<Receiver<DisplayCommand>>> = RefCell::new(None);
    static EVT_TX: RefCell<Option<Sender<DisplayEvent>>> = RefCell::new(None);
    static RENDER_MAILBOX: RefCell<Option<crate::display::RenderMailbox>> = RefCell::new(None);
    static NEXT_ID: RefCell<u64> = RefCell::new(1);
    static LAST_POINTER: std::cell::Cell<(i16, i16)> = const { std::cell::Cell::new((0, 0)) };
    /// Cascade offset for new windows (incremented each CreateWindow).
    static CASCADE_OFFSET: std::cell::Cell<i16> = const { std::cell::Cell::new(0) };
    /// Implicit pointer grab: window that received the most recent touch down.
    static GRAB_WINDOW: std::cell::Cell<Option<crate::display::Xid>> = const { std::cell::Cell::new(None) };
    /// Drag state: (native_window_id, start_layer_x, start_layer_y, start_touch_x, start_touch_y)
    static DRAG_STATE: std::cell::Cell<Option<(u64, f64, f64, f64, f64)>> = const { std::cell::Cell::new(None) };
    /// The main UIView (our single fullscreen view). Pointer stored for layer access.
    static MAIN_VIEW: RefCell<Option<*mut AnyObject>> = RefCell::new(None);
    /// The main UIWindow (used for keyboard/lifecycle, not for X11 content).
    static MAIN_UI_WINDOW: RefCell<Option<*mut AnyObject>> = RefCell::new(None);
    /// The main UIWindowScene — X11 UIWindows are created in this scene.
    /// Like macOS where all NSWindows are in the same app.
    static MAIN_SCENE: RefCell<Option<*mut AnyObject>> = RefCell::new(None);
    /// Screen dimensions in points.
    static SCREEN_WIDTH: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    static SCREEN_HEIGHT: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

fn alloc_id() -> u64 {
    NEXT_ID.with(|id| {
        let v = *id.borrow();
        *id.borrow_mut() = v + 1;
        v
    })
}

fn send_display_event(evt: DisplayEvent) {
    EVT_TX.with(|tx| {
        if let Some(ref tx) = *tx.borrow() {
            let _ = tx.send(evt);
        }
    });
}

/// Register the PSLXView class (custom UIView for touch and keyboard input).
fn get_pslx_view_class() -> &'static AnyClass {
    use std::sync::Once;
    static mut CLASS: *const AnyClass = std::ptr::null();
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let superclass = objc2::class!(UIView);
        let mut builder = ClassBuilder::new(c"PSLXView", superclass)
            .expect("Failed to create PSLXView class");

        builder.add_ivar::<u8>(c"textInserted");

        extern "C" {
            fn class_addMethod(
                cls: *mut c_void,
                sel: Sel,
                imp: *const c_void,
                types: *const std::ffi::c_char,
            ) -> bool;
            fn class_addProtocol(
                cls: *mut c_void,
                protocol: *const c_void,
            ) -> bool;
            fn objc_getProtocol(
                name: *const std::ffi::c_char,
            ) -> *const c_void;
        }

        let raw_cls = builder.register() as *const AnyClass as *mut c_void;

        unsafe {
            // Conform to UIKeyInput protocol for software keyboard
            let proto = objc_getProtocol(c"UIKeyInput".as_ptr() as _);
            if !proto.is_null() {
                class_addProtocol(raw_cls, proto);
            }

            // canBecomeFirstResponder -> YES
            class_addMethod(raw_cls, objc2::sel!(canBecomeFirstResponder),
                can_become_first_responder as *const c_void, c"B@:".as_ptr() as _);

            // UIKeyInput methods
            class_addMethod(raw_cls, objc2::sel!(hasText),
                has_text as *const c_void, c"B@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(insertText:),
                ios_insert_text as *const c_void, c"v@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(deleteBackward),
                delete_backward as *const c_void, c"v@:".as_ptr() as _);

            // Touch handling
            class_addMethod(raw_cls, objc2::sel!(touchesBegan:withEvent:),
                touches_began as *const c_void, c"v@:@@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(touchesMoved:withEvent:),
                touches_moved as *const c_void, c"v@:@@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(touchesEnded:withEvent:),
                touches_ended as *const c_void, c"v@:@@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(touchesCancelled:withEvent:),
                touches_cancelled as *const c_void, c"v@:@@".as_ptr() as _);

            // UITextInputTraits — keyboard type
            class_addMethod(raw_cls, objc2::sel!(keyboardType),
                keyboard_type as *const c_void, c"q@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(autocorrectionType),
                autocorrection_type as *const c_void, c"q@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(autocapitalizationType),
                autocapitalization_type as *const c_void, c"q@:".as_ptr() as _);

            // Hardware keyboard support (pressesBegan/pressesEnded)
            class_addMethod(raw_cls, objc2::sel!(pressesBegan:withEvent:),
                presses_began as *const c_void, c"v@:@@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(pressesEnded:withEvent:),
                presses_ended as *const c_void, c"v@:@@".as_ptr() as _);

            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

// --- PSLXView method implementations ---

unsafe extern "C" fn can_become_first_responder(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

unsafe extern "C" fn has_text(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

/// UIKeyInput insertText: — called when user types on software or hardware keyboard.
/// On hardware keyboard, pressesBegan already sends KeyPress for physical keys,
/// so single ASCII chars are skipped here (same as macOS insertText logic).
unsafe extern "C" fn ios_insert_text(_this: *mut AnyObject, _sel: Sel, text: *mut AnyObject) {
    if text.is_null() { return; }

    let utf8: *const std::os::raw::c_char = msg_send![&*text, UTF8String];
    if utf8.is_null() { return; }
    let rust_str = match std::ffi::CStr::from_ptr(utf8).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return,
    };
    if rust_str.is_empty() { return; }

    // Single ASCII characters: skip — pressesBegan already sent KeyPress via physical keycode.
    // This matches macOS insertText behavior (UCKeyTranslate keymapping handles ASCII).
    let is_single_ascii = rust_str.len() == 1 && rust_str.as_bytes()[0] < 0x80;
    if is_single_ascii {
        return;
    }

    // Non-ASCII (IME, emoji, etc.) → ImeCommit
    let x11_id = get_focused_x11_window();
    if x11_id == 0 { return; }
    info!("iOS insertText: '{}' for window 0x{:08x}", rust_str, x11_id);
    send_display_event(DisplayEvent::ImeCommit {
        window: x11_id,
        text: rust_str,
    });
}

/// UIKeyInput deleteBackward — Backspace key
unsafe extern "C" fn delete_backward(_this: *mut AnyObject, _sel: Sel) {
    let x11_id = get_focused_x11_window();
    if x11_id == 0 { return; }
    let time = get_timestamp();
    // X11 keycode for BackSpace = 22 (8 + macOS keycode 51... just use 22 directly)
    send_display_event(DisplayEvent::KeyPress {
        window: x11_id, keycode: 22, state: 0, time,
    });
    send_display_event(DisplayEvent::KeyRelease {
        window: x11_id, keycode: 22, state: 0, time,
    });
}

// UITextInputTraits
unsafe extern "C" fn keyboard_type(_this: *mut AnyObject, _sel: Sel) -> i64 {
    0 // UIKeyboardTypeDefault
}

unsafe extern "C" fn autocorrection_type(_this: *mut AnyObject, _sel: Sel) -> i64 {
    1 // UITextAutocorrectionTypeNo
}

unsafe extern "C" fn autocapitalization_type(_this: *mut AnyObject, _sel: Sel) -> i64 {
    0 // UITextAutocapitalizationTypeNone
}

// --- Touch handling → X11 mouse events ---

/// Height of the drag handle area at the top of each window (points).
const TITLE_BAR_HEIGHT: i16 = 30;

unsafe extern "C" fn touches_began(this: *mut AnyObject, _sel: Sel, touches: *mut AnyObject, _event: *mut AnyObject) {
    let touch = get_first_touch(touches);
    if touch.is_null() { return; }
    let (x, y) = touch_location_in_view(touch, this);
    let time = get_timestamp();

    // Check if touch is in a window's title bar (top TITLE_BAR_HEIGHT pixels)
    if let Some((native_id, layer_x, layer_y)) = find_window_titlebar_at(x, y) {
        DRAG_STATE.with(|ds| ds.set(Some((native_id, layer_x, layer_y, x as f64, y as f64))));
        return; // Don't send X11 events during drag
    }

    let x11_id = find_x11_window_at(x, y);
    if x11_id == 0 {
        // Tap on empty area — toggle keyboard
        let _: () = msg_send![this, becomeFirstResponder];
        return;
    }
    let (win_x, win_y) = screen_to_window_coords(x, y, x11_id);

    // Show keyboard when tapping an X11 window
    let _: () = msg_send![this, becomeFirstResponder];

    GRAB_WINDOW.with(|gw| gw.set(Some(x11_id)));
    send_display_event(DisplayEvent::ButtonPress {
        window: x11_id, button: 1,
        x: win_x, y: win_y, root_x: x, root_y: y,
        state: 0, time,
    });
    send_display_event(DisplayEvent::FocusIn { window: x11_id });
}

unsafe extern "C" fn touches_moved(this: *mut AnyObject, _sel: Sel, touches: *mut AnyObject, _event: *mut AnyObject) {
    let touch = get_first_touch(touches);
    if touch.is_null() { return; }
    let (x, y) = touch_location_in_view(touch, this);

    // Check if we're in drag mode
    let drag = DRAG_STATE.with(|ds| ds.get());
    if let Some((native_id, start_lx, start_ly, start_tx, start_ty)) = drag {
        let dx = x as f64 - start_tx;
        let dy = y as f64 - start_ty;
        let new_x = start_lx + dx;
        let new_y = start_ly + dy;
        move_window_layer(native_id, new_x, new_y);
        return;
    }

    let time = get_timestamp();
    let grab_xid = GRAB_WINDOW.with(|gw| gw.get());
    let x11_id = grab_xid.unwrap_or_else(|| find_x11_window_at(x, y));
    if x11_id == 0 { return; }
    let (win_x, win_y) = screen_to_window_coords(x, y, x11_id);

    LAST_POINTER.with(|lp| lp.set((x, y)));
    send_display_event(DisplayEvent::MotionNotify {
        window: x11_id, x: win_x, y: win_y,
        root_x: x, root_y: y, state: 0x100, time,
    });
}

unsafe extern "C" fn touches_ended(this: *mut AnyObject, _sel: Sel, touches: *mut AnyObject, _event: *mut AnyObject) {
    let touch = get_first_touch(touches);
    if touch.is_null() { return; }
    let (x, y) = touch_location_in_view(touch, this);

    // End drag if active
    let drag = DRAG_STATE.with(|ds| ds.get());
    if let Some((native_id, start_lx, start_ly, start_tx, start_ty)) = drag {
        let dx = x as f64 - start_tx;
        let dy = y as f64 - start_ty;
        let new_x = start_lx + dx;
        let new_y = start_ly + dy;
        move_window_layer(native_id, new_x, new_y);
        // Update x11 position
        update_window_position(native_id, new_x as i16, new_y as i16);
        DRAG_STATE.with(|ds| ds.set(None));
        return;
    }

    let time = get_timestamp();
    let grab_xid = GRAB_WINDOW.with(|gw| gw.get());
    let x11_id = grab_xid.unwrap_or_else(|| find_x11_window_at(x, y));
    GRAB_WINDOW.with(|gw| gw.set(None));
    if x11_id == 0 { return; }
    let (win_x, win_y) = screen_to_window_coords(x, y, x11_id);

    send_display_event(DisplayEvent::ButtonRelease {
        window: x11_id, button: 1,
        x: win_x, y: win_y, root_x: x, root_y: y,
        state: 0x100, time,
    });
}

unsafe extern "C" fn touches_cancelled(this: *mut AnyObject, sel: Sel, touches: *mut AnyObject, event: *mut AnyObject) {
    touches_ended(this, sel, touches, event);
}

// --- Hardware keyboard (UIPress) ---

/// UIKit keyCode → macOS virtual keycode mapping (for hardware keyboard in simulator).
fn uikit_keycode_to_mac(uikit_kc: i64) -> Option<u8> {
    // UIKeyboardHIDUsage values → macOS kVK codes
    let mac = match uikit_kc {
        4 => 0,    // A
        5 => 11,   // B
        6 => 8,    // C
        7 => 2,    // D
        8 => 14,   // E
        9 => 3,    // F
        10 => 5,   // G
        11 => 4,   // H
        12 => 34,  // I
        13 => 38,  // J
        14 => 40,  // K
        15 => 37,  // L
        16 => 46,  // M
        17 => 45,  // N
        18 => 31,  // O
        19 => 35,  // P
        20 => 12,  // Q
        21 => 15,  // R
        22 => 1,   // S
        23 => 17,  // T
        24 => 32,  // U
        25 => 9,   // V
        26 => 13,  // W
        27 => 7,   // X
        28 => 16,  // Y
        29 => 6,   // Z
        30 => 18,  // 1
        31 => 19,  // 2
        32 => 20,  // 3
        33 => 21,  // 4
        34 => 23,  // 5
        35 => 22,  // 6
        36 => 26,  // 7
        37 => 28,  // 8
        38 => 25,  // 9
        39 => 29,  // 0
        40 => 36,  // Return
        41 => 53,  // Escape
        42 => 51,  // Backspace
        43 => 48,  // Tab
        44 => 49,  // Space
        45 => 27,  // Minus
        46 => 24,  // Equal
        47 => 33,  // LeftBracket
        48 => 30,  // RightBracket
        49 => 42,  // Backslash
        51 => 41,  // Semicolon
        52 => 39,  // Quote
        53 => 50,  // Grave
        54 => 43,  // Comma
        55 => 47,  // Period
        56 => 44,  // Slash
        _ => return None,
    };
    Some(mac)
}

unsafe extern "C" fn presses_began(_this: *mut AnyObject, _sel: Sel, presses: *mut AnyObject, _event: *mut AnyObject) {
    let all: *mut AnyObject = msg_send![presses, allObjects];
    let count: usize = msg_send![all, count];
    for i in 0..count {
        let press: *mut AnyObject = msg_send![all, objectAtIndex: i];
        let key: *mut AnyObject = msg_send![press, key];
        if key.is_null() { continue; }
        let uikit_kc: i64 = msg_send![key, keyCode];
        let modifier_flags: u64 = msg_send![key, modifierFlags];

        let x11_id = get_focused_x11_window();
        if x11_id == 0 { continue; }
        let time = get_timestamp();

        if let Some(mac_kc) = uikit_keycode_to_mac(uikit_kc) {
            let keycode = mac_kc + 8;
            let mut state: u16 = 0;
            if modifier_flags & (1 << 17) != 0 { state |= 1; } // Shift
            if modifier_flags & (1 << 18) != 0 { state |= 4; } // Control
            if modifier_flags & (1 << 19) != 0 { state |= 8; } // Alt/Option
            if modifier_flags & (1 << 20) != 0 { state |= 64; } // Command → Mod4

            send_display_event(DisplayEvent::KeyPress {
                window: x11_id, keycode, state, time,
            });
        }
    }
}

unsafe extern "C" fn presses_ended(_this: *mut AnyObject, _sel: Sel, presses: *mut AnyObject, _event: *mut AnyObject) {
    let all: *mut AnyObject = msg_send![presses, allObjects];
    let count: usize = msg_send![all, count];
    for i in 0..count {
        let press: *mut AnyObject = msg_send![all, objectAtIndex: i];
        let key: *mut AnyObject = msg_send![press, key];
        if key.is_null() { continue; }
        let uikit_kc: i64 = msg_send![key, keyCode];
        let modifier_flags: u64 = msg_send![key, modifierFlags];

        let x11_id = get_focused_x11_window();
        if x11_id == 0 { continue; }
        let time = get_timestamp();

        if let Some(mac_kc) = uikit_keycode_to_mac(uikit_kc) {
            let keycode = mac_kc + 8;
            let mut state: u16 = 0;
            if modifier_flags & (1 << 17) != 0 { state |= 1; }
            if modifier_flags & (1 << 18) != 0 { state |= 4; }
            if modifier_flags & (1 << 19) != 0 { state |= 8; }
            if modifier_flags & (1 << 20) != 0 { state |= 64; }

            send_display_event(DisplayEvent::KeyRelease {
                window: x11_id, keycode, state, time,
            });
        }
    }
}

// --- Helper functions ---

fn get_first_touch(touches: *mut AnyObject) -> *mut AnyObject {
    unsafe {
        let any_touch: *mut AnyObject = msg_send![&*touches, anyObject];
        any_touch
    }
}

fn touch_location_in_view(touch: *mut AnyObject, view: *mut AnyObject) -> (i16, i16) {
    use objc2::encode::{Encode, Encoding};

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGPoint { x: f64, y: f64 }
    unsafe impl Encode for CGPoint {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }

    unsafe {
        let point: CGPoint = msg_send![&*touch, locationInView: &*view];
        (point.x as i16, point.y as i16)
    }
}

fn get_timestamp() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

fn get_focused_x11_window() -> crate::display::Xid {
    WINDOWS.with(|w| {
        let ws = w.borrow();
        for info in ws.values() {
            if info.visible { return info.x11_id; }
        }
        0
    })
}

/// Check if (x,y) is in the title bar area of any visible window.
/// Returns (native_id, layer_x, layer_y) for drag initiation.
fn find_window_titlebar_at(x: i16, y: i16) -> Option<(u64, f64, f64)> {
    WINDOWS.with(|w| {
        let ws = w.borrow();
        let mut best: Option<(u64, f64, f64, u64)> = None;
        for (id, info) in ws.iter() {
            if !info.visible { continue; }
            let wx = x - info.x11_x;
            let wy = y - info.x11_y;
            if wx >= 0 && (wx as u16) < info.width && wy >= 0 && wy < TITLE_BAR_HEIGHT {
                if best.is_none() || *id > best.unwrap().3 {
                    best = Some((*id, info.x11_x as f64, info.x11_y as f64, *id));
                }
            }
        }
        best.map(|(id, lx, ly, _)| (id, lx, ly))
    })
}

/// Move a window's CALayer to a new position using setFrame.
fn move_window_layer(native_id: u64, x: f64, y: f64) {
    WINDOWS.with(|w| {
        let ws = w.borrow();
        if let Some(info) = ws.get(&native_id) {
            if info.ca_layer.is_null() { return; }
            unsafe {
                use objc2::encode::{Encode, Encoding};

                #[repr(C)]
                #[derive(Copy, Clone)]
                struct CGPoint { x: f64, y: f64 }
                unsafe impl Encode for CGPoint {
                    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
                }
                #[repr(C)]
                #[derive(Copy, Clone)]
                struct CGSize { w: f64, h: f64 }
                unsafe impl Encode for CGSize {
                    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
                }
                #[repr(C)]
                #[derive(Copy, Clone)]
                struct CGRect { origin: CGPoint, size: CGSize }
                unsafe impl Encode for CGRect {
                    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
                }

                // Disable implicit animation
                let ca_cls = objc_getClass(b"CATransaction\0".as_ptr());
                let _: () = msg_send![ca_cls, begin];
                let _: () = msg_send![ca_cls, setDisableActions: true];

                let frame = CGRect {
                    origin: CGPoint { x, y },
                    size: CGSize { w: info.width as f64, h: info.height as f64 },
                };
                let _: () = msg_send![info.ca_layer, setFrame: frame];

                let _: () = msg_send![ca_cls, commit];
            }
        }
    });
}

/// Update window's x11 position after drag.
fn update_window_position(native_id: u64, new_x: i16, new_y: i16) {
    WINDOWS.with(|w| {
        let mut ws = w.borrow_mut();
        if let Some(info) = ws.get_mut(&native_id) {
            info.x11_x = new_x;
            info.x11_y = new_y;
        }
    });
}

/// Find which X11 window is at screen position (x, y).
fn find_x11_window_at(x: i16, y: i16) -> crate::display::Xid {
    WINDOWS.with(|w| {
        let ws = w.borrow();
        // Check windows in reverse insertion order (later = on top)
        let mut best: Option<(crate::display::Xid, u64)> = None;
        for (id, info) in ws.iter() {
            if !info.visible { continue; }
            let wx = x - info.x11_x;
            let wy = y - info.x11_y;
            if wx >= 0 && wy >= 0 && (wx as u16) < info.width && (wy as u16) < info.height {
                if best.is_none() || *id > best.unwrap().1 {
                    best = Some((info.x11_id, *id));
                }
            }
        }
        best.map(|(xid, _)| xid).unwrap_or(0)
    })
}

/// Convert screen coordinates to window-local coordinates.
fn screen_to_window_coords(screen_x: i16, screen_y: i16, x11_id: crate::display::Xid) -> (i16, i16) {
    WINDOWS.with(|w| {
        let ws = w.borrow();
        for info in ws.values() {
            if info.x11_id == x11_id {
                return (screen_x - info.x11_x, screen_y - info.x11_y);
            }
        }
        (screen_x, screen_y)
    })
}

/// Map ASCII byte to X11 keycode + modifier state.
/// X11 keycode = macOS virtual keycode + 8 (matches build_keyboard_map / macos.rs).
fn ascii_to_x11_keycode_state(ch: u8) -> (u8, u16) {
    // macOS kVK codes: A=0,S=1,D=2,F=3,H=4,G=5,Z=6,X=7,C=8,V=9,B=11,Q=12,W=13,
    // E=14,R=15,Y=16,T=17,1=18,2=19,3=20,4=21,6=22,5=23,=24,9=25,7=26,-=27,8=28,
    // 0=29,]=30,O=31,U=32,[=33,I=34,P=35,Return=36,L=37,J=38,'=39,K=40,;=41,\=42,
    // ,=43,/=44,N=45,M=46,.=47,Tab=48,Space=49,`=50,Delete=51,Escape=53
    let (mac_kc, shift): (u8, bool) = match ch {
        b'a' => (0, false), b'A' => (0, true),
        b's' => (1, false), b'S' => (1, true),
        b'd' => (2, false), b'D' => (2, true),
        b'f' => (3, false), b'F' => (3, true),
        b'h' => (4, false), b'H' => (4, true),
        b'g' => (5, false), b'G' => (5, true),
        b'z' => (6, false), b'Z' => (6, true),
        b'x' => (7, false), b'X' => (7, true),
        b'c' => (8, false), b'C' => (8, true),
        b'v' => (9, false), b'V' => (9, true),
        b'b' => (11, false), b'B' => (11, true),
        b'q' => (12, false), b'Q' => (12, true),
        b'w' => (13, false), b'W' => (13, true),
        b'e' => (14, false), b'E' => (14, true),
        b'r' => (15, false), b'R' => (15, true),
        b'y' => (16, false), b'Y' => (16, true),
        b't' => (17, false), b'T' => (17, true),
        b'o' => (31, false), b'O' => (31, true),
        b'u' => (32, false), b'U' => (32, true),
        b'i' => (34, false), b'I' => (34, true),
        b'p' => (35, false), b'P' => (35, true),
        b'l' => (37, false), b'L' => (37, true),
        b'j' => (38, false), b'J' => (38, true),
        b'k' => (40, false), b'K' => (40, true),
        b'n' => (45, false), b'N' => (45, true),
        b'm' => (46, false), b'M' => (46, true),
        b'1' => (18, false), b'!' => (18, true),
        b'2' => (19, false), b'@' => (19, true),
        b'3' => (20, false), b'#' => (20, true),
        b'4' => (21, false), b'$' => (21, true),
        b'5' => (23, false), b'%' => (23, true),
        b'6' => (22, false), b'^' => (22, true),
        b'7' => (26, false), b'&' => (26, true),
        b'8' => (28, false), b'*' => (28, true),
        b'9' => (25, false), b'(' => (25, true),
        b'0' => (29, false), b')' => (29, true),
        b'-' => (27, false), b'_' => (27, true),
        b'=' => (24, false), b'+' => (24, true),
        b'[' => (33, false), b'{' => (33, true),
        b']' => (30, false), b'}' => (30, true),
        b'\\' => (42, false), b'|' => (42, true),
        b';' => (41, false), b':' => (41, true),
        b'\'' => (39, false), b'"' => (39, true),
        b',' => (43, false), b'<' => (43, true),
        b'.' => (47, false), b'>' => (47, true),
        b'/' => (44, false), b'?' => (44, true),
        b'`' => (50, false), b'~' => (50, true),
        b' ' => (49, false),
        b'\n' | b'\r' => (36, false),
        b'\t' => (48, false),
        0x1b => (53, false),
        0x7f => (51, false),
        _ => return (0, 0),
    };
    let state = if shift { 1u16 } else { 0u16 };
    (mac_kc + 8, state)
}

/// Drop redundant full-window PutImage commands (same as macos.rs).
fn coalesce_putimage(commands: Vec<crate::display::RenderCommand>, win_w: u32, win_h: u32) -> Vec<crate::display::RenderCommand> {
    if commands.len() < 2 { return commands; }
    let mut last_full_idx: Option<usize> = None;
    for (i, cmd) in commands.iter().enumerate() {
        if let crate::display::RenderCommand::PutImage { x, y, width, height, .. } = cmd {
            if *x == 0 && *y == 0 && *width as u32 >= win_w && *height as u32 >= win_h {
                last_full_idx = Some(i);
            }
        }
    }
    if let Some(last_idx) = last_full_idx {
        commands.into_iter().enumerate().filter(|(i, cmd)| {
            if *i == last_idx { return true; }
            if let crate::display::RenderCommand::PutImage { x, y, width, height, .. } = cmd {
                if *x == 0 && *y == 0 && *width as u32 >= win_w && *height as u32 >= win_h {
                    return false;
                }
            }
            true
        }).map(|(_, cmd)| cmd).collect()
    } else {
        commands
    }
}

// --- CoreAnimation FFI ---
extern "C" {
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

// CoreGraphics FFI for CGImage-based rendering on iOS
extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(cs: *mut c_void);
    fn CGImageCreate(
        width: usize, height: usize, bits_per_component: usize,
        bits_per_pixel: usize, bytes_per_row: usize, space: *mut c_void,
        bitmap_info: u32, provider: *mut c_void, decode: *const f64,
        should_interpolate: bool, intent: u32,
    ) -> *mut c_void;
    fn CGImageRelease(image: *mut c_void);
    fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> *mut c_void;
    fn CGDataProviderCreateWithCFData(data: *mut c_void) -> *mut c_void;
    fn CGDataProviderRelease(provider: *mut c_void);
}

/// Try to claim the main scene's UIWindow for an X11 window.
/// This is the iOS equivalent of macOS creating the NSWindow: we reuse the auto-created
/// main UIScene and assign our X11 window to it, setting up the view layer like macOS does.
fn claim_main_scene(native_id: u64, info: &mut WindowInfo) -> bool {
    use objc2::encode::{Encode, Encoding};
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGRect { origin: [f64; 2], size: [f64; 2] }
    unsafe impl Encode for CGRect {
        const ENCODING: Encoding = Encoding::Struct("CGRect", &[
            Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]),
            Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]),
        ]);
    }

    MAIN_UI_WINDOW.with(|mw| {
        let mut mw = mw.borrow_mut();
        if let Some(main_win) = mw.take() {
            unsafe {
                // Same screen clamp as macOS (macos.rs:1767-1781)
                let screen: *mut AnyObject = msg_send![objc2::class!(UIScreen), mainScreen];
                let screen_bounds: CGRect = msg_send![screen, bounds];
                let mut pt_w = info.width as f64;
                let mut pt_h = info.height as f64;
                let max_w = screen_bounds.size[0];
                let max_h = screen_bounds.size[1] - 30.0;
                if pt_w > max_w || pt_h > max_h {
                    let fit = (max_w / pt_w).min(max_h / pt_h);
                    pt_w *= fit;
                    pt_h *= fit;
                }

                let main_view = MAIN_VIEW.with(|mv| *mv.borrow());
                if let Some(view) = main_view {
                    // Resize view to match X11 window (like macOS NSWindow contentRect)
                    let frame = CGRect { origin: [0.0, 0.0], size: [pt_w, pt_h] };
                    let _: () = msg_send![view, setFrame: frame];

                    let layer: *mut AnyObject = msg_send![view, layer];
                    if !layer.is_null() {
                        // Same layer config as macOS (macos.rs:1866-1884)
                        let gravity = NSString::from_str("topLeft");
                        let _: () = msg_send![layer, setContentsGravity: &*gravity];
                        let _: () = msg_send![layer, setContentsScale: 1.0_f64];
                        let _: () = msg_send![layer, setMasksToBounds: true];
                        // IOSurface as contents — zero-copy (like macOS)
                        let _: () = msg_send![layer, setContents: info.display_surface as *mut AnyObject];
                        info.ca_layer = layer;
                    }
                    info.ui_view = view;
                    let _: () = msg_send![view, becomeFirstResponder];
                }

                info.ui_window = main_win;

                // Set scene title (like macOS setTitle)
                let scene: *mut AnyObject = msg_send![main_win, windowScene];
                if !scene.is_null() {
                    let title_ns = NSString::from_str(&info.title);
                    let _: () = msg_send![scene, setTitle: &*title_ns];
                }

                // Resize Stage Manager window to X11 size
                resize_scene_to_x11(main_win, pt_w as u16, pt_h as u16);

                // Now show the window (was hidden at scene creation, like macOS visible=false → makeKeyAndOrderFront)
                let _: () = msg_send![main_win, setHidden: false];
                let _: () = msg_send![main_win, makeKeyAndVisible];

                info!("claim_main_scene: native_id={} '{}' ({}x{} → {:.0}x{:.0})",
                    native_id, info.title, info.width, info.height, pt_w, pt_h);
            }
            true
        } else {
            false
        }
    })
}

/// Create UIWindow + PSLXView + CALayer in a given UIWindowScene for a window.
/// Called from CreateWindow (initial scene) and scene_will_connect (new scenes).
/// This is the iOS equivalent of macOS's NSWindow + PSLXInputView creation.
fn setup_window_in_scene(native_id: u64) {
    ios_log(&format!("setup_window_in_scene: START id={}", native_id));
    // Get the scene to use — either MAIN_SCENE (for first window) or from PENDING_SCENE map
    let scene = {
        let from_pending = PENDING_SCENE_MAP.with(|pm| {
            let mut map = pm.borrow_mut();
            let pos = map.iter().position(|(id, _)| *id == native_id);
            pos.map(|idx| map.remove(idx).1)
        });
        if let Some(s) = from_pending { Some(s) }
        else { MAIN_SCENE.with(|ms| *ms.borrow()) }
    };

    if scene.is_none() {
        ios_log(&format!("setup_window_in_scene: no scene for window {}", native_id));
        return;
    }
    let scene = scene.unwrap();
    ios_log(&format!("setup_window_in_scene: got scene {:p} for id={}", scene, native_id));

    WINDOWS.with(|w| {
        let mut ws = w.borrow_mut();
        let info = match ws.get_mut(&native_id) {
            Some(i) => i,
            None => {
                ios_log(&format!("setup_window_in_scene: window {} not found", native_id));
                return;
            }
        };

        unsafe {
            use objc2::encode::{Encode, Encoding};
            #[repr(C)]
            #[derive(Copy, Clone)]
            struct CGRect { origin: [f64; 2], size: [f64; 2] }
            unsafe impl Encode for CGRect {
                const ENCODING: Encoding = Encoding::Struct("CGRect", &[
                    Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]),
                    Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]),
                ]);
            }

            // Create UIWindow in this scene (like macOS NSWindow)
            let win: *mut AnyObject = msg_send![objc2::class!(UIWindow), alloc];
            let win: *mut AnyObject = msg_send![win, initWithWindowScene: &*scene];

            // UIWindow fills the entire scene. On real iPad, sizeRestrictions shrinks the scene
            // to X11 window size. On simulator, scene is fullscreen — we fill it all with white bg
            // and put the X11 content view at top-left.
            let scene_bounds: CGRect = {
                let coord: *mut AnyObject = msg_send![scene, coordinateSpace];
                msg_send![coord, bounds]
            };
            let _: () = msg_send![win, setFrame: scene_bounds];

            // Create PSLXView at X11 window size (positioned at top-left of UIWindow)
            let view_cls = get_pslx_view_class();
            let view: *mut AnyObject = msg_send![view_cls, alloc];
            let view_frame = CGRect {
                origin: [0.0, 0.0],
                size: [info.width as f64, info.height as f64],
            };
            let view: *mut AnyObject = msg_send![view, initWithFrame: view_frame];

            // Layer config (same as macOS: topLeft gravity, contentsScale=1.0, masksToBounds)
            let layer: *mut AnyObject = msg_send![view, layer];
            if !layer.is_null() {
                let gravity = NSString::from_str("topLeft");
                let _: () = msg_send![layer, setContentsGravity: &*gravity];
                let _: () = msg_send![layer, setContentsScale: 1.0_f64];
                let _: () = msg_send![layer, setMasksToBounds: true];
            }

            // Set up view controller with a container view that fills the scene.
            // The X11 content view is a subview at (0,0) with X11 size.
            let vc_cls = get_fullscreen_vc_class();
            let vc: *mut AnyObject = msg_send![vc_cls, alloc];
            let vc: *mut AnyObject = msg_send![vc, init];
            // Create container view filling the scene
            let container_cls = objc2::class!(UIView);
            let container: *mut AnyObject = msg_send![container_cls, alloc];
            let container: *mut AnyObject = msg_send![container, initWithFrame: scene_bounds];
            let white: *mut AnyObject = msg_send![objc2::class!(UIColor), whiteColor];
            let _: () = msg_send![container, setBackgroundColor: white];
            // Add X11 content view as subview
            let _: () = msg_send![container, addSubview: view];
            let _: () = msg_send![vc, setView: container];
            let _: () = msg_send![win, setRootViewController: vc];

            // White background fills entire scene (xterm content overlays at top-left)
            let white_color: *mut AnyObject = msg_send![objc2::class!(UIColor), whiteColor];
            let _: () = msg_send![win, setBackgroundColor: white_color];
            // Make window non-opaque so scene background doesn't show through
            let _: () = msg_send![win, setOpaque: false];

            // Hidden until ShowWindow (like macOS visible=false)
            let _: () = msg_send![win, setHidden: true];

            // Set scene title (like macOS NSWindow.setTitle)
            let title_ns = NSString::from_str(&info.title);
            let _: () = msg_send![scene, setTitle: &*title_ns];

            // Resize Stage Manager window to X11 size
            resize_scene_to_x11(win, info.width, info.height);

            let _: *mut AnyObject = msg_send![win, retain];
            let _: *mut AnyObject = msg_send![layer, retain];

            info.ui_window = win;
            info.ui_view = view;
            info.ca_layer = layer;

            // If ShowWindow was called before we had a UIWindow, show it now
            if info.pending_show || info.visible {
                let _: () = msg_send![win, setHidden: false];
                let _: () = msg_send![win, makeKeyAndVisible];
                flush_window_to_layer(layer, info.display_surface);
                info.pending_show = false;
                info.visible = true;
                ios_log(&format!("setup_window_in_scene: id={} {}x{} scene={:p} (deferred show)",
                    native_id, info.width, info.height, scene));
            } else {
                ios_log(&format!("setup_window_in_scene: id={} {}x{} scene={:p}",
                    native_id, info.width, info.height, scene));
            }
        }
    });
}

/// Map from native_window_id → UIWindowScene pointer, for scene_will_connect to find.
/// Only accessed from main thread (UIKit callbacks + timer), but Mutex for Send safety.
thread_local! {
    static PENDING_SCENE_MAP: RefCell<Vec<(u64, *mut AnyObject)>> = RefCell::new(Vec::new());
}

/// Push IOSurface pixels to CALayer via CGImage.
/// On macOS, setContents:IOSurface works directly (zero-copy). On iOS, IOSurface as
/// layer contents has Y-axis flip issues, so we go through CGImage which handles
/// coordinate conversion correctly.
fn flush_window_to_layer(layer: *mut AnyObject, surface: *mut c_void) {
    if surface.is_null() || layer.is_null() { return; }
    unsafe {
        let width = IOSurfaceGetWidth(surface);
        let height = IOSurfaceGetHeight(surface);
        let bytes_per_row = IOSurfaceGetBytesPerRow(surface);

        IOSurfaceLock(surface, 1, std::ptr::null_mut()); // read-only lock
        let base = IOSurfaceGetBaseAddress(surface) as *const u8;
        let data_len = bytes_per_row * height;

        let cf_data = CFDataCreate(std::ptr::null(), base, data_len as isize);
        IOSurfaceUnlock(surface, 1, std::ptr::null_mut());
        if cf_data.is_null() { return; }

        let colorspace = CGColorSpaceCreateDeviceRGB();
        let provider = CGDataProviderCreateWithCFData(cf_data);
        CFRelease(cf_data);

        // BGRA premultiplied alpha (same pixel format as IOSurface)
        let bitmap_info: u32 = 0x2006;
        let image = CGImageCreate(
            width, height, 8, 32, bytes_per_row,
            colorspace, bitmap_info,
            provider, std::ptr::null(), false, 0,
        );
        CGDataProviderRelease(provider);
        CGColorSpaceRelease(colorspace);

        if !image.is_null() {
            let ca_cls = objc_getClass(b"CATransaction\0".as_ptr());
            let _: () = msg_send![ca_cls, begin];
            let _: () = msg_send![ca_cls, setDisableActions: true];
            let _: () = msg_send![layer, setContents: image as *mut AnyObject];
            let _: () = msg_send![ca_cls, commit];
            CGImageRelease(image);
        }
    }
}

// create_window_layer removed — each X11 window now uses its own UIView.layer
// (like macOS where each NSWindow has its own contentView.layer)

/// Request iPadOS to create a new UIScene for an X11 window.
/// The scene delegate (scene_will_connect) picks up the window ID from PENDING_SCENE_WINDOWS.
fn request_new_scene(native_window_id: u64) {
    info!("Requesting new UIScene for window {}", native_window_id);
    PENDING_SCENE_WINDOWS.lock().unwrap().push(native_window_id);
    unsafe {
        let app: *mut AnyObject = msg_send![objc2::class!(UIApplication), sharedApplication];
        // Create NSUserActivity to pass window ID
        let activity_type = NSString::from_str("com.pslx.x11window");
        let activity: *mut AnyObject = msg_send![objc2::class!(NSUserActivity), alloc];
        let activity: *mut AnyObject = msg_send![activity, initWithActivityType: &*activity_type];

        // Request scene activation
        let options: *mut AnyObject = msg_send![objc2::class!(UISceneActivationRequestOptions), alloc];
        let options: *mut AnyObject = msg_send![options, init];

        let _: () = msg_send![app, requestSceneSessionActivation:
            std::ptr::null_mut::<AnyObject>()
            userActivity: activity
            options: options
            errorHandler: std::ptr::null_mut::<AnyObject>()
        ];
    }
}

/// Resize a UIScene's Stage Manager window to match the X11 window dimensions.
/// Uses UIWindowScene.sizeRestrictions (iPadOS 16+) to constrain the window size,
/// then requestGeometryUpdate to apply it.
unsafe fn resize_scene_to_x11(ui_window: *mut AnyObject, width: u16, height: u16) {
    let scene: *mut AnyObject = msg_send![ui_window, windowScene];
    if scene.is_null() { return; }

    // Set sizeRestrictions to pin min==max to X11 window size
    let restrictions: *mut AnyObject = msg_send![scene, sizeRestrictions];
    if !restrictions.is_null() {
        use objc2::encode::{Encode, Encoding};
        #[repr(C)]
        #[derive(Copy, Clone)]
        struct CGSize { width: f64, height: f64 }
        unsafe impl Encode for CGSize {
            const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
        }
        let size = CGSize { width: width as f64, height: height as f64 };
        let _: () = msg_send![restrictions, setMinimumSize: size];
        let _: () = msg_send![restrictions, setMaximumSize: size];
        info!("resize_scene_to_x11: set sizeRestrictions {}x{}", width, height);
    }

    // Request geometry update (triggers sizeRestrictions enforcement on real device)
    let prefs_cls = objc2::class!(UIWindowSceneGeometryPreferencesIOS);
    let prefs: *mut AnyObject = msg_send![prefs_cls, alloc];
    let prefs: *mut AnyObject = msg_send![prefs, init];
    let _: () = msg_send![scene, requestGeometryUpdateWithPreferences: prefs
        errorHandler: std::ptr::null_mut::<AnyObject>()];
    ios_log(&format!("resize_scene_to_x11: sizeRestrictions {}x{}", width, height));
}

/// Resize UIWindow + view to fit visible X11 windows.
/// Sets UIWindow frame directly (works on simulator where sizeRestrictions doesn't).
fn resize_scene_to_content() {
    use objc2::encode::{Encode, Encoding};
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGRect { origin: [f64; 2], size: [f64; 2] }
    unsafe impl Encode for CGRect {
        const ENCODING: Encoding = Encoding::Struct("CGRect", &[
            Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]),
            Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]),
        ]);
    }

    // Calculate bounding box of all visible windows
    let (max_right, max_bottom) = WINDOWS.with(|w| {
        let ws = w.borrow();
        let mut r: i32 = 0;
        let mut b: i32 = 0;
        for info in ws.values() {
            if !info.visible { continue; }
            let right = info.x11_x as i32 + info.width as i32;
            let bottom = info.x11_y as i32 + info.height as i32;
            if right > r { r = right; }
            if bottom > b { b = bottom; }
        }
        (r, b)
    });

    if max_right <= 0 || max_bottom <= 0 { return; }

    let w = max_right as f64;
    let h = max_bottom as f64;

    MAIN_UI_WINDOW.with(|mw| {
        if let Some(win) = *mw.borrow() {
            unsafe {
                // sizeRestrictions (works on real iPad)
                resize_scene_to_x11(win, max_right as u16, max_bottom as u16);
                // Direct frame resize (works on simulator too)
                let frame = CGRect { origin: [0.0, 0.0], size: [w, h] };
                let _: () = msg_send![win, setFrame: frame];
            }
        }
    });

    MAIN_VIEW.with(|mv| {
        if let Some(view) = *mv.borrow() {
            unsafe {
                let frame = CGRect { origin: [0.0, 0.0], size: [w, h] };
                let _: () = msg_send![view, setFrame: frame];
            }
        }
    });

    ios_log(&format!("resize_scene_to_content: {}x{}", max_right, max_bottom));
}

// --- Timer callback (60fps render loop) ---

extern "C" fn timer_callback(_timer: *mut c_void, _info: *mut c_void) {
    static TIMER_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let cnt = TIMER_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if cnt == 0 {
        ios_log("timer_callback: first tick");
    }
    process_commands();
}

fn process_commands() {
    // 1. Process non-render commands from channel
    let cmds: Vec<DisplayCommand> = CMD_RX.with(|rx| {
        rx.borrow().as_ref().map_or_else(Vec::new, |rx| rx.try_iter().collect())
    });
    if !cmds.is_empty() {
        ios_log(&format!("process_commands: {} cmds received", cmds.len()));
    }
    for cmd in cmds {
        handle_command(cmd);
    }

    // 2. Drain render mailbox and render all windows to their IOSurfaces
    RENDER_MAILBOX.with(|mb| {
        let mb = mb.borrow();
        if let Some(ref mailbox) = *mb {
            if mailbox.is_empty() { return; }
            WINDOWS.with(|w| {
                let mut ws = w.borrow_mut();
                let win_ids: Vec<u64> = ws.keys().copied().collect();
                let mut any_rendered = false;
                for win_id in win_ids {
                    let commands = if let Some(mut entry) = mailbox.get_mut(&win_id) {
                        if entry.is_empty() { continue; }
                        std::mem::take(entry.value_mut())
                    } else {
                        continue;
                    };

                    if let Some(info) = ws.get_mut(&win_id) {
                        let width = info.width as u32;
                        let height = info.height as u32;
                        let commands = coalesce_putimage(commands, width, height);

                        let first_covers_all = matches!(commands.first(),
                            Some(crate::display::RenderCommand::PutImage { x, y, width: w, height: h, .. })
                            if *x == 0 && *y == 0 && *w as u32 >= width && *h as u32 >= height
                        );

                        unsafe {
                            IOSurfaceLock(info.surface, 0, std::ptr::null_mut());
                            if !first_covers_all {
                                // Copy display → render surface (preserve previous frame)
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
                        any_rendered = true;

                        // For visible windows, push to screen
                        if info.visible {
                            flush_window_to_layer(info.ca_layer, info.display_surface);
                        }
                    }
                }
                if any_rendered {
                    // Log only occasionally to avoid spam
                    static RENDER_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let cnt = RENDER_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if cnt < 5 || cnt % 60 == 0 {
                        let n_windows = ws.len();
                        let n_visible = ws.values().filter(|i| i.visible).count();
                        ios_log(&format!("render #{}: {} windows, {} visible", cnt, n_windows, n_visible));
                    }
                    ca_transaction_flush();
                }
            });
        }
    });
}

fn handle_command(cmd: DisplayCommand) {
    match cmd {
        DisplayCommand::CreateWindow {
            x11_id, x, y, width, height, title, override_redirect, reply,
        } => {
            // Multi-scene approach (like macOS: 1 X11 window = 1 NSWindow):
            // Each X11 window gets its own UIWindowScene (= Stage Manager window).
            // 1. Allocate ID + create IOSurfaces
            // 2. Store in WINDOWS with null UIWindow/UIView (created in scene_will_connect)
            // 3. First window → claim the initial scene; subsequent → requestSceneSessionActivation
            let id = alloc_id();

            let surface = create_iosurface(width, height);
            let display_surface = create_iosurface(width, height);
            if surface.is_null() || display_surface.is_null() {
                log::error!("Failed to create IOSurface for window {}", id);
                return;
            }

            // Fill both IOSurfaces with opaque black (same as macOS)
            unsafe {
                for s in [surface, display_surface] {
                    IOSurfaceLock(s, 0, std::ptr::null_mut());
                    let base = IOSurfaceGetBaseAddress(s) as *mut u8;
                    let stride = IOSurfaceGetBytesPerRow(s);
                    for row in 0..height as usize {
                        let row_ptr = base.add(row * stride) as *mut u32;
                        for col in 0..width as usize {
                            *row_ptr.add(col) = 0xFF000000;
                        }
                    }
                    IOSurfaceUnlock(s, 0, std::ptr::null_mut());
                }
            }

            WINDOWS.with(|w| {
                w.borrow_mut().insert(id, WindowInfo {
                    surface, display_surface, width, height, x11_id,
                    x11_x: x, x11_y: y,
                    background_pixel: 0xFFFFFFFF,
                    visible: false,
                    title,
                    override_redirect,
                    ca_layer: std::ptr::null_mut(),
                    ui_window: std::ptr::null_mut(),
                    ui_view: std::ptr::null_mut(),
                    pending_show: false,
                });
            });

            // Check if the initial scene is still unclaimed
            let main_scene_available = MAIN_SCENE.with(|ms| ms.borrow().is_some());
            let main_window_used = MAIN_UI_WINDOW.with(|mw| mw.borrow().is_none());
            // MAIN_UI_WINDOW is None when available (set to None after claim),
            // so we check MAIN_SCENE is set AND no window has claimed it yet.
            let first_window = WINDOWS.with(|w| w.borrow().len() == 1);

            if main_scene_available && first_window {
                // First X11 window: claim the initial scene that iOS gave us at launch.
                // Create UIWindow in the existing scene (like macOS first NSWindow).
                setup_window_in_scene(id);
                ios_log(&format!("CreateWindow: id={} x11=0x{:08X} {}x{} (claimed initial scene)",
                    id, x11_id, width, height));
            } else {
                // Subsequent X11 windows: request a new UIWindowScene.
                // scene_will_connect will pick up the window ID and create UIWindow there.
                request_new_scene(id);
                ios_log(&format!("CreateWindow: id={} x11=0x{:08X} {}x{} (requesting new scene)",
                    id, x11_id, width, height));
            }

            info!("Created window {} for X11 0x{:08X} ({}x{}) [iOS]", id, x11_id, width, height);
            let _ = reply.send(NativeWindowHandle { id });
        }

        DisplayCommand::ShowWindow { handle, visible } => {
            let show_info = WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().get_mut(&handle.id) {
                    info.visible = visible;
                    if visible {
                        if info.ui_window.is_null() {
                            // UIWindow not yet created (scene_will_connect hasn't fired).
                            // Defer — setup_window_in_scene will show it.
                            info.pending_show = true;
                            ios_log(&format!("ShowWindow: id={} deferred (no UIWindow yet)", handle.id));
                            Some((info.x11_id, info.width, info.height))
                        } else {
                            // Show the per-window UIWindow (like macOS makeKeyAndOrderFront)
                            unsafe {
                                let _: () = msg_send![info.ui_window, setHidden: false];
                                let _: () = msg_send![info.ui_window, makeKeyAndVisible];
                            }
                            flush_window_to_layer(info.ca_layer, info.display_surface);
                            Some((info.x11_id, info.width, info.height))
                        }
                    } else {
                        if !info.ui_window.is_null() {
                            unsafe {
                                let _: () = msg_send![info.ui_window, setHidden: true];
                            }
                        }
                        None
                    }
                } else {
                    None
                }
            });
            if let Some((x11_id, w, h)) = show_info {
                send_display_event(DisplayEvent::FocusIn { window: x11_id });
                ios_log(&format!("ShowWindow: id={} x11=0x{:08X} {}x{}",
                    handle.id, x11_id, w, h));
            }
        }

        DisplayCommand::HideWindow { handle } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().get_mut(&handle.id) {
                    info.visible = false;
                }
            });
        }

        DisplayCommand::DestroyWindow { handle } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().remove(&handle.id) {
                    unsafe {
                        if !info.ui_window.is_null() {
                            let _: () = msg_send![info.ui_window, setHidden: true];
                        }
                    }
                }
            });
        }

        DisplayCommand::MoveResizeWindow { handle, x, y, width, height } => {
            let event_info = WINDOWS.with(|w| {
                let mut ws = w.borrow_mut();
                if let Some(info) = ws.get_mut(&handle.id) {
                    let old_w = info.width;
                    let old_h = info.height;
                    info.x11_x = x;
                    info.x11_y = y;

                    if width != old_w || height != old_h {
                        // Resize: create new IOSurface
                        let new_surface = create_iosurface(width, height);
                        let new_display = create_iosurface(width, height);
                        if !new_surface.is_null() && !new_display.is_null() {
                            // Clear new surfaces
                            let bg = info.background_pixel;
                            let bg_bytes: [u8; 4] = [
                                (bg & 0xFF) as u8,
                                ((bg >> 8) & 0xFF) as u8,
                                ((bg >> 16) & 0xFF) as u8,
                                0xFF,
                            ];
                            for s in [new_surface, new_display] {
                                unsafe {
                                    IOSurfaceLock(s, 0, std::ptr::null_mut());
                                    let base = IOSurfaceGetBaseAddress(s) as *mut u8;
                                    let stride = IOSurfaceGetBytesPerRow(s);
                                    for row in 0..height as usize {
                                        let row_base = base.add(row * stride);
                                        for col in 0..width as usize {
                                            std::ptr::copy_nonoverlapping(
                                                bg_bytes.as_ptr(), row_base.add(col * 4), 4);
                                        }
                                    }
                                    IOSurfaceUnlock(s, 0, std::ptr::null_mut());
                                }
                            }

                            unsafe {
                                CFRelease(info.surface as *const c_void);
                                CFRelease(info.display_surface as *const c_void);
                            }
                            info.surface = new_surface;
                            info.display_surface = new_display;
                        }
                        info.width = width;
                        info.height = height;
                    }
                    Some(info.x11_id)
                } else {
                    None
                }
            });

            if let Some(x11_id) = event_info {
                send_display_event(DisplayEvent::ConfigureNotify {
                    window: x11_id, x, y, width, height,
                });
                send_display_event(DisplayEvent::Expose {
                    window: x11_id, x: 0, y: 0, width, height, count: 0,
                });
            }
        }

        DisplayCommand::SetWindowBackgroundPixel { handle, pixel } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().get_mut(&handle.id) {
                    info.background_pixel = pixel;
                }
            });
        }

        DisplayCommand::ReadPixels { handle, x, y, width, height, reply } => {
            let result = WINDOWS.with(|w| {
                let ws = w.borrow();
                if let Some(info) = ws.get(&handle.id) {
                    unsafe {
                        let lock_result = IOSurfaceLock(info.surface, 1, std::ptr::null_mut());
                        if lock_result != 0 { return None; }
                        let base = IOSurfaceGetBaseAddress(info.surface) as *const u8;
                        let stride = IOSurfaceGetBytesPerRow(info.surface);
                        let w = width as usize;
                        let h = height as usize;
                        let mut pixels = vec![0u8; w * h * 4];
                        for row in 0..h {
                            let sy = y as usize + row;
                            if sy >= info.height as usize { break; }
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

        DisplayCommand::SetClipboard { content } => {
            unsafe {
                let pb: *mut AnyObject = msg_send![objc2::class!(UIPasteboard), generalPasteboard];
                let ns_str = NSString::from_str(&content);
                let _: () = msg_send![pb, setString: &*ns_str];
            }
        }

        DisplayCommand::GetClipboard { reply } => {
            let content = unsafe {
                let pb: *mut AnyObject = msg_send![objc2::class!(UIPasteboard), generalPasteboard];
                let str_obj: *mut AnyObject = msg_send![pb, string];
                if !str_obj.is_null() {
                    let utf8: *const std::os::raw::c_char = msg_send![&*str_obj, UTF8String];
                    if !utf8.is_null() {
                        std::ffi::CStr::from_ptr(utf8).to_str().ok().map(|s| s.to_string())
                    } else { None }
                } else { None }
            };
            let _ = reply.send(content);
        }

        DisplayCommand::Shutdown => {
            info!("iOS: Shutdown requested");
            // On iOS, we don't terminate the app programmatically
        }

        _ => {
            debug!("Unhandled display command on iOS");
        }
    }
}

// --- UIApplicationDelegate ---

/// Register the PSLXAppDelegate class.
fn get_app_delegate_class() -> &'static AnyClass {
    use std::sync::Once;
    static mut CLASS: *const AnyClass = std::ptr::null();
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let superclass = objc2::class!(UIResponder);
        let mut builder = ClassBuilder::new(c"PSLXAppDelegate", superclass)
            .expect("Failed to create PSLXAppDelegate class");

        extern "C" {
            fn class_addMethod(
                cls: *mut c_void,
                sel: Sel,
                imp: *const c_void,
                types: *const std::ffi::c_char,
            ) -> bool;
            fn class_addProtocol(
                cls: *mut c_void,
                protocol: *const c_void,
            ) -> bool;
            fn objc_getProtocol(
                name: *const std::ffi::c_char,
            ) -> *const c_void;
        }

        let raw_cls = builder.register() as *const AnyClass as *mut c_void;

        unsafe {
            let proto = objc_getProtocol(c"UIApplicationDelegate".as_ptr() as _);
            if !proto.is_null() {
                class_addProtocol(raw_cls, proto);
            }

            // application:configurationForConnectingSceneSession:options:
            class_addMethod(raw_cls,
                objc2::sel!(application:configurationForConnectingSceneSession:options:),
                app_configuration_for_scene as *const c_void,
                c"@@:@@@".as_ptr() as _);

            // application:didFinishLaunchingWithOptions: — discard stale scene sessions
            class_addMethod(raw_cls,
                objc2::sel!(application:didFinishLaunchingWithOptions:),
                app_did_finish_launching as *const c_void,
                c"c@:@@".as_ptr() as _);

            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

/// Discard all stale scene sessions from previous launches.
/// iPadOS remembers scene sessions — without this, old scenes get restored and
/// we end up with ghost windows. Same principle as macOS where NSWindow count is
/// precisely controlled.
unsafe extern "C" fn app_did_finish_launching(
    _this: *mut AnyObject, _sel: Sel,
    app: *mut AnyObject, _options: *mut AnyObject,
) -> bool {
    info!("iOS: didFinishLaunchingWithOptions — discarding stale scene sessions");
    let sessions: *mut AnyObject = msg_send![app, openSessions];
    if !sessions.is_null() {
        // NSSet → allObjects → NSArray
        let all: *mut AnyObject = msg_send![sessions, allObjects];
        if !all.is_null() {
            let count: usize = msg_send![all, count];
            // Keep only 1 session (the main one), destroy extras
            for i in 1..count {
                let session: *mut AnyObject = msg_send![all, objectAtIndex: i];
                let _: () = msg_send![app, requestSceneSessionDestruction: session
                    options: std::ptr::null_mut::<AnyObject>()
                    errorHandler: std::ptr::null_mut::<AnyObject>()];
                info!("iOS: discarded stale scene session {}", i);
            }
        }
    }
    true // YES — launch succeeded
}

unsafe extern "C" fn app_configuration_for_scene(
    _this: *mut AnyObject, _sel: Sel,
    _app: *mut AnyObject, _session: *mut AnyObject, _options: *mut AnyObject,
) -> *mut AnyObject {
    let config: *mut AnyObject = msg_send![
        objc2::class!(UISceneConfiguration), alloc
    ];
    let name = NSString::from_str("Default Configuration");
    let role = NSString::from_str("UIWindowSceneSessionRoleApplication");
    let config: *mut AnyObject = msg_send![config, initWithName: &*name sessionRole: &*role];

    // Set our scene delegate class
    let scene_cls = get_scene_delegate_class();
    let _: () = msg_send![config, setDelegateClass: scene_cls];
    config
}

/// Register PSLXSceneDelegate for UIWindowSceneDelegate.
fn get_scene_delegate_class() -> &'static AnyClass {
    use std::sync::Once;
    static mut CLASS: *const AnyClass = std::ptr::null();
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let superclass = objc2::class!(UIResponder);
        let mut builder = ClassBuilder::new(c"PSLXSceneDelegate", superclass)
            .expect("Failed to create PSLXSceneDelegate class");

        // Add `window` property storage
        builder.add_ivar::<*mut AnyObject>(c"_window");

        extern "C" {
            fn class_addMethod(
                cls: *mut c_void,
                sel: Sel,
                imp: *const c_void,
                types: *const std::ffi::c_char,
            ) -> bool;
            fn class_addProtocol(
                cls: *mut c_void,
                protocol: *const c_void,
            ) -> bool;
            fn objc_getProtocol(
                name: *const std::ffi::c_char,
            ) -> *const c_void;
        }

        let raw_cls = builder.register() as *const AnyClass as *mut c_void;

        unsafe {
            let proto = objc_getProtocol(c"UIWindowSceneDelegate".as_ptr() as _);
            if !proto.is_null() {
                class_addProtocol(raw_cls, proto);
            }

            // window property getter/setter (required by UIWindowSceneDelegate)
            class_addMethod(raw_cls, objc2::sel!(window),
                scene_get_window as *const c_void, c"@@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(setWindow:),
                scene_set_window as *const c_void, c"v@:@".as_ptr() as _);

            // scene:willConnectToSession:options:
            class_addMethod(raw_cls,
                objc2::sel!(scene:willConnectToSession:options:),
                scene_will_connect as *const c_void,
                c"v@:@@@".as_ptr() as _);

            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

unsafe extern "C" fn scene_get_window(this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    let ivar = (*this).class().instance_variable(c"_window").unwrap();
    *ivar.load::<*mut AnyObject>(&*this)
}

unsafe extern "C" fn scene_set_window(this: *mut AnyObject, _sel: Sel, window: *mut AnyObject) {
    let ivar = (*this).class().instance_variable(c"_window").unwrap();
    *ivar.load_mut::<*mut AnyObject>(&mut *this) = window;
}

/// Custom UIViewController that hides the status bar (removes gap at top).
fn get_fullscreen_vc_class() -> &'static AnyClass {
    use std::sync::Once;
    static mut CLASS: *const AnyClass = std::ptr::null();
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let superclass = objc2::class!(UIViewController);
        let mut builder = ClassBuilder::new(c"PSLXViewController", superclass)
            .expect("Failed to create PSLXViewController class");

        extern "C" {
            fn class_addMethod(
                cls: *mut c_void, sel: Sel, imp: *const c_void, types: *const std::ffi::c_char,
            ) -> bool;
        }

        let raw_cls = builder.register() as *const AnyClass as *mut c_void;
        unsafe {
            // prefersStatusBarHidden → YES
            class_addMethod(raw_cls, objc2::sel!(prefersStatusBarHidden),
                vc_prefers_status_bar_hidden as *const c_void, c"B@:".as_ptr() as _);
            // prefersHomeIndicatorAutoHidden → YES
            class_addMethod(raw_cls, objc2::sel!(prefersHomeIndicatorAutoHidden),
                vc_prefers_status_bar_hidden as *const c_void, c"B@:".as_ptr() as _);
            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

unsafe extern "C" fn vc_prefers_status_bar_hidden(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

/// Scene connected — called for both initial scene and requestSceneSessionActivation scenes.
/// For the initial scene: save as MAIN_SCENE (X11 windows will claim it later).
/// For new scenes: pick up pending window ID and create UIWindow+UIView there.
unsafe extern "C" fn scene_will_connect(
    _this: *mut AnyObject, _sel: Sel,
    scene: *mut AnyObject, _session: *mut AnyObject, _options: *mut AnyObject,
) {
    info!("iOS: scene_will_connect");

    // Save screen dimensions
    use objc2::encode::{Encode, Encoding};
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGRect { origin: [f64; 2], size: [f64; 2] }
    unsafe impl Encode for CGRect {
        const ENCODING: Encoding = Encoding::Struct("CGRect", &[
            Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]),
            Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]),
        ]);
    }
    let screen: *mut AnyObject = msg_send![objc2::class!(UIScreen), mainScreen];
    let bounds: CGRect = msg_send![screen, bounds];
    SCREEN_WIDTH.with(|sw| sw.set(bounds.size[0]));
    SCREEN_HEIGHT.with(|sh| sh.set(bounds.size[1]));

    let _: *mut AnyObject = msg_send![scene, retain];

    // Check if there's a pending window waiting for a new scene
    let pending_id = {
        let mut pending = PENDING_SCENE_WINDOWS.lock().unwrap();
        if !pending.is_empty() {
            Some(pending.remove(0))
        } else {
            None
        }
    };

    if let Some(native_id) = pending_id {
        // New scene for an X11 window — store scene reference and set up UIWindow
        ios_log(&format!("scene_will_connect: new scene for window {} (screen={}x{})",
            native_id, bounds.size[0], bounds.size[1]));
        PENDING_SCENE_MAP.with(|pm| pm.borrow_mut().push((native_id, scene)));
        setup_window_in_scene(native_id);
    } else {
        // Initial scene at app launch — save for the first X11 window to claim
        MAIN_SCENE.with(|ms| *ms.borrow_mut() = Some(scene));
        ios_log(&format!("scene_will_connect: initial scene saved (screen={}x{})",
            bounds.size[0], bounds.size[1]));
    }
}

/// Build a keyboard map for iOS (simplified — no UCKeyTranslate on iOS).
/// Returns standard US QWERTY layout.
pub fn build_keyboard_map() -> Box<[(u32, u32); 128]> {
    let mut map = Box::new([(0u32, 0u32); 128]);
    // Standard US QWERTY layout keycodes (macOS keycode → keysym)
    // These are the same keycodes as macOS hardware keyboard
    let mappings: &[(usize, u32, u32)] = &[
        // (keycode, normal_keysym, shifted_keysym)
        (0, 0x61, 0x41),   // a A
        (1, 0x73, 0x53),   // s S
        (2, 0x64, 0x44),   // d D
        (3, 0x66, 0x46),   // f F
        (4, 0x68, 0x48),   // h H
        (5, 0x67, 0x47),   // g G
        (6, 0x7a, 0x5a),   // z Z
        (7, 0x78, 0x58),   // x X
        (8, 0x63, 0x43),   // c C
        (9, 0x76, 0x56),   // v V
        (11, 0x62, 0x42),  // b B
        (12, 0x71, 0x51),  // q Q
        (13, 0x77, 0x57),  // w W
        (14, 0x65, 0x45),  // e E
        (15, 0x72, 0x52),  // r R
        (16, 0x79, 0x59),  // y Y
        (17, 0x74, 0x54),  // t T
        (18, 0x31, 0x21),  // 1 !
        (19, 0x32, 0x40),  // 2 @
        (20, 0x33, 0x23),  // 3 #
        (21, 0x34, 0x24),  // 4 $
        (22, 0x36, 0x5e),  // 6 ^
        (23, 0x35, 0x25),  // 5 %
        (24, 0x3d, 0x2b),  // = +
        (25, 0x39, 0x28),  // 9 (
        (26, 0x37, 0x26),  // 7 &
        (27, 0x2d, 0x5f),  // - _
        (28, 0x38, 0x2a),  // 8 *
        (29, 0x30, 0x29),  // 0 )
        (30, 0x5d, 0x7d),  // ] }
        (31, 0x6f, 0x4f),  // o O
        (32, 0x75, 0x55),  // u U
        (33, 0x5b, 0x7b),  // [ {
        (34, 0x69, 0x49),  // i I
        (35, 0x70, 0x50),  // p P
        (36, 0xff0d, 0xff0d), // Return
        (37, 0x6c, 0x4c),  // l L
        (38, 0x6a, 0x4a),  // j J
        (39, 0x27, 0x22),  // ' "
        (40, 0x6b, 0x4b),  // k K
        (41, 0x3b, 0x3a),  // ; :
        (42, 0x5c, 0x7c),  // \ |
        (43, 0x2c, 0x3c),  // , <
        (44, 0x2f, 0x3f),  // / ?
        (45, 0x6e, 0x4e),  // n N
        (46, 0x6d, 0x4d),  // m M
        (47, 0x2e, 0x3e),  // . >
        (48, 0xff09, 0xff09), // Tab
        (49, 0x20, 0x20),  // Space
        (50, 0x60, 0x7e),  // ` ~
        (51, 0xff08, 0xff08), // Backspace
    ];

    for &(kc, normal, shifted) in mappings {
        map[kc] = (normal, shifted);
    }
    map
}

/// Run the iOS application.
/// This function blocks — it IS the main run loop (UIApplicationMain).
pub fn run_ios_app(
    cmd_rx: Receiver<DisplayCommand>,
    evt_tx: Sender<DisplayEvent>,
    render_mailbox: crate::display::RenderMailbox,
) {
    // Build and register keyboard map for iOS
    let kmap = build_keyboard_map();
    let _ = crate::display::KEYBOARD_MAP.set(kmap);
    info!("iOS: Keyboard map initialized");

    // Store channels in thread-local storage
    CMD_RX.with(|rx| *rx.borrow_mut() = Some(cmd_rx));
    EVT_TX.with(|tx| *tx.borrow_mut() = Some(evt_tx));
    RENDER_MAILBOX.with(|mb| *mb.borrow_mut() = Some(render_mailbox));

    // Create 60fps timer for command processing + rendering
    unsafe {
        let now = CFAbsoluteTimeGetCurrent();
        let timer = CFRunLoopTimerCreate(
            std::ptr::null(), now + 0.016, 0.016,
            0, 0, timer_callback, std::ptr::null_mut(),
        );
        CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopCommonModes);
    }

    ios_log("run_ios_app: about to call UIApplicationMain");
    info!("iOS: Starting UIApplicationMain");

    // Call UIApplicationMain — this never returns
    extern "C" {
        fn UIApplicationMain(
            argc: i32,
            argv: *const *const std::os::raw::c_char,
            principal_class_name: *mut AnyObject,
            delegate_class_name: *mut AnyObject,
        ) -> i32;
    }

    let delegate_name = NSString::from_str("PSLXAppDelegate");
    // Ensure the classes are registered before UIApplicationMain
    let _ = get_app_delegate_class();
    let _ = get_scene_delegate_class();
    let _ = get_pslx_view_class();

    unsafe {
        UIApplicationMain(
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
            &*delegate_name as *const _ as *mut AnyObject,
        );
    }
}
