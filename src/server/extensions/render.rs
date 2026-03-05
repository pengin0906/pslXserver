// RENDER extension — alpha compositing, gradients, glyph rendering
// Minimal stub: responds to QueryVersion and QueryPictFormats so that
// clients using libXrender don't crash. Actual rendering ops are no-ops.

use std::sync::Arc;
use log::debug;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::server::{XServer, ServerError};
use crate::server::connection::{ClientConnection, read_u32, write_u16_to, write_u32_to};

// RENDER sub-opcodes
const RENDER_QUERY_VERSION: u8 = 0;
const RENDER_QUERY_PICT_FORMATS: u8 = 1;
const RENDER_QUERY_PICT_INDEX_VALUES: u8 = 2;
const RENDER_CREATE_PICTURE: u8 = 4;
const RENDER_CHANGE_PICTURE: u8 = 5;
const RENDER_SET_PICTURE_CLIP_RECTANGLES: u8 = 6;
const RENDER_FREE_PICTURE: u8 = 7;
const RENDER_COMPOSITE: u8 = 8;
const RENDER_TRAPEZOIDS: u8 = 10;
const RENDER_TRIANGLES: u8 = 11;
const RENDER_CREATE_GLYPH_SET: u8 = 17;
const RENDER_FREE_GLYPH_SET: u8 = 18;
const RENDER_ADD_GLYPHS: u8 = 20;
const RENDER_FREE_GLYPHS: u8 = 22;
const RENDER_COMPOSITE_GLYPHS_8: u8 = 23;
const RENDER_COMPOSITE_GLYPHS_16: u8 = 24;
const RENDER_COMPOSITE_GLYPHS_32: u8 = 25;
const RENDER_FILL_RECTANGLES: u8 = 26;
const RENDER_CREATE_CURSOR: u8 = 27;
const RENDER_SET_PICTURE_TRANSFORM: u8 = 28;
const RENDER_QUERY_FILTERS: u8 = 29;
const RENDER_SET_PICTURE_FILTER: u8 = 30;
const RENDER_CREATE_ANIM_CURSOR: u8 = 31;
const RENDER_ADD_TRAPS: u8 = 32;
const RENDER_CREATE_SOLID_FILL: u8 = 33;
const RENDER_CREATE_LINEAR_GRADIENT: u8 = 34;
const RENDER_CREATE_RADIAL_GRADIENT: u8 = 35;
const RENDER_CREATE_CONICAL_GRADIENT: u8 = 36;

