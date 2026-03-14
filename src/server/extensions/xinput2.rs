//! XInput2 (XI2) — minimal implementation for Chromium/Electron keyboard input.
//!
//! Implements XIQueryVersion, XIQueryDevice, XISelectEvents, XIGetFocus,
//! and related stubs. GenericEvent delivery for XI2 KeyPress/KeyRelease
//! is handled in server/mod.rs.

use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use log::info;

use crate::server::{XServer, ServerError};
use crate::server::connection::{ClientConnection, read_u16, read_u32, write_u16_to, write_u32_to};

// Device IDs
pub const MASTER_POINTER: u16 = 2;
pub const MASTER_KEYBOARD: u16 = 3;
pub const SLAVE_POINTER: u16 = 4;
pub const SLAVE_KEYBOARD: u16 = 5;

// Device types
const XI_MASTER_POINTER: u16 = 1;
const XI_MASTER_KEYBOARD: u16 = 2;
const XI_SLAVE_POINTER: u16 = 3;
const XI_SLAVE_KEYBOARD: u16 = 4;

// XI2 event types
pub const XI_KEY_PRESS: u16 = 2;
pub const XI_KEY_RELEASE: u16 = 3;
pub const XI_BUTTON_PRESS: u16 = 4;
pub const XI_BUTTON_RELEASE: u16 = 5;
pub const XI_MOTION: u16 = 6;
pub const XI_ENTER: u16 = 7;
pub const XI_LEAVE: u16 = 8;
pub const XI_FOCUS_IN: u16 = 9;
pub const XI_FOCUS_OUT: u16 = 10;

// XI2 major opcode (must match mod.rs)
pub const XI2_MAJOR_OPCODE: u8 = 131;

pub async fn handle_xinput2_request<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let sub_opcode = data[1];
    match sub_opcode {
        // Legacy XInput 1.x (sub-opcodes 1-39)
        1 => handle_get_extension_version(server, conn, data, stream).await,
        2 => handle_list_input_devices(server, conn, data, stream).await,
        // Legacy no-reply opcodes
        3 | 4 | 5 | 6 => Ok(()), // OpenDevice, CloseDevice, SetDeviceMode, SelectExtensionEvent
        7 => handle_get_selected_extension_events(server, conn, data, stream).await,
        8..=39 => {
            info!("XI2: legacy sub-opcode {} (no-op)", sub_opcode);
            Ok(()) // legacy opcodes 8-39 are mostly no-ops for us
        }
        // XI2 opcodes (40+)
        40 => handle_xi_query_pointer(server, conn, data, stream).await,
        41 | 42 | 43 => Ok(()), // XIWarpPointer, XIChangeCursor, XIChangeHierarchy — no reply
        44 => Ok(()), // XISetClientPointer — no reply
        45 => handle_xi_get_client_pointer(server, conn, data, stream).await,
        46 => handle_xi_select_events(server, conn, data, stream).await,
        47 => handle_xi_query_version(server, conn, data, stream).await,
        48 => handle_xi_query_device(server, conn, data, stream).await,
        49 => Ok(()), // XISetFocus — no reply
        50 => handle_xi_get_focus(server, conn, data, stream).await,
        51 => handle_xi_grab_device(server, conn, data, stream).await,
        52 | 53 => Ok(()), // XIUngrabDevice, XIAllowEvents — no reply
        54 => handle_xi_passive_grab_device(server, conn, data, stream).await,
        55 => Ok(()), // XIPassiveUngrabDevice — no reply
        56 => handle_xi_list_properties(server, conn, data, stream).await,
        57 | 58 => Ok(()), // XIChangeProperty, XIDeleteProperty — no reply
        59 => handle_xi_get_property(server, conn, data, stream).await,
        60 => handle_xi_get_selected_events(server, conn, data, stream).await,
        _ => {
            info!("XI2: unhandled sub-opcode {}", sub_opcode);
            Ok(())
        }
    }
}

// --- XIQueryVersion (sub-opcode 47) ---
async fn handle_xi_query_version<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional length
    write_u16_to(conn, &mut reply, 2); // major version
    write_u16_to(conn, &mut reply, 0); // minor version
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    info!("XI2 QueryVersion: v2.0");
    Ok(())
}

