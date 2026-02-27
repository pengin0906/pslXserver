// Keyboard input handling — macOS virtual keycodes to X11 keycodes

use bitflags::bitflags;
use std::collections::HashMap;

bitflags! {
    /// X11 modifier state bits.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ModifierState: u16 {
        const SHIFT   = 0x0001;
        const LOCK    = 0x0002; // Caps Lock
        const CONTROL = 0x0004;
        const MOD1    = 0x0008; // Option/Alt
        const MOD2    = 0x0010; // Num Lock
        const MOD3    = 0x0020;
        const MOD4    = 0x0040; // Command (Super)
        const MOD5    = 0x0080;
        const BUTTON1 = 0x0100;
        const BUTTON2 = 0x0200;
        const BUTTON3 = 0x0400;
    }
}

/// Keyboard mapping between macOS and X11.
pub struct KeyboardMap {
    /// macOS virtual keycode (0-127) -> X11 keycode (8-255).
    /// Convention: x11_keycode = macos_keycode + 8 (XQuartz compatible).
    pub keycode_offset: u8,
    /// X11 keycode -> keysyms [normal, shift, option, shift+option].
    pub keysym_map: HashMap<u8, [u32; 4]>,
    /// Current modifier state.
    pub modifier_state: ModifierState,
}

impl KeyboardMap {
    pub fn new() -> Self {
        let mut keysym_map = HashMap::new();

        // Build basic ASCII keysym mappings.
        // XQuartz convention: x11_keycode = macos_keycode + 8.
        // Keysyms follow X11 standard (0x0020-0x007E for printable ASCII,
        // 0xFF00+ for special keys).

        // macOS kVK_ANSI_A=0x00 -> X11 keycode 8
        // ... these will be populated from actual keyboard layout on macOS.
        // For now, provide a minimal mapping:

        // Letters (macOS keycodes from Events.h)
        let letter_keycodes: &[(u8, char)] = &[
            (0x00, 'a'), (0x01, 's'), (0x02, 'd'), (0x03, 'f'),
            (0x04, 'h'), (0x05, 'g'), (0x06, 'z'), (0x07, 'x'),
            (0x08, 'c'), (0x09, 'v'), (0x0B, 'b'), (0x0C, 'q'),
            (0x0D, 'w'), (0x0E, 'e'), (0x0F, 'r'), (0x10, 'y'),
            (0x11, 't'), (0x12, '1'), (0x13, '2'), (0x14, '3'),
            (0x15, '4'), (0x16, '6'), (0x17, '5'), (0x18, '='),
            (0x19, '9'), (0x1A, '7'), (0x1B, '-'), (0x1C, '8'),
            (0x1D, '0'), (0x1E, ']'), (0x1F, 'o'), (0x20, 'u'),
            (0x21, '['), (0x22, 'i'), (0x23, 'p'), (0x25, 'l'),
            (0x26, 'j'), (0x27, '\''), (0x28, 'k'), (0x29, ';'),
            (0x2A, '\\'), (0x2B, ','), (0x2C, '/'), (0x2D, 'n'),
            (0x2E, 'm'), (0x2F, '.'), (0x32, '`'),
        ];

        for &(mac_kc, ch) in letter_keycodes {
            let x11_kc = mac_kc + 8;
            let normal_sym = ch as u32;
            let shift_sym = ch.to_uppercase().next().unwrap_or(ch) as u32;
            keysym_map.insert(x11_kc, [normal_sym, shift_sym, normal_sym, shift_sym]);
        }

        // Special keys
        let special_keycodes: &[(u8, u32)] = &[
            (0x24, 0xFF0D), // Return
            (0x30, 0xFF09), // Tab
            (0x31, 0x0020), // Space
            (0x33, 0xFF08), // Delete (Backspace)
            (0x35, 0xFF1B), // Escape
            (0x7B, 0xFF51), // Left arrow
            (0x7C, 0xFF53), // Right arrow
            (0x7D, 0xFF54), // Down arrow
            (0x7E, 0xFF52), // Up arrow
            (0x73, 0xFF50), // Home
            (0x77, 0xFF57), // End
            (0x74, 0xFF55), // Page Up
            (0x79, 0xFF56), // Page Down
            (0x75, 0xFFFF), // Forward Delete
        ];

        for &(mac_kc, keysym) in special_keycodes {
            let x11_kc = mac_kc.wrapping_add(8);
            keysym_map.insert(x11_kc, [keysym, keysym, keysym, keysym]);
        }

        Self {
            keycode_offset: 8,
            keysym_map,
            modifier_state: ModifierState::empty(),
        }
    }

    /// Convert macOS virtual keycode to X11 keycode.
    pub fn macos_to_x11_keycode(&self, macos_keycode: u16) -> u8 {
        (macos_keycode as u8).wrapping_add(self.keycode_offset)
    }

    /// Convert macOS NSEvent modifier flags to X11 modifier state.
    pub fn ns_modifiers_to_x11(&self, ns_flags: u64) -> u16 {
        let mut state = ModifierState::empty();

        // NSEventModifierFlagShift    = 1 << 17
        if ns_flags & (1 << 17) != 0 { state |= ModifierState::SHIFT; }
        // NSEventModifierFlagControl  = 1 << 18
        if ns_flags & (1 << 18) != 0 { state |= ModifierState::CONTROL; }
        // NSEventModifierFlagOption   = 1 << 19 -> Mod1 (Alt/Meta)
        if ns_flags & (1 << 19) != 0 { state |= ModifierState::MOD1; }
        // NSEventModifierFlagCommand  = 1 << 20 -> Mod4 (Super)
        if ns_flags & (1 << 20) != 0 { state |= ModifierState::MOD4; }
        // NSEventModifierFlagCapsLock = 1 << 16
        if ns_flags & (1 << 16) != 0 { state |= ModifierState::LOCK; }

        state.bits()
    }

    /// Get keysym for a given X11 keycode and modifier state.
    pub fn get_keysym(&self, x11_keycode: u8, state: u16) -> u32 {
        if let Some(syms) = self.keysym_map.get(&x11_keycode) {
            let shift = (state & ModifierState::SHIFT.bits()) != 0;
            let option = (state & ModifierState::MOD1.bits()) != 0;
            let index = match (shift, option) {
                (false, false) => 0,
                (true, false) => 1,
                (false, true) => 2,
                (true, true) => 3,
            };
            syms[index]
        } else {
            0 // NoSymbol
        }
    }
}
