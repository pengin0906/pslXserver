use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, Ordering};

/// Predefined X11 atoms (from X11/Xatom.h).
/// These must have specific IDs as defined by the X11 protocol.
pub mod predefined {
    pub const NONE: u32 = 0;
    pub const PRIMARY: u32 = 1;
    pub const SECONDARY: u32 = 2;
    pub const ARC: u32 = 3;
    pub const ATOM: u32 = 4;
    pub const BITMAP: u32 = 5;
    pub const CARDINAL: u32 = 6;
    pub const COLORMAP: u32 = 7;
    pub const CURSOR: u32 = 8;
    pub const CUT_BUFFER0: u32 = 9;
    pub const CUT_BUFFER1: u32 = 10;
    pub const CUT_BUFFER2: u32 = 11;
    pub const CUT_BUFFER3: u32 = 12;
    pub const CUT_BUFFER4: u32 = 13;
    pub const CUT_BUFFER5: u32 = 14;
    pub const CUT_BUFFER6: u32 = 15;
    pub const CUT_BUFFER7: u32 = 16;
    pub const DRAWABLE: u32 = 17;
    pub const FONT: u32 = 18;
    pub const INTEGER: u32 = 19;
    pub const PIXMAP: u32 = 20;
    pub const POINT: u32 = 21;
    pub const RECTANGLE: u32 = 22;
    pub const RESOURCE_MANAGER: u32 = 23;
    pub const RGB_COLOR_MAP: u32 = 24;
    pub const RGB_BEST_MAP: u32 = 25;
    pub const RGB_BLUE_MAP: u32 = 26;
    pub const RGB_DEFAULT_MAP: u32 = 27;
    pub const RGB_GRAY_MAP: u32 = 28;
    pub const RGB_GREEN_MAP: u32 = 29;
    pub const RGB_RED_MAP: u32 = 30;
    pub const STRING: u32 = 31;
    pub const VISUALID: u32 = 32;
    pub const WINDOW: u32 = 33;
    pub const WM_COMMAND: u32 = 34;
    pub const WM_HINTS: u32 = 35;
    pub const WM_CLIENT_MACHINE: u32 = 36;
    pub const WM_ICON_NAME: u32 = 37;
    pub const WM_ICON_SIZE: u32 = 38;
    pub const WM_NAME: u32 = 39;
    pub const WM_NORMAL_HINTS: u32 = 40;
    pub const WM_SIZE_HINTS: u32 = 41;
    pub const WM_ZOOM_HINTS: u32 = 42;
    pub const MIN_SPACE: u32 = 43;
    pub const NORM_SPACE: u32 = 44;
    pub const MAX_SPACE: u32 = 45;
    pub const END_SPACE: u32 = 46;
    pub const SUPERSCRIPT_X: u32 = 47;
    pub const SUPERSCRIPT_Y: u32 = 48;
    pub const SUBSCRIPT_X: u32 = 49;
    pub const SUBSCRIPT_Y: u32 = 50;
    pub const UNDERLINE_POSITION: u32 = 51;
    pub const UNDERLINE_THICKNESS: u32 = 52;
    pub const STRIKEOUT_ASCENT: u32 = 53;
    pub const STRIKEOUT_DESCENT: u32 = 54;
    pub const ITALIC_ANGLE: u32 = 55;
    pub const X_HEIGHT: u32 = 56;
    pub const QUAD_WIDTH: u32 = 57;
    pub const WEIGHT: u32 = 58;
    pub const POINT_SIZE: u32 = 59;
    pub const RESOLUTION: u32 = 60;
    pub const COPYRIGHT: u32 = 61;
    pub const NOTICE: u32 = 62;
    pub const FONT_NAME: u32 = 63;
    pub const FAMILY_NAME: u32 = 64;
    pub const FULL_NAME: u32 = 65;
    pub const CAP_HEIGHT: u32 = 66;
    pub const WM_CLASS: u32 = 67;
    pub const WM_TRANSIENT_FOR: u32 = 68;

    pub const LAST_PREDEFINED: u32 = 68;
}

