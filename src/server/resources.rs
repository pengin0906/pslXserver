use std::sync::Arc;
use parking_lot::RwLock;

use crate::display::{NativeWindowHandle, Xid};
use crate::util::coord::X11Point;

/// Resources managed by the X server: windows, pixmaps, GCs, fonts, cursors.
#[derive(Debug)]
pub enum Resource {
    Window(Arc<RwLock<WindowState>>),
    Pixmap(Arc<RwLock<PixmapState>>),
    GContext(Arc<RwLock<GContextState>>),
    Font(Arc<RwLock<FontState>>),
    Cursor(Arc<CursorState>),
}

/// State for an X11 window.
#[derive(Debug)]
pub struct WindowState {
    pub id: Xid,
    pub parent: Xid,
    pub children: Vec<Xid>,

    // Geometry (X11 coordinates: physical pixels, top-left origin)
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub depth: u8,
    pub class: WindowClass,
    pub visual: u32,

    // Attributes
    pub background_pixel: Option<u32>,
    pub border_pixel: Option<u32>,
    pub event_mask: u32,
    pub do_not_propagate_mask: u32,
    pub override_redirect: bool,
    pub backing_store: u8,
    pub colormap: Xid,
    pub cursor: Xid,
    pub bit_gravity: u8,
    pub win_gravity: u8,

    // Client event selections: (connection_id, event_mask)
    pub event_selections: Vec<(u32, u32)>,

    // XI2 event selections: (connection_id, device_id, xi2_mask_bits)
    pub xi2_event_selections: Vec<(u32, u16, u32)>,

    // State
    pub mapped: bool,
    pub viewable: bool,

    // Properties
    pub properties: Vec<Property>,

    // Connection to macOS native window (top-level windows only)
    pub native_window: Option<NativeWindowHandle>,

    // Backing store pixel data
    pub backing_buffer: Option<Vec<u8>>,

    // IME state
    pub ime_spot: Option<X11Point>,
    pub ime_focus: bool,
}

impl WindowState {
    pub fn new(
        id: Xid,
        parent: Xid,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        depth: u8,
        class: WindowClass,
        visual: u32,
    ) -> Self {
        Self {
            id,
            parent,
            children: Vec::new(),
            x,
            y,
            width,
            height,
            border_width,
            depth,
            class,
            visual,
            background_pixel: None,
            border_pixel: None,
            event_mask: 0,
            do_not_propagate_mask: 0,
            override_redirect: false,
            backing_store: 0,
            colormap: 0,
            cursor: 0,
            bit_gravity: 0,  // ForgetGravity
            win_gravity: 1,  // NorthWestGravity
            event_selections: Vec::new(),
            xi2_event_selections: Vec::new(),
            mapped: false,
            viewable: false,
            properties: Vec::new(),
            native_window: None,
            backing_buffer: None,
            ime_spot: None,
            ime_focus: false,
        }
    }

    /// Check if a client should receive events of the given type for this window.
    pub fn should_deliver_event(&self, conn_id: u32, event_mask_bit: u32) -> bool {
        for &(cid, mask) in &self.event_selections {
            if cid == conn_id && (mask & event_mask_bit) != 0 {
                return true;
            }
        }
        false
    }

    /// Get or set a property.
    pub fn get_property(&self, atom: u32) -> Option<&Property> {
        self.properties.iter().find(|p| p.name == atom)
    }

    pub fn set_property(&mut self, prop: Property) {
        if let Some(existing) = self.properties.iter_mut().find(|p| p.name == prop.name) {
            *existing = prop;
        } else {
            self.properties.push(prop);
        }
    }

