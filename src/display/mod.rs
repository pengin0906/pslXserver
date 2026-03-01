#[cfg(target_os = "macos")]
pub mod macos;
pub mod hidpi;
pub mod renderer;

use crate::util::coord::{X11Point, X11Rect};

pub type Xid = u32;
pub type Atom = u32;

/// Global IME cursor position — written by protocol thread, read by macOS display thread.
/// Stored in X11 window coordinates relative to the top-level native window.
pub static IME_SPOT_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
pub static IME_SPOT_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
pub static IME_SPOT_LINE_H: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(13);
/// Flag: true while IME is composing (setMarkedText active). Suppress KeyRelease during composition.
pub static IME_COMPOSING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Flag: suppress the very next KeyRelease after IME commit (the confirmation key like Enter/Space).
pub static SUPPRESS_NEXT_KEYUP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Shared render mailbox: native_window_id -> pending render commands.
/// Protocol handlers append commands; display thread drains each frame.
pub type RenderMailbox = std::sync::Arc<dashmap::DashMap<u64, Vec<RenderCommand>>>;

/// Opaque handle to a native macOS window.
#[derive(Debug, Clone)]
pub struct NativeWindowHandle {
    /// Identifier for the NSWindow, used to route commands from tokio thread.
    pub id: u64,
}

/// Commands sent from the X11 protocol thread to the macOS main thread.
/// Dispatched via crossbeam_channel and executed on the main RunLoop.
#[derive(Debug)]
pub enum DisplayCommand {
    /// Create a new native window for an X11 top-level window.
    CreateWindow {
        x11_id: Xid,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        title: String,
        override_redirect: bool,
        reply: tokio::sync::oneshot::Sender<NativeWindowHandle>,
    },
    /// Destroy a native window.
    DestroyWindow {
        handle: NativeWindowHandle,
    },
    /// Show (map) a window.
    ShowWindow {
        handle: NativeWindowHandle,
    },
    /// Hide (unmap) a window.
    HideWindow {
        handle: NativeWindowHandle,
    },
    /// Move and/or resize a window.
    MoveResizeWindow {
        handle: NativeWindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    },
    /// Set the window title.
    SetWindowTitle {
        handle: NativeWindowHandle,
        title: String,
    },
    /// Raise a window to the front.
    RaiseWindow {
        handle: NativeWindowHandle,
    },
    /// Send a batch of render commands to a window.
    RenderBatch {
        handle: NativeWindowHandle,
        commands: Vec<RenderCommand>,
    },
    /// Invalidate a region of a window (trigger redraw).
    Invalidate {
        handle: NativeWindowHandle,
        rect: Option<X11Rect>,
    },
    /// Update IME cursor position for a window.
    UpdateImeSpot {
        handle: NativeWindowHandle,
        spot: X11Point,
        line_height: u16,
    },
    /// Set clipboard content.
    SetClipboard {
        content: String,
    },
    /// Get clipboard content.
    GetClipboard {
        reply: tokio::sync::oneshot::Sender<Option<String>>,
    },
    /// Shut down the display.
    Shutdown,
}

/// Events sent from the macOS main thread back to the X11 protocol thread.
#[derive(Debug)]
pub enum DisplayEvent {
    KeyPress {
        window: Xid,
        keycode: u8,
        state: u16,
        time: u32,
    },
    KeyRelease {
        window: Xid,
        keycode: u8,
        state: u16,
        time: u32,
    },
    ButtonPress {
        window: Xid,
        button: u8,
        x: i16,
        y: i16,
        root_x: i16,
        root_y: i16,
        state: u16,
        time: u32,
    },
    ButtonRelease {
        window: Xid,
        button: u8,
        x: i16,
        y: i16,
        root_x: i16,
        root_y: i16,
        state: u16,
        time: u32,
    },
    MotionNotify {
        window: Xid,
        x: i16,
        y: i16,
        root_x: i16,
        root_y: i16,
        state: u16,
        time: u32,
    },
    EnterNotify {
        window: Xid,
        x: i16,
        y: i16,
        time: u32,
    },
    LeaveNotify {
        window: Xid,
        x: i16,
        y: i16,
        time: u32,
    },
    Expose {
        window: Xid,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        count: u16,
    },
    ConfigureNotify {
        window: Xid,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    },
    FocusIn {
        window: Xid,
    },
    FocusOut {
        window: Xid,
    },
    MapNotify {
        window: Xid,
    },
    UnmapNotify {
        window: Xid,
    },
    DestroyNotify {
        window: Xid,
    },
    /// IME committed text (from macOS insertText:).
    ImeCommit {
        window: Xid,
        text: String,
    },
    /// IME preedit started.
    ImePreeditStart {
        window: Xid,
    },
    /// IME preedit text updated (from macOS setMarkedText:).
    ImePreeditDraw {
        window: Xid,
        text: String,
        cursor_pos: u32,
    },
    /// IME preedit ended (from macOS unmarkText).
    ImePreeditDone {
        window: Xid,
    },
    /// Window close requested (macOS red button).
    WindowCloseRequested {
        window: Xid,
    },
    /// Screen geometry changed.
    ScreenChanged {
        width: u16,
        height: u16,
        scale_factor: f64,
    },
    /// Global pointer position update (sent every frame from macOS).
    /// Used to keep QueryPointer accurate even when cursor is outside X11 windows.
    GlobalPointerUpdate {
        root_x: i16,
        root_y: i16,
    },
}

/// Individual rendering commands mapped from X11 drawing operations.
#[derive(Debug, Clone)]
pub enum RenderCommand {
    FillRectangle {
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        color: u32,
    },
    DrawLine {
        x1: i16,
        y1: i16,
        x2: i16,
        y2: i16,
        color: u32,
        line_width: u16,
    },
    DrawRectangle {
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        color: u32,
        line_width: u16,
    },
    FillArc {
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        angle1: i16,
        angle2: i16,
        color: u32,
    },
    DrawArc {
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        angle1: i16,
        angle2: i16,
        color: u32,
        line_width: u16,
    },
    PutImage {
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        depth: u8,
        format: u8,
        data: Vec<u8>,
    },
    CopyArea {
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    },
    DrawText {
        x: i16,
        y: i16,
        text: Vec<u8>,
        font_id: Xid,
        color: u32,
        bg_color: Option<u32>,
    },
    ClearArea {
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        bg_color: u32,
    },
    FillPolygon {
        points: Vec<(i16, i16)>,
        color: u32,
    },
}
