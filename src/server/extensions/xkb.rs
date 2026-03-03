//! XKB (X Keyboard Extension) — minimal implementation for Chromium/Electron.
//!
//! Implements just enough of the XKEYBOARD extension for libxkbcommon to
//! build a keymap and for Chromium to accept keyboard input.

use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use log::info;

use crate::server::{XServer, ServerError};
use crate::server::connection::{
    ClientConnection, read_u16, write_u16_to, write_u32_to, macos_keycode_to_keysym,
};

const MIN_KEYCODE: u8 = 8;
const MAX_KEYCODE: u8 = 255;

pub async fn handle_xkb_request<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let sub_opcode = data[1];
    match sub_opcode {
        0 => handle_use_extension(server, conn, data, stream).await,
        1 => handle_select_events(server, conn, data, stream).await,
        4 => handle_get_state(server, conn, data, stream).await,
        5 => Ok(()), // LatchLockState — no-op
        6 => handle_get_controls(server, conn, data, stream).await,
        7 => Ok(()), // SetControls — no-op
        8 => handle_get_map(server, conn, data, stream).await,
        9 => Ok(()), // SetMap — no-op
        10 => handle_get_compat_map(server, conn, data, stream).await,
        11 => Ok(()), // SetCompatMap — no-op
        12 => handle_get_indicator_state(server, conn, data, stream).await,
        13 => handle_get_indicator_map(server, conn, data, stream).await,
        17 => handle_get_names(server, conn, data, stream).await,
        21 => handle_per_client_flags(server, conn, data, stream).await,
        _ => {
            info!("XKB: unhandled sub-opcode {}", sub_opcode);
            Ok(()) // silently ignore unknown XKB requests
        }
    }
}

// --- UseExtension (sub-opcode 0) ---
async fn handle_use_extension<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(1); // supported = true
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional length
    write_u16_to(conn, &mut reply, 1); // server major version
    write_u16_to(conn, &mut reply, 0); // server minor version
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    info!("XKB UseExtension: enabled v1.0");
    Ok(())
}

// --- SelectEvents (sub-opcode 1) ---
async fn handle_select_events<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // Accept and ignore — we send XKB events based on core keyboard events
    Ok(())
}

// --- GetState (sub-opcode 4) ---
async fn handle_get_state<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mods = server.modifier_state.load(std::sync::atomic::Ordering::Relaxed);
    let mods8 = (mods & 0xFF) as u8;

    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(3); // device_id = core keyboard
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional length
    reply.push(mods8); // mods
    reply.push(mods8); // base_mods
    reply.push(0);     // latched_mods
    reply.push(if mods & 2 != 0 { 2 } else { 0 }); // locked_mods (CapsLock)
    reply.push(0); // group
    reply.push(0); // locked_group
    write_u16_to(conn, &mut reply, 0); // base_group (i16)
    write_u16_to(conn, &mut reply, 0); // latched_group (i16)
    reply.push(mods8); // compat_state
    reply.push(mods8); // grab_mods
    reply.push(mods8); // compat_grab_mods
    reply.push(mods8); // lookup_mods
    reply.push(mods8); // compat_lookup_mods
    reply.push(0);     // pad
    write_u16_to(conn, &mut reply, 0); // ptr_btn_state
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    Ok(())
}