    pub fn delete_property(&mut self, atom: u32) {
        self.properties.retain(|p| p.name != atom);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowClass {
    CopyFromParent,
    InputOutput,
    InputOnly,
}

impl From<u16> for WindowClass {
    fn from(v: u16) -> Self {
        match v {
            0 => WindowClass::CopyFromParent,
            1 => WindowClass::InputOutput,
            2 => WindowClass::InputOnly,
            _ => WindowClass::CopyFromParent,
        }
    }
}

/// X11 window property.
#[derive(Debug, Clone)]
pub struct Property {
    pub name: u32,       // Atom
    pub type_atom: u32,  // Atom for the property type
    pub format: u8,      // 8, 16, or 32
    pub data: Vec<u8>,
}

/// State for a pixmap.
#[derive(Debug)]
pub struct PixmapState {
    pub id: Xid,
    pub drawable: Xid, // root window typically
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub data: Vec<u8>,  // ARGB8888 pixel data
}

/// State for a graphics context.
#[derive(Debug)]
pub struct GContextState {
    pub id: Xid,
    pub drawable: Xid,
    pub function: GcFunction,
    pub plane_mask: u32,
    pub foreground: u32,
    pub background: u32,
    pub line_width: u16,
    pub line_style: u8,
    pub cap_style: u8,
    pub join_style: u8,
    pub fill_style: u8,
    pub fill_rule: u8,
    pub arc_mode: u8,
    pub font: Xid,
    pub subwindow_mode: u8,
    pub graphics_exposures: bool,
    pub clip_x_origin: i16,
    pub clip_y_origin: i16,
    pub clip_mask: Xid,
    pub dash_offset: u16,
    pub dashes: u8,
}

impl GContextState {
    pub fn new(id: Xid, drawable: Xid) -> Self {
        Self {
            id,
            drawable,
            function: GcFunction::Copy,
            plane_mask: 0xFFFFFFFF,
            foreground: 0x00000000,
            background: 0x00FFFFFF,
            line_width: 0,
            line_style: 0, // LineSolid
            cap_style: 1,  // CapButt
            join_style: 0, // JoinMiter
            fill_style: 0, // FillSolid
            fill_rule: 0,  // EvenOddRule
            arc_mode: 1,   // ArcPieSlice
            font: 0,
            subwindow_mode: 0, // ClipByChildren
            graphics_exposures: true,
            clip_x_origin: 0,
            clip_y_origin: 0,
            clip_mask: 0, // None
            dash_offset: 0,
            dashes: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcFunction {
    Clear,
    And,
    AndReverse,
    Copy,
    AndInverted,
    Noop,
    Xor,
    Or,
    Nor,
    Equiv,
    Invert,
    OrReverse,
    CopyInverted,
    OrInverted,
    Nand,
    Set,
}

impl From<u8> for GcFunction {
    fn from(v: u8) -> Self {
        match v {
            0 => GcFunction::Clear,
            1 => GcFunction::And,
            2 => GcFunction::AndReverse,
            3 => GcFunction::Copy,
            4 => GcFunction::AndInverted,
            5 => GcFunction::Noop,
            6 => GcFunction::Xor,
            7 => GcFunction::Or,
            8 => GcFunction::Nor,
            9 => GcFunction::Equiv,
            10 => GcFunction::Invert,
            11 => GcFunction::OrReverse,
            12 => GcFunction::CopyInverted,
            13 => GcFunction::OrInverted,
            14 => GcFunction::Nand,
            15 => GcFunction::Set,
            _ => GcFunction::Copy,
        }
    }
}

/// Font state.
#[derive(Debug)]
pub struct FontState {
    pub id: Xid,
    pub name: String,
    pub ascent: i16,
    pub descent: i16,
    pub max_char_width: i16,
    pub min_char_width: i16,
    pub default_char: u16,
}

/// Cursor state.
#[derive(Debug)]
pub struct CursorState {
    pub id: Xid,
    pub source_font: Xid,
    pub source_char: u16,
    pub mask_font: Xid,
    pub mask_char: u16,
    pub fore_red: u16,
    pub fore_green: u16,
    pub fore_blue: u16,
    pub back_red: u16,
    pub back_green: u16,
    pub back_blue: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_state_defaults() {
        let w = WindowState::new(0x100, 0x80, 10, 20, 640, 480, 0, 24, WindowClass::InputOutput, 0x21);
        assert_eq!(w.id, 0x100);
        assert_eq!(w.parent, 0x80);
        assert_eq!(w.x, 10);
        assert_eq!(w.y, 20);
        assert_eq!(w.width, 640);
        assert_eq!(w.height, 480);
        assert_eq!(w.depth, 24);
        assert_eq!(w.class, WindowClass::InputOutput);
        assert!(!w.mapped);
        assert!(!w.viewable);
        assert!(!w.override_redirect);
        assert_eq!(w.background_pixel, None);
        assert_eq!(w.border_pixel, None);
        assert_eq!(w.event_mask, 0);
        assert_eq!(w.cursor, 0);
        assert_eq!(w.bit_gravity, 0); // ForgetGravity
        assert_eq!(w.win_gravity, 1); // NorthWestGravity
        assert!(w.children.is_empty());
        assert!(w.properties.is_empty());
        assert!(w.native_window.is_none());
    }

    #[test]
    fn test_window_class_from_u16() {
        assert_eq!(WindowClass::from(0), WindowClass::CopyFromParent);
        assert_eq!(WindowClass::from(1), WindowClass::InputOutput);
        assert_eq!(WindowClass::from(2), WindowClass::InputOnly);
        assert_eq!(WindowClass::from(99), WindowClass::CopyFromParent); // Unknown defaults
    }

    #[test]
    fn test_property_set_get_delete() {
        let mut w = WindowState::new(1, 0, 0, 0, 100, 100, 0, 24, WindowClass::InputOutput, 0x21);

        // Set property
        w.set_property(Property { name: 39, type_atom: 31, format: 8, data: vec![72, 101, 108, 108, 111] });
        assert!(w.get_property(39).is_some());
        assert_eq!(w.get_property(39).unwrap().data, vec![72, 101, 108, 108, 111]);
        assert_eq!(w.get_property(39).unwrap().type_atom, 31);

        // Update property (same name atom)
        w.set_property(Property { name: 39, type_atom: 31, format: 8, data: vec![87, 111, 114, 108, 100] });
        assert_eq!(w.get_property(39).unwrap().data, vec![87, 111, 114, 108, 100]);
        assert_eq!(w.properties.len(), 1); // No duplicate

        // Delete property
        w.delete_property(39);
        assert!(w.get_property(39).is_none());
        assert!(w.properties.is_empty());
    }

    #[test]
    fn test_event_delivery() {
        let mut w = WindowState::new(1, 0, 0, 0, 100, 100, 0, 24, WindowClass::InputOutput, 0x21);

        // No selections yet
        assert!(!w.should_deliver_event(1, 0x0001));

        // Add selection for conn 1
        w.event_selections.push((1, 0x0003)); // KEY_PRESS | KEY_RELEASE

        assert!(w.should_deliver_event(1, 0x0001));  // KEY_PRESS
        assert!(w.should_deliver_event(1, 0x0002));  // KEY_RELEASE
        assert!(!w.should_deliver_event(1, 0x0004)); // BUTTON_PRESS not selected
        assert!(!w.should_deliver_event(2, 0x0001)); // Different connection
    }

    #[test]
    fn test_gc_state_defaults() {
        let gc = GContextState::new(0x200, 0x100);
        assert_eq!(gc.id, 0x200);
        assert_eq!(gc.drawable, 0x100);
        assert_eq!(gc.function, GcFunction::Copy);
        assert_eq!(gc.plane_mask, 0xFFFFFFFF);
        assert_eq!(gc.foreground, 0x00000000);
        assert_eq!(gc.background, 0x00FFFFFF);
        assert_eq!(gc.line_width, 0);
        assert!(gc.graphics_exposures);
        assert_eq!(gc.font, 0);
    }

    #[test]
    fn test_gc_function_from_u8() {
        assert_eq!(GcFunction::from(0), GcFunction::Clear);
        assert_eq!(GcFunction::from(3), GcFunction::Copy);
        assert_eq!(GcFunction::from(6), GcFunction::Xor);
        assert_eq!(GcFunction::from(15), GcFunction::Set);
        assert_eq!(GcFunction::from(99), GcFunction::Copy); // Unknown defaults to Copy
    }

    #[test]
    fn test_multiple_event_selections() {
        let mut w = WindowState::new(1, 0, 0, 0, 100, 100, 0, 24, WindowClass::InputOutput, 0x21);

        // Two different connections select different events
        w.event_selections.push((1, 0x8001)); // KEY_PRESS | EXPOSURE
        w.event_selections.push((2, 0x0004)); // BUTTON_PRESS

        // Conn 1 gets KEY_PRESS and EXPOSURE
        assert!(w.should_deliver_event(1, 0x0001));
        assert!(w.should_deliver_event(1, 0x8000));
        assert!(!w.should_deliver_event(1, 0x0004));

        // Conn 2 gets BUTTON_PRESS only
        assert!(w.should_deliver_event(2, 0x0004));
        assert!(!w.should_deliver_event(2, 0x0001));
    }
}
