// macOS display backend — NSWindow management, IOSurface-backed pixel buffer rendering
#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use crossbeam_channel::{Receiver, Sender};
use log::{debug, info};

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplication, NSWindow, NSWindowStyleMask};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

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
    /// IOSurface-backed pixel buffer — GPU can composite directly, zero-copy.
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

                    // Update layer contents to new surface and force immediate display
                    unsafe {
                        if let Some(view) = info.window.contentView() {
                            let layer: *mut AnyObject = msg_send![&*view, layer];
                            if !layer.is_null() {
                                let _: () = msg_send![layer, setContents: new_surface as *mut AnyObject];
                            }
                        }
                    }
                    ca_transaction_flush();

                    info!("Window {} resized: {}x{} -> {}x{}", change.win_id, old_w, old_h, change.new_w, change.new_h);
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
            for (win_id, mut commands) in render_batches {
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

            // Enable layer-backing on content view for CG rendering
            unsafe {
                if let Some(view) = window.contentView() {
                    let _: () = msg_send![&*view, setWantsLayer: true];
                    let layer: *mut AnyObject = msg_send![&*view, layer];
                    if !layer.is_null() {
                        // Pin content to top-left during resize so text doesn't stretch.
                        // When the IOSurface matches the view size, it fills exactly.
                        // During a resize drag, old content stays at correct size with blank space.
                        let gravity = NSString::from_str("topLeft");
                        let _: () = msg_send![&*layer, setContentsGravity: &*gravity];
                        // Set contentsScale to 1.0 so IOSurface pixels map to points
                        let _: () = msg_send![&*layer, setContentsScale: 1.0_f64];
                        // Clip layer contents to view bounds — essential for right-side clipping
                        // during resize. Without this, content extends beyond the view edge.
                        let _: () = msg_send![&*layer, setMasksToBounds: true];
                    }
                }
            }

            // IOSurface for zero-copy compositing via CoreAnimation
            let surface = create_iosurface(width, height);
            if surface.is_null() {
                log::error!("Failed to create IOSurface for window {}", id);
                return;
            }

            // Calculate initial X11 screen position from actual macOS window position
            let (x11_x, x11_y) = macos_frame_to_x11_pos(&window);

            // Set IOSurface as initial layer contents
            unsafe {
                if let Some(view) = window.contentView() {
                    let layer: *mut AnyObject = msg_send![&*view, layer];
                    if !layer.is_null() {
                        let _: () = msg_send![layer, setContents: surface as *mut AnyObject];
                    }
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

        DisplayCommand::Shutdown => {
            let mtm = MainThreadMarker::new().unwrap();
            unsafe { NSApplication::sharedApplication(mtm).terminate(None); }
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

/// Notify CoreAnimation that the IOSurface contents have changed.
/// The IOSurface is already set as layer.contents — we just re-assign
/// it to trigger a redisplay. No buffer copy or CGImage creation needed.
fn flush_window(info: &WindowInfo) {
    unsafe {
        if let Some(view) = info.window.contentView() {
            let layer: *mut AnyObject = msg_send![&*view, layer];
            if !layer.is_null() {
                // Nil-then-set forces CA to recreate its internal Surface wrapper,
                // which triggers WindowServer to re-read IOSurface pixels.
                // This is the only reliable way to invalidate IOSurface-backed contents.
                let null: *mut AnyObject = std::ptr::null_mut();
                let _: () = msg_send![layer, setContents: null];
                let _: () = msg_send![layer, setContents: info.surface as *mut AnyObject];
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
            let keycode: u16 = unsafe { msg_send![event, keyCode] };
            let state = get_modifier_state(event);
            debug!("KeyPress: window=0x{:08x} macOS_keycode={} x11_keycode={}", x11_id, keycode, (keycode as u8).wrapping_add(8));
            // macOS keycode + 8 maps to X11 keycode (approximate)
            send_display_event(DisplayEvent::KeyPress {
                window: x11_id, keycode: (keycode as u8).wrapping_add(8), state, time,
            });
        }
        NS_KEY_UP => {
            let keycode: u16 = unsafe { msg_send![event, keyCode] };
            let state = get_modifier_state(event);
            send_display_event(DisplayEvent::KeyRelease {
                window: x11_id, keycode: (keycode as u8).wrapping_add(8), state, time,
            });
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
