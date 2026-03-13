// XTEST extension — synthetic input injection (xdotool support)
// X11 XTest Extension version 2.2

use std::sync::Arc;
use log::debug;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::server::{XServer, ServerError};
use crate::server::connection::{ClientConnection, read_u32, write_u16_to, write_u32_to};
use crate::server::protocol;

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
            let root_x = {
                let bytes = [data[24], data[25]];
                match conn.byte_order {
                    crate::server::connection::ByteOrder::LittleEndian => i16::from_le_bytes(bytes),
                    crate::server::connection::ByteOrder::BigEndian => i16::from_be_bytes(bytes),
                }
            };
            let root_y = {
                let bytes = [data[26], data[27]];
                match conn.byte_order {
                    crate::server::connection::ByteOrder::LittleEndian => i16::from_le_bytes(bytes),
                    crate::server::connection::ByteOrder::BigEndian => i16::from_be_bytes(bytes),
                }
            };

            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u32;

            // Use the focus window for keyboard events, root for pointer events
            let focus = server.focus_window.load(std::sync::atomic::Ordering::Relaxed);
            let root = server.screens.first().map(|s| s.root_window).unwrap_or(0);
            let target = if focus > 1 { focus } else { root };

            match event_type {
                2 => { // KeyPress
                    debug!("XTEST FakeInput: KeyPress keycode={}", detail);
                    crate::server::send_key_event(
                        server, protocol::event_type::KEY_PRESS,
                        target, detail, 0, time,
                    );
                }
                3 => { // KeyRelease
                    debug!("XTEST FakeInput: KeyRelease keycode={}", detail);
                    crate::server::send_key_event(
                        server, protocol::event_type::KEY_RELEASE,
                        target, detail, 0, time,
                    );
                }
                4 => { // ButtonPress
                    debug!("XTEST FakeInput: ButtonPress button={}", detail);
                    let px = server.pointer_x.load(std::sync::atomic::Ordering::Relaxed) as i16;
                    let py = server.pointer_y.load(std::sync::atomic::Ordering::Relaxed) as i16;
                    crate::server::send_button_event(
                        server, protocol::event_type::BUTTON_PRESS,
                        target, detail, px, py, px, py, 0, time,
                    );
                }
                5 => { // ButtonRelease
                    debug!("XTEST FakeInput: ButtonRelease button={}", detail);
                    let px = server.pointer_x.load(std::sync::atomic::Ordering::Relaxed) as i16;
                    let py = server.pointer_y.load(std::sync::atomic::Ordering::Relaxed) as i16;
                    crate::server::send_button_event(
                        server, protocol::event_type::BUTTON_RELEASE,
                        target, detail, px, py, px, py, 0, time,
                    );
                }
                6 => { // MotionNotify
                    debug!("XTEST FakeInput: MotionNotify ({}, {})", root_x, root_y);
                    server.pointer_x.store(root_x as i32, std::sync::atomic::Ordering::Relaxed);
                    server.pointer_y.store(root_y as i32, std::sync::atomic::Ordering::Relaxed);
                    crate::server::send_motion_event(
                        server, target,
                        root_x, root_y, root_x, root_y, 0, time,
                    );
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