pub async fn handle_render_request<S: AsyncRead + AsyncWrite + Unpin>(
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
        RENDER_QUERY_VERSION => {
            // Reply: version 0.11 (widely supported baseline)
            let mut reply = vec![1u8, 0]; // reply, unused
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0); // additional data
            write_u32_to(conn, &mut reply, 0); // major version
            write_u32_to(conn, &mut reply, 11); // minor version
            reply.extend(std::iter::repeat(0).take(16)); // pad to 32
            stream.write_all(&reply).await?;
            debug!("RENDER QueryVersion: 0.11");
            Ok(())
        }
        RENDER_QUERY_PICT_FORMATS => {
            // Return a minimal set of PictFormats that covers ARGB32 and RGB24
            // This is what most apps need to proceed
            let mut reply = vec![1u8, 0];
            write_u16_to(conn, &mut reply, seq);

            // We'll build the body, then set the length
            let mut body = Vec::new();

            // numFormats
            write_u32_to(conn, &mut body, 3); // 3 formats: ARGB32, RGB24, A8
            // numScreens
            write_u32_to(conn, &mut body, 1);
            // numDepths
            write_u32_to(conn, &mut body, 2); // depth 24, depth 32
            // numVisuals
            write_u32_to(conn, &mut body, 1); // 1 visual
            // numSubpixel (v0.6+)
            write_u32_to(conn, &mut body, 1);

            // PictFormInfo entries (28 bytes each):
            // id, type, depth, pad, direct{red,redMask,green,greenMask,blue,blueMask,alpha,alphaMask}, colormap

            // Format 1: ARGB32 (id=1)
            write_u32_to(conn, &mut body, 1); // id
            body.push(1); // type = Direct
            body.push(32); // depth
            body.extend([0u8; 2]); // pad
            // Direct: red offset=16, mask=0xFF, green offset=8, mask=0xFF, blue offset=0, mask=0xFF, alpha offset=24, mask=0xFF
            write_u16_to(conn, &mut body, 16); write_u16_to(conn, &mut body, 0xFF); // red
            write_u16_to(conn, &mut body, 8); write_u16_to(conn, &mut body, 0xFF);  // green
            write_u16_to(conn, &mut body, 0); write_u16_to(conn, &mut body, 0xFF);  // blue
            write_u16_to(conn, &mut body, 24); write_u16_to(conn, &mut body, 0xFF); // alpha
            write_u32_to(conn, &mut body, 0); // colormap

            // Format 2: RGB24 (id=2)
            write_u32_to(conn, &mut body, 2); // id
            body.push(1); // type = Direct
            body.push(24); // depth
            body.extend([0u8; 2]); // pad
            write_u16_to(conn, &mut body, 16); write_u16_to(conn, &mut body, 0xFF);
            write_u16_to(conn, &mut body, 8); write_u16_to(conn, &mut body, 0xFF);
            write_u16_to(conn, &mut body, 0); write_u16_to(conn, &mut body, 0xFF);
            write_u16_to(conn, &mut body, 0); write_u16_to(conn, &mut body, 0); // no alpha
            write_u32_to(conn, &mut body, 0);

            // Format 3: A8 (id=3)
            write_u32_to(conn, &mut body, 3); // id
            body.push(1); // type = Direct
            body.push(8); // depth
            body.extend([0u8; 2]); // pad
            write_u16_to(conn, &mut body, 0); write_u16_to(conn, &mut body, 0); // no red
            write_u16_to(conn, &mut body, 0); write_u16_to(conn, &mut body, 0); // no green
            write_u16_to(conn, &mut body, 0); write_u16_to(conn, &mut body, 0); // no blue
            write_u16_to(conn, &mut body, 0); write_u16_to(conn, &mut body, 0xFF); // alpha
            write_u32_to(conn, &mut body, 0);

            // Screen info: numDepths, fallback pictformat
            write_u32_to(conn, &mut body, 2); // numDepths
            write_u32_to(conn, &mut body, 2); // fallback = RGB24

            // Depth 24: numPictVisuals=1
            body.push(24);
            body.push(0); // pad
            write_u16_to(conn, &mut body, 1); // numPictVisuals
            body.extend([0u8; 4]); // pad
            // PictVisual: visual=0x21, format=2 (RGB24)
            write_u32_to(conn, &mut body, 0x21); // visual id
            write_u32_to(conn, &mut body, 2); // format = RGB24

            // Depth 32: numPictVisuals=0
            body.push(32);
            body.push(0);
            write_u16_to(conn, &mut body, 0); // numPictVisuals
            body.extend([0u8; 4]); // pad

            // Subpixel info: 1 screen, subpixel order = Unknown (0)
            write_u32_to(conn, &mut body, 0); // SubPixelUnknown

            // Set reply length
            let pad = (4 - (body.len() % 4)) % 4;
            let reply_len = (body.len() + pad) / 4;
            write_u32_to(conn, &mut reply, reply_len as u32);
            reply.extend(std::iter::repeat(0).take(24)); // pad header to 32
            reply.extend_from_slice(&body);
            reply.extend(std::iter::repeat(0).take(pad));
            stream.write_all(&reply).await?;
            debug!("RENDER QueryPictFormats: 3 formats, 1 screen");
            Ok(())
        }
        RENDER_QUERY_PICT_INDEX_VALUES => {
            // Empty reply for indexed formats (we only use Direct)
            let mut reply = vec![1u8, 0];
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0);
            write_u32_to(conn, &mut reply, 0); // numValues=0
            reply.extend(std::iter::repeat(0).take(20));
            stream.write_all(&reply).await?;
            Ok(())
        }
        RENDER_QUERY_FILTERS => {
            // Return empty filter lists
            let mut reply = vec![1u8, 0];
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0);
            write_u32_to(conn, &mut reply, 0); // numAliases
            write_u32_to(conn, &mut reply, 0); // numFilters
            reply.extend(std::iter::repeat(0).take(16));
            stream.write_all(&reply).await?;
            Ok(())
        }
        // All rendering operations: accept silently (no-op)
        RENDER_CREATE_PICTURE | RENDER_CHANGE_PICTURE |
        RENDER_SET_PICTURE_CLIP_RECTANGLES | RENDER_FREE_PICTURE |
        RENDER_COMPOSITE | RENDER_TRAPEZOIDS | RENDER_TRIANGLES |
        RENDER_CREATE_GLYPH_SET | RENDER_FREE_GLYPH_SET |
        RENDER_ADD_GLYPHS | RENDER_FREE_GLYPHS |
        RENDER_COMPOSITE_GLYPHS_8 | RENDER_COMPOSITE_GLYPHS_16 | RENDER_COMPOSITE_GLYPHS_32 |
        RENDER_FILL_RECTANGLES | RENDER_CREATE_CURSOR |
        RENDER_SET_PICTURE_TRANSFORM | RENDER_SET_PICTURE_FILTER |
        RENDER_CREATE_ANIM_CURSOR | RENDER_ADD_TRAPS |
        RENDER_CREATE_SOLID_FILL | RENDER_CREATE_LINEAR_GRADIENT |
        RENDER_CREATE_RADIAL_GRADIENT | RENDER_CREATE_CONICAL_GRADIENT => {
            debug!("RENDER sub-opcode {} (no-op)", sub_opcode);
            Ok(())
        }
        _ => {
            debug!("RENDER: unknown sub-opcode {}", sub_opcode);
            Ok(())
        }
    }
}
