// iOS display backend — UIWindow/UIView management, IOSurface-backed pixel buffer rendering
// Mirrors macos.rs but uses UIKit instead of AppKit.
// Single fullscreen UIView — all X11 windows rendered to one surface.
#![cfg(target_os = "ios")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use crossbeam_channel::{Receiver, Sender};
use log::{debug, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Stores the X11 keycode (mac_kc + 8) of the most recent key handled by pressesBegan.
/// When insertText: sees a matching keycode, it skips the event (already sent by pressesBegan).
/// 0 = no pending hardware key. This is a one-shot: insertText: swaps it to 0 on read.
/// This approach allows Japanese romaji input from software keyboard: pressesBegan is NOT called
/// for software keyboard romaji letters, so LAST_HW_KEYCODE stays 0 and insertText: sends them.
static LAST_HW_KEYCODE: AtomicU8 = AtomicU8::new(0);

/// Current preedit/marked text buffer for UITextInput.
/// UIKit calls textInRange: to know the current text in the marked range.
/// Without this buffer returning the correct preedit, UIKit won't call insertText
/// for the previous character when switching to a new kana key.
std::thread_local! {
    static PREEDIT_BUF: RefCell<String> = const { RefCell::new(String::new()) };
}

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2_foundation::{MainThreadMarker, NSString};

use crate::display::{DisplayCommand, DisplayEvent, NativeWindowHandle};
use crate::display::renderer::render_to_buffer;

/// Call becomeFirstResponder on the next run loop iteration.
/// Uses performSelector:withObject:afterDelay:0 (pure ObjC messaging, no Dispatch linkage needed).
/// Deferring avoids silent failure when the view hierarchy isn't fully set up yet.
unsafe fn become_first_responder_deferred(view: *mut AnyObject) {
    if view.is_null() { return; }
    // Defer to next run loop iteration so UIKit finishes current layout pass first.
    // Direct call inside timer callback confuses UIKit's software keyboard management.
    // NOTE: reloadInputViews removed — it caused UIKit to suppress the software keyboard.
    let sel_become = objc2::sel!(becomeFirstResponder);
    let nil: *mut AnyObject = std::ptr::null_mut();
    let _: () = msg_send![view, performSelector: sel_become withObject: nil afterDelay: 0.0_f64];
}

/// Write debug message to stderr (visible via devicectl --console on real device).
fn ios_log(msg: &str) {
    eprintln!("[pslx-ios] {}", msg);
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
    /// Content CALayer — holds IOSurface, positioned at y=TITLE_BAR_HEIGHT within container_layer.
    ca_layer: *mut AnyObject,
    /// Container CALayer — parent of title bar + ca_layer. This is what gets moved for dragging.
    /// Positioned at (x11_x, x11_y), size = (width, height + TITLE_BAR_HEIGHT).
    container_layer: *mut AnyObject,
    /// Per-window UIWindow (like macOS NSWindow). May be null until scene_will_connect.
    ui_window: *mut AnyObject,
    /// Per-window PSLXView (like macOS PSLXInputView/contentView).
    ui_view: *mut AnyObject,
    /// Title bar UIView with traffic light buttons (red/yellow/green).
    title_bar: *mut AnyObject,
    /// Whether this window is in fullscreen mode (title bar hidden).
    is_fullscreen: bool,
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
        // CALayer (IOSurface layer on PSLXView) — just release, no removeFromSuperlayer needed
        // (UIWindow is hidden, view will be released separately)
        if !self.ca_layer.is_null() {
            unsafe {
                CFRelease(self.ca_layer as *const c_void);
            }
        }
        // UIWindow cleanup: just hide.
        if !self.ui_window.is_null() {
            unsafe {
                let _: () = msg_send![self.ui_window, setHidden: true];
            }
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
    /// Button number for the current grab (1=left, 2=middle, 3=right).
    static GRAB_BUTTON: std::cell::Cell<u8> = const { std::cell::Cell::new(1) };
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
    /// Current keyboard height in points (0 when hidden).
    static KEYBOARD_HEIGHT: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    /// Pending resize from keyboard change — (width, height) applied on next timer tick.
    static PENDING_RESIZE: std::cell::Cell<Option<(u16, u16)>> = const { std::cell::Cell::new(None) };
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
        builder.add_ivar::<u32>(c"x11WindowId"); // X11 window ID for per-window event routing
        builder.add_ivar::<*mut AnyObject>(c"_inputDelegate"); // UITextInput inputDelegate property
        builder.add_ivar::<*mut AnyObject>(c"_tokenizer"); // UITextInput tokenizer property

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
            // Conform to UIKeyInput AND UITextInput protocols.
            // UITextInput is required for Japanese kana keyboard (Kana-RTL) to route input.
            // Without UITextInput protocol adoption, kana keyboard taps are silently ignored.
            let proto_keyinput = objc_getProtocol(c"UIKeyInput".as_ptr() as _);
            if !proto_keyinput.is_null() {
                class_addProtocol(raw_cls, proto_keyinput);
            }
            let proto_textinput = objc_getProtocol(c"UITextInput".as_ptr() as _);
            if !proto_textinput.is_null() {
                class_addProtocol(raw_cls, proto_textinput);
            }

            // canBecomeFirstResponder -> YES
            class_addMethod(raw_cls, objc2::sel!(canBecomeFirstResponder),
                can_become_first_responder as *const c_void, c"B@:".as_ptr() as _);
            // becomeFirstResponder — track focused view for keyboard event routing
            class_addMethod(raw_cls, objc2::sel!(becomeFirstResponder),
                become_first_responder as *const c_void, c"B@:".as_ptr() as _);
            // resignFirstResponder — log when UIKit takes focus away
            class_addMethod(raw_cls, objc2::sel!(resignFirstResponder),
                resign_first_responder as *const c_void, c"B@:".as_ptr() as _);

            // UIKeyInput methods
            class_addMethod(raw_cls, objc2::sel!(hasText),
                has_text as *const c_void, c"B@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(insertText:),
                ios_insert_text as *const c_void, c"v@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(deleteBackward),
                delete_backward as *const c_void, c"v@:".as_ptr() as _);

            // UITextInput methods — required for Japanese IME (kana/romaji keyboards)
            class_addMethod(raw_cls, objc2::sel!(setMarkedText:selectedRange:),
                ios_set_marked_text as *const c_void, c"v@:@{_NSRange=QQ}".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(unmarkText),
                ios_unmark_text as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(markedTextRange),
                ios_marked_text_range as *const c_void, c"@@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(markedTextStyle),
                ios_return_nil as *const c_void, c"@@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(setMarkedTextStyle:),
                ios_set_noop as *const c_void, c"v@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(selectedTextRange),
                ios_selected_text_range as *const c_void, c"@@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(setSelectedTextRange:),
                ios_set_selected_text_range as *const c_void, c"v@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(beginningOfDocument),
                ios_beginning_of_document as *const c_void, c"@@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(endOfDocument),
                ios_end_of_document as *const c_void, c"@@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(textInRange:),
                ios_text_in_range as *const c_void, c"@@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(replaceRange:withText:),
                ios_replace_range as *const c_void, c"v@:@@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(textRangeFromPosition:toPosition:),
                ios_text_range_from_positions as *const c_void, c"@@:@@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(positionFromPosition:offset:),
                ios_position_from_offset as *const c_void, c"@@:@q".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(positionFromPosition:inDirection:offset:),
                ios_position_from_direction as *const c_void, c"@@:@qq".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(comparePosition:toPosition:),
                ios_compare_position as *const c_void, c"q@:@@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(offsetFromPosition:toPosition:),
                ios_offset_from_position as *const c_void, c"q@:@@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(inputDelegate),
                ios_get_input_delegate as *const c_void, c"@@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(setInputDelegate:),
                ios_set_input_delegate as *const c_void, c"v@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(tokenizer),
                ios_tokenizer as *const c_void, c"@@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(firstRectForRange:),
                ios_first_rect_for_range as *const c_void, c"{CGRect={CGPoint=dd}{CGSize=dd}}@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(caretRectForPosition:),
                ios_caret_rect_for_position as *const c_void, c"{CGRect={CGPoint=dd}{CGSize=dd}}@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(selectionRectsForRange:),
                ios_selection_rects as *const c_void, c"@@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(closestPositionToPoint:),
                ios_closest_position_to_point as *const c_void, c"@@:{CGPoint=dd}".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(closestPositionToPoint:withinRange:),
                ios_closest_position_to_point_in_range as *const c_void, c"@@:{CGPoint=dd}@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(characterRangeAtPoint:),
                ios_character_range_at_point as *const c_void, c"@@:{CGPoint=dd}".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(baseWritingDirectionForPosition:inDirection:),
                ios_base_writing_direction as *const c_void, c"q@:@q".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(setBaseWritingDirection:forRange:),
                ios_set_base_writing_direction as *const c_void, c"v@:q@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(positionWithinRange:farthestInDirection:),
                ios_position_within_range as *const c_void, c"@@:@q".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(characterRangeByExtendingPosition:inDirection:),
                ios_character_range_by_extending as *const c_void, c"@@:@q".as_ptr() as _);

            // insertText:alternatives:style: — iOS 16+ richer insertion method
            // Called by kana/IME keyboards with candidate alternatives. Forward to insertText:.
            class_addMethod(raw_cls, objc2::sel!(insertText:alternatives:style:),
                ios_insert_text_with_alternatives as *const c_void, c"v@:@@q".as_ptr() as _);

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

            // Mouse hover handler (called by UIHoverGestureRecognizer added in setup_window_in_scene)
            class_addMethod(raw_cls, objc2::sel!(handleHover:),
                handle_hover as *const c_void, c"v@:@".as_ptr() as _);

            // Scroll handler (called by UIPanGestureRecognizer with allowedScrollTypesMask)
            class_addMethod(raw_cls, objc2::sel!(handleScroll:),
                handle_scroll as *const c_void, c"v@:@".as_ptr() as _);

            // Touch scroll handler (2-finger pan → Button4/5 emulation, like jog dial)
            class_addMethod(raw_cls, objc2::sel!(handleTouchScroll:),
                handle_touch_scroll as *const c_void, c"v@:@".as_ptr() as _);

            // inputAccessoryView — shows Ctrl/Esc/Tab/arrow toolbar above the keyboard
            class_addMethod(raw_cls, objc2::sel!(inputAccessoryView),
                input_accessory_view as *const c_void, c"@@:".as_ptr() as _);

            // Control key shortcuts (targets for accessory toolbar buttons)
            class_addMethod(raw_cls, objc2::sel!(sendCtrlC),
                send_ctrl_c as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendCtrlD),
                send_ctrl_d as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendCtrlZ),
                send_ctrl_z as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendCtrlA),
                send_ctrl_a as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendCtrlE),
                send_ctrl_e as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendCtrlU),
                send_ctrl_u as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendCtrlL),
                send_ctrl_l as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendEsc),
                send_esc as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendTab),
                send_tab as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendArrowUp),
                send_arrow_up as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendArrowDown),
                send_arrow_down as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendArrowLeft),
                send_arrow_left as *const c_void, c"v@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(sendArrowRight),
                send_arrow_right as *const c_void, c"v@:".as_ptr() as _);

            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

// --- PSLXView method implementations ---

unsafe extern "C" fn can_become_first_responder(_this: *mut AnyObject, _sel: Sel) -> Bool {
    ios_log("canBecomeFirstResponder -> YES");
    Bool::YES
}

unsafe extern "C" fn become_first_responder(this: *mut AnyObject, sel: Sel) -> Bool {
    // Update FOCUSED_VIEW so get_focused_x11_window() returns this view's x11_id
    FOCUSED_VIEW.store(this as usize, std::sync::atomic::Ordering::Relaxed);
    let x11_id = view_x11_id(this);
    ios_log(&format!("becomeFirstResponder: view={:p} x11=0x{:08x}", this, x11_id));
    // Call super implementation
    let superclass = objc2::class!(UIView);
    objc2::msg_send![super(this, superclass), becomeFirstResponder]
}

unsafe extern "C" fn resign_first_responder(_this: *mut AnyObject, _sel: Sel) -> Bool {
    ios_log("resignFirstResponder called — view losing focus!");
    // Return YES to allow resignation (default behavior)
    Bool::YES
}


