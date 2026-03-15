// RENDER extension — alpha compositing, gradients, glyph rendering
// Minimal implementation: enough for software-rendered clients such as Chrome
// to composite pixmaps into windows. More advanced operators still fall back
// to no-op.

use std::sync::OnceLock;
use std::sync::Arc;
use log::debug;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::server::{XServer, ServerError};
use crate::server::connection::{ClientConnection, read_u32, write_u16_to, write_u32_to};
use crate::display::RenderCommand;

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

#[derive(Clone, Copy, Debug)]
struct PictureState {
    drawable: u32,
    format: u32,
}

fn picture_map() -> &'static dashmap::DashMap<u32, PictureState> {
    static PICTURES: OnceLock<dashmap::DashMap<u32, PictureState>> = OnceLock::new();
    PICTURES.get_or_init(dashmap::DashMap::new)
}

fn premultiplied_argb_to_bgra(mut data: Vec<u8>) -> Vec<u8> {
    for px in data.chunks_exact_mut(4) {
        let b = px[0] as u32;
        let g = px[1] as u32;
        let r = px[2] as u32;
        let a = px[3] as u32;
        if a == 0 {
            px.copy_from_slice(&[0, 0, 0, 0xFF]);
            continue;
        }
        let r = ((r * 255) / a).min(255) as u8;
        let g = ((g * 255) / a).min(255) as u8;
        let b = ((b * 255) / a).min(255) as u8;
        px.copy_from_slice(&[b, g, r, 0xFF]);
    }
    data
}

