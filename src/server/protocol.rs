// X11 protocol dispatch — currently handled inline in connection.rs
// This module will be expanded as we add more request handlers.

/// X11 event mask bits used for event delivery filtering.
pub mod event_mask {
    pub const KEY_PRESS: u32 = 1 << 0;
    pub const KEY_RELEASE: u32 = 1 << 1;
    pub const BUTTON_PRESS: u32 = 1 << 2;
    pub const BUTTON_RELEASE: u32 = 1 << 3;
    pub const ENTER_WINDOW: u32 = 1 << 4;
    pub const LEAVE_WINDOW: u32 = 1 << 5;
    pub const POINTER_MOTION: u32 = 1 << 6;
    pub const POINTER_MOTION_HINT: u32 = 1 << 7;
    pub const BUTTON1_MOTION: u32 = 1 << 8;
    pub const BUTTON2_MOTION: u32 = 1 << 9;
    pub const BUTTON3_MOTION: u32 = 1 << 10;
    pub const BUTTON4_MOTION: u32 = 1 << 11;
    pub const BUTTON5_MOTION: u32 = 1 << 12;
    pub const BUTTON_MOTION: u32 = 1 << 13;
    pub const KEYMAP_STATE: u32 = 1 << 14;
    pub const EXPOSURE: u32 = 1 << 15;
    pub const VISIBILITY_CHANGE: u32 = 1 << 16;
    pub const STRUCTURE_NOTIFY: u32 = 1 << 17;
    pub const RESIZE_REDIRECT: u32 = 1 << 18;
    pub const SUBSTRUCTURE_NOTIFY: u32 = 1 << 19;
    pub const SUBSTRUCTURE_REDIRECT: u32 = 1 << 20;
    pub const FOCUS_CHANGE: u32 = 1 << 21;
    pub const PROPERTY_CHANGE: u32 = 1 << 22;
    pub const COLORMAP_CHANGE: u32 = 1 << 23;
    pub const OWNER_GRAB_BUTTON: u32 = 1 << 24;
}

/// X11 event type codes.
pub mod event_type {
    pub const KEY_PRESS: u8 = 2;
    pub const KEY_RELEASE: u8 = 3;
    pub const BUTTON_PRESS: u8 = 4;
    pub const BUTTON_RELEASE: u8 = 5;
    pub const MOTION_NOTIFY: u8 = 6;
    pub const ENTER_NOTIFY: u8 = 7;
    pub const LEAVE_NOTIFY: u8 = 8;
    pub const FOCUS_IN: u8 = 9;
    pub const FOCUS_OUT: u8 = 10;
    pub const KEYMAP_NOTIFY: u8 = 11;
    pub const EXPOSE: u8 = 12;
    pub const GRAPHICS_EXPOSURE: u8 = 13;
    pub const NO_EXPOSURE: u8 = 14;
    pub const VISIBILITY_NOTIFY: u8 = 15;
    pub const CREATE_NOTIFY: u8 = 16;
    pub const DESTROY_NOTIFY: u8 = 17;
    pub const UNMAP_NOTIFY: u8 = 18;
    pub const MAP_NOTIFY: u8 = 19;
    pub const MAP_REQUEST: u8 = 20;
    pub const REPARENT_NOTIFY: u8 = 21;
    pub const CONFIGURE_NOTIFY: u8 = 22;
    pub const CONFIGURE_REQUEST: u8 = 23;
    pub const GRAVITY_NOTIFY: u8 = 24;
    pub const RESIZE_REQUEST: u8 = 25;
    pub const CIRCULATE_NOTIFY: u8 = 26;
    pub const CIRCULATE_REQUEST: u8 = 27;
    pub const PROPERTY_NOTIFY: u8 = 28;
    pub const SELECTION_CLEAR: u8 = 29;
    pub const SELECTION_REQUEST: u8 = 30;
    pub const SELECTION_NOTIFY: u8 = 31;
    pub const COLORMAP_NOTIFY: u8 = 32;
    pub const CLIENT_MESSAGE: u8 = 33;
    pub const MAPPING_NOTIFY: u8 = 34;
}