unsafe extern "C" fn has_text(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

/// insertText:alternatives:style: — iOS 16+ richer text insertion.
/// Called by kana/IME keyboards with autocorrect alternatives.
/// Just forward text to our main insertText: implementation.
unsafe extern "C" fn ios_insert_text_with_alternatives(this: *mut AnyObject, sel: Sel, text: *mut AnyObject, _alternatives: *mut AnyObject, _style: i64) {
    ios_log("insertText:alternatives:style: called");
    ios_insert_text(this, sel, text);
}

/// UIKeyInput insertText: — called when user confirms text input.
/// For IME: called after setMarkedText when user confirms (like macOS insertText:replacementRange:).
/// For direct typing: called for each character (like macOS interpretKeyEvents → insertText:).
unsafe extern "C" fn ios_insert_text(_this: *mut AnyObject, _sel: Sel, text: *mut AnyObject) {
    ios_log("insertText: called");
    if text.is_null() { ios_log("insertText: null text"); return; }

    // End IME composition and clear preedit buffer
    crate::display::IME_COMPOSING.store(false, std::sync::atomic::Ordering::Relaxed);
    crate::display::IME_CONVERTING.store(false, std::sync::atomic::Ordering::Relaxed);
    PREEDIT_BUF.with(|b| b.borrow_mut().clear());

    // Try NSString first, then NSAttributedString (UIKit may pass either for IME)
    let utf8: *const std::os::raw::c_char = msg_send![&*text, UTF8String];
    let rust_str = if !utf8.is_null() {
        match std::ffi::CStr::from_ptr(utf8).to_str() {
            Ok(s) => s.to_string(),
            Err(_) => { ios_log("insertText: utf8 decode error"); return; },
        }
    } else {
        // Try NSAttributedString → string
        let nsstring: *mut AnyObject = msg_send![&*text, string];
        if nsstring.is_null() { ios_log("insertText: null utf8 and null .string"); return; }
        let utf8b: *const std::os::raw::c_char = msg_send![nsstring, UTF8String];
        if utf8b.is_null() { ios_log("insertText: null utf8b"); return; }
        match std::ffi::CStr::from_ptr(utf8b).to_str() {
            Ok(s) => s.to_string(),
            Err(_) => { ios_log("insertText: utf8b decode error"); return; },
        }
    };
    if rust_str.is_empty() { ios_log("insertText: empty string"); return; }

    let x11_id = get_focused_x11_window();
    ios_log(&format!("insertText: '{}' x11=0x{:08x}", rust_str.escape_debug(), x11_id));
    if x11_id == 0 { return; }
    let time = get_timestamp();

    // Single ASCII character: send as KeyPress/KeyRelease (like macOS insertText for ASCII)
    // Skip if this exact keycode was already sent by pressesBegan (hardware keyboard).
    // Software keyboard romaji: pressesBegan is NOT called, so LAST_HW_KEYCODE=0 and we send.
    if rust_str.len() == 1 && rust_str.as_bytes()[0] < 0x80 {
        let ch = rust_str.as_bytes()[0];
        let (keycode, state) = ascii_to_x11_keycode_state(ch);
        // Swap LAST_HW_KEYCODE atomically: if it matches, pressesBegan already sent this key.
        let hw_kc = LAST_HW_KEYCODE.swap(0, Ordering::Relaxed);
        if hw_kc != 0 && hw_kc == keycode {
            // Hardware keyboard: pressesBegan already sent KeyPress; skip to avoid double-send.
            return;
        }
        if keycode != 0 {
            send_display_event(DisplayEvent::KeyPress {
                window: x11_id, keycode, state, time,
            });
            send_display_event(DisplayEvent::KeyRelease {
                window: x11_id, keycode, state, time,
            });
        }
        return;
    }

    // Non-ASCII (IME kanji, emoji, etc.) → ImeCommit (like macOS insertText for CJK)
    // Suppress next KeyRelease — ImeCommit already handles the text (same as macOS line 549)
    crate::display::SUPPRESS_NEXT_KEYUP.store(true, std::sync::atomic::Ordering::Relaxed);
    info!("iOS insertText: '{}' for window 0x{:08x}", rust_str, x11_id);
    send_display_event(DisplayEvent::ImeCommit {
        window: x11_id,
        text: rust_str,
    });
}

// === UITextInput method implementations ===
// These mirror macOS NSTextInputClient methods for IME composition support.

/// setMarkedText:selectedRange: — called by iOS IME during composition.
/// This is the iOS equivalent of macOS setMarkedText:selectedRange:replacementRange:.
/// Sends ImePreeditDraw to show preedit text inline in xterm.
unsafe extern "C" fn ios_set_marked_text(_this: *mut AnyObject, _sel: Sel, text: *mut AnyObject, _sel_range: NSRange) {
    ios_log("setMarkedText: called");
    // Extract preedit text (may be NSString or NSAttributedString, same as macOS line 636-649)
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
        // Preedit cleared (e.g., BS deleted all preedit chars) — end composition (macOS line 653-656)
        crate::display::IME_COMPOSING.store(false, std::sync::atomic::Ordering::Relaxed);
        crate::display::IME_CONVERTING.store(false, std::sync::atomic::Ordering::Relaxed);
    } else {
        // Active composition (macOS line 658)
        crate::display::IME_COMPOSING.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    if let Some(s) = preedit_str {
        if !s.is_empty() {
            // Update preedit buffer so textInRange: returns correct text to UIKit.
            // UIKit needs this to call insertText(prev_preedit) before switching to new character.
            PREEDIT_BUF.with(|b| *b.borrow_mut() = s.clone());
            let x11_id = get_focused_x11_window();
            if x11_id != 0 {
                ios_log(&format!("setMarkedText: preedit='{}' x11=0x{:08x}", s, x11_id));
                info!("iOS setMarkedText: preedit='{}' (sent to X11)", s);
                send_display_event(DisplayEvent::ImePreeditDraw {
                    window: x11_id,
                    text: s,
                    cursor_pos: 0,
                });
            }
        } else {
            PREEDIT_BUF.with(|b| b.borrow_mut().clear());
        }
    } else {
        PREEDIT_BUF.with(|b| b.borrow_mut().clear());
    }
}

/// unmarkText — called when iOS ends composition without committing.
/// Same as macOS unmarkText (line 686-689).
unsafe extern "C" fn ios_unmark_text(_this: *mut AnyObject, _sel: Sel) {
    crate::display::IME_COMPOSING.store(false, std::sync::atomic::Ordering::Relaxed);
    crate::display::IME_CONVERTING.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// markedTextRange — returns UITextRange for current marked text, nil if not composing.
unsafe extern "C" fn ios_marked_text_range(_this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    if crate::display::IME_COMPOSING.load(std::sync::atomic::Ordering::Relaxed) {
        // Return a non-nil UITextRange to indicate active composition
        // Use UITextRange from beginningOfDocument to endOfDocument as a simple placeholder
        ios_create_text_range(0, 1)
    } else {
        std::ptr::null_mut()
    }
}

/// selectedTextRange — returns UITextRange for current selection (stub: empty range at position 0).
unsafe extern "C" fn ios_selected_text_range(_this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    ios_log("selectedTextRange: called");
    ios_create_text_range(0, 0)
}

/// setSelectedTextRange: — no-op (we don't track selection in X11 text buffer).
unsafe extern "C" fn ios_set_selected_text_range(_this: *mut AnyObject, _sel: Sel, _range: *mut AnyObject) {}

/// Generic nil-returning stub for optional UITextInput properties.
unsafe extern "C" fn ios_return_nil(_this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    std::ptr::null_mut()
}

/// Generic no-op setter stub.
unsafe extern "C" fn ios_set_noop(_this: *mut AnyObject, _sel: Sel, _value: *mut AnyObject) {}

/// textInRange: — return preedit buffer text (so UIKit knows what's in the marked range).
/// Without this, UIKit won't call insertText for previous kana when switching characters.
unsafe extern "C" fn ios_text_in_range(_this: *mut AnyObject, _sel: Sel, _range: *mut AnyObject) -> *mut AnyObject {
    let text = PREEDIT_BUF.with(|b| b.borrow().clone());
    let ns = NSString::from_str(&text);
    let ptr: *mut AnyObject = &*ns as *const _ as *mut AnyObject;
    let _: *mut AnyObject = msg_send![ptr, retain];
    ptr
}

/// replaceRange:withText: — no-op (we don't maintain a text buffer, text goes via insertText).
unsafe extern "C" fn ios_replace_range(_this: *mut AnyObject, _sel: Sel, _range: *mut AnyObject, _text: *mut AnyObject) {}

/// beginningOfDocument — return a UITextPosition stub.
unsafe extern "C" fn ios_beginning_of_document(_this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    ios_create_text_position(0)
}

/// endOfDocument — return a UITextPosition stub.
unsafe extern "C" fn ios_end_of_document(_this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    ios_create_text_position(0)
}

/// positionFromPosition:offset: — return a UITextPosition stub.
unsafe extern "C" fn ios_position_from_offset(_this: *mut AnyObject, _sel: Sel, _position: *mut AnyObject, _offset: i64) -> *mut AnyObject {
    ios_create_text_position(0)
}

/// positionFromPosition:inDirection:offset: — stub.
unsafe extern "C" fn ios_position_from_direction(_this: *mut AnyObject, _sel: Sel, _position: *mut AnyObject, _direction: i64, _offset: i64) -> *mut AnyObject {
    ios_create_text_position(0)
}

/// textRangeFromPosition:toPosition: — stub.
unsafe extern "C" fn ios_text_range_from_positions(_this: *mut AnyObject, _sel: Sel, _from: *mut AnyObject, _to: *mut AnyObject) -> *mut AnyObject {
    ios_create_text_range(0, 0)
}

/// comparePosition:toPosition: — stub (return NSOrderedSame = 0).
unsafe extern "C" fn ios_compare_position(_this: *mut AnyObject, _sel: Sel, _pos1: *mut AnyObject, _pos2: *mut AnyObject) -> i64 {
    0 // NSOrderedSame
}

/// offsetFromPosition:toPosition: — stub (return 0).
unsafe extern "C" fn ios_offset_from_position(_this: *mut AnyObject, _sel: Sel, _from: *mut AnyObject, _to: *mut AnyObject) -> i64 {
    0
}

/// inputDelegate getter — return stored delegate.
unsafe extern "C" fn ios_get_input_delegate(this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    let ivar = (*this).class().instance_variable(c"_inputDelegate").unwrap();
    *ivar.load::<*mut AnyObject>(&*this)
}

/// inputDelegate setter — store delegate.
unsafe extern "C" fn ios_set_input_delegate(this: *mut AnyObject, _sel: Sel, delegate: *mut AnyObject) {
    let ivar = (*this).class().instance_variable(c"_inputDelegate").unwrap();
    *ivar.load_mut::<*mut AnyObject>(&mut *this) = delegate;
}

/// tokenizer — return UITextInputStringTokenizer (default tokenizer).
unsafe extern "C" fn ios_tokenizer(this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    let ivar = (*this).class().instance_variable(c"_tokenizer").unwrap();
    let existing = *ivar.load::<*mut AnyObject>(&*this);
    if !existing.is_null() { return existing; }
    // Create default tokenizer
    let cls = objc2::class!(UITextInputStringTokenizer);
    let tokenizer: *mut AnyObject = msg_send![cls, alloc];
    let tokenizer: *mut AnyObject = msg_send![tokenizer, initWithTextInput: this];
    *ivar.load_mut::<*mut AnyObject>(&mut *this) = tokenizer;
    tokenizer
}

/// firstRectForRange: — return rect at IME cursor position (for candidate window placement).
/// Uses IME_SPOT_X/Y from mod.rs (same as macOS firstRectForCharacterRange:actualRange:).
unsafe extern "C" fn ios_first_rect_for_range(_this: *mut AnyObject, _sel: Sel, _range: *mut AnyObject) -> CGRectVal {
    let spot_x = crate::display::IME_SPOT_X.load(std::sync::atomic::Ordering::Relaxed) as f64;
    let spot_y = crate::display::IME_SPOT_Y.load(std::sync::atomic::Ordering::Relaxed) as f64;
    let line_h = crate::display::IME_SPOT_LINE_H.load(std::sync::atomic::Ordering::Relaxed) as f64;
    CGRectVal { origin: CGPointVal { x: spot_x, y: spot_y }, size: CGSizeVal { width: 10.0, height: line_h } }
}

/// caretRectForPosition: — return rect at IME cursor position.
unsafe extern "C" fn ios_caret_rect_for_position(_this: *mut AnyObject, _sel: Sel, _position: *mut AnyObject) -> CGRectVal {
    let spot_x = crate::display::IME_SPOT_X.load(std::sync::atomic::Ordering::Relaxed) as f64;
    let spot_y = crate::display::IME_SPOT_Y.load(std::sync::atomic::Ordering::Relaxed) as f64;
    let line_h = crate::display::IME_SPOT_LINE_H.load(std::sync::atomic::Ordering::Relaxed) as f64;
    CGRectVal { origin: CGPointVal { x: spot_x, y: spot_y }, size: CGSizeVal { width: 2.0, height: line_h } }
}

/// selectionRectsForRange: — return empty array (no selection rects).
unsafe extern "C" fn ios_selection_rects(_this: *mut AnyObject, _sel: Sel, _range: *mut AnyObject) -> *mut AnyObject {
    let arr: *mut AnyObject = msg_send![objc2::class!(NSArray), array];
    arr
}

/// closestPositionToPoint: — stub, return position 0.
unsafe extern "C" fn ios_closest_position_to_point(_this: *mut AnyObject, _sel: Sel, _point: CGPointVal) -> *mut AnyObject {
    ios_create_text_position(0)
}

/// closestPositionToPoint:withinRange: — stub, return position 0.
unsafe extern "C" fn ios_closest_position_to_point_in_range(_this: *mut AnyObject, _sel: Sel, _point: CGPointVal, _range: *mut AnyObject) -> *mut AnyObject {
    ios_create_text_position(0)
}

/// characterRangeAtPoint: — stub, return nil.
unsafe extern "C" fn ios_character_range_at_point(_this: *mut AnyObject, _sel: Sel, _point: CGPointVal) -> *mut AnyObject {
    std::ptr::null_mut()
}

/// baseWritingDirectionForPosition:inDirection: — return LeftToRight (0).
unsafe extern "C" fn ios_base_writing_direction(_this: *mut AnyObject, _sel: Sel, _pos: *mut AnyObject, _direction: i64) -> i64 {
    0 // UITextWritingDirectionLeftToRight = NSWritingDirectionLeftToRight = 0
}

/// setBaseWritingDirection:forRange: — no-op.
unsafe extern "C" fn ios_set_base_writing_direction(_this: *mut AnyObject, _sel: Sel, _direction: i64, _range: *mut AnyObject) {}

/// positionWithinRange:farthestInDirection: — stub.
unsafe extern "C" fn ios_position_within_range(_this: *mut AnyObject, _sel: Sel, _range: *mut AnyObject, _direction: i64) -> *mut AnyObject {
    ios_create_text_position(0)
}

/// characterRangeByExtendingPosition:inDirection: — stub.
unsafe extern "C" fn ios_character_range_by_extending(_this: *mut AnyObject, _sel: Sel, _pos: *mut AnyObject, _direction: i64) -> *mut AnyObject {
    ios_create_text_range(0, 0)
}

// --- CG value types for UITextInput geometry methods ---
use objc2::encode::{Encode, Encoding};

#[repr(C)]
#[derive(Copy, Clone)]
struct CGPointVal { x: f64, y: f64 }
unsafe impl Encode for CGPointVal {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Copy, Clone)]
struct CGSizeVal { width: f64, height: f64 }
unsafe impl Encode for CGSizeVal {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Copy, Clone)]
struct CGRectVal { origin: CGPointVal, size: CGSizeVal }
unsafe impl Encode for CGRectVal {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPointVal::ENCODING, CGSizeVal::ENCODING]);
}