// --- XIQueryDevice (sub-opcode 48) ---
// Wire format per XI2proto.h:
//   xXIDeviceInfo: 12 bytes (deviceid, use, attachment, num_classes, name_len, enabled, pad)
//   followed by name (padded to 4 bytes), then class structs
//   All class `length` fields are in 4-byte WORDS (not bytes!)
async fn handle_xi_query_device<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let requested_device = if data.len() >= 6 { read_u16(conn, &data[4..6]) } else { 0 };

    let mut body = Vec::new();
    let mut num_devices: u16 = 0;

    let all_devices = requested_device == 0; // XIAllDevices
    let all_masters = requested_device == 1; // XIAllMasterDevices

    // Write xXIDeviceInfo (12 bytes) + padded name + classes
    let write_device_info = |body: &mut Vec<u8>, device_id: u16, dev_type: u16, attachment: u16,
                              num_classes: u16, name: &str, classes: &[u8]| {
        let name_bytes = name.as_bytes();
        let name_pad = (4 - (name_bytes.len() % 4)) % 4;
        write_u16_to(conn, body, device_id);
        write_u16_to(conn, body, dev_type);
        write_u16_to(conn, body, attachment);
        write_u16_to(conn, body, num_classes);
        write_u16_to(conn, body, name_bytes.len() as u16);
        body.push(1); // enabled
        body.push(0); // pad
        body.extend_from_slice(name_bytes);
        body.extend(std::iter::repeat(0).take(name_pad));
        body.extend_from_slice(classes);
    };

    // KeyClass (type=0): 8-byte header + num_keycodes * u32
    // length = header_words(2) + num_keycodes
    let write_key_class = |cls: &mut Vec<u8>, source_id: u16| {
        let num_keycodes: u16 = 248; // keycodes 8-255
        let len_words: u16 = 2 + num_keycodes;
        write_u16_to(conn, cls, 0); // type = KeyClass
        write_u16_to(conn, cls, len_words); // length in 4-byte words
        write_u16_to(conn, cls, source_id);
        write_u16_to(conn, cls, num_keycodes);
        for kc in 8u32..=255u32 {
            write_u32_to(conn, cls, kc);
        }
    };

    // ButtonClass (type=1): 8-byte header + state_mask + labels
    // state_mask = pad_to_4(ceil(num_buttons/8)) bytes
    // length = header_words(2) + mask_words + num_buttons
    let write_button_class = |cls: &mut Vec<u8>, source_id: u16| {
        let num_buttons: u16 = 3;
        let mask_words: u16 = 1; // ceil(3/8)=1 byte, padded to 4 = 1 word
        let len_words: u16 = 2 + mask_words + num_buttons;
        write_u16_to(conn, cls, 1); // type = ButtonClass
        write_u16_to(conn, cls, len_words); // length in 4-byte words
        write_u16_to(conn, cls, source_id);
        write_u16_to(conn, cls, num_buttons);
        // button state bitmask (mask_words * 4 bytes, no pressed buttons)
        write_u32_to(conn, cls, 0);
        // button labels (atoms)
        let btn_left = server.atoms.intern_or_create("Button Left");
        let btn_mid = server.atoms.intern_or_create("Button Middle");
        let btn_right = server.atoms.intern_or_create("Button Right");
        write_u32_to(conn, cls, btn_left);
        write_u32_to(conn, cls, btn_mid);
        write_u32_to(conn, cls, btn_right);
    };

    // ValuatorClass (type=2): fixed 44 bytes = 11 words
    let write_valuator_class = |cls: &mut Vec<u8>, source_id: u16, number: u16, label_atom: u32| {
        write_u16_to(conn, cls, 2); // type = ValuatorClass
        write_u16_to(conn, cls, 11); // length = 44/4 = 11 words
        write_u16_to(conn, cls, source_id);
        write_u16_to(conn, cls, number);
        write_u32_to(conn, cls, label_atom);
        // min (FP3232: i32 integral + u32 frac)
        write_u32_to(conn, cls, 0); write_u32_to(conn, cls, 0);
        // max
        write_u32_to(conn, cls, 0x7FFF); write_u32_to(conn, cls, 0);
        // value
        write_u32_to(conn, cls, 0); write_u32_to(conn, cls, 0);
        // resolution
        write_u32_to(conn, cls, 1);
        // mode(u8) + 3 pad bytes
        cls.push(0); cls.push(0); cls.push(0); cls.push(0);
    };

    // Master Pointer (ID=2): 3 classes (ButtonClass + 2x ValuatorClass)
    if all_devices || all_masters || requested_device == MASTER_POINTER {
        let mut classes = Vec::new();
        write_button_class(&mut classes, SLAVE_POINTER);
        let rel_x = server.atoms.intern_or_create("Rel X");
        let rel_y = server.atoms.intern_or_create("Rel Y");
        write_valuator_class(&mut classes, SLAVE_POINTER, 0, rel_x);
        write_valuator_class(&mut classes, SLAVE_POINTER, 1, rel_y);
        write_device_info(&mut body, MASTER_POINTER, XI_MASTER_POINTER, MASTER_KEYBOARD,
                         3, "Virtual core pointer", &classes);
        num_devices += 1;
    }

    // Master Keyboard (ID=3): 1 class (KeyClass)
    if all_devices || all_masters || requested_device == MASTER_KEYBOARD {
        let mut classes = Vec::new();
        write_key_class(&mut classes, SLAVE_KEYBOARD);
        write_device_info(&mut body, MASTER_KEYBOARD, XI_MASTER_KEYBOARD, MASTER_POINTER,
                         1, "Virtual core keyboard", &classes);
        num_devices += 1;
    }

    // Slave Pointer (ID=4): 3 classes
    if all_devices || requested_device == SLAVE_POINTER {
        let mut classes = Vec::new();
        write_button_class(&mut classes, SLAVE_POINTER);
        let rel_x = server.atoms.intern_or_create("Rel X");
        let rel_y = server.atoms.intern_or_create("Rel Y");
        write_valuator_class(&mut classes, SLAVE_POINTER, 0, rel_x);
        write_valuator_class(&mut classes, SLAVE_POINTER, 1, rel_y);
        write_device_info(&mut body, SLAVE_POINTER, XI_SLAVE_POINTER, MASTER_POINTER,
                         3, "Xerver pointer", &classes);
        num_devices += 1;
    }

    // Slave Keyboard (ID=5): 1 class
    if all_devices || requested_device == SLAVE_KEYBOARD {
        let mut classes = Vec::new();
        write_key_class(&mut classes, SLAVE_KEYBOARD);
        write_device_info(&mut body, SLAVE_KEYBOARD, XI_SLAVE_KEYBOARD, MASTER_KEYBOARD,
                         1, "Xerver keyboard", &classes);
        num_devices += 1;
    }

    // body is always 4-byte aligned (all structs are 4-byte aligned)
    let additional_words = body.len() / 4;

    let mut reply = Vec::with_capacity(32 + body.len());
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, additional_words as u32);
    write_u16_to(conn, &mut reply, num_devices);
    reply.resize(32, 0);
    reply.extend_from_slice(&body);

    stream.write_all(&reply).await?;
    info!("XI2 QueryDevice: {} devices, {} bytes", num_devices, body.len());
    Ok(())
}

