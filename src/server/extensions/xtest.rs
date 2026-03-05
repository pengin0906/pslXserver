// XTEST extension — synthetic input injection (xdotool support)
// X11 XTest Extension version 2.2

use std::sync::Arc;
use log::debug;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::server::{XServer, ServerError};
use crate::server::connection::{ClientConnection, read_u32, write_u16_to, write_u32_to};

/// XTEST extension major opcode (assigned in extensions/mod.rs)
pub const XTEST_MAJOR_OPCODE: u8 = 132;

// XTEST sub-opcodes
const XTEST_GET_VERSION: u8 = 0;
const XTEST_COMPARE_CURSOR: u8 = 1;
const XTEST_FAKE_INPUT: u8 = 2;
const XTEST_GRAB_CONTROL: u8 = 3;

pub async fn handle_xtest_request<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 4 {
        return Err(ServerError::Protocol);
    }
    let sub_opcode = data[1];
    let seq = conn.current_request_sequence();

    match sub_opcode {
        XTEST_GET_VERSION => {
            // Reply: major=2, minor=2
            let mut reply = vec![1u8, 2]; // reply, major_version=2
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0); // additional data
            write_u16_to(conn, &mut reply, 2); // minor_version
            reply.extend(std::iter::repeat(0).take(22)); // pad to 32
            stream.write_all(&reply).await?;
            debug!("XTEST GetVersion: 2.2");
            Ok(())
        }
        XTEST_COMPARE_CURSOR => {
            // Reply: same=true (we don't track cursor identity precisely)
            let mut reply = vec![1u8, 1]; // reply, same=true
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0);
            reply.extend(std::iter::repeat(0).take(24));
            stream.write_all(&reply).await?;
            Ok(())
        }
        XTEST_FAKE_INPUT => {
            // FakeInput: inject a synthetic event
            // Format: type(1) detail(1) seq(2) time(4) root(4) pad(8) rootx(2) rooty(2) pad(8)
            if data.len() < 36 {
                return Err(ServerError::Protocol);
            }
            let event_type = data[4];
            let detail = data[5];
            let _time = read_u32(conn, &data[8..12]);
            let _root = read_u32(conn, &data[12..16]);
            let root_x = i16::from_le_bytes([data[24], data[25]]);
            let root_y = i16::from_le_bytes([data[26], data[27]]);

            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u32;

            // Find the focus window for keyboard events or pointer window for mouse
            let focus = server.focus_window.load(std::sync::atomic::Ordering::Relaxed);
            let target = if focus > 1 { focus } else {
                server.screens.first().map(|s| s.root_window).unwrap_or(0)
            };

            match event_type {
                2 => { // KeyPress
                    debug!("XTEST FakeInput: KeyPress keycode={}", detail);
                    if let Some(evt_tx) = server.connections.get(&conn.id) {
                        let _ = evt_tx.event_tx.send(build_key_event(conn, 2, target, detail, 0, seq));
                    }
                }
                3 => { // KeyRelease
                    debug!("XTEST FakeInput: KeyRelease keycode={}", detail);
                    if let Some(evt_tx) = server.connections.get(&conn.id) {
                        let _ = evt_tx.event_tx.send(build_key_event(conn, 3, target, detail, 0, seq));
                    }
                }
                4 | 5 => { // ButtonPress / ButtonRelease
                    debug!("XTEST FakeInput: Button{} button={}", if event_type == 4 { "Press" } else { "Release" }, detail);
                    // Button events require pointer position — use current pointer
                }
                6 => { // MotionNotify
                    debug!("XTEST FakeInput: MotionNotify ({}, {})", root_x, root_y);
                    server.pointer_x.store(root_x as i32, std::sync::atomic::Ordering::Relaxed);
                    server.pointer_y.store(root_y as i32, std::sync::atomic::Ordering::Relaxed);
                }
                _ => {
                    debug!("XTEST FakeInput: unknown type {}", event_type);
                }
            }
            Ok(())
        }
        XTEST_GRAB_CONTROL => {
            // GrabControl: accept but no-op (we don't need to override grabs)
            debug!("XTEST GrabControl (no-op)");
            Ok(())
        }
        _ => {
            debug!("XTEST: unknown sub-opcode {}", sub_opcode);
            Ok(())
        }
    }
}

/// Build a 32-byte key event for FakeInput
fn build_key_event(conn: &Arc<ClientConnection>, event_type: u8, window: u32, keycode: u8, state: u16, seq: u16) -> Vec<u8> {
    use crate::server::connection::write_u16_to;
    use crate::server::connection::write_u32_to;

    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;

    let mut evt = vec![0u8; 32];
    evt[0] = event_type;
    evt[1] = keycode;
    // sequence at [2..4] will be stamped on write
    let mut tmp = Vec::new();
    write_u32_to(conn, &mut tmp, time);
    evt[4..8].copy_from_slice(&tmp);
    tmp.clear();
    write_u32_to(conn, &mut tmp, window); // root
    evt[8..12].copy_from_slice(&tmp);
    tmp.clear();
    write_u32_to(conn, &mut tmp, window); // event window
    evt[12..16].copy_from_slice(&tmp);
    tmp.clear();
    write_u32_to(conn, &mut tmp, window); // child
    evt[16..20].copy_from_slice(&tmp);
    // root_x, root_y, event_x, event_y at offsets 20-27 = 0
    tmp.clear();
    write_u16_to(conn, &mut tmp, state);
    evt[28..30].copy_from_slice(&tmp);
    evt[30] = 1; // same_screen = true
    evt
}