#[repr(C)]
#[derive(Copy, Clone)]
struct NSRange { location: usize, length: usize }
unsafe impl Encode for NSRange {
    const ENCODING: Encoding = Encoding::Struct("_NSRange", &[usize::ENCODING, usize::ENCODING]);
}

// --- UITextPosition / UITextRange helper creation ---

/// Create a UITextPosition (opaque position object for UITextInput).
/// We use a tag-based approach: store the offset as the object's tag.
fn ios_create_text_position(offset: i64) -> *mut AnyObject {
    unsafe {
        let cls = objc2::class!(UITextPosition);
        let obj: *mut AnyObject = msg_send![cls, alloc];
        let obj: *mut AnyObject = msg_send![obj, init];
        obj
    }
}

/// Create a UITextRange (from position to position).
/// Returns a simple UITextRange via UITextRange subclass or the base class.
fn ios_create_text_range(start: i64, length: i64) -> *mut AnyObject {
    unsafe {
        // UITextRange is abstract — we need to create positions and use them.
        // For our stub purposes, we return a minimal valid object.
        // iOS will accept this for composition tracking.
        let start_pos = ios_create_text_position(start);
        let end_pos = ios_create_text_position(start + length);
        // Use the concrete PSLXTextRange class
        let cls = get_text_range_class();
        let obj: *mut AnyObject = msg_send![cls, alloc];
        let obj: *mut AnyObject = msg_send![obj, init];
        // Store start and end positions as ivars
        let start_ivar = (*obj).class().instance_variable(c"_start").unwrap();
        *start_ivar.load_mut::<*mut AnyObject>(&mut *obj) = start_pos;
        let end_ivar = (*obj).class().instance_variable(c"_end").unwrap();
        *end_ivar.load_mut::<*mut AnyObject>(&mut *obj) = end_pos;
        let empty_ivar = (*obj).class().instance_variable(c"_isEmpty").unwrap();
        *empty_ivar.load_mut::<u8>(&mut *obj) = if length == 0 { 1 } else { 0 };
        obj
    }
}