// --- XISelectEvents (sub-opcode 46) ---
async fn handle_xi_select_events<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 { return Ok(()); }
    let window = read_u32(conn, &data[4..8]);
    let num_masks = read_u16(conn, &data[8..10]);

    let mut offset = 12;
    for _ in 0..num_masks {
        if offset + 4 > data.len() { break; }
        let deviceid = read_u16(conn, &data[offset..offset+2]);
        let mask_len = read_u16(conn, &data[offset+2..offset+4]) as usize;
        offset += 4;

        let mask_bytes = mask_len * 4;
        let mut xi2_mask: u32 = 0;
        if offset + mask_bytes <= data.len() {
            // Read mask bits — we only care about first 4 bytes (32 event types)
            for i in 0..mask_bytes.min(4) {
                xi2_mask |= (data[offset + i] as u32) << (i * 8);
            }
        }
        offset += mask_bytes;

        // Store the XI2 event mask for this window+connection
        if let Some(res) = server.resources.get(&window) {
            if let crate::server::resources::Resource::Window(win) = res.value() {
                let mut w = win.write();
                // Remove existing mask for this connection+device
                w.xi2_event_selections.retain(|&(cid, did, _)| !(cid == conn.id && did == deviceid));
                if xi2_mask != 0 {
                    w.xi2_event_selections.push((conn.id, deviceid, xi2_mask));
                }
            }
        }

        info!("XI2 SelectEvents: window=0x{:08x} device={} mask=0x{:08x}", window, deviceid, xi2_mask);
    }
    Ok(())
}

// --- XIGetFocus (sub-opcode 50) ---
async fn handle_xi_get_focus<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let focus = server.focus_window.load(std::sync::atomic::Ordering::Relaxed);

    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, focus);
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    Ok(())
}

// --- XIGetClientPointer (sub-opcode 45) ---
async fn handle_xi_get_client_pointer<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(1); // set = true
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u16_to(conn, &mut reply, MASTER_POINTER); // device_id
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    Ok(())
}

// --- XIListProperties (sub-opcode 56) ---
async fn handle_xi_list_properties<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1);
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u16_to(conn, &mut reply, 0); // num_properties
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    Ok(())
}

