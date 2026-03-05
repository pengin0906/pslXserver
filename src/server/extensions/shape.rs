// SHAPE extension — non-rectangular window support (xeyes, xlogo, etc.)
// X11 Shape Extension version 1.1

use std::sync::Arc;
use log::debug;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::server::{XServer, ServerError};
use crate::server::connection::{ClientConnection, read_u32, write_u16_to, write_u32_to};

/// SHAPE extension major opcode (assigned in extensions/mod.rs)
pub const SHAPE_MAJOR_OPCODE: u8 = 134;

// SHAPE sub-opcodes
const SHAPE_QUERY_VERSION: u8 = 0;
const SHAPE_RECTANGLES: u8 = 1;
const SHAPE_MASK: u8 = 2;
const SHAPE_COMBINE: u8 = 3;
const SHAPE_OFFSET: u8 = 4;
const SHAPE_QUERY_EXTENTS: u8 = 5;
const SHAPE_SELECT_INPUT: u8 = 6;
const SHAPE_INPUT_SELECTED: u8 = 7;
const SHAPE_GET_RECTANGLES: u8 = 8;

pub async fn handle_shape_request<S: AsyncRead + AsyncWrite + Unpin>(
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
        SHAPE_QUERY_VERSION => {
            // Reply: major_version=1, minor_version=1
            let mut reply = vec![1u8, 0]; // reply, unused
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0); // additional data length
            write_u16_to(conn, &mut reply, 1); // major version
            write_u16_to(conn, &mut reply, 1); // minor version
            reply.extend(std::iter::repeat(0).take(20)); // padding to 32
            stream.write_all(&reply).await?;
            debug!("SHAPE QueryVersion: 1.1");
            Ok(())
        }
        SHAPE_RECTANGLES => {
            // ShapeRectangles: set the shape of a window using rectangles
            // We accept the request but don't actually clip the window (macOS windows are always rectangular)
            // This allows xeyes/xlogo to run without errors
            if data.len() >= 12 {
                let dest_window = read_u32(conn, &data[4..8]);
                debug!("SHAPE Rectangles: window=0x{:08X} (accepted, no-op rendering)", dest_window);
            }
            Ok(())
        }
        SHAPE_MASK => {
            // ShapeMask: set shape from a pixmap mask
            if data.len() >= 12 {
                let dest_window = read_u32(conn, &data[4..8]);
                debug!("SHAPE Mask: window=0x{:08X} (accepted, no-op)", dest_window);
            }
            Ok(())
        }
        SHAPE_COMBINE => {
            // ShapeCombine: combine shapes
            debug!("SHAPE Combine (no-op)");
            Ok(())
        }
        SHAPE_OFFSET => {
            // ShapeOffset: offset the shape
            debug!("SHAPE Offset (no-op)");
            Ok(())
        }
        SHAPE_QUERY_EXTENTS => {
            // Reply with the full window bounding box
            if data.len() < 8 {
                return Err(ServerError::Protocol);
            }
            let window_id = read_u32(conn, &data[4..8]);
            let (x, y, w, h) = if let Some(res) = server.resources.get(&window_id) {
                if let crate::server::resources::Resource::Window(win) = res.value() {
                    let ws = win.read();
                    (ws.x, ws.y, ws.width, ws.height)
                } else { (0, 0, 1, 1) }
            } else { (0, 0, 1, 1) };

            let mut reply = vec![1u8, 0]; // reply, unused
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0); // additional data length
            // bounding shaped, clip shaped
            reply.push(0); // bounding_shaped = false (full rectangle)
            reply.push(0); // clip_shaped = false
            reply.extend([0u8; 2]); // padding
            // bounding shape extents
            write_i16_to(conn, &mut reply, x);
            write_i16_to(conn, &mut reply, y);
            write_u16_to(conn, &mut reply, w);
            write_u16_to(conn, &mut reply, h);
            // clip shape extents
            write_i16_to(conn, &mut reply, x);
            write_i16_to(conn, &mut reply, y);
            write_u16_to(conn, &mut reply, w);
            write_u16_to(conn, &mut reply, h);
            // pad to 32 bytes
            while reply.len() < 32 { reply.push(0); }
            stream.write_all(&reply).await?;
            debug!("SHAPE QueryExtents: window=0x{:08X} {}x{}+{}+{}", window_id, w, h, x, y);
            Ok(())
        }
        SHAPE_SELECT_INPUT => {
            // Select shape events — accept but don't actually send ShapeNotify
            debug!("SHAPE SelectInput (no-op)");
            Ok(())
        }
        SHAPE_INPUT_SELECTED => {
            // Reply: whether shape events are selected
            let mut reply = vec![1u8, 0]; // reply, enabled=false
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0);
            reply.extend(std::iter::repeat(0).take(24));
            stream.write_all(&reply).await?;
            Ok(())
        }
        SHAPE_GET_RECTANGLES => {
            // Return the full window as one rectangle
            if data.len() < 8 {
                return Err(ServerError::Protocol);
            }
            let window_id = read_u32(conn, &data[4..8]);
            let (x, y, w, h) = if let Some(res) = server.resources.get(&window_id) {
                if let crate::server::resources::Resource::Window(win) = res.value() {
                    let ws = win.read();
                    (ws.x, ws.y, ws.width, ws.height)
                } else { (0, 0, 1, 1) }
            } else { (0, 0, 1, 1) };

            // Reply header
            let mut reply = vec![1u8, 0]; // reply, ordering=UnSorted
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 2); // additional data = 8 bytes = 2 words
            write_u32_to(conn, &mut reply, 1); // nrectangles
            reply.extend(std::iter::repeat(0).take(20)); // padding to 32
            // One RECTANGLE (8 bytes)
            write_i16_to(conn, &mut reply, 0); // x relative to window
            write_i16_to(conn, &mut reply, 0); // y relative to window
            write_u16_to(conn, &mut reply, w);
            write_u16_to(conn, &mut reply, h);
            stream.write_all(&reply).await?;
            debug!("SHAPE GetRectangles: window=0x{:08X} -> 1 rect {}x{}", window_id, w, h);
            Ok(())
        }
        _ => {
            debug!("SHAPE: unknown sub-opcode {}", sub_opcode);
            Ok(())
        }
    }
}

fn write_i16_to(conn: &Arc<ClientConnection>, buf: &mut Vec<u8>, val: i16) {
    write_u16_to(conn, buf, val as u16);
}