fn composite_src_pixmap_to_drawable(
    server: &Arc<XServer>,
    src_drawable: u32,
    dst_drawable: u32,
    src_x: i16,
    src_y: i16,
    dst_x: i16,
    dst_y: i16,
    width: u16,
    height: u16,
) {
    let src_pixels = if let Some(res) = server.resources.get(&src_drawable) {
        if let crate::server::resources::Resource::Pixmap(pix) = res.value() {
            let p = pix.read();
            let pw = p.width as usize;
            let ph = p.height as usize;
            let sx = src_x.max(0) as usize;
            let sy = src_y.max(0) as usize;
            let copy_w = (width as usize).min(pw.saturating_sub(sx));
            let copy_h = (height as usize).min(ph.saturating_sub(sy));
            if copy_w == 0 || copy_h == 0 {
                return;
            }
            let mut out = vec![0u8; copy_w * copy_h * 4];
            for row in 0..copy_h {
                let src_off = ((sy + row) * pw + sx) * 4;
                let dst_off = row * copy_w * 4;
                out[dst_off..dst_off + copy_w * 4]
                    .copy_from_slice(&p.data[src_off..src_off + copy_w * 4]);
            }
            (premultiplied_argb_to_bgra(out), copy_w as u16, copy_h as u16)
        } else {
            return;
        }
    } else {
        return;
    };

    let (data, copy_w, copy_h) = src_pixels;
    let cmd = RenderCommand::PutImage {
        x: dst_x,
        y: dst_y,
        width: copy_w,
        height: copy_h,
        depth: 32,
        format: 2,
        data,
        gc_function: 3,
    };
    crate::server::connection::dispatch_render_commands(server, dst_drawable, vec![cmd]);
}

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
            // Return a minimal set of PictFormats that covers ARGB32, RGB24, A8
            // Header: 32 bytes (type, pad, seq, length, numFormats, numScreens,
            //         numDepths, numVisuals, numSubpixel, pad)
            // Body:   PictFormInfo[numFormats] + Screen[numScreens] + Subpixel[numScreens]

            // Build body first to compute length
            let mut body = Vec::new();

            // PictFormInfo entries (28 bytes each):
            // id(4), type(1), depth(1), pad(2), direct(16), colormap(4)

            // Format 1: ARGB32 (id=1)
            write_u32_to(conn, &mut body, 1); // id
            body.push(1); // type = Direct
            body.push(32); // depth
            body.extend([0u8; 2]); // pad
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

            // Screen[0]: numDepths(4) + fallback(4) + Depth entries
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

            // Build reply header (32 bytes)
            let mut reply = vec![1u8, 0]; // type=Reply, pad
            write_u16_to(conn, &mut reply, seq);
            let reply_len = body.len() / 4;
            write_u32_to(conn, &mut reply, reply_len as u32); // length in 4-byte words
            write_u32_to(conn, &mut reply, 3); // numFormats
            write_u32_to(conn, &mut reply, 1); // numScreens
            write_u32_to(conn, &mut reply, 2); // numDepths (total across all screens)
            write_u32_to(conn, &mut reply, 1); // numVisuals (total across all screens)
            write_u32_to(conn, &mut reply, 1); // numSubpixel
            write_u32_to(conn, &mut reply, 0); // pad
            // header is now 32 bytes: 1+1+2+4 + 6*4 = 32

            reply.extend_from_slice(&body);
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
        RENDER_CREATE_PICTURE => {
            if data.len() >= 16 {
                let pid = read_u32(conn, &data[4..8]);
                let drawable = read_u32(conn, &data[8..12]);
                let format = read_u32(conn, &data[12..16]);
                picture_map().insert(pid, PictureState { drawable, format });
                debug!("RENDER CreatePicture: pict=0x{:08X} drawable=0x{:08X} format={}", pid, drawable, format);
            }
            Ok(())
        }
        RENDER_FREE_PICTURE => {
            if data.len() >= 8 {
                let pid = read_u32(conn, &data[4..8]);
                picture_map().remove(&pid);
            }
            Ok(())
        }
        RENDER_FILL_RECTANGLES => {
            if data.len() >= 20 {
                let op = data[4];
                let dst_picture = read_u32(conn, &data[8..12]);
                let color_r = u16::from_le_bytes([data[12], data[13]]);
                let color_g = u16::from_le_bytes([data[14], data[15]]);
                let color_b = u16::from_le_bytes([data[16], data[17]]);
                let dst = picture_map().get(&dst_picture).map(|p| *p);
                if let Some(dst) = dst {
                    let color = (((color_r >> 8) as u32) << 16)
                        | (((color_g >> 8) as u32) << 8)
                        | ((color_b >> 8) as u32);
                    let mut commands = Vec::new();
                    let mut offset = 20;
                    while offset + 8 <= data.len() {
                        let x = i16::from_le_bytes([data[offset], data[offset + 1]]);
                        let y = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
                        let width = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
                        let height = u16::from_le_bytes([data[offset + 6], data[offset + 7]]);
                        offset += 8;
                        commands.push(RenderCommand::FillRectangle {
                            x, y, width, height, color,
                            gc_function: if op == 0 { 3 } else { 7 },
                        });
                    }
                    crate::server::connection::dispatch_render_commands(server, dst.drawable, commands);
                }
            }
            Ok(())
        }
        RENDER_COMPOSITE => {
            if data.len() >= 36 {
                let op = data[4];
                let src_picture = read_u32(conn, &data[8..12]);
                let _mask_picture = read_u32(conn, &data[12..16]);
                let dst_picture = read_u32(conn, &data[16..20]);
                let src_x = i16::from_le_bytes([data[20], data[21]]);
                let src_y = i16::from_le_bytes([data[22], data[23]]);
                let _mask_x = i16::from_le_bytes([data[24], data[25]]);
                let _mask_y = i16::from_le_bytes([data[26], data[27]]);
                let dst_x = i16::from_le_bytes([data[28], data[29]]);
                let dst_y = i16::from_le_bytes([data[30], data[31]]);
                let width = u16::from_le_bytes([data[32], data[33]]);
                let height = u16::from_le_bytes([data[34], data[35]]);
                let src = picture_map().get(&src_picture).map(|p| *p);
                let dst = picture_map().get(&dst_picture).map(|p| *p);
                if let (Some(src), Some(dst)) = (src, dst) {
                    if op <= 3 {
                        composite_src_pixmap_to_drawable(
                            server, src.drawable, dst.drawable,
                            src_x, src_y, dst_x, dst_y, width, height,
                        );
                    } else {
                        debug!("RENDER Composite: unsupported op={}", op);
                    }
                }
            }
            Ok(())
        }
        // Remaining rendering operations: accept silently (no-op)
        RENDER_CHANGE_PICTURE |
        RENDER_SET_PICTURE_CLIP_RECTANGLES | RENDER_TRAPEZOIDS | RENDER_TRIANGLES |
        RENDER_CREATE_GLYPH_SET | RENDER_FREE_GLYPH_SET |
        RENDER_ADD_GLYPHS | RENDER_FREE_GLYPHS |
        RENDER_COMPOSITE_GLYPHS_8 | RENDER_COMPOSITE_GLYPHS_16 | RENDER_COMPOSITE_GLYPHS_32 |
        RENDER_CREATE_CURSOR |
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