// --- XIGetProperty (sub-opcode 59) ---
async fn handle_xi_get_property<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1);
    reply.push(0); // format = 0 (not found)
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, 0); // type = None
    write_u32_to(conn, &mut reply, 0); // bytes_after
    write_u32_to(conn, &mut reply, 0); // num_items
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    Ok(())
}

// --- GetExtensionVersion (sub-opcode 1, legacy) ---
async fn handle_get_extension_version<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u16_to(conn, &mut reply, 1); // server major version (XI 1.x legacy)
    write_u16_to(conn, &mut reply, 1); // server minor version
    reply.push(1); // present = true
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    info!("XI GetExtensionVersion: v1.1 (legacy)");
    Ok(())
}

// --- ListInputDevices (sub-opcode 2, legacy) ---
async fn handle_list_input_devices<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();

    let pointer_atom = server.atoms.intern_or_create("MOUSE");
    let keyboard_atom = server.atoms.intern_or_create("KEYBOARD");

    struct DevInfo {
        type_atom: u32,
        id: u8,
        use_type: u8, // 0=IsXPointer, 1=IsXKeyboard, 3=IsXExtensionPointer, 4=IsXExtensionKeyboard
        name: &'static str,
        has_key_class: bool,
    }
    let devices = [
        DevInfo { type_atom: pointer_atom, id: MASTER_POINTER as u8, use_type: 0, name: "Virtual core pointer", has_key_class: false },
        DevInfo { type_atom: keyboard_atom, id: MASTER_KEYBOARD as u8, use_type: 1, name: "Virtual core keyboard", has_key_class: true },
        DevInfo { type_atom: pointer_atom, id: SLAVE_POINTER as u8, use_type: 4, name: "Xerver pointer", has_key_class: false },
        DevInfo { type_atom: keyboard_atom, id: SLAVE_KEYBOARD as u8, use_type: 3, name: "Xerver keyboard", has_key_class: true },
    ];

    // Wire format: xDeviceInfo[N] + xInputClassInfo[] + xStrings
    let mut body = Vec::new();

    // 1. All xDeviceInfo entries (8 bytes each)
    for dev in &devices {
        // xDeviceInfo: type(CARD32), id(CARD8), num_classes(CARD8), use(CARD8), attached(CARD8)
        write_u32_to(conn, &mut body, dev.type_atom); // type atom
        body.push(dev.id);
        body.push(if dev.has_key_class { 1 } else { 0 }); // num_classes
        body.push(dev.use_type);
        body.push(0); // attached
    }

    // 2. Class infos (immediately after all device infos)
    for dev in &devices {
        if dev.has_key_class {
            // xKeyInfo: class(CARD8=0), length(CARD8=6), min_keycode, max_keycode, num_keys(CARD16)
            body.push(0); // class = KeyClass
            body.push(6); // length in bytes
            body.push(8); // min_keycode
            body.push(255u8); // max_keycode
            write_u16_to(conn, &mut body, 248); // num_keys
        }
    }

    // 3. Device names as xStrings (length-prefixed, no null terminator)
    for dev in &devices {
        let name_bytes = dev.name.as_bytes();
        body.push(name_bytes.len() as u8);
        body.extend_from_slice(name_bytes);
    }

    // Pad to 4-byte boundary
    let pad_len = (4 - (body.len() % 4)) % 4;
    body.extend(std::iter::repeat(0).take(pad_len));

    let additional_words = body.len() / 4;
    let mut reply = Vec::with_capacity(32 + body.len());
    reply.push(1); // reply
    reply.push(0); // xi_reply_type
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, additional_words as u32);
    reply.push(devices.len() as u8); // num_devices
    reply.resize(32, 0); // pad to 32 bytes
    reply.extend_from_slice(&body);

    stream.write_all(&reply).await?;
    info!("XI ListInputDevices: {} devices", devices.len());
    Ok(())
}

// --- GetSelectedExtensionEvents (sub-opcode 7, legacy) ---
async fn handle_get_selected_extension_events<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1);
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u16_to(conn, &mut reply, 0); // num_this_classes
    write_u16_to(conn, &mut reply, 0); // num_all_classes
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    Ok(())
}

