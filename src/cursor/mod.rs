// Cursor manager — maps X11 cursor font glyph codes to macOS NSCursor types.

/// macOS cursor types supported for X11 cursor mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MacOSCursorType {
    Arrow = 0,
    IBeam = 1,
    Crosshair = 2,
    PointingHand = 3,
    OpenHand = 4,
    ClosedHand = 5,
    ResizeLeftRight = 6,
    ResizeUpDown = 7,
    ResizeLeft = 8,
    ResizeRight = 9,
    ResizeUp = 10,
    ResizeDown = 11,
    OperationNotAllowed = 12,
}

/// Map an X11 cursor font glyph code to a macOS cursor type.
/// The standard cursor font uses even-numbered glyph codes (0, 2, 4, ...).
pub fn x11_cursor_glyph_to_macos(source_char: u16) -> MacOSCursorType {
    match source_char {
        // XC_X_cursor = 0
        0 => MacOSCursorType::Arrow,
        // XC_arrow = 2
        2 => MacOSCursorType::Arrow,
        // XC_based_arrow_down = 4, XC_based_arrow_up = 6
        4 | 6 => MacOSCursorType::Arrow,
        // XC_boat = 8
        8 => MacOSCursorType::Arrow,
        // XC_bogosity = 10
        10 => MacOSCursorType::Arrow,
        // XC_bottom_left_corner = 12
        12 => MacOSCursorType::ResizeDown,
        // XC_bottom_right_corner = 14
        14 => MacOSCursorType::ResizeDown,
        // XC_bottom_side = 16
        16 => MacOSCursorType::ResizeDown,
        // XC_bottom_tee = 18
        18 => MacOSCursorType::ResizeDown,
        // XC_center_ptr = 22
        22 => MacOSCursorType::Arrow,
        // XC_crosshair = 34
        34 => MacOSCursorType::Crosshair,
        // XC_diamond_cross = 36
        36 => MacOSCursorType::Crosshair,
        // XC_double_arrow = 42
        42 => MacOSCursorType::ResizeUpDown,
        // XC_draft_large = 44, XC_draft_small = 46
        44 | 46 => MacOSCursorType::Arrow,
        // XC_draped_box = 48
        48 => MacOSCursorType::Crosshair,
        // XC_exchange = 50
        50 => MacOSCursorType::Arrow,
        // XC_fleur = 52 (move/drag)
        52 => MacOSCursorType::OpenHand,
        // XC_gobbler = 54
        54 => MacOSCursorType::Arrow,
        // XC_gumby = 56
        56 => MacOSCursorType::Arrow,
        // XC_hand1 = 58, XC_hand2 = 60
        58 | 60 => MacOSCursorType::PointingHand,
        // XC_heart = 62
        62 => MacOSCursorType::Arrow,
        // XC_icon = 64
        64 => MacOSCursorType::Arrow,
        // XC_iron_cross = 66
        66 => MacOSCursorType::Crosshair,
        // XC_left_ptr = 68 (standard arrow)
        68 => MacOSCursorType::Arrow,
        // XC_left_side = 70
        70 => MacOSCursorType::ResizeLeft,
        // XC_ll_angle = 76
        76 => MacOSCursorType::Arrow,
        // XC_lr_angle = 78
        78 => MacOSCursorType::Arrow,
        // XC_pencil = 86
        86 => MacOSCursorType::Crosshair,
        // XC_pirate = 88
        88 => MacOSCursorType::OperationNotAllowed,
        // XC_plus = 90
        90 => MacOSCursorType::Crosshair,
        // XC_question_arrow = 92
        92 => MacOSCursorType::Arrow,
        // XC_right_ptr = 94
        94 => MacOSCursorType::Arrow,
        // XC_right_side = 96
        96 => MacOSCursorType::ResizeRight,
        // XC_sb_down_arrow = 106
        106 => MacOSCursorType::ResizeDown,
        // XC_sb_h_double_arrow = 108
        108 => MacOSCursorType::ResizeLeftRight,
        // XC_sb_left_arrow = 110
        110 => MacOSCursorType::ResizeLeft,
        // XC_sb_right_arrow = 112
        112 => MacOSCursorType::ResizeRight,
        // XC_sb_up_arrow = 114
        114 => MacOSCursorType::ResizeUp,
        // XC_sb_v_double_arrow = 116
        116 => MacOSCursorType::ResizeUpDown,
        // XC_sizing = 120
        120 => MacOSCursorType::ResizeLeftRight,
        // XC_target = 128
        128 => MacOSCursorType::Crosshair,
        // XC_tcross = 130
        130 => MacOSCursorType::Crosshair,
        // XC_top_left_arrow = 132
        132 => MacOSCursorType::Arrow,
        // XC_top_left_corner = 134
        134 => MacOSCursorType::ResizeUp,
        // XC_top_right_corner = 136
        136 => MacOSCursorType::ResizeUp,
        // XC_top_side = 138
        138 => MacOSCursorType::ResizeUp,
        // XC_top_tee = 140
        140 => MacOSCursorType::ResizeUp,
        // XC_trek = 142
        142 => MacOSCursorType::Arrow,
        // XC_ul_angle = 144
        144 => MacOSCursorType::Arrow,
        // XC_ur_angle = 146
        146 => MacOSCursorType::Arrow,
        // XC_watch = 150
        150 => MacOSCursorType::OperationNotAllowed,
        // XC_xterm = 152
        152 => MacOSCursorType::IBeam,
        // Default: arrow
        _ => MacOSCursorType::Arrow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cursor_glyph_mapping() {
        assert_eq!(x11_cursor_glyph_to_macos(68), MacOSCursorType::Arrow);
        assert_eq!(x11_cursor_glyph_to_macos(152), MacOSCursorType::IBeam);
        assert_eq!(x11_cursor_glyph_to_macos(34), MacOSCursorType::Crosshair);
        assert_eq!(x11_cursor_glyph_to_macos(58), MacOSCursorType::PointingHand);
        assert_eq!(x11_cursor_glyph_to_macos(60), MacOSCursorType::PointingHand);
        assert_eq!(x11_cursor_glyph_to_macos(52), MacOSCursorType::OpenHand);
        assert_eq!(x11_cursor_glyph_to_macos(108), MacOSCursorType::ResizeLeftRight);
        assert_eq!(x11_cursor_glyph_to_macos(116), MacOSCursorType::ResizeUpDown);
        assert_eq!(x11_cursor_glyph_to_macos(150), MacOSCursorType::OperationNotAllowed);
        assert_eq!(x11_cursor_glyph_to_macos(999), MacOSCursorType::Arrow);
    }

    #[test]
    fn test_all_standard_cursors_mapped() {
        // All even numbers 0-152 should not panic
        for i in (0..=154).step_by(2) {
            let _ = x11_cursor_glyph_to_macos(i);
        }
    }
}