/// Register PSLXTextRange — a concrete UITextRange subclass.
/// UITextRange is abstract and requires `start` and `end` properties.
fn get_text_range_class() -> &'static AnyClass {
    use std::sync::Once;
    static mut CLASS: *const AnyClass = std::ptr::null();
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let superclass = objc2::class!(UITextRange);
        let mut builder = ClassBuilder::new(c"PSLXTextRange", superclass)
            .expect("Failed to create PSLXTextRange class");

        builder.add_ivar::<*mut AnyObject>(c"_start");
        builder.add_ivar::<*mut AnyObject>(c"_end");
        builder.add_ivar::<u8>(c"_isEmpty");  // 1 = empty (cursor), 0 = has selection

        extern "C" {
            fn class_addMethod(
                cls: *mut c_void, sel: Sel, imp: *const c_void, types: *const std::ffi::c_char,
            ) -> bool;
        }

        let raw_cls = builder.register() as *const AnyClass as *mut c_void;
        unsafe {
            class_addMethod(raw_cls, objc2::sel!(start),
                text_range_start as *const c_void, c"@@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(end),
                text_range_end as *const c_void, c"@@:".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(isEmpty),
                text_range_is_empty as *const c_void, c"B@:".as_ptr() as _);
            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

unsafe extern "C" fn text_range_start(this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    let ivar = (*this).class().instance_variable(c"_start").unwrap();
    *ivar.load::<*mut AnyObject>(&*this)
}

unsafe extern "C" fn text_range_end(this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    let ivar = (*this).class().instance_variable(c"_end").unwrap();
    *ivar.load::<*mut AnyObject>(&*this)
}

unsafe extern "C" fn text_range_is_empty(this: *mut AnyObject, _sel: Sel) -> Bool {
    let ivar = (*this).class().instance_variable(c"_isEmpty").unwrap();
    let v = *ivar.load::<u8>(&*this);
    if v != 0 { Bool::YES } else { Bool::NO }
}

// --- Keyboard accessory view (Ctrl/Esc/Tab/arrow toolbar above software keyboard) ---

/// Helper: send a key event to the focused X11 window.
fn send_key_to_focused(keycode: u8, state: u16) {
    let x11_id = get_focused_x11_window();
    if x11_id == 0 { return; }
    let time = get_timestamp();
    send_display_event(DisplayEvent::KeyPress { window: x11_id, keycode, state, time });
    send_display_event(DisplayEvent::KeyRelease { window: x11_id, keycode, state, time });
}

// Ctrl modifier state = 4
const CTRL: u16 = 4;

unsafe extern "C" fn send_ctrl_c(_this: *mut AnyObject, _sel: Sel) { send_key_to_focused(16, CTRL); } // C = mac 8, X11=16
unsafe extern "C" fn send_ctrl_d(_this: *mut AnyObject, _sel: Sel) { send_key_to_focused(10, CTRL); } // D = mac 2, X11=10
unsafe extern "C" fn send_ctrl_z(_this: *mut AnyObject, _sel: Sel) { send_key_to_focused(14, CTRL); } // Z = mac 6, X11=14
unsafe extern "C" fn send_ctrl_a(_this: *mut AnyObject, _sel: Sel) { send_key_to_focused(8,  CTRL); } // A = mac 0, X11=8
unsafe extern "C" fn send_ctrl_e(_this: *mut AnyObject, _sel: Sel) { send_key_to_focused(22, CTRL); } // E = mac 14, X11=22
unsafe extern "C" fn send_ctrl_u(_this: *mut AnyObject, _sel: Sel) { send_key_to_focused(40, CTRL); } // U = mac 32, X11=40
unsafe extern "C" fn send_ctrl_l(_this: *mut AnyObject, _sel: Sel) { send_key_to_focused(45, CTRL); } // L = mac 37, X11=45
unsafe extern "C" fn send_esc(_this: *mut AnyObject, _sel: Sel)    { send_key_to_focused(61, 0); }    // Esc = mac 53, X11=61
unsafe extern "C" fn send_tab(_this: *mut AnyObject, _sel: Sel)    { send_key_to_focused(56, 0); }    // Tab = mac 48, X11=56
unsafe extern "C" fn send_arrow_up(_this: *mut AnyObject, _sel: Sel)    { send_key_to_focused(134, 0); } // ↑ = mac 126, X11=134
unsafe extern "C" fn send_arrow_down(_this: *mut AnyObject, _sel: Sel)  { send_key_to_focused(133, 0); } // ↓ = mac 125, X11=133
unsafe extern "C" fn send_arrow_left(_this: *mut AnyObject, _sel: Sel)  { send_key_to_focused(131, 0); } // ← = mac 123, X11=131
unsafe extern "C" fn send_arrow_right(_this: *mut AnyObject, _sel: Sel) { send_key_to_focused(132, 0); } // → = mac 124, X11=132

/// inputAccessoryView — creates the Ctrl/Esc/Tab/arrow toolbar shown above the keyboard.
/// Returns a UIScrollView containing shortcut buttons.
unsafe extern "C" fn input_accessory_view(_this: *mut AnyObject, _sel: Sel) -> *mut AnyObject {
    // Return cached accessory view (created once).
    static CACHED_VIEW: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
    let cached = *CACHED_VIEW.lock().unwrap();
    if cached != 0 { return cached as *mut AnyObject; }

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
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGPoint { x: f64, y: f64 }
    unsafe impl Encode for CGPoint {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }

    let height = 44.0_f64;
    let bar_frame = CGRect { origin: [0.0, 0.0], size: [0.0, height] };

    // Use UIScrollView so buttons can be scrolled if they don't all fit.
    let scroll: *mut AnyObject = msg_send![objc2::class!(UIScrollView), alloc];
    let scroll: *mut AnyObject = msg_send![scroll, initWithFrame: bar_frame];
    // Dark toolbar background
    let dark: *mut AnyObject = msg_send![objc2::class!(UIColor),
        colorWithRed: 0.18_f64 green: 0.18_f64 blue: 0.18_f64 alpha: 1.0_f64];
    let _: () = msg_send![scroll, setBackgroundColor: dark];
    let _: () = msg_send![scroll, setShowsHorizontalScrollIndicator: false];

    // Button definitions: (title, selector_string)
    let buttons: &[(&str, objc2::runtime::Sel)] = &[
        ("^C",  objc2::sel!(sendCtrlC)),
        ("^D",  objc2::sel!(sendCtrlD)),
        ("^Z",  objc2::sel!(sendCtrlZ)),
        ("^A",  objc2::sel!(sendCtrlA)),
        ("^E",  objc2::sel!(sendCtrlE)),
        ("^U",  objc2::sel!(sendCtrlU)),
        ("^L",  objc2::sel!(sendCtrlL)),
        ("Esc", objc2::sel!(sendEsc)),
        ("Tab", objc2::sel!(sendTab)),
        ("↑",   objc2::sel!(sendArrowUp)),
        ("↓",   objc2::sel!(sendArrowDown)),
        ("←",   objc2::sel!(sendArrowLeft)),
        ("→",   objc2::sel!(sendArrowRight)),
    ];

    let btn_w = 54.0_f64;
    let btn_gap = 4.0_f64;
    let btn_h = height - 8.0;
    let btn_y = 4.0;
    let white: *mut AnyObject = msg_send![objc2::class!(UIColor), whiteColor];
    let white_cg: *const std::ffi::c_void = msg_send![white, CGColor];
    let btn_bg: *mut AnyObject = msg_send![objc2::class!(UIColor),
        colorWithRed: 0.30_f64 green: 0.30_f64 blue: 0.30_f64 alpha: 1.0_f64];
    let btn_bg_cg: *const std::ffi::c_void = msg_send![btn_bg, CGColor];

    // We need a view to act as the button target (must be a PSLXView for our selectors).
    // Create a thin proxy PSLXView that is NOT displayed but handles the button actions.
    let view_cls = get_pslx_view_class();
    let proxy: *mut AnyObject = msg_send![view_cls, alloc];
    let proxy_frame = CGRect { origin: [0.0, 0.0], size: [0.0, 0.0] };
    let proxy: *mut AnyObject = msg_send![proxy, initWithFrame: proxy_frame];
    // Retain so it stays alive
    let _: *mut AnyObject = msg_send![proxy, retain];

    for (i, (title, sel)) in buttons.iter().enumerate() {
        let x = i as f64 * (btn_w + btn_gap) + btn_gap;
        let btn_frame = CGRect { origin: [x, btn_y], size: [btn_w, btn_h] };

        // UIButtonTypeSystem = 1
        let btn: *mut AnyObject = msg_send![objc2::class!(UIButton), buttonWithType: 1i64];
        let _: () = msg_send![btn, setFrame: btn_frame];

        let title_ns = NSString::from_str(title);
        // UIControlStateNormal = 0
        let _: () = msg_send![btn, setTitle: &*title_ns forState: 0u64];
        let _: () = msg_send![btn, setTitleColor: white forState: 0u64];

        // Round corners
        let layer: *mut AnyObject = msg_send![btn, layer];
        let _: () = msg_send![layer, setCornerRadius: 5.0_f64];
        let _: () = msg_send![layer, setBackgroundColor: btn_bg_cg];

        // Target-action: UIControlEventTouchUpInside = 64
        let _: () = msg_send![btn, addTarget: proxy action: *sel forControlEvents: 64u64];
        let _: () = msg_send![scroll, addSubview: btn];
    }

    // Set content size so scrolling works
    let total_w = buttons.len() as f64 * (btn_w + btn_gap) + btn_gap;
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGSize { w: f64, h: f64 }
    unsafe impl Encode for CGSize {
        const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
    }
    let content_size = CGSize { w: total_w, h: height };
    let _: () = msg_send![scroll, setContentSize: content_size];

    let _: *mut AnyObject = msg_send![scroll, retain];
    *CACHED_VIEW.lock().unwrap() = scroll as usize;
    scroll
}

/// UIKeyInput deleteBackward — Backspace key (software keyboard)
unsafe extern "C" fn delete_backward(this: *mut AnyObject, _sel: Sel) {
    let x11_id = view_x11_id(this);
    ios_log(&format!("deleteBackward: x11=0x{:08x}", x11_id));
    if x11_id == 0 { return; }
    let time = get_timestamp();
    // macOS keycode 51 = Backspace → X11 keycode = 51 + 8 = 59
    send_display_event(DisplayEvent::KeyPress {
        window: x11_id, keycode: 59, state: 0, time,
    });
    send_display_event(DisplayEvent::KeyRelease {
        window: x11_id, keycode: 59, state: 0, time,
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

// --- Title bar with traffic light buttons (red/yellow/green) ---

const IOS_TITLE_BAR_HEIGHT: f64 = 36.0;

/// Create a title bar UIView with red (close), yellow (minimize), green (fullscreen) buttons.
/// Returns the title bar view (retained). Also sets window title text.
unsafe fn create_title_bar(width: f64, title: &str, x11_id: u32) -> *mut AnyObject {
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

    let bar_frame = CGRect { origin: [0.0, 0.0], size: [width, IOS_TITLE_BAR_HEIGHT] };
    let bar: *mut AnyObject = msg_send![objc2::class!(UIView), alloc];
    let bar: *mut AnyObject = msg_send![bar, initWithFrame: bar_frame];

    // Dark background (#2D2D2D)
    let bg_color: *mut AnyObject = msg_send![objc2::class!(UIColor),
        colorWithRed: 0.176_f64 green: 0.176_f64 blue: 0.176_f64 alpha: 1.0_f64];
    let _: () = msg_send![bar, setBackgroundColor: bg_color];

    // Traffic light button target handler class
    let handler_cls = get_title_bar_handler_class();
    let handler: *mut AnyObject = msg_send![handler_cls, alloc];
    let handler: *mut AnyObject = msg_send![handler, init];
    // Store x11_id in handler via ivar
    if let Some(ivar) = (*handler).class().instance_variable(c"x11WindowId") {
        *ivar.load_mut::<u32>(&mut *handler) = x11_id;
    }
    let _: *mut AnyObject = msg_send![handler, retain];

    // Button definitions: (color_r, color_g, color_b, selector)
    let buttons: &[(f64, f64, f64, Sel)] = &[
        (0.937, 0.325, 0.314, objc2::sel!(closeWindow:)),    // Red — close
        (0.988, 0.741, 0.176, objc2::sel!(minimizeWindow:)), // Yellow — minimize
        (0.157, 0.788, 0.263, objc2::sel!(fullscreenWindow:)), // Green — fullscreen
    ];

    let btn_size = 14.0_f64;
    let btn_gap = 8.0_f64;
    let start_x = 12.0_f64;
    let btn_y = (IOS_TITLE_BAR_HEIGHT - btn_size) / 2.0;

    for (i, (r, g, b, sel)) in buttons.iter().enumerate() {
        let x = start_x + i as f64 * (btn_size + btn_gap);
        let btn_frame = CGRect { origin: [x, btn_y], size: [btn_size, btn_size] };

        // UIButtonTypeCustom = 0
        let btn: *mut AnyObject = msg_send![objc2::class!(UIButton), buttonWithType: 0i64];
        let _: () = msg_send![btn, setFrame: btn_frame];

        let color: *mut AnyObject = msg_send![objc2::class!(UIColor),
            colorWithRed: *r green: *g blue: *b alpha: 1.0_f64];
        let _: () = msg_send![btn, setBackgroundColor: color];

        // Round circle
        let layer: *mut AnyObject = msg_send![btn, layer];
        let _: () = msg_send![layer, setCornerRadius: btn_size / 2.0];
        let _: () = msg_send![layer, setMasksToBounds: true];

        // Target-action: UIControlEventTouchUpInside = 64
        let _: () = msg_send![btn, addTarget: handler action: *sel forControlEvents: 64u64];
        let _: () = msg_send![bar, addSubview: btn];
    }

    // Title label
    let label_x = start_x + 3.0 * (btn_size + btn_gap) + 8.0;
    let label_frame = CGRect { origin: [label_x, 0.0], size: [width - label_x - 8.0, IOS_TITLE_BAR_HEIGHT] };
    let label: *mut AnyObject = msg_send![objc2::class!(UILabel), alloc];
    let label: *mut AnyObject = msg_send![label, initWithFrame: label_frame];
    let title_ns = NSString::from_str(title);
    let _: () = msg_send![label, setText: &*title_ns];
    let white: *mut AnyObject = msg_send![objc2::class!(UIColor),
        colorWithRed: 0.85_f64 green: 0.85_f64 blue: 0.85_f64 alpha: 1.0_f64];
    let _: () = msg_send![label, setTextColor: white];
    let font: *mut AnyObject = msg_send![objc2::class!(UIFont), systemFontOfSize: 13.0_f64];
    let _: () = msg_send![label, setFont: font];
    // Tag = 100 so we can find it later to update title
    let _: () = msg_send![label, setTag: 100i64];
    let _: () = msg_send![bar, addSubview: label];

    let _: *mut AnyObject = msg_send![bar, retain];
    bar
}

/// ObjC class for title bar button actions.
fn get_title_bar_handler_class() -> &'static AnyClass {
    use std::sync::Once;
    static mut CLASS: *const AnyClass = std::ptr::null();
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let superclass = objc2::class!(NSObject);
        let mut builder = ClassBuilder::new(c"PSLXTitleBarHandler", superclass)
            .expect("Failed to create PSLXTitleBarHandler class");

        builder.add_ivar::<u32>(c"x11WindowId");

        extern "C" {
            fn class_addMethod(
                cls: *mut c_void, sel: Sel, imp: *const c_void, types: *const std::ffi::c_char,
            ) -> bool;
        }

        let raw_cls = builder.register() as *const AnyClass as *mut c_void;
        unsafe {
            class_addMethod(raw_cls, objc2::sel!(closeWindow:),
                title_bar_close as *const c_void, c"v@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(minimizeWindow:),
                title_bar_minimize as *const c_void, c"v@:@".as_ptr() as _);
            class_addMethod(raw_cls, objc2::sel!(fullscreenWindow:),
                title_bar_fullscreen as *const c_void, c"v@:@".as_ptr() as _);
            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

/// Red button — close: send DestroyNotify to X11 client
unsafe extern "C" fn title_bar_close(this: *mut AnyObject, _sel: Sel, _sender: *mut AnyObject) {
    let x11_id = if let Some(ivar) = (*this).class().instance_variable(c"x11WindowId") {
        *ivar.load::<u32>(&*this)
    } else { return; };
    ios_log(&format!("title_bar_close: x11=0x{:08X}", x11_id));
    // Send WM_DELETE_WINDOW ClientMessage (graceful close)
    send_display_event(DisplayEvent::WindowCloseRequested { window: x11_id });
}

/// Yellow button — minimize: hide the UIWindow (Stage Manager keeps it in the shelf)
unsafe extern "C" fn title_bar_minimize(this: *mut AnyObject, _sel: Sel, _sender: *mut AnyObject) {
    let x11_id = if let Some(ivar) = (*this).class().instance_variable(c"x11WindowId") {
        *ivar.load::<u32>(&*this)
    } else { return; };
    ios_log(&format!("title_bar_minimize: x11=0x{:08X}", x11_id));
    // Find the UIWindow for this X11 window and hide it
    WINDOWS.with(|w| {
        let ws = w.borrow();
        for info in ws.values() {
            if info.x11_id == x11_id && !info.ui_window.is_null() {
                let _: () = msg_send![info.ui_window, setHidden: true];
                break;
            }
        }
    });
}

/// Green button — toggle fullscreen
unsafe extern "C" fn title_bar_fullscreen(this: *mut AnyObject, _sel: Sel, _sender: *mut AnyObject) {
    let x11_id = if let Some(ivar) = (*this).class().instance_variable(c"x11WindowId") {
        *ivar.load::<u32>(&*this)
    } else { return; };
    ios_log(&format!("title_bar_fullscreen: x11=0x{:08X}", x11_id));
    toggle_fullscreen(x11_id);
}

/// Toggle fullscreen: hide/show title bar, resize content to fill window.
fn toggle_fullscreen(x11_id: u32) {
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

    WINDOWS.with(|w| {
        let mut ws = w.borrow_mut();
        for info in ws.values_mut() {
            if info.x11_id != x11_id { continue; }
            info.is_fullscreen = !info.is_fullscreen;
            let fs = info.is_fullscreen;

            unsafe {
                if !info.title_bar.is_null() {
                    // Hide/show title bar
                    let _: () = msg_send![info.title_bar, setHidden: fs];
                }

                if !info.ui_view.is_null() {
                    // Reposition content view: fullscreen → y=0, normal → y=title_bar_height
                    let y_offset = if fs { 0.0 } else { IOS_TITLE_BAR_HEIGHT };
                    let win_bounds: CGRect = if !info.ui_window.is_null() {
                        msg_send![info.ui_window, bounds]
                    } else {
                        CGRect { origin: [0.0, 0.0], size: [info.width as f64, info.height as f64 + IOS_TITLE_BAR_HEIGHT] }
                    };
                    let content_h = win_bounds.size[1] - y_offset;
                    let content_w = win_bounds.size[0];
                    let view_frame = CGRect {
                        origin: [0.0, y_offset],
                        size: [content_w, content_h],
                    };
                    let _: () = msg_send![info.ui_view, setFrame: view_frame];
                }
            }

            ios_log(&format!("toggle_fullscreen: x11=0x{:08X} fullscreen={}", x11_id, fs));
            break;
        }
    });
}

// --- Touch handling → X11 mouse events ---

/// Height of the drag handle area at the top of each window (points = X11 pixels at contentsScale=1.0).
const TITLE_BAR_HEIGHT: i16 = 30;

/// Touch mode state machine:
/// - Undecided: just touched, waiting to see if it's a tap, scroll, or drag
/// - Scrolling: finger moved >10px before long-press threshold → emit Button4/5
/// - Dragging: long press detected (>300ms) → emit ButtonPress + MotionNotify
#[derive(Clone, Copy, PartialEq)]
enum TouchMode { Undecided, Scrolling, Dragging }

thread_local! {
    static TOUCH_MODE: std::cell::Cell<TouchMode> = const { std::cell::Cell::new(TouchMode::Undecided) };
    static TOUCH_START_XY: std::cell::Cell<(i16, i16)> = const { std::cell::Cell::new((0, 0)) };
    static TOUCH_START_TIME: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static TOUCH_SCROLL_Y: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    static TOUCH_SCROLL_HX: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

const TOUCH_MOVE_THRESHOLD: i16 = 10; // pixels to distinguish scroll from tap
const TOUCH_LONG_PRESS_MS: u32 = 300; // ms before long-press = drag mode

unsafe extern "C" fn touches_began(this: *mut AnyObject, _sel: Sel, touches: *mut AnyObject, event: *mut AnyObject) {
    let touch = get_first_touch(touches);
    if touch.is_null() { return; }
    let (x, y) = touch_location_in_view(touch, this);
    let time = get_timestamp();

    // Detect mouse button from UIEvent.buttonMask (iOS 13.4+)
    let button: u8 = if !event.is_null() {
        let mask: i64 = msg_send![event, buttonMask];
        if mask & 2 != 0 { 3 } else if mask & 4 != 0 { 2 } else { 1 }
    } else { 1 };

    let x11_id = view_x11_id(this);
    if x11_id == 0 { return; }

    let _: () = msg_send![this, becomeFirstResponder];

    // If using mouse/trackpad (buttonMask != 0), go straight to drag mode
    let is_mouse = if !event.is_null() {
        let mask: i64 = msg_send![event, buttonMask];
        mask != 0
    } else { false };

    if is_mouse {
        TOUCH_MODE.with(|m| m.set(TouchMode::Dragging));
        GRAB_BUTTON.with(|gb| gb.set(button));
        GRAB_WINDOW.with(|gw| gw.set(Some(x11_id)));
        send_display_event(DisplayEvent::ButtonPress {
            window: x11_id, button, x, y, root_x: x, root_y: y, state: 0, time,
        });
        send_display_event(DisplayEvent::FocusIn { window: x11_id });
    } else {
        // Touch: start in undecided mode
        TOUCH_MODE.with(|m| m.set(TouchMode::Undecided));
        TOUCH_START_XY.with(|s| s.set((x, y)));
        TOUCH_START_TIME.with(|t| t.set(time));
        TOUCH_SCROLL_Y.with(|a| a.set(0));
        TOUCH_SCROLL_HX.with(|a| a.set(0));
        GRAB_BUTTON.with(|gb| gb.set(button));
        GRAB_WINDOW.with(|gw| gw.set(Some(x11_id)));
        send_display_event(DisplayEvent::FocusIn { window: x11_id });
    }
}

unsafe extern "C" fn touches_moved(this: *mut AnyObject, _sel: Sel, touches: *mut AnyObject, _event: *mut AnyObject) {
    let touch = get_first_touch(touches);
    if touch.is_null() { return; }
    let (x, y) = touch_location_in_view(touch, this);
    let time = get_timestamp();
    let x11_id = view_x11_id(this);
    if x11_id == 0 { return; }

    LAST_POINTER.with(|lp| lp.set((x, y)));

    let mode = TOUCH_MODE.with(|m| m.get());
    match mode {
        TouchMode::Undecided => {
            let (sx, sy) = TOUCH_START_XY.with(|s| s.get());
            let dx = (x - sx).abs();
            let dy = (y - sy).abs();
            let start_time = TOUCH_START_TIME.with(|t| t.get());
            let elapsed = time.wrapping_sub(start_time);

            if dx > TOUCH_MOVE_THRESHOLD || dy > TOUCH_MOVE_THRESHOLD {
                if elapsed < TOUCH_LONG_PRESS_MS {
                    // Moved quickly → scroll mode
                    TOUCH_MODE.with(|m| m.set(TouchMode::Scrolling));
                    // Emit scroll for initial movement
                    emit_touch_scroll(x11_id, x, y, sy - y, sx - x, time);
                } else {
                    // Moved after long press → drag mode
                    TOUCH_MODE.with(|m| m.set(TouchMode::Dragging));
                    let button = GRAB_BUTTON.with(|gb| gb.get());
                    send_display_event(DisplayEvent::ButtonPress {
                        window: x11_id, button, x: sx, y: sy,
                        root_x: sx, root_y: sy, state: 0, time,
                    });
                    // Send initial motion
                    let btn_state = match button { 1 => 0x100u16, 2 => 0x200, 3 => 0x400, _ => 0x100 };
                    send_display_event(DisplayEvent::MotionNotify {
                        window: x11_id, x, y, root_x: x, root_y: y, state: btn_state, time,
                    });
                }
            }
        }
        TouchMode::Scrolling => {
            // Continue scrolling — use delta from previous position
            let (sx, sy) = TOUCH_START_XY.with(|s| s.get());
            TOUCH_START_XY.with(|s| s.set((x, y)));
            emit_touch_scroll(x11_id, x, y, sy - y, sx - x, time);
        }
        TouchMode::Dragging => {
            let button = GRAB_BUTTON.with(|gb| gb.get());
            let btn_state = match button { 1 => 0x100u16, 2 => 0x200, 3 => 0x400, _ => 0x100 };
            send_display_event(DisplayEvent::MotionNotify {
                window: x11_id, x, y, root_x: x, root_y: y, state: btn_state, time,
            });
        }
    }
}

/// Convert touch scroll delta to Button4/5/6/7 events.
fn emit_touch_scroll(x11_id: u32, x: i16, y: i16, dy: i16, dx: i16, time: u32) {
    let threshold = 80; // pixels per scroll click — match finger movement 1:1

    // Vertical
    if dy != 0 {
        let accum = TOUCH_SCROLL_Y.with(|a| {
            let v = a.get() + dy as i32;
            a.set(v);
            v
        });
        if accum.abs() >= threshold {
            // Natural scrolling: swipe up (dy>0) → scroll down (Button5), swipe down → scroll up (Button4)
            let button = if accum > 0 { 5u8 } else { 4u8 };
            let clicks = (accum.abs() / threshold) as u32;
            TOUCH_SCROLL_Y.with(|a| a.set(accum % threshold));
            for _ in 0..clicks.min(5) {
                send_display_event(DisplayEvent::ButtonPress {
                    window: x11_id, button, x, y, root_x: x, root_y: y, state: 0, time,
                });
                send_display_event(DisplayEvent::ButtonRelease {
                    window: x11_id, button, x, y, root_x: x, root_y: y, state: 0, time,
                });
            }
        }
    }

    // Horizontal
    if dx != 0 {
        let accum = TOUCH_SCROLL_HX.with(|a| {
            let v = a.get() + dx as i32;
            a.set(v);
            v
        });
        if accum.abs() >= threshold {
            let button = if accum > 0 { 6u8 } else { 7u8 };
            let clicks = (accum.abs() / threshold) as u32;
            TOUCH_SCROLL_HX.with(|a| a.set(accum % threshold));
            for _ in 0..clicks.min(5) {
                send_display_event(DisplayEvent::ButtonPress {
                    window: x11_id, button, x, y, root_x: x, root_y: y, state: 0, time,
                });
                send_display_event(DisplayEvent::ButtonRelease {
                    window: x11_id, button, x, y, root_x: x, root_y: y, state: 0, time,
                });
            }
        }
    }
}

unsafe extern "C" fn touches_ended(this: *mut AnyObject, _sel: Sel, touches: *mut AnyObject, _event: *mut AnyObject) {
    let touch = get_first_touch(touches);
    if touch.is_null() { return; }
    let (x, y) = touch_location_in_view(touch, this);
    let time = get_timestamp();
    let x11_id = view_x11_id(this);
    GRAB_WINDOW.with(|gw| gw.set(None));
    if x11_id == 0 { return; }

    let mode = TOUCH_MODE.with(|m| m.get());
    let button = GRAB_BUTTON.with(|gb| gb.get());
    GRAB_BUTTON.with(|gb| gb.set(1));

    match mode {
        TouchMode::Undecided => {
            // No significant movement → tap = click
            send_display_event(DisplayEvent::ButtonPress {
                window: x11_id, button, x, y, root_x: x, root_y: y, state: 0, time,
            });
            send_display_event(DisplayEvent::ButtonRelease {
                window: x11_id, button, x, y, root_x: x, root_y: y,
                state: match button { 1 => 0x100u16, 2 => 0x200, 3 => 0x400, _ => 0x100 }, time,
            });
        }
        TouchMode::Scrolling => {
            // Scroll ended — nothing to send
        }
        TouchMode::Dragging => {
            let btn_state = match button { 1 => 0x100u16, 2 => 0x200, 3 => 0x400, _ => 0x100 };
            send_display_event(DisplayEvent::ButtonRelease {
                window: x11_id, button, x, y, root_x: x, root_y: y, state: btn_state, time,
            });
        }
    }

    TOUCH_MODE.with(|m| m.set(TouchMode::Undecided));
}

unsafe extern "C" fn touches_cancelled(this: *mut AnyObject, sel: Sel, touches: *mut AnyObject, event: *mut AnyObject) {
    touches_ended(this, sel, touches, event);
}

// --- Hardware keyboard (UIPress) ---

/// UIKit keyCode → macOS virtual keycode mapping for NON-TEXT keys only.
/// Text keys (a-z, 0-9, symbols) are handled by insertText: (like macOS interpretKeyEvents).
/// This only maps keys that don't produce text: arrows, Escape, function keys, Return, etc.
fn uikit_keycode_to_mac_nontext(uikit_kc: i64) -> Option<u8> {
    // UIKeyboardHIDUsage values → macOS kVK codes (non-text keys only)
    let mac = match uikit_kc {
        40 => 36,  // Return
        41 => 53,  // Escape
        // Note: Backspace (42) is handled by deleteBackward in UIKeyInput
        43 => 48,  // Tab
        // Arrow keys
        79 => 124, // RightArrow (macOS kVK=124, X11=124+8=132... wait, need correct mapping)
        80 => 123, // LeftArrow
        81 => 125, // DownArrow
        82 => 126, // UpArrow
        // Function keys
        58 => 122, // F1
        59 => 120, // F2
        60 => 99,  // F3
        61 => 118, // F4
        62 => 96,  // F5
        63 => 97,  // F6
        64 => 98,  // F7
        65 => 100, // F8
        66 => 101, // F9
        67 => 109, // F10
        68 => 103, // F11
        69 => 111, // F12
        // Home/End/PageUp/PageDown
        74 => 115, // Home
        75 => 116, // PageUp
        76 => 117, // Delete (forward)
        77 => 119, // End
        78 => 121, // PageDown
        _ => return None,
    };
    Some(mac)
}

/// Full UIKit keyCode → macOS virtual keycode mapping (all keys including text).
/// Used for KeyRelease only (to match KeyPress sent by insertText:).
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
        79 => 124, // RightArrow
        80 => 123, // LeftArrow
        81 => 125, // DownArrow
        82 => 126, // UpArrow
        _ => return None,
    };
    Some(mac)
}

unsafe extern "C" fn presses_began(this: *mut AnyObject, _sel: Sel, presses: *mut AnyObject, _event: *mut AnyObject) {
    let all: *mut AnyObject = msg_send![presses, allObjects];
    let count: usize = msg_send![all, count];
    let x11_id = view_x11_id(this);
    for i in 0..count {
        let press: *mut AnyObject = msg_send![all, objectAtIndex: i];
        let key: *mut AnyObject = msg_send![press, key];
        if key.is_null() { continue; }
        let uikit_kc: i64 = msg_send![key, keyCode];
        let modifier_flags: u64 = msg_send![key, modifierFlags];

        ios_log(&format!("pressesBegan: uikit_kc={} x11=0x{:08x}", uikit_kc, x11_id));
        if x11_id == 0 { continue; }
        let time = get_timestamp();

        // Suppress key events during IME composition (same as macOS doCommandBySelector check)
        if crate::display::IME_COMPOSING.load(std::sync::atomic::Ordering::Relaxed) {
            ios_log(&format!("pressesBegan: suppressed keycode {} during IME composition", uikit_kc));
            continue;
        }

        let mut state: u16 = 0;
        if modifier_flags & (1 << 17) != 0 { state |= 1; } // Shift
        if modifier_flags & (1 << 18) != 0 { state |= 4; } // Control
        if modifier_flags & (1 << 19) != 0 { state |= 8; } // Alt/Option
        if modifier_flags & (1 << 20) != 0 { state |= 64; } // Command → Mod4

        // Ctrl+key or Command+key combinations: iOS doesn't send these through insertText,
        // so we must handle them here (e.g., Ctrl+C, Ctrl+D, Ctrl+Z for terminal).
        let has_modifier = (state & (4 | 64)) != 0; // Control or Command

        if has_modifier {
            // With Control/Command modifier: use full keycode map (text keys too)
            if let Some(mac_kc) = uikit_keycode_to_mac(uikit_kc) {
                let keycode = mac_kc + 8;
                send_display_event(DisplayEvent::KeyPress {
                    window: x11_id, keycode, state, time,
                });
            }
        } else if uikit_kc == 42 {
            // Backspace — mac keycode 51 + 8 = 59
            send_display_event(DisplayEvent::KeyPress {
                window: x11_id, keycode: 59, state, time,
            });
        } else {
            // Use full keycode map: in the iOS simulator (and iPad with hardware keyboard),
            // text keys come through pressesBegan, NOT insertText:.
            // Set HARDWARE_KB_DETECTED so insertText: knows to skip ASCII to avoid double-send.
            if let Some(mac_kc) = uikit_keycode_to_mac(uikit_kc) {
                let keycode = mac_kc + 8;
                // Store keycode so insertText: can detect duplicate and skip.
                LAST_HW_KEYCODE.store(keycode, Ordering::Relaxed);
                send_display_event(DisplayEvent::KeyPress {
                    window: x11_id, keycode, state, time,
                });
            }
        }
    }
}

unsafe extern "C" fn presses_ended(this: *mut AnyObject, _sel: Sel, presses: *mut AnyObject, _event: *mut AnyObject) {
    let all: *mut AnyObject = msg_send![presses, allObjects];
    let count: usize = msg_send![all, count];
    let x11_id = view_x11_id(this);
    for i in 0..count {
        let press: *mut AnyObject = msg_send![all, objectAtIndex: i];
        let key: *mut AnyObject = msg_send![press, key];
        if key.is_null() { continue; }
        let uikit_kc: i64 = msg_send![key, keyCode];
        let modifier_flags: u64 = msg_send![key, modifierFlags];

        if x11_id == 0 { continue; }
        let time = get_timestamp();

        // Suppress KeyRelease during IME composition (same as macOS IME_COMPOSING check)
        if crate::display::IME_COMPOSING.load(std::sync::atomic::Ordering::Relaxed) {
            continue;
        }

        // Suppress the next KeyRelease after IME commit (same as macOS SUPPRESS_NEXT_KEYUP)
        if crate::display::SUPPRESS_NEXT_KEYUP.swap(false, std::sync::atomic::Ordering::Relaxed) {
            debug!("pressesEnded: suppressed keycode {} (SUPPRESS_NEXT_KEYUP)", uikit_kc);
            continue;
        }

        let mut state: u16 = 0;
        if modifier_flags & (1 << 17) != 0 { state |= 1; }
        if modifier_flags & (1 << 18) != 0 { state |= 4; }
        if modifier_flags & (1 << 19) != 0 { state |= 8; }
        if modifier_flags & (1 << 20) != 0 { state |= 64; }

        let has_modifier = (state & (4 | 64)) != 0;

        if has_modifier {
            if let Some(mac_kc) = uikit_keycode_to_mac(uikit_kc) {
                let keycode = mac_kc + 8;
                send_display_event(DisplayEvent::KeyRelease {
                    window: x11_id, keycode, state, time,
                });
            }
        } else if uikit_kc == 42 {
            // Backspace KeyRelease — mac keycode 51 + 8 = 59
            send_display_event(DisplayEvent::KeyRelease {
                window: x11_id, keycode: 59, state, time,
            });
        } else {
            if let Some(mac_kc) = uikit_keycode_to_mac(uikit_kc) {
                let keycode = mac_kc + 8;
                send_display_event(DisplayEvent::KeyRelease {
                    window: x11_id, keycode, state, time,
                });
            }
        }
    }
}

// --- Mouse/Pointer support (iPad with mouse/trackpad) ---

/// UIHoverGestureRecognizer handler — mouse/trackpad movement → X11 MotionNotify.
unsafe extern "C" fn handle_hover(this: *mut AnyObject, _sel: Sel, gesture: *mut AnyObject) {
    use objc2::encode::{Encode, Encoding};
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGPoint { x: f64, y: f64 }
    unsafe impl Encode for CGPoint {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }

    let loc: CGPoint = msg_send![gesture, locationInView: &*this];
    // X11 pixels = UIKit points (contentsScale=1.0), so no conversion needed.
    let x = loc.x as i16;
    let y = loc.y as i16;
    let time = get_timestamp();

    LAST_POINTER.with(|lp| lp.set((x, y)));

    // Per-window design: this PSLXView = one X11 window; coords are view-local = X11-local
    let x11_id = view_x11_id(this);
    if x11_id == 0 { return; }

    send_display_event(DisplayEvent::MotionNotify {
        window: x11_id, x, y,
        root_x: x, root_y: y, state: 0, time,
    });
}

/// UIPanGestureRecognizer handler for scroll wheel (mouse/trackpad scroll → X11 Button4/5).
unsafe extern "C" fn handle_scroll(this: *mut AnyObject, _sel: Sel, gesture: *mut AnyObject) {
    use objc2::encode::{Encode, Encoding};
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGPoint { x: f64, y: f64 }
    unsafe impl Encode for CGPoint {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }

    // Only handle changed state (ongoing scroll)
    let state: i64 = msg_send![gesture, state];
    if state != 1 && state != 2 { return; } // UIGestureRecognizerStateBegan=1, Changed=2

    let translation: CGPoint = msg_send![gesture, translationInView: &*this];
    // Reset translation to get delta per callback
    let zero = CGPoint { x: 0.0, y: 0.0 };
    let _: () = msg_send![gesture, setTranslation: zero inView: &*this];

    let loc: CGPoint = msg_send![gesture, locationInView: &*this];
    // X11 pixels = UIKit points (contentsScale=1.0), so no conversion needed.
    let x = loc.x as i16;
    let y = loc.y as i16;
    let time = get_timestamp();

    // Per-window design: this PSLXView = one X11 window; coords are view-local = X11-local
    let x11_id = view_x11_id(this);
    if x11_id == 0 { return; }

    // Convert pixel delta to scroll events (80px threshold like macOS)
    static SCROLL_ACCUM: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    let dy = translation.y as i32;
    let accum = SCROLL_ACCUM.fetch_add(dy, std::sync::atomic::Ordering::Relaxed) + dy;

    let threshold = 30; // Lower threshold for smoother scrolling
    if accum.abs() >= threshold {
        let button = if accum < 0 { 4u8 } else { 5u8 }; // 4=scroll up, 5=scroll down
        let clicks = (accum.abs() / threshold) as u32;
        SCROLL_ACCUM.store(accum % threshold, std::sync::atomic::Ordering::Relaxed);

        for _ in 0..clicks.min(5) {
            send_display_event(DisplayEvent::ButtonPress {
                window: x11_id, button,
                x, y, root_x: x, root_y: y,
                state: 0, time,
            });
            send_display_event(DisplayEvent::ButtonRelease {
                window: x11_id, button,
                x, y, root_x: x, root_y: y,
                state: 0, time,
            });
        }
    }
}

/// 2-finger touch pan → Button4/5 scroll emulation (jog dial).
/// Single finger = mouse drag (handled by touchesBegan/Moved).
/// Two fingers = scroll wheel emulation.
unsafe extern "C" fn handle_touch_scroll(this: *mut AnyObject, _sel: Sel, gesture: *mut AnyObject) {
    use objc2::encode::{Encode, Encoding};
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGPoint { x: f64, y: f64 }
    unsafe impl Encode for CGPoint {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }

    let state: i64 = msg_send![gesture, state];
    // Began=1, Changed=2, Ended/Cancelled=3,4
    if state == 3 || state == 4 {
        // Reset accumulator on gesture end
        TOUCH_SCROLL_ACCUM_Y.store(0, std::sync::atomic::Ordering::Relaxed);
        TOUCH_SCROLL_ACCUM_X.store(0, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    if state != 1 && state != 2 { return; }

    let translation: CGPoint = msg_send![gesture, translationInView: &*this];
    let zero = CGPoint { x: 0.0, y: 0.0 };
    let _: () = msg_send![gesture, setTranslation: zero inView: &*this];

    let loc: CGPoint = msg_send![gesture, locationInView: &*this];
    let x = loc.x as i16;
    let y = loc.y as i16;
    let time = get_timestamp();

    let x11_id = view_x11_id(this);
    if x11_id == 0 { return; }

    let threshold = 20; // pixels per scroll click — responsive like a jog dial

    // Vertical scroll (Button 4=up, 5=down)
    let dy = translation.y as i32;
    if dy != 0 {
        let accum = TOUCH_SCROLL_ACCUM_Y.fetch_add(dy, std::sync::atomic::Ordering::Relaxed) + dy;
        if accum.abs() >= threshold {
            // Natural scrolling: swipe up (dy<0) → scroll down (Button5), swipe down (dy>0) → scroll up (Button4)
            let button = if accum > 0 { 4u8 } else { 5u8 };
            let clicks = (accum.abs() / threshold) as u32;
            TOUCH_SCROLL_ACCUM_Y.store(accum % threshold, std::sync::atomic::Ordering::Relaxed);
            for _ in 0..clicks.min(5) {
                send_display_event(DisplayEvent::ButtonPress {
                    window: x11_id, button, x, y, root_x: x, root_y: y, state: 0, time,
                });
                send_display_event(DisplayEvent::ButtonRelease {
                    window: x11_id, button, x, y, root_x: x, root_y: y, state: 0, time,
                });
            }
        }
    }

    // Horizontal scroll: natural direction
    let dx = translation.x as i32;
    if dx != 0 {
        let accum = TOUCH_SCROLL_ACCUM_X.fetch_add(dx, std::sync::atomic::Ordering::Relaxed) + dx;
        if accum.abs() >= threshold {
            let button = if accum < 0 { 6u8 } else { 7u8 };
            let clicks = (accum.abs() / threshold) as u32;
            TOUCH_SCROLL_ACCUM_X.store(accum % threshold, std::sync::atomic::Ordering::Relaxed);
            for _ in 0..clicks.min(5) {
                send_display_event(DisplayEvent::ButtonPress {
                    window: x11_id, button, x, y, root_x: x, root_y: y, state: 0, time,
                });
                send_display_event(DisplayEvent::ButtonRelease {
                    window: x11_id, button, x, y, root_x: x, root_y: y, state: 0, time,
                });
            }
        }
    }
}

static TOUCH_SCROLL_ACCUM_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static TOUCH_SCROLL_ACCUM_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

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
        // X11 pixels = UIKit points (contentsScale=1.0), so no conversion needed.
        (point.x as i16, point.y as i16)
    }
}

fn get_timestamp() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

/// Pointer to the PSLXView that currently has focus (first responder), stored as usize.
/// Updated when a PSLXView becomes first responder.
static FOCUSED_VIEW: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Read x11WindowId ivar from a PSLXView pointer.
fn view_x11_id(view: *mut AnyObject) -> crate::display::Xid {
    if view.is_null() { return 0; }
    unsafe {
        if let Some(ivar) = (*view).class().instance_variable(c"x11WindowId") {
            return *ivar.load::<u32>(&*view) as crate::display::Xid;
        }
        0
    }
}

fn get_focused_x11_window() -> crate::display::Xid {
    let view = FOCUSED_VIEW.load(std::sync::atomic::Ordering::Relaxed) as *mut AnyObject;
    if view.is_null() { return 0; }
    view_x11_id(view)
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

/// Move a window's panel to a new position using setFrame on the container_layer.
/// x, y are in X11 physical pixels which map 1:1 to UIKit points (contentsScale=1.0).
fn move_window_layer(native_id: u64, x: f64, y: f64) {
    WINDOWS.with(|w| {
        let ws = w.borrow();
        if let Some(info) = ws.get(&native_id) {
            // Use container_layer (title bar + content) for drag; fall back to ca_layer.
            let layer = if !info.container_layer.is_null() { info.container_layer }
                        else if !info.ca_layer.is_null() { info.ca_layer }
                        else { return; };
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

                let ca_cls = objc_getClass(b"CATransaction\0".as_ptr());
                let _: () = msg_send![ca_cls, begin];
                let _: () = msg_send![ca_cls, setDisableActions: true];

                let panel_h = info.height as f64 + TITLE_BAR_HEIGHT as f64;
                let frame = CGRect {
                    origin: CGPoint { x, y },
                    size: CGSize { w: info.width as f64, h: panel_h },
                };
                let _: () = msg_send![layer, setFrame: frame];

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
/// Skips the title bar area (top TITLE_BAR_HEIGHT pixels of each window panel).
fn find_x11_window_at(x: i16, y: i16) -> crate::display::Xid {
    WINDOWS.with(|w| {
        let ws = w.borrow();
        let mut best: Option<(crate::display::Xid, u64)> = None;
        for (id, info) in ws.iter() {
            if !info.visible { continue; }
            let wx = x - info.x11_x;
            // Content starts at y = x11_y + TITLE_BAR_HEIGHT (title bar is above).
            let wy = y - info.x11_y - TITLE_BAR_HEIGHT;
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
/// Subtracts TITLE_BAR_HEIGHT so y=0 in X11 space aligns with the top of the content (not title bar).
fn screen_to_window_coords(screen_x: i16, screen_y: i16, x11_id: crate::display::Xid) -> (i16, i16) {
    WINDOWS.with(|w| {
        let ws = w.borrow();
        for info in ws.values() {
            if info.x11_id == x11_id {
                return (screen_x - info.x11_x, screen_y - info.x11_y - TITLE_BAR_HEIGHT);
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

/// Add a window as a floating panel: title bar + content CALayer.
/// The panel is a container CALayer with:
///   - title bar sublayer (dark gray, with window title text)
///   - content sublayer (IOSurface at y=TITLE_BAR_HEIGHT)
/// This gives each X11 window its own draggable panel UI.
fn add_window_panel(native_id: u64) {
    let main_view = MAIN_VIEW.with(|mv| *mv.borrow());
    let main_view = match main_view {
        Some(v) => v,
        None => {
            ios_log(&format!("add_window_panel: no main view for window {}", native_id));
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().get_mut(&native_id) {
                    info.pending_show = true;
                }
            });
            return;
        }
    };

    WINDOWS.with(|w| {
        let mut ws = w.borrow_mut();
        let info = match ws.get_mut(&native_id) {
            Some(i) => i,
            None => return,
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

            let ca_cls = objc_getClass(b"CALayer\0".as_ptr());
            let tb_h = TITLE_BAR_HEIGHT as f64;

            // --- 1. Container layer (title bar + content, positioned at x11_x/y) ---
            let container: *mut AnyObject = msg_send![ca_cls, layer];
            let container_frame = CGRect {
                origin: [info.x11_x as f64, info.x11_y as f64],
                size: [info.width as f64, info.height as f64 + tb_h],
            };
            let _: () = msg_send![container, setFrame: container_frame];
            let _: () = msg_send![container, setMasksToBounds: true];
            // zPosition > 0 so panels render above the desktop background
            let _: () = msg_send![container, setZPosition: 1.0_f64];

            // --- 2. Title bar sublayer ---
            let title_layer: *mut AnyObject = msg_send![ca_cls, layer];
            let title_frame = CGRect {
                origin: [0.0, 0.0],
                size: [info.width as f64, tb_h],
            };
            let _: () = msg_send![title_layer, setFrame: title_frame];
            // Dark title bar background (#333333)
            let title_color: *mut AnyObject = msg_send![objc2::class!(UIColor),
                colorWithRed: 0.2_f64 green: 0.2_f64 blue: 0.2_f64 alpha: 1.0_f64];
            let title_cgcolor: *const std::ffi::c_void = msg_send![title_color, CGColor];
            let _: () = msg_send![title_layer, setBackgroundColor: title_cgcolor];

            // Note: CATextLayer avoided — it triggers UIKit safe-area layout crashes
            // when multiple windows are open. Plain CALayer only.
            let _: () = msg_send![container, addSublayer: title_layer];

            // --- 3. Content sublayer (IOSurface, below title bar) ---
            let content_layer: *mut AnyObject = msg_send![ca_cls, layer];
            let content_frame = CGRect {
                origin: [0.0, tb_h],
                size: [info.width as f64, info.height as f64],
            };
            let _: () = msg_send![content_layer, setFrame: content_frame];
            let gravity = NSString::from_str("topLeft");
            let _: () = msg_send![content_layer, setContentsGravity: &*gravity];
            let _: () = msg_send![content_layer, setContentsScale: 1.0_f64];
            let _: () = msg_send![content_layer, setMasksToBounds: true];
            if !info.display_surface.is_null() {
                let _: () = msg_send![content_layer, setContents: info.display_surface as *mut AnyObject];
            }
            let _: () = msg_send![container, addSublayer: content_layer];

            // --- 4. Add container to main view's layer ---
            let parent_layer: *mut AnyObject = msg_send![main_view, layer];
            if !parent_layer.is_null() {
                let _: () = msg_send![parent_layer, addSublayer: container];
            }

            let _: *mut AnyObject = msg_send![container, retain];
            let _: *mut AnyObject = msg_send![content_layer, retain];
            info.container_layer = container;
            info.ca_layer = content_layer;
            info.ui_view = main_view;

            ios_log(&format!("add_window_panel: id={} '{}' {}x{} at ({},{})",
                native_id, info.title, info.width, info.height, info.x11_x, info.x11_y));
        }
    });
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

// CGImage FFI removed — using zero-copy IOSurface as CALayer contents (same as macOS)

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
                // Same screen clamp as macOS (macos.rs:1767-1781).
                // X11 pixels map 1:1 to UIKit points (contentsScale=1.0).
                let screen: *mut AnyObject = msg_send![objc2::class!(UIScreen), mainScreen];
                let screen_bounds: CGRect = msg_send![screen, bounds];
                let mut pt_w = info.width as f64;
                let mut pt_h = info.height as f64;
                // Screen bounds are in points; at 2x scale a 1366pt screen = 2732px.
                // Since we report physical pixels to X11, xterm creates a 484px window
                // which maps to 484pt here. Clamp to screen bounds in points.
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
                    become_first_responder_deferred(view);
                }

                info.ui_window = main_win;

                // Set scene title (like macOS setTitle)
                let scene: *mut AnyObject = msg_send![main_win, windowScene];
                if !scene.is_null() {
                    let title_ns = NSString::from_str(&info.title);
                    let _: () = msg_send![scene, setTitle: &*title_ns];
                }

                // Resize Stage Manager window to X11 size (sizeRestrictions for real iPad)
                resize_scene_to_x11(main_win, pt_w as u16, pt_h as u16);
                // Direct setFrame for simulator (sizeRestrictions ignored on simulator)
                let win_frame = CGRect { origin: [0.0, 0.0], size: [pt_w, pt_h] };
                let _: () = msg_send![main_win, setFrame: win_frame];

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

/// Create a per-window UIWindow + PSLXView in a UIWindowScene.
/// Like macOS: 1 X11 window = 1 NSWindow → iOS: 1 X11 window = 1 UIWindow in its own UIWindowScene.
/// For the first window: uses MAIN_SCENE (the auto-created initial scene).
/// For subsequent windows: called from scene_will_connect after request_new_scene;
///   the scene is already in PENDING_SCENE_MAP keyed by native_id.
fn setup_window_in_scene(native_id: u64) {
    ios_log(&format!("setup_window_in_scene: START id={}", native_id));

    // Get scene: PENDING_SCENE_MAP for new windows, MAIN_SCENE for first window.
    let scene = PENDING_SCENE_MAP.with(|pm| {
        let mut map = pm.borrow_mut();
        if let Some(pos) = map.iter().position(|(id, _)| *id == native_id) {
            Some(map.remove(pos).1)
        } else {
            None
        }
    }).or_else(|| MAIN_SCENE.with(|ms| *ms.borrow()));

    let scene = match scene {
        Some(s) => s,
        None => {
            ios_log("setup_window_in_scene: no scene available");
            return;
        }
    };

    // Read window info (size, x11_id) from WINDOWS
    let (width, height, x11_id, display_surface, title, pending_show) = WINDOWS.with(|w| {
        let ws = w.borrow();
        if let Some(info) = ws.get(&native_id) {
            (info.width, info.height, info.x11_id, info.display_surface, info.title.clone(), info.pending_show)
        } else {
            ios_log(&format!("setup_window_in_scene: native_id={} not found in WINDOWS", native_id));
            return (800, 600, 0u32, std::ptr::null_mut(), String::new(), false);
        }
    });

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

        // Clamp window size to screen bounds (same as macOS clamp to visibleFrame)
        let screen_w = SCREEN_WIDTH.with(|sw| sw.get());
        let screen_h = SCREEN_HEIGHT.with(|sh| sh.get());
        let mut pt_w = width as f64;
        let mut pt_h = height as f64;
        if screen_w > 0.0 && screen_h > 0.0 {
            let max_w = screen_w;
            let max_h = screen_h - IOS_TITLE_BAR_HEIGHT; // reserve title bar space
            if pt_w > max_w || pt_h > max_h {
                let fit = (max_w / pt_w).min(max_h / pt_h);
                pt_w *= fit;
                pt_h *= fit;
            }
        }
        // Window frame includes title bar height
        let total_h = pt_h + IOS_TITLE_BAR_HEIGHT;
        let win_frame = CGRect { origin: [0.0, 0.0], size: [pt_w, total_h] };

        // Create UIWindow sized to X11 window + title bar
        let win: *mut AnyObject = msg_send![objc2::class!(UIWindow), alloc];
        let win: *mut AnyObject = msg_send![win, initWithWindowScene: &*scene];
        let _: () = msg_send![win, setFrame: win_frame];

        // Create container UIView (title bar + content)
        let container: *mut AnyObject = msg_send![objc2::class!(UIView), alloc];
        let container: *mut AnyObject = msg_send![container, initWithFrame: win_frame];

        // Create title bar with traffic light buttons
        let title_bar = create_title_bar(pt_w, &title, x11_id as u32);
        let _: () = msg_send![container, addSubview: title_bar];

        // Create PSLXView below title bar
        let content_frame = CGRect { origin: [0.0, IOS_TITLE_BAR_HEIGHT], size: [pt_w, pt_h] };
        let view_cls = get_pslx_view_class();
        let view: *mut AnyObject = msg_send![view_cls, alloc];
        let view: *mut AnyObject = msg_send![view, initWithFrame: content_frame];
        let _: () = msg_send![container, addSubview: view];

        // Set x11WindowId ivar so keyboard/touch handlers know which X11 window this is
        if let Some(ivar) = (*view).class().instance_variable(c"x11WindowId") {
            *ivar.load_mut::<u32>(&mut *view) = x11_id as u32;
        }

        // Set up view.layer with IOSurface (same as macOS: zero-copy IOSurface as CALayer contents)
        let layer: *mut AnyObject = msg_send![view, layer];
        if !layer.is_null() {
            let gravity = NSString::from_str("topLeft");
            let _: () = msg_send![layer, setContentsGravity: &*gravity];
            let _: () = msg_send![layer, setContentsScale: 1.0_f64];
            let _: () = msg_send![layer, setMasksToBounds: true];
            if !display_surface.is_null() {
                let _: () = msg_send![layer, setContents: display_surface as *mut AnyObject];
            }
        }

        // UIHoverGestureRecognizer: mouse/trackpad movement → X11 MotionNotify
        let hover_cls = objc2::class!(UIHoverGestureRecognizer);
        let hover: *mut AnyObject = msg_send![hover_cls, alloc];
        let hover: *mut AnyObject = msg_send![hover, initWithTarget: view action: objc2::sel!(handleHover:)];
        let _: () = msg_send![view, addGestureRecognizer: hover];

        // UIPanGestureRecognizer: mouse scroll → X11 Button4/5
        let pan_cls = objc2::class!(UIPanGestureRecognizer);
        let pan: *mut AnyObject = msg_send![pan_cls, alloc];
        let pan: *mut AnyObject = msg_send![pan, initWithTarget: view action: objc2::sel!(handleScroll:)];
        let _: () = msg_send![pan, setAllowedScrollTypesMask: 1i64];
        let _: () = msg_send![pan, setMaximumNumberOfTouches: 0usize];
        let _: () = msg_send![view, addGestureRecognizer: pan];

        // 2-finger touch pan → scroll emulation (jog dial)
        let touch_pan_cls = objc2::class!(UIPanGestureRecognizer);
        let touch_pan: *mut AnyObject = msg_send![touch_pan_cls, alloc];
        let touch_pan: *mut AnyObject = msg_send![touch_pan, initWithTarget: view action: objc2::sel!(handleTouchScroll:)];
        let _: () = msg_send![touch_pan, setMinimumNumberOfTouches: 2usize];
        let _: () = msg_send![touch_pan, setMaximumNumberOfTouches: 2usize];
        let _: () = msg_send![view, addGestureRecognizer: touch_pan];

        // Set up view controller
        let vc_cls = get_fullscreen_vc_class();
        let vc: *mut AnyObject = msg_send![vc_cls, alloc];
        let vc: *mut AnyObject = msg_send![vc, init];
        let _: () = msg_send![vc, setView: container];
        let _: () = msg_send![win, setRootViewController: vc];

        // Set scene title (like macOS window.setTitle)
        if !title.is_empty() {
            let title_ns = NSString::from_str(&title);
            let _: () = msg_send![scene, setTitle: &*title_ns];
        }

        // Resize Stage Manager window to X11 size + title bar
        resize_scene_to_x11(win, pt_w as u16, total_h as u16);

        let _: *mut AnyObject = msg_send![win, retain];
        let _: *mut AnyObject = msg_send![view, retain];
        let _: *mut AnyObject = msg_send![container, retain];

        // Update WINDOWS with UIWindow + PSLXView + CALayer
        WINDOWS.with(|w| {
            if let Some(info) = w.borrow_mut().get_mut(&native_id) {
                info.ui_window = win;
                info.ui_view = view;
                info.ca_layer = layer;
                info.title_bar = title_bar;
                // Update clamped dimensions
                info.width = pt_w as u16;
                info.height = pt_h as u16;
            }
        });

        // Update MAIN_UI_WINDOW/MAIN_VIEW for timer callback (first window only)
        let is_main_scene = MAIN_SCENE.with(|ms| ms.borrow().map_or(false, |s| s == scene));
        if is_main_scene {
            MAIN_UI_WINDOW.with(|mw| *mw.borrow_mut() = Some(win));
            MAIN_VIEW.with(|mv| *mv.borrow_mut() = Some(view));
        }

        // If ShowWindow was already called (pending_show), show immediately.
        // Otherwise, wait for ShowWindow command (like macOS: CreateWindow doesn't show).
        if pending_show {
            ios_log(&format!("setup_window_in_scene: pending_show=true, showing now"));
            let _: () = msg_send![win, setHidden: false];
            let _: () = msg_send![win, makeKeyAndVisible];
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().get_mut(&native_id) {
                    info.visible = true;
                    info.pending_show = false;
                }
            });
            become_first_responder_deferred(view);
            if x11_id != 0 {
                send_display_event(DisplayEvent::FocusIn { window: x11_id });
            }
        }

        ios_log(&format!("setup_window_in_scene: created UIWindow {:.0}x{:.0} x11=0x{:08x} scene={:p}",
            pt_w, pt_h, x11_id, scene));
    }
}

/// Map from native_window_id → UIWindowScene pointer, for scene_will_connect to find.
/// Only accessed from main thread (UIKit callbacks + timer), but Mutex for Send safety.
thread_local! {
    static PENDING_SCENE_MAP: RefCell<Vec<(u64, *mut AnyObject)>> = RefCell::new(Vec::new());
}

/// Push IOSurface to CALayer — zero-copy (same as macOS).
/// setContents: with IOSurface works on iOS (CALayer accepts it directly).
fn flush_window_to_layer(layer: *mut AnyObject, surface: *mut c_void) {
    if surface.is_null() || layer.is_null() { return; }
    unsafe {
        let ca_cls = objc_getClass(b"CATransaction\0".as_ptr());
        let _: () = msg_send![ca_cls, begin];
        let _: () = msg_send![ca_cls, setDisableActions: true];
        let _: () = msg_send![layer, setContents: surface as *mut AnyObject];
        let _: () = msg_send![ca_cls, commit];
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
/// width/height are X11 dimensions in pixels = UIKit points (contentsScale=1.0 mapping).
unsafe fn resize_scene_to_x11(ui_window: *mut AnyObject, width: u16, height: u16) {
    let scene: *mut AnyObject = msg_send![ui_window, windowScene];
    if scene.is_null() { return; }

    // Set sizeRestrictions: minimumSize = X11 window size, maximumSize = screen size.
    // This allows Stage Manager resize and fullscreen while starting at X11 size.
    let restrictions: *mut AnyObject = msg_send![scene, sizeRestrictions];
    if !restrictions.is_null() {
        use objc2::encode::{Encode, Encoding};
        #[repr(C)]
        #[derive(Copy, Clone)]
        struct CGSize { width: f64, height: f64 }
        unsafe impl Encode for CGSize {
            const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
        }
        let min_size = CGSize { width: 320.0, height: 240.0 };
        let screen_w = SCREEN_WIDTH.with(|sw| sw.get());
        let screen_h = SCREEN_HEIGHT.with(|sh| sh.get());
        let max_size = CGSize {
            width: if screen_w > 0.0 { screen_w } else { 2732.0 },
            height: if screen_h > 0.0 { screen_h } else { 2048.0 },
        };
        let _: () = msg_send![restrictions, setMinimumSize: min_size];
        let _: () = msg_send![restrictions, setMaximumSize: max_size];
        info!("resize_scene_to_x11: sizeRestrictions min=320x240 max={:.0}x{:.0}", max_size.width, max_size.height);
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

    // X11 pixels map 1:1 to UIKit points (contentsScale=1.0).
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

    // Every ~2s (120 ticks at 60fps): check first responder status and window
    if cnt % 120 == 60 {
        MAIN_VIEW.with(|mv| {
            if let Some(view) = *mv.borrow() {
                unsafe {
                    let is_fr: Bool = objc2::msg_send![&*view, isFirstResponder];
                    // Check key window
                    let app: *mut AnyObject = objc2::msg_send![objc2::class!(UIApplication), sharedApplication];
                    let key_win: *mut AnyObject = if !app.is_null() {
                        objc2::msg_send![app, keyWindow]
                    } else { std::ptr::null_mut() };
                    let win: *mut AnyObject = objc2::msg_send![&*view, window];
                    let is_key: bool = !key_win.is_null() && !win.is_null() && key_win == win;
                    ios_log(&format!("timer check: isFirstResponder={} isKeyWindow={} view={:p} win={:p} keyWin={:p}",
                        is_fr.as_bool(), is_key, view, win, key_win));
                    if !is_fr.as_bool() {
                        ios_log("timer: re-acquiring first responder");
                        let _: Bool = objc2::msg_send![&*view, becomeFirstResponder];
                    }
                }
            }
        });
    }

    process_commands();

    // Check if AudioQueue needs initialization (triggered by PA server)
    unsafe { crate::audio::check_audio_init(); }

    // Apply pending keyboard resize (deferred to allow child windows to exist)
    if let Some((w, h)) = PENDING_RESIZE.with(|pr| pr.get()) {
        // Only apply after a short delay (30 ticks ≈ 0.5s at 60fps) so child windows exist
        static RESIZE_COUNTDOWN: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
        let val = RESIZE_COUNTDOWN.load(std::sync::atomic::Ordering::Relaxed);
        if val == -1 {
            // Start countdown
            RESIZE_COUNTDOWN.store(30, std::sync::atomic::Ordering::Relaxed);
        } else if val > 0 {
            RESIZE_COUNTDOWN.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            // Countdown done, apply resize
            PENDING_RESIZE.with(|pr| pr.set(None));
            RESIZE_COUNTDOWN.store(-1, std::sync::atomic::Ordering::Relaxed);
            resize_primary_window(w, h);
        }
    }
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
            // Note: do NOT call mailbox.is_empty() — it locks all 16 DashMap shards,
            // causing heavy contention with the protocol thread's write lock.
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
                        // Debug: dump window info to file
                        if cnt < 5 || (n_windows > 1 && cnt < 300) {
                            let mut debug = String::new();
                            for (id, info) in ws.iter() {
                                debug.push_str(&format!("  id={} x11=0x{:08X} {}x{} at ({},{}) vis={} layer={:?} uiwin={:?}\n",
                                    id, info.x11_id, info.width, info.height,
                                    info.x11_x, info.x11_y, info.visible,
                                    info.ca_layer.is_null(), info.ui_window.is_null()));
                            }
                            // Also check sublayer count on container
                            let main_view_ptr = MAIN_VIEW.with(|mv| *mv.borrow());
                            if let Some(mv) = main_view_ptr {
                                unsafe {
                                    let container: *mut AnyObject = msg_send![mv, superview];
                                    if !container.is_null() {
                                        let clayer: *mut AnyObject = msg_send![container, layer];
                                        let sublayers: *mut AnyObject = msg_send![clayer, sublayers];
                                        let sublayer_count: usize = if !sublayers.is_null() {
                                            msg_send![sublayers, count]
                                        } else { 0 };
                                        debug.push_str(&format!("  container.layer sublayers: {}\n", sublayer_count));
                                    }
                                    let vlayer: *mut AnyObject = msg_send![mv, layer];
                                    let vsublayers: *mut AnyObject = msg_send![vlayer, sublayers];
                                    let vsub_count: usize = if !vsublayers.is_null() {
                                        msg_send![vsublayers, count]
                                    } else { 0 };
                                    debug.push_str(&format!("  main_view.layer sublayers: {}\n", vsub_count));
                                }
                            }
                            let _ = std::fs::write("/tmp/pslx_render_debug.txt", debug);
                        }
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
                    container_layer: std::ptr::null_mut(),
                    ui_window: std::ptr::null_mut(),
                    ui_view: std::ptr::null_mut(),
                    title_bar: std::ptr::null_mut(),
                    is_fullscreen: false,
                    pending_show: false,
                });
            });

            // All windows use MAIN_SCENE — setup UIWindow immediately.
            // (Like macOS: every X11 window gets its own NSWindow)
            setup_window_in_scene(id);
            ios_log(&format!("CreateWindow: id={} x11=0x{:08X} {}x{} (main scene)",
                id, x11_id, width, height));

            info!("Created window {} for X11 0x{:08X} ({}x{}) [iOS]", id, x11_id, width, height);
            let _ = reply.send(NativeWindowHandle { id });
        }

        DisplayCommand::ShowWindow { handle, visible } => {
            // Per-window design: ShowWindow → makeKeyAndVisible on per-window UIWindow (like macOS makeKeyAndOrderFront)
            let show_info = WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().get_mut(&handle.id) {
                    info.visible = visible;
                    if visible {
                        if !info.ui_window.is_null() {
                            // UIWindow exists — show it (like macOS makeKeyAndOrderFront)
                            unsafe {
                                let _: () = msg_send![info.ui_window, setHidden: false];
                                let _: () = msg_send![info.ui_window, makeKeyAndVisible];
                            }
                            flush_window_to_layer(info.ca_layer, info.display_surface);
                            Some((info.x11_id, info.width, info.height, info.ui_view))
                        } else {
                            // UIWindow not yet created — defer
                            info.pending_show = true;
                            ios_log(&format!("ShowWindow: id={} deferred (no UIWindow yet)", handle.id));
                            None
                        }
                    } else {
                        // Hide (like macOS orderOut)
                        if !info.ui_window.is_null() {
                            unsafe { let _: () = msg_send![info.ui_window, setHidden: true]; }
                        }
                        None
                    }
                } else {
                    None
                }
            });
            if let Some((x11_id, w, h, view)) = show_info {
                // Make view first responder — deferred so UIKit has settled after makeKeyAndVisible
                if !view.is_null() {
                    unsafe { become_first_responder_deferred(view); }
                    ios_log(&format!("ShowWindow: id={} become_first_responder_deferred", handle.id));
                }
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
            // WindowInfo::Drop handles all releases (surface, display_surface, ca_layer, ui_window).
            // Just hide the UIWindow before removing, then let Drop clean up.
            WINDOWS.with(|w| {
                if let Some(info) = w.borrow_mut().get(&handle.id) {
                    if !info.ui_window.is_null() {
                        unsafe { let _: () = msg_send![info.ui_window, setHidden: true]; }
                    }
                }
                w.borrow_mut().remove(&handle.id); // triggers Drop
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

                    // Update container_layer frame (panel position/size including title bar).
                    if !info.container_layer.is_null() {
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
                            let panel_h = info.height as f64 + TITLE_BAR_HEIGHT as f64;
                            let frame = CGRect {
                                origin: [info.x11_x as f64, info.x11_y as f64],
                                size: [info.width as f64, panel_h],
                            };
                            let _: () = msg_send![info.container_layer, setFrame: frame];
                        }
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
            // viewDidLayoutSubviews — detect Stage Manager resize and update X11 window
            class_addMethod(raw_cls, objc2::sel!(viewDidLayoutSubviews),
                vc_view_did_layout_subviews as *const c_void, c"v@:".as_ptr() as _);
            CLASS = raw_cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

unsafe extern "C" fn vc_prefers_status_bar_hidden(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

/// viewDidLayoutSubviews — called when Stage Manager resizes the window.
/// Detects actual view size and resizes the X11 window (IOSurface + ConfigureNotify) to match.
unsafe extern "C" fn vc_view_did_layout_subviews(this: *mut AnyObject, _sel: Sel) {
    // Call super
    let superclass = objc2::class!(UIViewController);
    let _: () = msg_send![super(this, superclass), viewDidLayoutSubviews];

    let view: *mut AnyObject = msg_send![this, view];
    if view.is_null() { return; }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGRect { origin: [f64; 2], size: [f64; 2] }
    unsafe impl objc2::encode::Encode for CGRect {
        const ENCODING: objc2::encode::Encoding = objc2::encode::Encoding::Struct("CGRect", &[
            objc2::encode::Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]),
            objc2::encode::Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]),
        ]);
    }

    let bounds: CGRect = msg_send![view, bounds];
    let new_w = bounds.size[0] as u16;
    // Subtract title bar height — the view is the container (title bar + content)
    let raw_h = bounds.size[1] - IOS_TITLE_BAR_HEIGHT;
    let new_h = if raw_h > 0.0 { raw_h as u16 } else { return; };
    if new_w == 0 || new_h == 0 { return; }

    // Find the window by matching the UIWindow from the VC
    let ui_window: *mut AnyObject = msg_send![view, window];
    if ui_window.is_null() { return; }

    // Find native_id by matching ui_window in WINDOWS
    // Use try_borrow to avoid panic if WINDOWS is already borrowed (e.g. timer_callback)
    let needs_resize = WINDOWS.with(|w| {
        let ws = match w.try_borrow() {
            Ok(ws) => ws,
            Err(_) => return None, // Already borrowed — skip this layout pass
        };
        for (id, info) in ws.iter() {
            if info.ui_window == ui_window {
                if info.width != new_w || info.height != new_h {
                    return Some((*id, info.x11_id, info.width, info.height));
                }
                return None;
            }
        }
        None
    });

    if let Some((native_id, x11_id, old_w, old_h)) = needs_resize {
        ios_log(&format!("viewDidLayoutSubviews: Stage Manager resize {}x{} -> {}x{} x11=0x{:08X}",
            old_w, old_h, new_w, new_h, x11_id));

        // Also resize the PSLXView (content view) to match
        WINDOWS.with(|w| {
            let mut ws = match w.try_borrow_mut() {
                Ok(ws) => ws,
                Err(_) => return, // Already borrowed — skip
            };
            if let Some(info) = ws.get_mut(&native_id) {
                let new_surface = create_iosurface(new_w, new_h);
                let new_display = create_iosurface(new_w, new_h);
                if new_surface.is_null() || new_display.is_null() { return; }

                // Copy old content to new surfaces
                for (old_s, new_s) in [(info.surface, new_surface), (info.display_surface, new_display)] {
                    IOSurfaceLock(new_s, 0, std::ptr::null_mut());
                    IOSurfaceLock(old_s, 0, std::ptr::null_mut());
                    let src = IOSurfaceGetBaseAddress(old_s) as *const u8;
                    let dst = IOSurfaceGetBaseAddress(new_s) as *mut u8;
                    let src_stride = IOSurfaceGetBytesPerRow(old_s);
                    let dst_stride = IOSurfaceGetBytesPerRow(new_s);
                    let copy_h = old_h.min(new_h) as usize;
                    let copy_w = (old_w.min(new_w) as usize) * 4;
                    for row in 0..copy_h {
                        std::ptr::copy_nonoverlapping(
                            src.add(row * src_stride),
                            dst.add(row * dst_stride),
                            copy_w,
                        );
                    }
                    IOSurfaceUnlock(old_s, 0, std::ptr::null_mut());
                    IOSurfaceUnlock(new_s, 0, std::ptr::null_mut());
                }

                CFRelease(info.surface);
                CFRelease(info.display_surface);
                info.surface = new_surface;
                info.display_surface = new_display;
                info.width = new_w;
                info.height = new_h;

                // Resize the PSLXView frame to match new content size
                if !info.ui_view.is_null() {
                    let content_frame = CGRect {
                        origin: [0.0, IOS_TITLE_BAR_HEIGHT],
                        size: [new_w as f64, new_h as f64],
                    };
                    let _: () = msg_send![info.ui_view, setFrame: content_frame];
                }

                // Update layer contents
                flush_window_to_layer(info.ca_layer, info.display_surface);
            }
        });

        // Notify X11 client of new size
        send_display_event(DisplayEvent::ConfigureNotify {
            window: x11_id,
            x: 0, y: 0,
            width: new_w, height: new_h,
        });
        send_display_event(DisplayEvent::Expose {
            window: x11_id,
            x: 0, y: 0,
            width: new_w, height: new_h, count: 0,
        });
    }
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

        // Register for keyboard show/hide notifications
        register_keyboard_observers();
    }
}

/// Register NSNotificationCenter observers for keyboard frame changes.
/// When the keyboard appears/disappears, we resize the first X11 window
/// to fit the available space above the keyboard.
fn register_keyboard_observers() {
    unsafe {
        let center: *mut AnyObject = msg_send![objc2::class!(NSNotificationCenter), defaultCenter];

        // UIKeyboardWillChangeFrameNotification gives us the final keyboard frame
        let notif_name = NSString::from_str("UIKeyboardWillChangeFrameNotification");

        // Register class that handles keyboard notifications
        let cls = {
            let existing = objc_getClass(b"PSLXKeyboardObserver\0".as_ptr());
            if existing.is_null() {
                let superclass = objc2::class!(NSObject);
                let mut builder = ClassBuilder::new(c"PSLXKeyboardObserver", superclass)
                    .expect("Failed to create PSLXKeyboardObserver class");
                unsafe extern "C" fn keyboard_changed(_this: *mut AnyObject, _sel: Sel, notification: *mut AnyObject) {
                    handle_keyboard_notification(notification);
                }
                builder.add_method(
                    objc2::sel!(keyboardChanged:),
                    keyboard_changed as unsafe extern "C" fn(_, _, _),
                );
                builder.register() as *const AnyClass
            } else {
                existing as *const AnyClass
            }
        };

        let observer: *mut AnyObject = msg_send![cls, alloc];
        let observer: *mut AnyObject = msg_send![observer, init];
        let _: *mut AnyObject = msg_send![observer, retain]; // prevent dealloc

        let _: () = msg_send![center,
            addObserver: observer
            selector: objc2::sel!(keyboardChanged:)
            name: &*notif_name
            object: std::ptr::null::<AnyObject>()
        ];
        info!("Registered keyboard frame observer");
    }
}

/// Handle UIKeyboardWillChangeFrameNotification.
/// Extracts the keyboard end frame and resizes the first X11 window accordingly.
fn handle_keyboard_notification(notification: *mut AnyObject) {
    if notification.is_null() { return; }

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

        let user_info: *mut AnyObject = msg_send![notification, userInfo];
        if user_info.is_null() { return; }

        let key = NSString::from_str("UIKeyboardFrameEndUserInfoKey");
        let value: *mut AnyObject = msg_send![user_info, objectForKey: &*key];
        if value.is_null() { return; }

        let end_frame: CGRect = msg_send![value, CGRectValue];
        let screen_h = SCREEN_HEIGHT.with(|sh| sh.get());
        let screen_w = SCREEN_WIDTH.with(|sw| sw.get());

        // Keyboard height = screen height - keyboard frame origin Y
        // When keyboard is hidden, end_frame.origin.y == screen_h
        let kb_height = (screen_h - end_frame.origin[1]).max(0.0);
        let old_kb_height = KEYBOARD_HEIGHT.with(|kh| kh.get());

        if (kb_height - old_kb_height).abs() < 1.0 { return; } // no change

        KEYBOARD_HEIGHT.with(|kh| kh.set(kb_height));
        let available_h = (screen_h - kb_height) as u16;
        let available_w = screen_w as u16;

        ios_log(&format!("keyboard frame changed: kb_h={:.0} available={}x{}",
            kb_height, available_w, available_h));

        // Don't resize immediately — child windows may not exist yet.
        // Store the pending size; the timer tick will apply it.
        // TODO: re-enable after debugging multi-window
        // PENDING_RESIZE.with(|pr| pr.set(Some((available_w, available_h))));
    }
}

/// Resize the primary (first) X11 window to the given dimensions.
/// Sends ConfigureNotify + Expose so the X11 client (e.g. xterm) reflows.
fn resize_primary_window(new_width: u16, new_height: u16) {
    WINDOWS.with(|w| {
        let mut ws = w.borrow_mut();
        // Find the first window (id=1) which is the primary UIWindow-backed one
        if let Some(info) = ws.get_mut(&1) {
            if info.width == new_width && info.height == new_height { return; }

            let old_w = info.width;
            let old_h = info.height;
            info.width = new_width;
            info.height = new_height;

            // Create new IOSurfaces at the new size
            let new_surface = create_iosurface(new_width, new_height);
            let new_display = create_iosurface(new_width, new_height);
            if new_surface.is_null() || new_display.is_null() { return; }

            // Copy old content to new surfaces
            unsafe {
                for (old_s, new_s) in [(info.surface, new_surface), (info.display_surface, new_display)] {
                    IOSurfaceLock(new_s, 0, std::ptr::null_mut());
                    IOSurfaceLock(old_s, 0, std::ptr::null_mut());
                    let src = IOSurfaceGetBaseAddress(old_s) as *const u8;
                    let dst = IOSurfaceGetBaseAddress(new_s) as *mut u8;
                    let src_stride = IOSurfaceGetBytesPerRow(old_s);
                    let dst_stride = IOSurfaceGetBytesPerRow(new_s);
                    let copy_h = old_h.min(new_height) as usize;
                    let copy_w = (old_w.min(new_width) as usize) * 4;
                    for row in 0..copy_h {
                        std::ptr::copy_nonoverlapping(
                            src.add(row * src_stride),
                            dst.add(row * dst_stride),
                            copy_w,
                        );
                    }
                    IOSurfaceUnlock(old_s, 0, std::ptr::null_mut());
                    IOSurfaceUnlock(new_s, 0, std::ptr::null_mut());
                }
                // Release old surfaces (IOSurface is refcounted via CFRelease)
                CFRelease(info.surface);
                CFRelease(info.display_surface);
            }
            info.surface = new_surface;
            info.display_surface = new_display;

            // Update the view/layer frame
            if !info.ui_view.is_null() {
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
                    let frame = CGRect {
                        origin: [0.0, 0.0],
                        size: [new_width as f64, new_height as f64],
                    };
                    let _: () = msg_send![info.ui_view, setFrame: frame];
                }
            }

            // Flush new surface to layer
            flush_window_to_layer(info.ca_layer, info.display_surface);

            let x11_id = info.x11_id;
                ios_log(&format!("resize_primary_window: {}x{} -> {}x{} x11=0x{:08X}",
                old_w, old_h, new_width, new_height, x11_id));

            // Send ConfigureNotify + Expose to the X11 client so it reflows
            send_display_event(DisplayEvent::ConfigureNotify {
                window: x11_id,
                x: 0,
                y: 0,
                width: new_width,
                height: new_height,
            });
            send_display_event(DisplayEvent::Expose {
                window: x11_id,
                x: 0,
                y: 0,
                width: new_width,
                height: new_height,
                count: 0,
            });
        }
    });
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
    let _ = get_text_range_class();

    unsafe {
        UIApplicationMain(
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
            &*delegate_name as *const _ as *mut AnyObject,
        );
    }
}

// Old audio code removed — now in src/audio.rs (PulseAudio native protocol server)