// --- XIQueryPointer (sub-opcode 40) ---
async fn handle_xi_query_pointer<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let (mx, my) = {
        let x = server.pointer_x.load(std::sync::atomic::Ordering::Relaxed);
        let y = server.pointer_y.load(std::sync::atomic::Ordering::Relaxed);
        (x as i16, y as i16)
    };
    let root_wid = server.screens[0].root_window;
    let mods = server.modifier_state.load(std::sync::atomic::Ordering::Relaxed);

    // Reply: 56 bytes = 32 header + 24 body (buttons_len=1 → 4 bytes mask + mods)
    let buttons_len: u16 = 1; // 1 u32 of button mask
    let additional_words = (24 + buttons_len as u32 * 4) / 4;
    let mut reply = Vec::with_capacity(56);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, additional_words);
    write_u32_to(conn, &mut reply, root_wid); // root
    write_u32_to(conn, &mut reply, 0); // child
    // root_x, root_y as FP16.16
    write_u32_to(conn, &mut reply, ((mx as i32) << 16) as u32);
    write_u32_to(conn, &mut reply, ((my as i32) << 16) as u32);
    // win_x, win_y as FP16.16
    write_u32_to(conn, &mut reply, ((mx as i32) << 16) as u32);
    write_u32_to(conn, &mut reply, ((my as i32) << 16) as u32);
    reply.push(1); // same_screen
    reply.push(0); // pad
    write_u16_to(conn, &mut reply, buttons_len);
    // mods: base, latched, locked, effective
    let mods_val = (mods & 0xFF) as u32;
    write_u32_to(conn, &mut reply, mods_val);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, mods_val);
    // group
    reply.push(0); reply.push(0); reply.push(0); reply.push(0);
    // buttons mask
    write_u32_to(conn, &mut reply, 0);

    stream.write_all(&reply).await?;
    Ok(())
}

// --- XIGrabDevice (sub-opcode 51) ---
async fn handle_xi_grab_device<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1);
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    reply.push(0); // status = Success (0=GrabSuccess)
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    info!("XI2 GrabDevice: GrabSuccess");
    Ok(())
}

// --- XIPassiveGrabDevice (sub-opcode 54) ---
async fn handle_xi_passive_grab_device<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1);
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u16_to(conn, &mut reply, 0); // num_modifiers (0 = all succeeded)
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    info!("XI2 PassiveGrabDevice: success");
    Ok(())
}

// --- XIGetSelectedEvents (sub-opcode 60) ---
async fn handle_xi_get_selected_events<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1);
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u16_to(conn, &mut reply, 0); // num_masks
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    Ok(())
}

/// Build an XI2 GenericEvent for KeyPress/KeyRelease.
/// Returns an 80-byte event buffer.
pub fn build_xi2_key_event(
    conn: &ClientConnection,
    evtype: u16, // XI_KEY_PRESS or XI_KEY_RELEASE
    keycode: u8,
    time: u32,
    root: u32,
    event_window: u32,
    child: u32,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
    state: u16,
) -> Vec<u8> {
    let mut evt = Vec::with_capacity(80);
    evt.push(35); // type = GenericEvent
    evt.push(XI2_MAJOR_OPCODE); // extension
    write_u16_to(conn, &mut evt, 0); // sequence (stamped later)
    write_u32_to(conn, &mut evt, 12); // length = (80-32)/4 = 12
    write_u16_to(conn, &mut evt, evtype); // evtype
    write_u16_to(conn, &mut evt, MASTER_KEYBOARD); // deviceid
    write_u32_to(conn, &mut evt, time);
    write_u32_to(conn, &mut evt, keycode as u32); // detail
    write_u32_to(conn, &mut evt, root);
    write_u32_to(conn, &mut evt, event_window);
    write_u32_to(conn, &mut evt, child);
    // root_x, root_y as FP16.16 (integer in high 16 bits)
    write_u32_to(conn, &mut evt, ((root_x as i32) << 16) as u32);
    write_u32_to(conn, &mut evt, ((root_y as i32) << 16) as u32);
    write_u32_to(conn, &mut evt, ((event_x as i32) << 16) as u32);
    write_u32_to(conn, &mut evt, ((event_y as i32) << 16) as u32);
    write_u16_to(conn, &mut evt, 0); // buttons_len
    write_u16_to(conn, &mut evt, 0); // valuators_len
    write_u16_to(conn, &mut evt, SLAVE_KEYBOARD); // sourceid
    write_u16_to(conn, &mut evt, 0); // pad
    write_u32_to(conn, &mut evt, 0); // flags
    // mods: base, latched, locked, effective (each u32)
    let mods8 = (state & 0xFF) as u32;
    let locked = if state & 2 != 0 { 2u32 } else { 0 };
    write_u32_to(conn, &mut evt, mods8);  // base_mods
    write_u32_to(conn, &mut evt, 0);      // latched_mods
    write_u32_to(conn, &mut evt, locked); // locked_mods
    write_u32_to(conn, &mut evt, mods8);  // effective_mods
    // group: base(u8), latched(u8), locked(u8), effective(u8)
    evt.push(0); evt.push(0); evt.push(0); evt.push(0);

    debug_assert_eq!(evt.len(), 80);
    evt
}