// --- GetControls (sub-opcode 6) ---
async fn handle_get_controls<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    // Reply is 32 header + 60 body = 92 bytes total
    let mut reply = Vec::with_capacity(92);
    reply.push(1); // reply
    reply.push(3); // device_id
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 15); // additional length = 60/4 = 15
    // mouse_keys_dflt_btn(u8), num_groups(u8), groups_wrap(u8), internal_mods(mask,mods,vmods=4 bytes)
    reply.push(0); // mouse_keys_dflt_btn
    reply.push(1); // num_groups
    reply.push(0); // groups_wrap
    reply.push(0); // internal_mods_mask
    reply.push(0); // internal_mods_mods
    reply.push(0); // internal_mods_vmods high
    reply.push(0); // internal_mods_vmods low
    reply.push(0); // ignore_lock_mods_mask
    reply.push(0); // ignore_lock_mods_mods
    reply.push(0); // ignore_lock_mods_vmods high
    reply.push(0); // ignore_lock_mods_vmods low
    // enabled_ctrls(u32), repeat_delay(u16), repeat_interval(u16)
    write_u32_to(conn, &mut reply, 1 << 0); // RepeatKeys enabled
    write_u16_to(conn, &mut reply, 660);    // repeat_delay (ms)
    write_u16_to(conn, &mut reply, 40);     // repeat_interval (ms)
    // slow_keys_delay(u16), debounce_delay(u16), mouse_keys_delay(u16),
    // mouse_keys_interval(u16), mouse_keys_time_to_max(u16), mouse_keys_max_speed(u16),
    // mouse_keys_curve(i16)
    write_u16_to(conn, &mut reply, 0); // slow_keys_delay
    write_u16_to(conn, &mut reply, 0); // debounce_delay
    write_u16_to(conn, &mut reply, 0); // mouse_keys_delay
    write_u16_to(conn, &mut reply, 0); // mouse_keys_interval
    write_u16_to(conn, &mut reply, 0); // mouse_keys_time_to_max
    write_u16_to(conn, &mut reply, 0); // mouse_keys_max_speed
    write_u16_to(conn, &mut reply, 0); // mouse_keys_curve
    // access_x_option(u16), access_x_timeout(u16), access_x_timeout_options_mask(u16),
    // access_x_timeout_options_values(u16), access_x_timeout_mask(u32), access_x_timeout_values(u32)
    write_u16_to(conn, &mut reply, 0);
    write_u16_to(conn, &mut reply, 0);
    write_u16_to(conn, &mut reply, 0);
    write_u16_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, 0);
    // per_key_repeat: 32 bytes (bit mask, all keys repeat)
    reply.extend_from_slice(&[0xFF; 32]);
    stream.write_all(&reply).await?;
    Ok(())
}

