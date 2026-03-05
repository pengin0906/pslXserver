pub mod render;
pub mod xfixes;
pub mod randr;
pub mod xkb;
pub mod xinput2;
pub mod shape;
pub mod xtest;

/// Extension information.
pub struct ExtensionInfo {
    pub name: &'static str,
    pub major_opcode: u8,
    pub first_event: u8,
    pub first_error: u8,
}

/// List of supported extensions (will be populated as we implement them).
pub fn supported_extensions() -> Vec<ExtensionInfo> {
    vec![
        ExtensionInfo { name: "BIG-REQUESTS", major_opcode: 133, first_event: 0, first_error: 0 },
        ExtensionInfo { name: "SHAPE", major_opcode: 134, first_event: 76, first_error: 0 },
        ExtensionInfo { name: "XTEST", major_opcode: 132, first_event: 0, first_error: 0 },
        ExtensionInfo { name: "RENDER", major_opcode: 139, first_event: 0, first_error: 142 },
        // ExtensionInfo { name: "XKEYBOARD", major_opcode: 135, first_event: 85, first_error: 137 },
        // ExtensionInfo { name: "XInputExtension", major_opcode: 131, first_event: 66, first_error: 129 },
        // ExtensionInfo { name: "XFIXES", major_opcode: 138, first_event: 87, first_error: 0 },
        // ExtensionInfo { name: "RANDR", major_opcode: 140, first_event: 89, first_error: 147 },
    ]
}
