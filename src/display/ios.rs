// iOS display backend — UIWindow/UIView management, IOSurface-backed pixel buffer rendering
// Mirrors macos.rs but uses UIKit instead of AppKit.
// Single fullscreen UIView — all X11 windows rendered to one surface.
#![cfg(target_os = "ios")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use crossbeam_channel::{Receiver, Sender};
use log::{debug, info};

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2_foundation::{MainThreadMarker, NSString};

use crate::display::{DisplayCommand, DisplayEvent, NativeWindowHandle};
use crate::display::renderer::render_to_buffer;

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
    static LAST_POINTER: std::cell::Cell<(i16, i16)> = const { std::cell::Cell::new((0, 0)) };
    /// Implicit pointer grab: window that received the most recent touch down.
    static GRAB_WINDOW: std::cell::Cell<Option<crate::display::Xid>> = const { std::cell::Cell::new(None) };
    /// The main UIView (our single fullscreen view). Pointer stored for layer access.
    static MAIN_VIEW: RefCell<Option<*mut AnyObject>> = RefCell::new(None);
    /// The main UIWindow.
    static MAIN_UI_WINDOW: RefCell<Option<*mut AnyObject>> = RefCell::new(None);
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

/// UIKeyInput insertText: — called when user types on software keyboard.
/// Single characters come as NSString.
unsafe extern "C" fn ios_insert_text(_this: *mut AnyObject, _sel: Sel, text: *mut AnyObject) {
    if text.is_null() { return; }

    let utf8: *const std::os::raw::c_char = msg_send![&*text, UTF8String];
    if utf8.is_null() { return; }
    let rust_str = match std::ffi::CStr::from_ptr(utf8).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return,
    };
    if rust_str.is_empty() { return; }

    // Find the focused X11 window (first visible one)
    let x11_id = get_focused_x11_window();
    if x11_id == 0 { return; }

    // Single ASCII characters → KeyPress with physical keycode
    let is_single_ascii = rust_str.len() == 1 && rust_str.as_bytes()[0] < 0x80;
    if is_single_ascii {
        let ch = rust_str.as_bytes()[0];
        let (keycode, state) = ascii_to_x11_keycode_state(ch);
        let time = get_timestamp();
        send_display_event(DisplayEvent::KeyPress {
            window: x11_id, keycode, state, time,
        });
        send_display_event(DisplayEvent::KeyRelease {
            window: x11_id, keycode, state, time,
        });
    } else {
        // Non-ASCII (IME, emoji, etc.) → ImeCommit
        info!("iOS insertText: '{}' for window 0x{:08x}", rust_str, x11_id);
        send_display_event(DisplayEvent::ImeCommit {
            window: x11_id,
            text: rust_str,
        });
    }
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

unsafe extern "C" fn touches_began(this: *mut AnyObject, _sel: Sel, touches: *mut AnyObject, _event: *mut AnyObject) {
    let touch = get_first_touch(touches);
    if touch.is_null() { return; }
    let (x, y) = touch_location_in_view(touch, this);
    let time = get_timestamp();
    let x11_id = find_x11_window_at(x, y);
    if x11_id == 0 { return; }
    let (win_x, win_y) = screen_to_window_coords(x, y, x11_id);

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

// --- Helper functions ---

fn get_first_touch(touches: *mut AnyObject) -> *mut AnyObject {
    unsafe {
        let any_touch: *mut AnyObject = msg_send![&*touches, anyObject];
        any_touch
    }
}

fn touch_location_in_view(touch: *mut AnyObject, view: *mut AnyObject) -> (i16, i16) {
    #[repr(C)]
    struct CGPoint { x: f64, y: f64 }

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

/// Find which X11 window is at screen position (x, y).
/// On iOS, X11 windows are rendered in Z-order to a single surface.
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
fn ascii_to_x11_keycode_state(ch: u8) -> (u8, u16) {
    // Map common ASCII to X11 keycodes (offset by 8 from evdev)
    let (keycode, shift) = match ch {
        b'a'..=b'z' => (ch - b'a' + 38, false), // a=38, b=56, c=54... simplified
        b'A'..=b'Z' => (ch - b'A' + 38, true),
        b'0' => (19, false),
        b'1'..=b'9' => (ch - b'1' + 10, false),
        b' ' => (65, false),   // Space
        b'\n' | b'\r' => (36, false), // Return
        b'\t' => (23, false),  // Tab
        b'-' => (20, false),
        b'=' => (21, false),
        b'[' => (34, false),
        b']' => (35, false),
        b'\\' => (51, false),
        b';' => (47, false),
        b'\'' => (48, false),
        b',' => (59, false),
        b'.' => (60, false),
        b'/' => (61, false),
        b'`' => (49, false),
        b'!' => (10, true),
        b'@' => (11, true),
        b'#' => (12, true),
        b'$' => (13, true),
        b'%' => (14, true),
        b'^' => (15, true),
        b'&' => (16, true),
        b'*' => (17, true),
        b'(' => (18, true),
        b')' => (19, true),
        b'_' => (20, true),
        b'+' => (21, true),
        b'{' => (34, true),
        b'}' => (35, true),
        b'|' => (51, true),
        b':' => (47, true),
        b'"' => (48, true),
        b'<' => (59, true),
        b'>' => (60, true),
        b'?' => (61, true),
        b'~' => (49, true),
        0x1b => (9, false),    // Escape
        0x7f => (22, false),   // Backspace (DEL on iOS)
        _ => (0, false),
    };
    let state = if shift { 1u16 } else { 0u16 }; // ShiftMask = 1
    (keycode, state)
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

/// Set display_surface as CALayer contents for the main view.
fn flush_to_screen(surface: *mut c_void) {
    if surface.is_null() { return; }
    MAIN_VIEW.with(|mv| {
        let mv = mv.borrow();
        if let Some(view) = *mv {
            unsafe {
                let layer: *mut AnyObject = msg_send![view, layer];
                if !layer.is_null() {
                    let ca_cls = objc_getClass(b"CATransaction\0".as_ptr());
                    let _: () = msg_send![ca_cls, begin];
                    let _: () = msg_send![ca_cls, setDisableActions: true];
                    let _: () = msg_send![layer, setContents: surface as *mut AnyObject];
                    let _: () = msg_send![ca_cls, commit];
                }
            }
        }
    });
}

// --- Timer callback (60fps render loop) ---

extern "C" fn timer_callback(_timer: *mut c_void, _info: *mut c_void) {
    process_commands();
}

fn process_commands() {
    // 1. Process non-render commands from channel
    let cmds: Vec<DisplayCommand> = CMD_RX.with(|rx| {
        rx.borrow().as_ref().map_or_else(Vec::new, |rx| rx.try_iter().collect())
    });
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
                            flush_to_screen(info.display_surface);
                        }
                    }
                }
                if any_rendered {
                    ca_transaction_flush();
                }
            });
        }
    });
}