// --- GetMap (sub-opcode 8) --- Most complex handler
async fn handle_get_map<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let full = if data.len() >= 8 { read_u16(conn, &data[6..8]) } else { 0x03 };

    let want_types = (full & 0x01) != 0;
    let want_syms = (full & 0x02) != 0;
    let want_modmap = (full & 0x04) != 0;

    // Build body components
    let mut types_data = Vec::new();
    let n_types: u8;
    let total_types: u8;
    if want_types {
        n_types = 3;
        total_types = 3;
        // Type 0: ONE_LEVEL — no modifiers, 1 level
        types_data.push(0);   // mods_mask
        types_data.push(0);   // mods_mods
        write_u16_to(conn, &mut types_data, 0); // mods_vmods
        types_data.push(1);   // num_levels
        types_data.push(0);   // n_map_entries
        types_data.push(0);   // has_preserve (bool)
        types_data.push(0);   // pad

        // Type 1: TWO_LEVEL — Shift modifier, 2 levels
        types_data.push(1);   // mods_mask = ShiftMask
        types_data.push(1);   // mods_mods = ShiftMask
        write_u16_to(conn, &mut types_data, 0);
        types_data.push(2);   // num_levels
        types_data.push(1);   // n_map_entries
        types_data.push(0);   // has_preserve
        types_data.push(0);   // pad
        // Map entry: Shift → level 2
        types_data.push(1);   // active = true
        types_data.push(1);   // mods_mask = ShiftMask
        types_data.push(1);   // level = 1 (0-indexed)
        types_data.push(1);   // mods_mods = ShiftMask
        write_u16_to(conn, &mut types_data, 0); // mods_vmods
        write_u16_to(conn, &mut types_data, 0); // pad

        // Type 2: ALPHABETIC — Shift + Lock, 2 levels
        types_data.push(3);   // mods_mask = Shift|Lock
        types_data.push(3);   // mods_mods
        write_u16_to(conn, &mut types_data, 0);
        types_data.push(2);   // num_levels
        types_data.push(2);   // n_map_entries
        types_data.push(0);   // has_preserve
        types_data.push(0);   // pad
        // Entry 1: Shift → level 2
        types_data.push(1); types_data.push(1); types_data.push(1); types_data.push(1);
        write_u16_to(conn, &mut types_data, 0); write_u16_to(conn, &mut types_data, 0);
        // Entry 2: Lock → level 2
        types_data.push(1); types_data.push(2); types_data.push(1); types_data.push(2);
        write_u16_to(conn, &mut types_data, 0); write_u16_to(conn, &mut types_data, 0);
    } else {
        n_types = 0;
        total_types = 0;
    }

    // Build KeySyms
    let mut syms_data = Vec::new();
    let first_key_sym = MIN_KEYCODE;
    let n_key_syms = MAX_KEYCODE - MIN_KEYCODE + 1;
    let mut total_syms: u16 = 0;

    if want_syms {
        let vk = server.virtual_keysyms.read();
        for keycode in MIN_KEYCODE..=MAX_KEYCODE {
            let mac_key = (keycode as u32).wrapping_sub(8);
            let (normal, shifted) = if keycode >= 200 {
                let idx = (keycode as usize) - 200;
                if idx < vk.len() { (vk[idx], vk[idx]) } else { (0, 0) }
            } else {
                macos_keycode_to_keysym(mac_key)
            };

            // Determine key type
            let is_alpha = normal >= 0x0061 && normal <= 0x007A; // a-z
            let type_idx: u8 = if is_alpha {
                2 // ALPHABETIC
            } else if normal != shifted && shifted != 0 {
                1 // TWO_LEVEL
            } else {
                0 // ONE_LEVEL
            };

            let width: u8 = if type_idx == 0 { 1 } else { 2 };
            let n_syms_this: u16 = width as u16;

            // kt_index[4] — key type for each of 4 groups
            syms_data.push(type_idx);
            syms_data.push(0);
            syms_data.push(0);
            syms_data.push(0);
            // group_info: numGroups=1 (bits 0-3)
            syms_data.push(1);
            // width
            syms_data.push(width);
            // n_syms (u16)
            write_u16_to(conn, &mut syms_data, n_syms_this);
            // keysyms
            write_u32_to(conn, &mut syms_data, normal);
            if width == 2 {
                write_u32_to(conn, &mut syms_data, shifted);
            }
            total_syms += n_syms_this;
        }
    }

    // Build ModifierMap
    let mut modmap_data = Vec::new();
    let modifier_keycodes: &[(u8, u8)] = &[
        (64, 1),  // Shift_L → ShiftMask
        (68, 1),  // Shift_R → ShiftMask
        (65, 2),  // CapsLock → LockMask
        (67, 4),  // Control_L → ControlMask
        (70, 4),  // Control_R → ControlMask
        (66, 8),  // Alt_L → Mod1Mask
        (69, 8),  // Alt_R → Mod1Mask
        (63, 64), // Super_L → Mod4Mask
    ];
    let n_mod_map_keys: u8;
    let first_mod_map_key: u8;
    if want_modmap {
        n_mod_map_keys = modifier_keycodes.len() as u8;
        first_mod_map_key = modifier_keycodes.iter().map(|&(k, _)| k).min().unwrap_or(0);
        for &(keycode, mods) in modifier_keycodes {
            modmap_data.push(keycode);
            modmap_data.push(mods);
            write_u16_to(conn, &mut modmap_data, 0); // pad
        }
    } else {
        n_mod_map_keys = 0;
        first_mod_map_key = 0;
    }

    let present: u16 = (if want_types { 0x01 } else { 0 })
                      | (if want_syms { 0x02 } else { 0 })
                      | (if want_modmap { 0x04 } else { 0 });

    // Compute body length
    // XKB GetMap reply header is 40 bytes (8 bytes beyond standard 32-byte X11 reply header)
    let body_len = types_data.len() + syms_data.len() + modmap_data.len();
    let pad_len = (4 - (body_len % 4)) % 4;
    let additional_words = (8 + body_len + pad_len) / 4; // 8 extra header bytes + body

    // Build reply header (40 bytes per XKB spec)
    let mut reply = Vec::with_capacity(40 + body_len + pad_len);
    reply.push(1); // byte 0: reply
    reply.push(3); // byte 1: device_id
    write_u16_to(conn, &mut reply, seq); // bytes 2-3: sequence
    write_u32_to(conn, &mut reply, additional_words as u32); // bytes 4-7: reply length
    write_u16_to(conn, &mut reply, 0); // bytes 8-9: pad
    reply.push(MIN_KEYCODE); // byte 10: minKeyCode
    reply.push(MAX_KEYCODE); // byte 11: maxKeyCode
    write_u16_to(conn, &mut reply, present); // bytes 12-13: present
    reply.push(0);           // byte 14: firstType
    reply.push(n_types);     // byte 15: nTypes
    reply.push(total_types); // byte 16: totalTypes
    reply.push(first_key_sym); // byte 17: firstKeySym
    write_u16_to(conn, &mut reply, total_syms); // bytes 18-19: totalSyms (CARD16)
    reply.push(n_key_syms);    // byte 20: nKeySyms
    reply.push(0); // byte 21: firstKeyAction
    write_u16_to(conn, &mut reply, 0); // bytes 22-23: totalActions
    reply.push(0); // byte 24: nKeyActions
    reply.push(0); // byte 25: firstKeyBehavior
    reply.push(0); // byte 26: nKeyBehaviors
    reply.push(0); // byte 27: totalKeyBehaviors
    reply.push(0); // byte 28: firstKeyExplicit
    reply.push(0); // byte 29: nKeyExplicit
    reply.push(0); // byte 30: totalKeyExplicit
    reply.push(first_mod_map_key); // byte 31: firstModMapKey
    reply.push(n_mod_map_keys);    // byte 32: nModMapKeys
    reply.push(n_mod_map_keys);    // byte 33: totalModMapKeys
    reply.push(0); // byte 34: firstVModMapKey
    reply.push(0); // byte 35: nVModMapKeys
    reply.push(0); // byte 36: totalVModMapKeys
    reply.push(0); // byte 37: pad
    write_u16_to(conn, &mut reply, 0); // bytes 38-39: virtualMods

    // Append body
    reply.extend_from_slice(&types_data);
    reply.extend_from_slice(&syms_data);
    reply.extend_from_slice(&modmap_data);
    // Pad to 4-byte boundary
    reply.extend(std::iter::repeat(0).take(pad_len));

    stream.write_all(&reply).await?;
    info!("XKB GetMap: present=0x{:04x} types={} syms={} modmap={} body={}",
          present, n_types, total_syms, n_mod_map_keys, body_len);
    Ok(())
}