/// Names for predefined atoms.
static PREDEFINED_NAMES: &[&str] = &[
    "", // 0 = None
    "PRIMARY",
    "SECONDARY",
    "ARC",
    "ATOM",
    "BITMAP",
    "CARDINAL",
    "COLORMAP",
    "CURSOR",
    "CUT_BUFFER0",
    "CUT_BUFFER1",
    "CUT_BUFFER2",
    "CUT_BUFFER3",
    "CUT_BUFFER4",
    "CUT_BUFFER5",
    "CUT_BUFFER6",
    "CUT_BUFFER7",
    "DRAWABLE",
    "FONT",
    "INTEGER",
    "PIXMAP",
    "POINT",
    "RECTANGLE",
    "RESOURCE_MANAGER",
    "RGB_COLOR_MAP",
    "RGB_BEST_MAP",
    "RGB_BLUE_MAP",
    "RGB_DEFAULT_MAP",
    "RGB_GRAY_MAP",
    "RGB_GREEN_MAP",
    "RGB_RED_MAP",
    "STRING",
    "VISUALID",
    "WINDOW",
    "WM_COMMAND",
    "WM_HINTS",
    "WM_CLIENT_MACHINE",
    "WM_ICON_NAME",
    "WM_ICON_SIZE",
    "WM_NAME",
    "WM_NORMAL_HINTS",
    "WM_SIZE_HINTS",
    "WM_ZOOM_HINTS",
    "MIN_SPACE",
    "NORM_SPACE",
    "MAX_SPACE",
    "END_SPACE",
    "SUPERSCRIPT_X",
    "SUPERSCRIPT_Y",
    "SUBSCRIPT_X",
    "SUBSCRIPT_Y",
    "UNDERLINE_POSITION",
    "UNDERLINE_THICKNESS",
    "STRIKEOUT_ASCENT",
    "STRIKEOUT_DESCENT",
    "ITALIC_ANGLE",
    "X_HEIGHT",
    "QUAD_WIDTH",
    "WEIGHT",
    "POINT_SIZE",
    "RESOLUTION",
    "COPYRIGHT",
    "NOTICE",
    "FONT_NAME",
    "FAMILY_NAME",
    "FULL_NAME",
    "CAP_HEIGHT",
    "WM_CLASS",
    "WM_TRANSIENT_FOR",
];

/// Atom interning table. Maps between atom names and numeric atom IDs.
pub struct AtomTable {
    /// name -> atom ID
    name_to_id: DashMap<String, u32>,
    /// atom ID -> name
    id_to_name: DashMap<u32, String>,
    /// Next available atom ID for dynamic atoms.
    next_id: AtomicU32,
}

impl AtomTable {
    pub fn new() -> Self {
        let name_to_id = DashMap::new();
        let id_to_name = DashMap::new();

        // Register all predefined atoms
        for (i, name) in PREDEFINED_NAMES.iter().enumerate() {
            if i == 0 {
                continue; // Skip None
            }
            name_to_id.insert(name.to_string(), i as u32);
            id_to_name.insert(i as u32, name.to_string());
        }

        Self {
            name_to_id,
            id_to_name,
            next_id: AtomicU32::new(predefined::LAST_PREDEFINED + 1),
        }
    }

    /// Intern an atom. If the atom already exists, return its ID.
    /// If `only_if_exists` is true and the atom doesn't exist, return None.
    pub fn intern(&self, name: &str, only_if_exists: bool) -> Option<u32> {
        // Check if already exists
        if let Some(id) = self.name_to_id.get(name) {
            return Some(*id);
        }

        if only_if_exists {
            return None;
        }

        // Create new atom
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.name_to_id.insert(name.to_string(), id);
        self.id_to_name.insert(id, name.to_string());
        Some(id)
    }

    /// Get the name of an atom by its ID.
    pub fn get_name(&self, id: u32) -> Option<String> {
        self.id_to_name.get(&id).map(|v| v.clone())
    }

    /// Get the ID of an atom by its name (without interning).
    pub fn get_id(&self, name: &str) -> Option<u32> {
        self.name_to_id.get(name).map(|v| *v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_predefined_atoms() {
        let table = AtomTable::new();
        assert_eq!(table.get_id("PRIMARY"), Some(predefined::PRIMARY));
        assert_eq!(table.get_id("WM_NAME"), Some(predefined::WM_NAME));
        assert_eq!(table.get_id("WM_CLASS"), Some(predefined::WM_CLASS));
        assert_eq!(table.get_name(predefined::STRING), Some("STRING".to_string()));
    }

    #[test]
    fn test_intern_new_atom() {
        let table = AtomTable::new();
        let id = table.intern("_NET_WM_NAME", false).unwrap();
        assert!(id > predefined::LAST_PREDEFINED);
        assert_eq!(table.get_name(id), Some("_NET_WM_NAME".to_string()));

        // Interning again returns same ID
        let id2 = table.intern("_NET_WM_NAME", false).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn test_intern_only_if_exists() {
        let table = AtomTable::new();
        // Predefined atom exists
        assert!(table.intern("PRIMARY", true).is_some());
        // Unknown atom with only_if_exists returns None
        assert!(table.intern("DOES_NOT_EXIST", true).is_none());
    }
}
