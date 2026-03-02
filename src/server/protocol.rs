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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_mask_bits_are_unique() {
        // Verify that each event mask bit is a distinct power of 2
        let masks = [
            event_mask::KEY_PRESS, event_mask::KEY_RELEASE,
            event_mask::BUTTON_PRESS, event_mask::BUTTON_RELEASE,
            event_mask::ENTER_WINDOW, event_mask::LEAVE_WINDOW,
            event_mask::POINTER_MOTION, event_mask::POINTER_MOTION_HINT,
            event_mask::BUTTON1_MOTION, event_mask::BUTTON2_MOTION,
            event_mask::BUTTON3_MOTION, event_mask::BUTTON4_MOTION,
            event_mask::BUTTON5_MOTION, event_mask::BUTTON_MOTION,
            event_mask::KEYMAP_STATE, event_mask::EXPOSURE,
            event_mask::VISIBILITY_CHANGE, event_mask::STRUCTURE_NOTIFY,
            event_mask::RESIZE_REDIRECT, event_mask::SUBSTRUCTURE_NOTIFY,
            event_mask::SUBSTRUCTURE_REDIRECT, event_mask::FOCUS_CHANGE,
            event_mask::PROPERTY_CHANGE, event_mask::COLORMAP_CHANGE,
            event_mask::OWNER_GRAB_BUTTON,
        ];
        for (i, &a) in masks.iter().enumerate() {
            assert!(a.is_power_of_two(), "Mask at index {} is not power of 2: 0x{:08x}", i, a);
            for (j, &b) in masks.iter().enumerate() {
                if i != j {
                    assert_eq!(a & b, 0, "Masks at indices {} and {} overlap: 0x{:08x} & 0x{:08x}", i, j, a, b);
                }
            }
        }
    }

    #[test]
    fn test_event_type_codes_match_spec() {
        // Verify key event type codes per X11 protocol spec
        assert_eq!(event_type::KEY_PRESS, 2);
        assert_eq!(event_type::KEY_RELEASE, 3);
        assert_eq!(event_type::BUTTON_PRESS, 4);
        assert_eq!(event_type::BUTTON_RELEASE, 5);
        assert_eq!(event_type::MOTION_NOTIFY, 6);
        assert_eq!(event_type::ENTER_NOTIFY, 7);
        assert_eq!(event_type::LEAVE_NOTIFY, 8);
        assert_eq!(event_type::FOCUS_IN, 9);
        assert_eq!(event_type::FOCUS_OUT, 10);
        assert_eq!(event_type::EXPOSE, 12);
        assert_eq!(event_type::NO_EXPOSURE, 14);
        assert_eq!(event_type::MAP_NOTIFY, 19);
        assert_eq!(event_type::CONFIGURE_NOTIFY, 22);
        assert_eq!(event_type::SELECTION_NOTIFY, 31);
        assert_eq!(event_type::CLIENT_MESSAGE, 33);
        assert_eq!(event_type::MAPPING_NOTIFY, 34);
    }

    #[test]
    fn test_event_mask_exposure_bit() {
        // EXPOSURE is bit 15 per X11 spec
        assert_eq!(event_mask::EXPOSURE, 1 << 15);
        assert_eq!(event_mask::EXPOSURE, 0x8000);
    }

    #[test]
    fn test_event_mask_structure_notify_bit() {
        // STRUCTURE_NOTIFY is bit 17 per X11 spec
        assert_eq!(event_mask::STRUCTURE_NOTIFY, 1 << 17);
        assert_eq!(event_mask::STRUCTURE_NOTIFY, 0x20000);
    }
}