// --- GetCompatMap (sub-opcode 10) ---
async fn handle_get_compat_map<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(3); // device_id
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional length
    reply.push(0); // groups_rtrn
    reply.push(0); // pad
    write_u16_to(conn, &mut reply, 0); // first_si_rtrn
    write_u16_to(conn, &mut reply, 0); // n_si_rtrn
    write_u16_to(conn, &mut reply, 0); // n_total_si
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    Ok(())
}

// --- GetIndicatorState (sub-opcode 12) ---
async fn handle_get_indicator_state<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(3); // device_id
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, 0); // state (no indicators lit)
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    Ok(())
}

// --- GetIndicatorMap (sub-opcode 13) ---
async fn handle_get_indicator_map<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(3); // device_id
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, 0); // which (no indicators)
    write_u32_to(conn, &mut reply, 0); // real_indicators
    reply.push(0); // n_indicators
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    Ok(())
}

// --- GetNames (sub-opcode 17) ---
async fn handle_get_names<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let which: u32 = if data.len() >= 10 {
        read_u16(conn, &data[8..10]) as u32
    } else {
        0
    };

    // Intern atoms for standard names
    let keycodes_atom = server.atoms.intern_or_create("evdev+aliases(qwerty)");
    let geometry_atom = server.atoms.intern_or_create("pc(pc105)");
    let symbols_atom = server.atoms.intern_or_create("pc+us");
    let types_atom = server.atoms.intern_or_create("complete");

    let mut body = Vec::new();

    // Bit 0: Keycodes name
    if which & (1 << 0) != 0 {
        write_u32_to(conn, &mut body, keycodes_atom);
    }
    // Bit 1: Geometry name
    if which & (1 << 1) != 0 {
        write_u32_to(conn, &mut body, geometry_atom);
    }
    // Bit 2: Symbols name
    if which & (1 << 2) != 0 {
        write_u32_to(conn, &mut body, symbols_atom);
    }
    // Bit 6: Types name (Compat was bit 5)
    if which & (1 << 6) != 0 {
        write_u32_to(conn, &mut body, types_atom);
    }
    // Bit 8: KeyTypeNames — one atom per type
    if which & (1 << 8) != 0 {
        let one_level = server.atoms.intern_or_create("ONE_LEVEL");
        let two_level = server.atoms.intern_or_create("TWO_LEVEL");
        let alphabetic = server.atoms.intern_or_create("ALPHABETIC");
        write_u32_to(conn, &mut body, one_level);
        write_u32_to(conn, &mut body, two_level);
        write_u32_to(conn, &mut body, alphabetic);
    }
    // Bit 9: KTLevelNames — level names per type
    if which & (1 << 9) != 0 {
        // n_levels_per_type: 1, 2, 2 = 5 levels total
        // ONE_LEVEL: 1 level name
        let any = server.atoms.intern_or_create("Any");
        let shift = server.atoms.intern_or_create("Shift");
        let caps = server.atoms.intern_or_create("Caps Lock");
        write_u32_to(conn, &mut body, any);     // ONE_LEVEL level 0
        write_u32_to(conn, &mut body, any);     // TWO_LEVEL level 0
        write_u32_to(conn, &mut body, shift);   // TWO_LEVEL level 1
        write_u32_to(conn, &mut body, any);     // ALPHABETIC level 0
        write_u32_to(conn, &mut body, caps);    // ALPHABETIC level 1
    }
    // Bit 12: KeyNames — 4-char name per keycode
    if which & (1 << 12) != 0 {
        for keycode in MIN_KEYCODE..=MAX_KEYCODE {
            // Generate standard key name like <AE01>, <AC01>, etc.
            // For simplicity, use keycode number as the name
            let name = format!("K{:03}", keycode);
            let bytes = name.as_bytes();
            body.push(bytes.get(0).copied().unwrap_or(0));
            body.push(bytes.get(1).copied().unwrap_or(0));
            body.push(bytes.get(2).copied().unwrap_or(0));
            body.push(bytes.get(3).copied().unwrap_or(0));
        }
    }

    let pad_len = (4 - (body.len() % 4)) % 4;
    let additional_words = (body.len() + pad_len) / 4;

    let mut reply = Vec::with_capacity(32 + body.len() + pad_len);
    reply.push(1); // reply
    reply.push(3); // device_id
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, additional_words as u32);
    // which (u32) — echo back what we actually provided
    write_u32_to(conn, &mut reply, which);
    // min_key_code, max_key_code
    reply.push(MIN_KEYCODE);
    reply.push(MAX_KEYCODE);
    // n_types
    reply.push(3);
    // group_names (u8)
    reply.push(0);
    // virtual_mods (u16)
    write_u16_to(conn, &mut reply, 0);
    // first_key, n_keys
    reply.push(MIN_KEYCODE);
    reply.push(MAX_KEYCODE - MIN_KEYCODE + 1);
    // indicators (u32)
    write_u32_to(conn, &mut reply, 0);
    // n_radio_groups, n_key_aliases, n_kt_levels
    reply.push(0);
    reply.push(0);
    write_u16_to(conn, &mut reply, 5); // total KT levels: 1+2+2=5
    // Pad header to 32
    // We should be at 32 now; let's check and pad if needed
    while reply.len() < 32 {
        reply.push(0);
    }

    reply.extend_from_slice(&body);
    reply.extend(std::iter::repeat(0).take(pad_len));

    stream.write_all(&reply).await?;
    info!("XKB GetNames: which=0x{:08x} body_len={}", which, body.len());
    Ok(())
}

// --- PerClientFlags (sub-opcode 21) ---
async fn handle_per_client_flags<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let change: u32 = if data.len() >= 12 { crate::server::connection::read_u32(conn, &data[8..12]) } else { 0 };
    let value: u32 = if data.len() >= 16 { crate::server::connection::read_u32(conn, &data[12..16]) } else { 0 };
    let flags_set = change & value;

    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(3); // device_id
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional length
    write_u32_to(conn, &mut reply, flags_set); // supported flags
    write_u32_to(conn, &mut reply, flags_set); // value
    write_u32_to(conn, &mut reply, 0); // auto_ctrls
    write_u32_to(conn, &mut reply, 0); // auto_ctrls_values
    reply.resize(32, 0);
    stream.write_all(&reply).await?;
    info!("XKB PerClientFlags: change=0x{:08x} value=0x{:08x}", change, value);
    Ok(())
}