fn handle_command(cmd: DisplayCommand) {
    match cmd {
        DisplayCommand::CreateWindow {
            x11_id, x, y, width, height, title: _, override_redirect: _, reply,
        } => {
            let id = alloc_id();

            let surface = create_iosurface(width, height);
            let display_surface = create_iosurface(width, height);
            if surface.is_null() || display_surface.is_null() {
                log::error!("Failed to create IOSurface for window {}", id);
                return;
            }

            // Fill with opaque black
            unsafe {
                for s in [surface, display_surface] {
                    IOSurfaceLock(s, 0, std::ptr::null_mut());
                    let base = IOSurfaceGetBaseAddress(s) as *mut u32;
                    let stride = IOSurfaceGetBytesPerRow(s);
                    for row in 0..height as usize {
                        let row_ptr = (base as *mut u8).add(row * stride) as *mut u32;
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
                });
            });

            info!("Created window {} for X11 0x{:08X} ({}x{}) [iOS IOSurface]", id, x11_id, width, height);
            let _ = reply.send(NativeWindowHandle { id });
        }

        DisplayCommand::ShowWindow { handle, visible } => {
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().get_mut(&handle.id) {
                    info.visible = visible;
                    if visible {
                        info!("ShowWindow: id={} x11=0x{:08X} - visible on iOS", handle.id, info.x11_id);
                        let x11_id = info.x11_id;
                        flush_to_screen(info.display_surface);
                        send_display_event(DisplayEvent::FocusIn { window: x11_id });
                    }
                }
            });
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
                w.borrow_mut().remove(&handle.id);
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

            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
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

unsafe extern "C" fn scene_will_connect(
    this: *mut AnyObject, _sel: Sel,
    scene: *mut AnyObject, _session: *mut AnyObject, _options: *mut AnyObject,
) {
    info!("iOS: scene:willConnectToSession:options: called");

    // Create UIWindow with the scene
    let window: *mut AnyObject = msg_send![objc2::class!(UIWindow), alloc];
    let window: *mut AnyObject = msg_send![window, initWithWindowScene: &*scene];

    // Get screen bounds
    let screen: *mut AnyObject = msg_send![objc2::class!(UIScreen), mainScreen];
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGRect { x: f64, y: f64, w: f64, h: f64 }
    let bounds: CGRect = msg_send![screen, bounds];
    SCREEN_WIDTH.with(|sw| sw.set(bounds.w));
    SCREEN_HEIGHT.with(|sh| sh.set(bounds.h));
    info!("iOS screen: {}x{}", bounds.w, bounds.h);

    // Create our custom PSLXView covering the full screen
    let view_cls = get_pslx_view_class();
    let view: *mut AnyObject = msg_send![view_cls, alloc];
    let view: *mut AnyObject = msg_send![view, initWithFrame: bounds];

    // Enable layer-backed rendering
    let layer: *mut AnyObject = msg_send![view, layer];
    if !layer.is_null() {
        let gravity = NSString::from_str("topLeft");
        let _: () = msg_send![layer, setContentsGravity: &*gravity];
        let _: () = msg_send![layer, setContentsScale: 1.0_f64];
        let _: () = msg_send![layer, setMasksToBounds: true];
    }

    // Set black background
    let black: *mut AnyObject = msg_send![objc2::class!(UIColor), blackColor];
    let _: () = msg_send![view, setBackgroundColor: black];
    let _: () = msg_send![view, setMultipleTouchEnabled: true];

    // Create a UIViewController and set the view
    let vc: *mut AnyObject = msg_send![objc2::class!(UIViewController), alloc];
    let vc: *mut AnyObject = msg_send![vc, init];
    let _: () = msg_send![vc, setView: view];

    let _: () = msg_send![window, setRootViewController: vc];
    let _: () = msg_send![window, makeKeyAndVisible];

    // Make the view first responder (for keyboard)
    let _: () = msg_send![view, becomeFirstResponder];

    // Store references
    MAIN_VIEW.with(|mv| *mv.borrow_mut() = Some(view));
    MAIN_UI_WINDOW.with(|mw| *mw.borrow_mut() = Some(window));

    // Store window in scene delegate
    let _: () = msg_send![this, setWindow: window];

    info!("iOS: UIWindow and PSLXView created");
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
