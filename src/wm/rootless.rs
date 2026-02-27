// Rootless window manager
//
// Each X11 top-level window (direct child of root) maps to an independent
// macOS NSWindow, fully under macOS Window Manager control.
//
// This means:
// - Each X window can be moved, resized, minimized independently
// - macOS Cmd+Tab shows individual X11 windows
// - No "X11 desktop" window containing sub-windows
// - Dialogs, menus, tooltips are each their own NSWindow

use crate::display::Xid;

/// Tracks the mapping between X11 windows and native macOS windows.
pub struct RootlessManager {
    /// Maps X11 window ID -> native window info.
    windows: std::collections::HashMap<Xid, RootlessWindow>,
}

pub struct RootlessWindow {
    pub x11_id: Xid,
    pub native_id: u64,
    pub visible: bool,
    /// Window type inferred from _NET_WM_WINDOW_TYPE.
    pub window_type: WindowType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowType {
    Normal,
    Dialog,
    Menu,
    Toolbar,
    Utility,
    Splash,
    Dock,
    Tooltip,
    DropdownMenu,
    PopupMenu,
    Notification,
}

impl Default for WindowType {
    fn default() -> Self {
        WindowType::Normal
    }
}

impl RootlessManager {
    pub fn new() -> Self {
        Self {
            windows: std::collections::HashMap::new(),
        }
    }

    pub fn register_window(&mut self, x11_id: Xid, native_id: u64) {
        self.windows.insert(x11_id, RootlessWindow {
            x11_id,
            native_id,
            visible: false,
            window_type: WindowType::Normal,
        });
    }

    pub fn unregister_window(&mut self, x11_id: Xid) {
        self.windows.remove(&x11_id);
    }

    pub fn set_window_type(&mut self, x11_id: Xid, wtype: WindowType) {
        if let Some(win) = self.windows.get_mut(&x11_id) {
            win.window_type = wtype;
        }
    }

    pub fn get_native_id(&self, x11_id: Xid) -> Option<u64> {
        self.windows.get(&x11_id).map(|w| w.native_id)
    }
}
