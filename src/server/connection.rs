use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use bytes::BytesMut;
use log::{debug, info, warn};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::{ServerError, XServer};

/// Byte order of the client connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    BigEndian,
    LittleEndian,
}

/// Per-client connection state.
pub struct ClientConnection {
    pub id: u32,
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub byte_order: ByteOrder,
    pub sequence_number: AtomicU16,
    /// Channel for sending X11 events to this client's connection task.
    pub event_tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl ClientConnection {
    pub fn next_sequence(&self) -> u16 {
        self.sequence_number.fetch_add(1, Ordering::Relaxed)
    }

    /// Get the sequence number of the current request (for use in replies).
    pub fn current_request_sequence(&self) -> u16 {
        self.sequence_number.load(Ordering::Relaxed).wrapping_sub(1)
    }
}

impl std::fmt::Debug for ClientConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConnection")
            .field("id", &self.id)
            .field("resource_id_base", &format_args!("0x{:08X}", self.resource_id_base))
            .field("byte_order", &self.byte_order)
            .finish()
    }
}

/// Handle a new X11 client connection.
pub async fn handle_connection<S>(
    server: Arc<XServer>,
    mut stream: S,
    conn_id: u32,
) -> Result<(), ServerError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Phase 1: Connection setup handshake

    // Read the initial connection setup request
    // Minimum: 12 bytes header + auth data
    let mut setup_buf = [0u8; 1024];
    let n = stream.read(&mut setup_buf).await?;
    if n < 12 {
        return Err(ServerError::Protocol);
    }

    // Byte 0: byte order ('B' = 0x42 MSB first, 'l' = 0x6C LSB first)
    let byte_order = match setup_buf[0] {
        0x42 => ByteOrder::BigEndian,
        0x6C => ByteOrder::LittleEndian,
        _ => {
            warn!("Invalid byte order byte: 0x{:02X}", setup_buf[0]);
            return Err(ServerError::Protocol);
        }
    };

    // Read protocol version
    let (proto_major, proto_minor) = match byte_order {
        ByteOrder::BigEndian => (
            u16::from_be_bytes([setup_buf[2], setup_buf[3]]),
            u16::from_be_bytes([setup_buf[4], setup_buf[5]]),
        ),
        ByteOrder::LittleEndian => (
            u16::from_le_bytes([setup_buf[2], setup_buf[3]]),
            u16::from_le_bytes([setup_buf[4], setup_buf[5]]),
        ),
    };

    debug!(
        "Connection {}: byte_order={:?}, protocol={}.{}",
        conn_id, byte_order, proto_major, proto_minor
    );

    // Read auth protocol name and data lengths
    let (auth_name_len, auth_data_len) = match byte_order {
        ByteOrder::BigEndian => (
            u16::from_be_bytes([setup_buf[6], setup_buf[7]]) as usize,
            u16::from_be_bytes([setup_buf[8], setup_buf[9]]) as usize,
        ),
        ByteOrder::LittleEndian => (
            u16::from_le_bytes([setup_buf[6], setup_buf[7]]) as usize,
            u16::from_le_bytes([setup_buf[8], setup_buf[9]]) as usize,
        ),
    };

    debug!(
        "Connection {}: auth_name_len={}, auth_data_len={}",
        conn_id, auth_name_len, auth_data_len
    );

    // TODO: Validate MIT-MAGIC-COOKIE-1 authentication
    // For now, accept all connections

    // Allocate resource ID space for this client
    let resource_id_base = server.alloc_resource_id_base();
    let resource_id_mask = 0x001FFFFF; // 21 bits for resource IDs within this base

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let conn = Arc::new(ClientConnection {
        id: conn_id,
        resource_id_base,
        resource_id_mask,
        byte_order,
        sequence_number: AtomicU16::new(1),
        event_tx,
    });

    // Register the connection
    server.connections.insert(conn_id, Arc::clone(&conn));

    // Build and send the connection setup reply
    let reply = build_setup_reply(&server, &conn);
    stream.write_all(&reply).await?;

    info!("Connection {} setup complete (resource base: 0x{:08X})", conn_id, resource_id_base);

    // Phase 2: Main request processing loop (with event multiplexing)
    let mut buf = vec![0u8; 65536];
    let mut pending = BytesMut::with_capacity(65536);
    let mut err_buf = [0u8; 32];

    loop {
        tokio::select! {
            biased;
            // Incoming data from the X11 client (prioritized to avoid starvation)
            result = stream.read(&mut buf) => {
                let n = match result {
                    Ok(n) => n,
                    Err(e) => {
                        warn!("Connection {} read error: {}", conn_id, e);
                        break;
                    }
                };
                if n == 0 {
                    info!("Connection {} disconnected (read 0 bytes, pending={})", conn_id, pending.len());
                    break;
                }

                pending.extend_from_slice(&buf[..n]);

                // Process all complete requests in the buffer
                while pending.len() >= 4 {
                    let request_len = {
                        let len = match byte_order {
                            ByteOrder::BigEndian => u16::from_be_bytes([pending[2], pending[3]]),
                            ByteOrder::LittleEndian => u16::from_le_bytes([pending[2], pending[3]]),
                        };
                        if len == 0 {
                            if pending.len() < 8 { break; }
                            let ext_len = match byte_order {
                                ByteOrder::BigEndian => u32::from_be_bytes([
                                    pending[4], pending[5], pending[6], pending[7],
                                ]),
                                ByteOrder::LittleEndian => u32::from_le_bytes([
                                    pending[4], pending[5], pending[6], pending[7],
                                ]),
                            };
                            ext_len as usize * 4
                        } else {
                            len as usize * 4
                        }
                    };

                    if pending.len() < request_len { break; }

                    let _seq = conn.next_sequence();
                    let opcode = pending[0];
                    debug!("Request seq={} opcode={} len={} bytes", conn.current_request_sequence(), opcode, request_len);
                    let seq = conn.current_request_sequence();

                    let request_data = pending.split_to(request_len);

                    match handle_request(&server, &conn, opcode, &request_data, &mut stream).await {
                        Ok(()) => {}
                        Err(ServerError::NotImplemented) => {
                            debug!("Unimplemented opcode: {} — sending Implementation error", opcode);
                            err_buf = [0u8; 32];
                            err_buf[0] = 0;
                            err_buf[1] = 17;
                            err_buf[2] = (seq & 0xFF) as u8;
                            err_buf[3] = ((seq >> 8) & 0xFF) as u8;
                            err_buf[8] = opcode;
                            err_buf[10] = opcode;
                            let _ = stream.write_all(&err_buf).await;
                        }
                        Err(e) => {
                            warn!("Request error (opcode {}): {}", opcode, e);
                            err_buf = [0u8; 32];
                            err_buf[0] = 0;
                            err_buf[1] = 17;
                            err_buf[2] = (seq & 0xFF) as u8;
                            err_buf[3] = ((seq >> 8) & 0xFF) as u8;
                            err_buf[8] = opcode;
                            err_buf[10] = opcode;
                            let _ = stream.write_all(&err_buf).await;
                        }
                    }
                }
            }

            // Events from the server's event dispatcher
            Some(mut event_data) = event_rx.recv() => {
                // Re-stamp sequence number to current request sequence.
                // X11 spec: event seq = last request processed by server.
                // Events queued via event_tx may have stale seq numbers that
                // would violate xcb's monotonic sequence requirement.
                let cur_seq = conn.current_request_sequence();
                set_event_sequence(&conn, &mut event_data, cur_seq);
                debug!("Writing event to conn {}: type={} seq={} len={}", conn_id, event_data[0], cur_seq, event_data.len());
                match stream.write_all(&event_data).await {
                    Ok(()) => {
                        // Drain any additional queued events before going back to select
                        // This prevents event backlog from starving the read branch
                        let mut extra = 0;
                        while let Ok(mut more) = event_rx.try_recv() {
                            let cur_seq = conn.current_request_sequence();
                            set_event_sequence(&conn, &mut more, cur_seq);
                            if stream.write_all(&more).await.is_err() { break; }
                            extra += 1;
                            if extra >= 16 { break; } // cap to avoid starving reads
                        }
                    }
                    Err(e) => {
                        warn!("Event write error on conn {}: {}", conn_id, e);
                        break;
                    }
                }
            }
        }
    }

    // Clean up: remove selections owned by this connection's windows
    let base = conn.resource_id_base;
    let mask = conn.resource_id_mask;
    server.selections.retain(|_sel, (owner, _ts)| {
        // Keep selections NOT owned by this connection
        (*owner & !mask) != (base & !mask)
    });

    // Clean up: remove resources owned by this connection
    server.resources.retain(|xid, _| {
        (*xid & !mask) != (base & !mask)
    });

    server.connections.remove(&conn_id);
    info!("Connection {} disconnected", conn_id);

    Ok(())
}

/// Build the X11 connection setup success reply.
fn build_setup_reply(server: &XServer, conn: &ClientConnection) -> Vec<u8> {
    let screen = &server.screens[0];
    let vendor = b"pslXserver";
    let vendor_pad = (4 - (vendor.len() % 4)) % 4;

    // Build the reply buffer
    let mut reply = Vec::with_capacity(1024);

    let write_u16 = |buf: &mut Vec<u8>, val: u16| match conn.byte_order {
        ByteOrder::BigEndian => buf.extend_from_slice(&val.to_be_bytes()),
        ByteOrder::LittleEndian => buf.extend_from_slice(&val.to_le_bytes()),
    };
    let write_u32 = |buf: &mut Vec<u8>, val: u32| match conn.byte_order {
        ByteOrder::BigEndian => buf.extend_from_slice(&val.to_be_bytes()),
        ByteOrder::LittleEndian => buf.extend_from_slice(&val.to_le_bytes()),
    };

    // Success byte
    reply.push(1); // Success
    reply.push(0); // unused

    // Protocol version
    write_u16(&mut reply, 11); // major
    write_u16(&mut reply, 0);  // minor

    // Additional data length (in 4-byte units) - we'll fill this later
    let additional_len_offset = reply.len();
    write_u16(&mut reply, 0); // placeholder

    // Fixed fields (starting at byte 8)
    write_u32(&mut reply, 0); // release number
    write_u32(&mut reply, conn.resource_id_base); // resource-id-base
    write_u32(&mut reply, conn.resource_id_mask); // resource-id-mask
    write_u32(&mut reply, 0); // motion-buffer-size
    write_u16(&mut reply, vendor.len() as u16); // vendor length
    write_u16(&mut reply, 65535); // maximum-request-length
    reply.push(1); // number of screens
    reply.push(1); // number of pixmap formats

    reply.push(0); // image-byte-order (LSBFirst)
    reply.push(0); // bitmap-bit-order (LSBFirst)
    reply.push(32); // bitmap-scanline-unit
    reply.push(32); // bitmap-scanline-pad
    reply.push(8);  // min-keycode
    reply.push(255); // max-keycode

    write_u32(&mut reply, 0); // unused (4 bytes)

    // Vendor string
    reply.extend_from_slice(vendor);
    reply.extend(std::iter::repeat(0).take(vendor_pad));

    // Pixmap formats (1 format: depth 24, bpp 32, scanline-pad 32)
    reply.push(24); // depth
    reply.push(32); // bits-per-pixel
    reply.push(32); // scanline-pad
    reply.extend(std::iter::repeat(0).take(5)); // unused padding

    // Screen(s)
    write_u32(&mut reply, screen.root_window); // root window
    write_u32(&mut reply, screen.default_colormap); // default colormap
    write_u32(&mut reply, screen.white_pixel); // white pixel
    write_u32(&mut reply, screen.black_pixel); // black pixel
    write_u32(&mut reply, 0); // current input masks
    write_u16(&mut reply, screen.width_in_pixels); // width in pixels
    write_u16(&mut reply, screen.height_in_pixels); // height in pixels
    write_u16(&mut reply, screen.width_in_mm); // width in mm
    write_u16(&mut reply, screen.height_in_mm); // height in mm
    write_u16(&mut reply, 1); // min installed maps
    write_u16(&mut reply, 1); // max installed maps
    write_u32(&mut reply, screen.root_visual.id); // root visual
    reply.push(0); // backing-stores (Never)
    reply.push(0); // save-unders (false)
    reply.push(screen.root_depth); // root depth
    reply.push(1); // number of allowed depths

    // Allowed depth (depth 24)
    reply.push(24); // depth
    reply.push(0);  // unused
    write_u16(&mut reply, 1); // number of visuals
    write_u32(&mut reply, 0); // unused

    // Visual (TrueColor, depth 24)
    write_u32(&mut reply, screen.root_visual.id); // visual ID
    reply.push(screen.root_visual.class); // class (TrueColor = 4)
    reply.push(screen.root_visual.bits_per_rgb); // bits per RGB value
    write_u16(&mut reply, screen.root_visual.colormap_entries); // colormap entries
    write_u32(&mut reply, screen.root_visual.red_mask);   // red mask
    write_u32(&mut reply, screen.root_visual.green_mask);  // green mask
    write_u32(&mut reply, screen.root_visual.blue_mask);   // blue mask
    write_u32(&mut reply, 0); // unused

    // Fix up the additional data length
    let additional_data_bytes = reply.len() - 8; // everything after the first 8 bytes
    let additional_data_units = (additional_data_bytes / 4) as u16;
    let len_bytes = match conn.byte_order {
        ByteOrder::BigEndian => additional_data_units.to_be_bytes(),
        ByteOrder::LittleEndian => additional_data_units.to_le_bytes(),
    };
    reply[additional_len_offset] = len_bytes[0];
    reply[additional_len_offset + 1] = len_bytes[1];

    reply
}

/// Handle a single X11 request.
async fn handle_request<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    opcode: u8,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    match opcode {
        1 => handle_create_window(server, conn, data, stream).await,
        2 => handle_change_window_attributes(server, conn, data, stream).await,
        3 => handle_get_window_attributes(server, conn, data, stream).await,
        4 => handle_destroy_window(server, conn, data, stream).await,
        8 => handle_map_window(server, conn, data, stream).await,
        9 => handle_map_subwindows(server, conn, data, stream).await,
        10 => handle_unmap_window(server, conn, data, stream).await,
        12 => handle_configure_window(server, conn, data, stream).await,
        14 => handle_get_geometry(server, conn, data, stream).await,
        15 => handle_query_tree(server, conn, data, stream).await,
        16 => handle_intern_atom(server, conn, data, stream).await,
        17 => handle_get_atom_name(server, conn, data, stream).await,
        18 => handle_change_property(server, conn, data, stream).await,
        19 => handle_delete_property(server, conn, data, stream).await,
        20 => handle_get_property(server, conn, data, stream).await,
        22 => handle_set_selection_owner(server, conn, data, stream).await,
        23 => handle_get_selection_owner(server, conn, data, stream).await,
        24 => handle_convert_selection(server, conn, data, stream).await,
        25 => handle_send_event(server, conn, data, stream).await,
        26 => handle_grab_pointer(server, conn, data, stream).await,
        27 => handle_ungrab_pointer(server, conn, data, stream).await,
        31 => handle_grab_keyboard(server, conn, data, stream).await,
        32 => handle_ungrab_keyboard(server, conn, data, stream).await,
        35 => { Ok(()) }, // AllowEvents
        36 => handle_grab_server(server, conn, data, stream).await,
        37 => handle_ungrab_server(server, conn, data, stream).await,
        38 => handle_query_pointer(server, conn, data, stream).await,
        40 => handle_translate_coordinates(server, conn, data, stream).await,
        42 => handle_set_input_focus(server, conn, data, stream).await,
        43 => handle_get_input_focus(server, conn, data, stream).await,
        45 => handle_open_font(server, conn, data, stream).await,
        46 => handle_close_font(server, conn, data, stream).await,
        47 => handle_query_font(server, conn, data, stream).await,
        49 => handle_list_fonts(server, conn, data, stream).await,
        53 => handle_create_pixmap(server, conn, data, stream).await,
        54 => handle_free_pixmap(server, conn, data, stream).await,
        55 => handle_create_gc(server, conn, data, stream).await,
        56 => handle_change_gc(server, conn, data, stream).await,
        60 => handle_free_gc(server, conn, data, stream).await,
        61 => handle_clear_area(server, conn, data, stream).await,
        62 => handle_copy_area(server, conn, data, stream).await,
        64 => handle_poly_point(server, conn, data, stream).await,
        65 => handle_poly_line(server, conn, data, stream).await,
        66 => handle_poly_segment(server, conn, data, stream).await,
        67 => handle_poly_rectangle(server, conn, data, stream).await,
        68 => handle_poly_arc(server, conn, data, stream).await,
        69 => handle_fill_poly(server, conn, data, stream).await,
        70 => handle_poly_fill_rectangle(server, conn, data, stream).await,
        71 => handle_poly_fill_arc(server, conn, data, stream).await,
        72 => handle_put_image(server, conn, data, stream).await,
        74 => handle_poly_text8(server, conn, data, stream).await,
        76 => handle_image_text8(server, conn, data, stream).await,
        78 => handle_create_colormap(server, conn, data, stream).await,
        79 => handle_free_colormap(server, conn, data, stream).await,
        84 => handle_alloc_color(server, conn, data, stream).await,
        91 => handle_query_colors(server, conn, data, stream).await,
        92 => handle_lookup_color(server, conn, data, stream).await,
        97 => handle_query_best_size(server, conn, data, stream).await,
        98 => handle_query_extension(server, conn, data, stream).await,
        99 => handle_list_extensions(server, conn, data, stream).await,
        101 => handle_get_keyboard_mapping(server, conn, data, stream).await,
        102 => handle_change_keyboard_mapping(server, conn, data, stream).await,
        103 => handle_get_keyboard_control(server, conn, data, stream).await,
        108 => handle_get_screen_saver(server, conn, data, stream).await,
        115 => handle_set_close_down_mode(server, conn, data, stream).await,
        116 => handle_set_pointer_mapping(server, conn, data, stream).await,
        117 => handle_get_pointer_mapping(server, conn, data, stream).await,
        119 => handle_get_modifier_mapping(server, conn, data, stream).await,
        93 => handle_create_cursor(server, conn, data, stream).await,
        94 => handle_create_glyph_cursor(server, conn, data, stream).await,
        95 => handle_free_cursor(server, conn, data, stream).await,
        96 => handle_recolor_cursor(server, conn, data, stream).await,
        // Round-trip stubs: these opcodes REQUIRE replies
        44 => { // QueryKeymap
            let seq = conn.current_request_sequence();
            let mut reply = vec![1u8, 0]; // reply, unused
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 2); // additional 8 bytes = 2 words
            reply.extend(std::iter::repeat(0).take(24)); // padding to 32
            reply.extend(std::iter::repeat(0).take(8)); // 32 bytes of key state (all up)
            stream.write_all(&reply).await?;
            Ok(())
        }
        48 => { // QueryTextExtents
            let seq = conn.current_request_sequence();
            let mut reply = vec![1u8, 0]; // reply, draw-direction=LeftToRight
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0);
            write_u16_to(conn, &mut reply, 13); // font-ascent
            write_u16_to(conn, &mut reply, 2); // font-descent
            write_u16_to(conn, &mut reply, 13); // overall-ascent
            write_u16_to(conn, &mut reply, 2); // overall-descent
            write_u32_to(conn, &mut reply, 0); // overall-width (INT32)
            write_u32_to(conn, &mut reply, 0); // overall-left (INT32)
            write_u32_to(conn, &mut reply, 0); // overall-right (INT32)
            reply.extend(std::iter::repeat(0).take(32 - reply.len()));
            stream.write_all(&reply).await?;
            Ok(())
        }
        52 => { // GetFontPath
            let seq = conn.current_request_sequence();
            let mut reply = vec![1u8, 0];
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0);
            write_u16_to(conn, &mut reply, 0); // nPaths=0
            reply.extend(std::iter::repeat(0).take(22));
            stream.write_all(&reply).await?;
            Ok(())
        }
        73 => { // GetImage
            let format = data[1]; // 1=XYBitmap, 2=ZPixmap
            let drawable = read_u32(conn, &data[4..8]);
            let x = read_i16(conn, &data[8..10]);
            let y = read_i16(conn, &data[10..12]);
            let width = read_u16(conn, &data[12..14]);
            let height = read_u16(conn, &data[14..16]);
            let _plane_mask = read_u32(conn, &data[16..20]);
            let seq = conn.current_request_sequence();

            // Determine depth, visual, and pixel data from the drawable
            let (depth, visual, img_data) = if let Some(res) = server.resources.get(&drawable) {
                match res.value() {
                    super::resources::Resource::Window(win) => {
                        let w = win.read();
                        let bg = w.background_pixel.unwrap_or(0x00000000);
                        let depth = w.depth;
                        let visual = w.visual;
                        // Fill with background pixel
                        let pixel_count = width as usize * height as usize;
                        let mut pixels = Vec::with_capacity(pixel_count * 4);
                        let bg_bytes = match conn.byte_order {
                            ByteOrder::BigEndian => bg.to_be_bytes(),
                            ByteOrder::LittleEndian => bg.to_le_bytes(),
                        };
                        for _ in 0..pixel_count {
                            pixels.extend_from_slice(&bg_bytes);
                        }
                        (depth, visual, pixels)
                    }
                    super::resources::Resource::Pixmap(pix) => {
                        let pix = pix.read();
                        let depth = pix.depth;
                        let src_w = pix.width as usize;
                        let src_h = pix.height as usize;
                        let x = x.max(0) as usize;
                        let y = y.max(0) as usize;
                        let w = width as usize;
                        let h = height as usize;
                        let mut pixels = vec![0u8; w * h * 4];
                        for row in 0..h {
                            let sy = y + row;
                            if sy >= src_h { break; }
                            for col in 0..w {
                                let sx = x + col;
                                if sx >= src_w { continue; }
                                let src_off = (sy * src_w + sx) * 4;
                                let dst_off = (row * w + col) * 4;
                                if src_off + 4 <= pix.data.len() {
                                    pixels[dst_off..dst_off + 4].copy_from_slice(&pix.data[src_off..src_off + 4]);
                                }
                            }
                        }
                        (depth, 0, pixels) // pixmaps have no visual
                    }
                    _ => {
                        // Return error for non-drawable
                        let mut err = [0u8; 32];
                        err[0] = 0;
                        err[1] = 9; // BadDrawable
                        err[2] = (seq & 0xFF) as u8;
                        err[3] = ((seq >> 8) & 0xFF) as u8;
                        err[8] = opcode;
                        stream.write_all(&err).await?;
                        return Ok(());
                    }
                }
            } else {
                let mut err = [0u8; 32];
                err[0] = 0;
                err[1] = 9; // BadDrawable
                err[2] = (seq & 0xFF) as u8;
                err[3] = ((seq >> 8) & 0xFF) as u8;
                err[8] = opcode;
                stream.write_all(&err).await?;
                return Ok(());
            };

            // Build reply: 32-byte header + image data
            let img_len = img_data.len();
            let pad = (4 - (img_len % 4)) % 4;
            let reply_data_len = (img_len + pad) / 4;

            let mut reply = vec![1u8]; // reply
            reply.push(depth); // depth
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, reply_data_len as u32);
            write_u32_to(conn, &mut reply, visual); // visual
            reply.extend(std::iter::repeat(0).take(20)); // padding
            reply.extend_from_slice(&img_data);
            reply.extend(std::iter::repeat(0).take(pad));
            stream.write_all(&reply).await?;
            debug!("GetImage: drawable=0x{:08X} {}x{} format={} depth={} data_len={}", drawable, width, height, format, depth, img_len);
            Ok(())
        }
        106 => { // GetPointerControl
            let seq = conn.current_request_sequence();
            let mut reply = vec![1u8, 0];
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0);
            write_u16_to(conn, &mut reply, 2); // acceleration-numerator
            write_u16_to(conn, &mut reply, 1); // acceleration-denominator
            write_u16_to(conn, &mut reply, 4); // threshold
            reply.extend(std::iter::repeat(0).take(18));
            stream.write_all(&reply).await?;
            Ok(())
        }
        110 => { // ListHosts
            let seq = conn.current_request_sequence();
            let mut reply = vec![1u8, 0]; // reply, mode=disabled
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0);
            write_u16_to(conn, &mut reply, 0); // nHosts=0
            reply.extend(std::iter::repeat(0).take(22));
            stream.write_all(&reply).await?;
            Ok(())
        }
        // No-op for common requests that don't need replies
        5 | 11 | 28 | 29 | 30 | 33 | 34 | 41 | 51 | 57 | 58 | 59 |
        63 | 75 | 77 | 100 | 104 | 105 | 107 | 109 | 111 | 112 | 113 | 114 => {
            debug!("Stubbed opcode: {} (no-op)", opcode);
            Ok(())
        }
        _ => {
            debug!("Unimplemented opcode: {}", opcode);
            Err(ServerError::NotImplemented)
        }
    }
}

// --- Request Handlers ---

async fn handle_intern_atom<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 8 {
        return Err(ServerError::Protocol);
    }

    let only_if_exists = data[1] != 0;
    let name_len = read_u16(conn, &data[4..6]) as usize;

    if data.len() < 8 + name_len {
        return Err(ServerError::Protocol);
    }

    let name = std::str::from_utf8(&data[8..8 + name_len]).unwrap_or("");
    let atom = server.atoms.intern(name, only_if_exists).unwrap_or(0);

    debug!("InternAtom: '{}' only_if_exists={} -> {}", name, only_if_exists, atom);

    // Build reply
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data length
    write_u32_to(conn, &mut reply, atom); // atom
    reply.extend(std::iter::repeat(0).take(20)); // padding to 32 bytes

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_get_atom_name<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let atom = read_u32(conn, &data[4..8]);
    let name = server.atoms.get_name(atom).unwrap_or_default();
    let name_bytes = name.as_bytes();
    let pad = (4 - (name_bytes.len() % 4)) % 4;

    let seq = conn.current_request_sequence();
    let additional_len = (name_bytes.len() + pad) / 4;

    let mut reply = Vec::with_capacity(32 + name_bytes.len() + pad);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, additional_len as u32);
    write_u16_to(conn, &mut reply, name_bytes.len() as u16);
    reply.extend(std::iter::repeat(0).take(22)); // padding
    reply.extend_from_slice(name_bytes);
    reply.extend(std::iter::repeat(0).take(pad));

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_create_window<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 32 {
        return Err(ServerError::Protocol);
    }

    let depth = data[1];
    let wid = read_u32(conn, &data[4..8]);
    let parent = read_u32(conn, &data[8..12]);
    let x = read_i16(conn, &data[12..14]);
    let y = read_i16(conn, &data[14..16]);
    let width = read_u16(conn, &data[16..18]);
    let height = read_u16(conn, &data[18..20]);
    let border_width = read_u16(conn, &data[20..22]);
    let class = read_u16(conn, &data[22..24]);
    let visual = read_u32(conn, &data[24..28]);
    let value_mask = read_u32(conn, &data[28..32]);

    debug!(
        "CreateWindow: wid=0x{:08X} parent=0x{:08X} {}x{}+{}+{} depth={} class={}",
        wid, parent, width, height, x, y, depth, class
    );

    let effective_depth = if depth == 0 { 24 } else { depth };

    let mut window = super::resources::WindowState::new(
        wid,
        parent,
        x, y,
        width, height,
        border_width,
        effective_depth,
        super::resources::WindowClass::from(class),
        visual,
    );

    // Parse value list based on value_mask
    let mut offset = 32;
    if value_mask & 0x0001 != 0 { // BackPixmap
        let _v = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & 0x0002 != 0 { // BackPixel
        window.background_pixel = Some(read_u32(conn, &data[offset..offset+4]));
        offset += 4;
    }
    if value_mask & 0x0004 != 0 { // BorderPixmap
        let _v = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & 0x0008 != 0 { // BorderPixel
        window.border_pixel = Some(read_u32(conn, &data[offset..offset+4]));
        offset += 4;
    }
    if value_mask & 0x0010 != 0 { // BitGravity
        window.bit_gravity = read_u32(conn, &data[offset..offset+4]) as u8;
        offset += 4;
    }
    if value_mask & 0x0020 != 0 { // WinGravity
        window.win_gravity = read_u32(conn, &data[offset..offset+4]) as u8;
        offset += 4;
    }
    if value_mask & 0x0040 != 0 { // BackingStore
        window.backing_store = read_u32(conn, &data[offset..offset+4]) as u8;
        offset += 4;
    }
    if value_mask & 0x0080 != 0 { // BackingPlanes
        let _v = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & 0x0100 != 0 { // BackingPixel
        let _v = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & 0x0200 != 0 { // OverrideRedirect
        window.override_redirect = read_u32(conn, &data[offset..offset+4]) != 0;
        offset += 4;
    }
    if value_mask & 0x0400 != 0 { // SaveUnder
        let _v = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & 0x0800 != 0 { // EventMask
        window.event_mask = read_u32(conn, &data[offset..offset+4]);
        window.event_selections.push((conn.id, window.event_mask));
        offset += 4;
    }
    if value_mask & 0x1000 != 0 { // DoNotPropagateMask
        window.do_not_propagate_mask = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & 0x2000 != 0 { // Colormap
        window.colormap = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & 0x4000 != 0 { // Cursor
        window.cursor = read_u32(conn, &data[offset..offset+4]);
        let _ = offset;
    }

    // Add as child of parent
    if let Some(parent_res) = server.resources.get(&parent) {
        if let super::resources::Resource::Window(parent_win) = parent_res.value() {
            parent_win.write().children.push(wid);
        }
    }

    // Store the window resource
    server.resources.insert(
        wid,
        super::resources::Resource::Window(Arc::new(parking_lot::RwLock::new(window))),
    );

    Ok(())
}

async fn handle_change_window_attributes<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }
    let wid = read_u32(conn, &data[4..8]);
    let value_mask = read_u32(conn, &data[8..12]);

    debug!("ChangeWindowAttributes: window=0x{:08x} value_mask=0x{:04x}", wid, value_mask);

    if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let mut w = win.write();
            let mut offset = 12;

            if value_mask & 0x0001 != 0 { // BackPixmap
                offset += 4;
            }
            if value_mask & 0x0002 != 0 { // BackPixel
                w.background_pixel = Some(read_u32(conn, &data[offset..offset+4]));
                offset += 4;
            }
            if value_mask & 0x0004 != 0 { // BorderPixmap
                offset += 4;
            }
            if value_mask & 0x0008 != 0 { // BorderPixel
                w.border_pixel = Some(read_u32(conn, &data[offset..offset+4]));
                offset += 4;
            }
            if value_mask & 0x0010 != 0 { // BitGravity
                w.bit_gravity = read_u32(conn, &data[offset..offset+4]) as u8;
                offset += 4;
            }
            if value_mask & 0x0020 != 0 { // WinGravity
                w.win_gravity = read_u32(conn, &data[offset..offset+4]) as u8;
                offset += 4;
            }
            if value_mask & 0x0040 != 0 { // BackingStore
                w.backing_store = read_u32(conn, &data[offset..offset+4]) as u8;
                offset += 4;
            }
            if value_mask & 0x0080 != 0 { // BackingPlanes
                offset += 4;
            }
            if value_mask & 0x0100 != 0 { // BackingPixel
                offset += 4;
            }
            if value_mask & 0x0200 != 0 { // OverrideRedirect
                w.override_redirect = read_u32(conn, &data[offset..offset+4]) != 0;
                offset += 4;
            }
            if value_mask & 0x0400 != 0 { // SaveUnder
                offset += 4;
            }
            if value_mask & 0x0800 != 0 { // EventMask
                let mask = read_u32(conn, &data[offset..offset+4]);
                debug!("  EventMask for conn {} on window 0x{:08x}: 0x{:08x}", conn.id, wid, mask);
                // Update or add this client's event selection
                if let Some(entry) = w.event_selections.iter_mut().find(|(cid, _)| *cid == conn.id) {
                    entry.1 = mask;
                } else {
                    w.event_selections.push((conn.id, mask));
                }
                offset += 4;
            }
            if value_mask & 0x1000 != 0 { // DoNotPropagateMask
                w.do_not_propagate_mask = read_u32(conn, &data[offset..offset+4]);
                offset += 4;
            }
            if value_mask & 0x2000 != 0 { // Colormap
                w.colormap = read_u32(conn, &data[offset..offset+4]);
                offset += 4;
            }
            if value_mask & 0x4000 != 0 { // Cursor
                w.cursor = read_u32(conn, &data[offset..offset+4]);
                offset += 4;
            }
            let _ = offset;
        }
    }
    Ok(())
}

async fn handle_get_window_attributes<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let raw_wid = read_u32(conn, &data[4..8]);
    // Map window 0 (None) to root window for compatibility
    let wid = if raw_wid == 0 { server.screens[0].root_window } else { raw_wid };
    let seq = conn.current_request_sequence();

    let reply = if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            let mut reply = Vec::with_capacity(44);
            reply.push(1); // reply
            reply.push(w.backing_store);
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 3); // additional data length
            write_u32_to(conn, &mut reply, w.visual); // visual
            write_u16_to(conn, &mut reply, w.class as u16); // class
            reply.push(w.bit_gravity);
            reply.push(w.win_gravity);
            write_u32_to(conn, &mut reply, 0); // backing planes
            write_u32_to(conn, &mut reply, 0); // backing pixel
            reply.push(0); // save-under
            reply.push(if w.mapped { 1 } else { 0 }); // map-is-installed
            reply.push(if w.mapped { 2 } else { 0 }); // map-state (2=Viewable,1=Unviewable,0=Unmapped)
            reply.push(if w.override_redirect { 1 } else { 0 });
            write_u32_to(conn, &mut reply, w.colormap);
            write_u32_to(conn, &mut reply, w.event_mask); // all-event-masks
            write_u32_to(conn, &mut reply, w.event_mask); // your-event-mask
            write_u16_to(conn, &mut reply, 0); // do-not-propagate-mask
            write_u16_to(conn, &mut reply, 0); // unused
            // Drop the read guard before .await
            Some(reply)
        } else {
            None
        }
    } else {
        None
    };

    if let Some(reply) = reply {
        stream.write_all(&reply).await?;
        return Ok(());
    }
    Err(ServerError::ResourceNotFound(wid))
}

async fn handle_destroy_window<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    let wid = read_u32(conn, &data[4..8]);
    debug!("DestroyWindow: 0x{:08X}", wid);
    server.resources.remove(&wid);
    Ok(())
}

async fn handle_map_window<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let wid = read_u32(conn, &data[4..8]);
    debug!("MapWindow: 0x{:08X}", wid);

    // Extract window info while holding locks, then release before any .await
    let create_info = {
        if let Some(res) = server.resources.get(&wid) {
            if let super::resources::Resource::Window(win) = res.value() {
                let mut w = win.write();
                w.mapped = true;
                w.viewable = true;

                let is_toplevel = w.parent == server.screens[0].root_window;
                if is_toplevel && w.native_window.is_none() {
                    Some((w.x, w.y, w.width, w.height, w.override_redirect))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    }; // All DashMap refs and RwLock guards dropped here

    // Now safe to .await — no lock guards held
    if let Some((x, y, width, height, override_redirect)) = create_info {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = server.display_cmd_tx.send(crate::display::DisplayCommand::CreateWindow {
            x11_id: wid,
            x,
            y,
            width,
            height,
            title: format!("X11 Window 0x{:08X}", wid),
            override_redirect,
            reply: tx,
        });

        if let Ok(handle) = rx.await {
            // Re-acquire lock to store handle
            if let Some(res) = server.resources.get(&wid) {
                if let super::resources::Resource::Window(win) = res.value() {
                    win.write().native_window = Some(handle.clone());
                }
            }
            let _ = server.display_cmd_tx.send(
                crate::display::DisplayCommand::ShowWindow { handle },
            );

            // Clear the window and all mapped children with their background_pixel.
            // This ensures the IOSurface has correct background before client draws.
            clear_window_tree(server, wid);
        }
    }

    // Send MapNotify event to the client (inline for correct sequence ordering)
    let seq = conn.current_request_sequence();
    info!("MapWindow 0x{:08X}: sending MapNotify+Expose inline seq={}", wid, seq);
    let mut map_notify = super::events::build_map_notify(conn, wid, wid, false);
    set_event_sequence(conn, &mut map_notify, seq);
    stream.write_all(&map_notify).await?;

    // Send Expose events via event_tx (queued delivery) instead of inline.
    // This ensures xterm finishes its startup requests before receiving Expose,
    // preventing the race where xterm draws text then ClearArea wipes it.
    let (width, height) = if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            (w.width, w.height)
        } else { (0, 0) }
    } else { (0, 0) };

    if width > 0 && height > 0 {
        let mut expose = super::events::build_expose_event(conn, wid, 0, 0, width, height, 0);
        set_event_sequence(conn, &mut expose, seq);
        let _ = conn.event_tx.send(expose);
    }

    // Also send Expose to any mapped child windows via event_tx
    let children = if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            win.read().children.clone()
        } else { Vec::new() }
    } else { Vec::new() };
    for child_id in children {
        let child_info = if let Some(res) = server.resources.get(&child_id) {
            if let super::resources::Resource::Window(win) = res.value() {
                let w = win.read();
                if w.mapped { Some((w.width, w.height, w.background_pixel.unwrap_or(0))) } else { None }
            } else { None }
        } else { None };
        if let Some((cw, ch, _bg)) = child_info {
            if cw > 0 && ch > 0 {
                // Clear child window too (deferred from before MapWindow)
                dispatch_render_commands(server, child_id, vec![
                    crate::display::RenderCommand::ClearArea {
                        x: 0, y: 0, width: cw, height: ch,
                        bg_color: _bg,
                    }
                ]);
                let mut child_expose = super::events::build_expose_event(conn, child_id, 0, 0, cw, ch, 0);
                set_event_sequence(conn, &mut child_expose, seq);
                let _ = conn.event_tx.send(child_expose);
            }
        }
    }

    Ok(())
}

async fn handle_unmap_window<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    let wid = read_u32(conn, &data[4..8]);
    debug!("UnmapWindow: 0x{:08X}", wid);

    if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let mut w = win.write();
            w.mapped = false;
            w.viewable = false;

            if let Some(ref handle) = w.native_window {
                let _ = server.display_cmd_tx.send(
                    crate::display::DisplayCommand::HideWindow {
                        handle: handle.clone(),
                    },
                );
            }
        }
    }
    Ok(())
}

async fn handle_configure_window<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }
    let wid = read_u32(conn, &data[4..8]);
    let value_mask = read_u16(conn, &data[8..10]);

    let configure_info = if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let mut w = win.write();
            let old_width = w.width;
            let old_height = w.height;
            let mut offset = 12;

            if value_mask & 0x01 != 0 { // X
                w.x = read_i16(conn, &data[offset..offset+4]);
                offset += 4;
            }
            if value_mask & 0x02 != 0 { // Y
                w.y = read_i16(conn, &data[offset..offset+4]);
                offset += 4;
            }
            if value_mask & 0x04 != 0 { // Width
                w.width = read_u16(conn, &data[offset..offset+4]);
                offset += 4;
            }
            if value_mask & 0x08 != 0 { // Height
                w.height = read_u16(conn, &data[offset..offset+4]);
                offset += 4;
            }
            if value_mask & 0x10 != 0 { // BorderWidth
                w.border_width = read_u16(conn, &data[offset..offset+4]);
                offset += 4;
            }
            // 0x20 = Sibling, 0x40 = StackMode
            let _ = offset;

            debug!("ConfigureWindow: 0x{:08X} mask=0x{:04X} => {}x{} at ({},{}) border={}",
                wid, value_mask, w.width, w.height, w.x, w.y, w.border_width);

            // Update native window if exists
            if let Some(ref handle) = w.native_window {
                let _ = server.display_cmd_tx.send(
                    crate::display::DisplayCommand::MoveResizeWindow {
                        handle: handle.clone(),
                        x: w.x,
                        y: w.y,
                        width: w.width,
                        height: w.height,
                    },
                );
            }

            let size_changed = w.width != old_width || w.height != old_height;
            // Check if StructureNotify is selected on this window
            let wants_structure_notify = w.event_selections.iter()
                .any(|&(_cid, emask)| (emask & 0x00020000) != 0); // StructureNotifyMask
            Some((w.x, w.y, w.width, w.height, w.border_width, w.override_redirect, w.parent, size_changed, wants_structure_notify))
        } else { None }
    } else { None };

    // Send ConfigureNotify event inline (must maintain sequence order)
    if let Some((x, y, width, height, border_width, override_redirect, _parent, _size_changed, wants_structure_notify)) = configure_info {
        if wants_structure_notify {
            let seq = conn.current_request_sequence();
            let mut config_notify = super::events::build_configure_notify(
                conn, wid, wid, 0, x, y, width, height, border_width, override_redirect,
            );
            set_event_sequence(conn, &mut config_notify, seq);
            stream.write_all(&config_notify).await?;
        }
    }

    Ok(())
}

async fn handle_change_property<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 24 {
        return Err(ServerError::Protocol);
    }

    let _mode = data[1]; // 0=Replace, 1=Prepend, 2=Append
    let wid = read_u32(conn, &data[4..8]);
    let property = read_u32(conn, &data[8..12]);
    let type_atom = read_u32(conn, &data[12..16]);
    let format = data[16]; // 8, 16, or 32
    // bytes 17-19: unused
    let num_elements = read_u32(conn, &data[20..24]) as usize;

    let bytes_per_element = match format {
        8 => 1,
        16 => 2,
        32 => 4,
        _ => return Err(ServerError::Protocol),
    };
    let data_len = num_elements * bytes_per_element;
    let data_start = 24;

    if data.len() < data_start + data_len {
        return Err(ServerError::Protocol);
    }

    let prop_data = data[data_start..data_start + data_len].to_vec();

    if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let mut w = win.write();
            w.set_property(super::resources::Property {
                name: property,
                type_atom,
                format,
                data: prop_data,
            });

            // Check for WM_NAME changes to update native window title
            if property == crate::server::atoms::predefined::WM_NAME {
                if let Some(prop) = w.get_property(property) {
                    if let Ok(title) = std::str::from_utf8(&prop.data) {
                        if let Some(ref handle) = w.native_window {
                            let _ = server.display_cmd_tx.send(
                                crate::display::DisplayCommand::SetWindowTitle {
                                    handle: handle.clone(),
                                    title: title.to_string(),
                                },
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_get_property<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 24 {
        return Err(ServerError::Protocol);
    }

    let _delete = data[1] != 0;
    let wid = read_u32(conn, &data[4..8]);
    let property = read_u32(conn, &data[8..12]);
    let type_atom = read_u32(conn, &data[12..16]);
    let long_offset = read_u32(conn, &data[16..20]) as usize;
    let long_length = read_u32(conn, &data[20..24]) as usize;

    let seq = conn.current_request_sequence();

    // Build reply while holding lock, then write after dropping it
    let reply = if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            if let Some(prop) = w.get_property(property) {
                // Check type match
                if type_atom != 0 && type_atom != prop.type_atom {
                    // Type mismatch: return the actual type, no data
                    let mut reply = Vec::with_capacity(32);
                    reply.push(1); // reply
                    reply.push(prop.format);
                    write_u16_to(conn, &mut reply, seq);
                    write_u32_to(conn, &mut reply, 0); // additional data
                    write_u32_to(conn, &mut reply, prop.type_atom); // type
                    write_u32_to(conn, &mut reply, 0); // bytes after
                    write_u32_to(conn, &mut reply, 0); // length of value
                    reply.extend(std::iter::repeat(0).take(12)); // padding
                    Some(reply)
                } else {
                    let byte_offset = long_offset * 4;
                    let max_bytes = long_length * 4;
                    let available = prop.data.len().saturating_sub(byte_offset);
                    let actual_bytes = available.min(max_bytes);
                    let remaining = prop.data.len().saturating_sub(byte_offset + actual_bytes);

                    let prop_data = &prop.data[byte_offset..byte_offset + actual_bytes];
                    let pad = (4 - (actual_bytes % 4)) % 4;
                    let additional_len = (actual_bytes + pad) / 4;

                    let num_elements = match prop.format {
                        8 => actual_bytes,
                        16 => actual_bytes / 2,
                        32 => actual_bytes / 4,
                        _ => 0,
                    };

                    let mut reply = Vec::with_capacity(32 + actual_bytes + pad);
                    reply.push(1); // reply
                    reply.push(prop.format);
                    write_u16_to(conn, &mut reply, seq);
                    write_u32_to(conn, &mut reply, additional_len as u32);
                    write_u32_to(conn, &mut reply, prop.type_atom);
                    write_u32_to(conn, &mut reply, remaining as u32);
                    write_u32_to(conn, &mut reply, num_elements as u32);
                    reply.extend(std::iter::repeat(0).take(12)); // padding
                    reply.extend_from_slice(prop_data);
                    reply.extend(std::iter::repeat(0).take(pad));
                    Some(reply)
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    if let Some(reply) = reply {
        stream.write_all(&reply).await?;
        return Ok(());
    }

    // Property not found
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // format
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data
    write_u32_to(conn, &mut reply, 0); // type (None)
    write_u32_to(conn, &mut reply, 0); // bytes after
    write_u32_to(conn, &mut reply, 0); // length of value
    reply.extend(std::iter::repeat(0).take(12)); // padding
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_query_tree<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let wid = read_u32(conn, &data[4..8]);
    let seq = conn.current_request_sequence();

    let (parent, children) = if wid == server.screens[0].root_window {
        (0u32, Vec::new()) // Root has no parent
    } else if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            (w.parent, w.children.clone())
        } else {
            (0, Vec::new())
        }
    } else {
        (0, Vec::new())
    };

    let additional_len = children.len();
    let mut reply = Vec::with_capacity(32 + children.len() * 4);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, additional_len as u32);
    write_u32_to(conn, &mut reply, server.screens[0].root_window); // root
    write_u32_to(conn, &mut reply, parent);
    write_u16_to(conn, &mut reply, children.len() as u16);
    reply.extend(std::iter::repeat(0).take(14)); // padding

    for child in &children {
        write_u32_to(conn, &mut reply, *child);
    }

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_get_geometry<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let raw_drawable = read_u32(conn, &data[4..8]);
    // Map drawable 0 to root window for compatibility
    let drawable = if raw_drawable == 0 { server.screens[0].root_window } else { raw_drawable };
    let seq = conn.current_request_sequence();

    // Check if it's the root window
    if drawable == server.screens[0].root_window {
        let screen = &server.screens[0];
        let mut reply = Vec::with_capacity(32);
        reply.push(1); // reply
        reply.push(screen.root_depth);
        write_u16_to(conn, &mut reply, seq);
        write_u32_to(conn, &mut reply, 0); // additional data
        write_u32_to(conn, &mut reply, screen.root_window); // root
        write_i16_to(conn, &mut reply, 0); // x
        write_i16_to(conn, &mut reply, 0); // y
        write_u16_to(conn, &mut reply, screen.width_in_pixels); // width
        write_u16_to(conn, &mut reply, screen.height_in_pixels); // height
        write_u16_to(conn, &mut reply, 0); // border width
        reply.extend(std::iter::repeat(0).take(10)); // padding
        stream.write_all(&reply).await?;
        return Ok(());
    }

    let reply = if let Some(res) = server.resources.get(&drawable) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            let mut reply = Vec::with_capacity(32);
            reply.push(1); // reply
            reply.push(w.depth);
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0);
            write_u32_to(conn, &mut reply, server.screens[0].root_window); // root
            write_i16_to(conn, &mut reply, w.x);
            write_i16_to(conn, &mut reply, w.y);
            write_u16_to(conn, &mut reply, w.width);
            write_u16_to(conn, &mut reply, w.height);
            write_u16_to(conn, &mut reply, w.border_width);
            reply.extend(std::iter::repeat(0).take(10));
            Some(reply)
        } else {
            None
        }
    } else {
        None
    };

    if let Some(reply) = reply {
        stream.write_all(&reply).await?;
        return Ok(());
    }
    Err(ServerError::ResourceNotFound(drawable))
}

async fn handle_get_input_focus<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let focus = server.focus_window.load(std::sync::atomic::Ordering::Relaxed);
    let revert_to = server.focus_revert_to.load(std::sync::atomic::Ordering::Relaxed) as u8;
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(revert_to);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, focus);
    reply.extend(std::iter::repeat(0).take(20));
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_create_gc<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 16 {
        return Err(ServerError::Protocol);
    }

    let gcid = read_u32(conn, &data[4..8]);
    let drawable = read_u32(conn, &data[8..12]);
    let value_mask = read_u32(conn, &data[12..16]);

    let mut gc = super::resources::GContextState::new(gcid, drawable);

    // Parse GC value list
    let mut offset = 16;
    if value_mask & (1 << 0) != 0 { // Function
        gc.function = super::resources::GcFunction::from(read_u32(conn, &data[offset..offset+4]) as u8);
        offset += 4;
    }
    if value_mask & (1 << 1) != 0 { // PlaneMask
        gc.plane_mask = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & (1 << 2) != 0 { // Foreground
        gc.foreground = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & (1 << 3) != 0 { // Background
        gc.background = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & (1 << 4) != 0 { // LineWidth
        gc.line_width = read_u32(conn, &data[offset..offset+4]) as u16;
        offset += 4;
    }
    if value_mask & (1 << 5) != 0 { // LineStyle
        gc.line_style = read_u32(conn, &data[offset..offset+4]) as u8;
        offset += 4;
    }
    if value_mask & (1 << 14) != 0 { // Font
        gc.font = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
    }
    if value_mask & (1 << 16) != 0 { // GraphicsExposures
        gc.graphics_exposures = read_u32(conn, &data[offset..offset+4]) != 0;
        offset += 4;
    }
    let _ = offset; // suppress warning

    debug!("CreateGC: 0x{:08X} fg=0x{:06X} bg=0x{:06X}", gcid, gc.foreground, gc.background);

    server.resources.insert(
        gcid,
        super::resources::Resource::GContext(Arc::new(parking_lot::RwLock::new(gc))),
    );
    Ok(())
}

async fn handle_change_gc<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }

    let gcid = read_u32(conn, &data[4..8]);
    let value_mask = read_u32(conn, &data[8..12]);

    if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let mut g = gc.write();
            let mut offset = 12;

            if value_mask & (1 << 2) != 0 { // Foreground
                g.foreground = read_u32(conn, &data[offset..offset+4]);
                offset += 4;
            }
            if value_mask & (1 << 3) != 0 { // Background
                g.background = read_u32(conn, &data[offset..offset+4]);
                offset += 4;
            }
            if value_mask & (1 << 14) != 0 { // Font
                g.font = read_u32(conn, &data[offset..offset+4]);
                offset += 4;
            }
            let _ = offset;
        }
    }
    Ok(())
}

async fn handle_free_gc<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    let gcid = read_u32(conn, &data[4..8]);
    server.resources.remove(&gcid);
    Ok(())
}

/// Clear a window and all its mapped children with their background_pixel.
/// Called after MapWindow creates the native surface, so deferred ClearAreas work.
fn clear_window_tree(server: &Arc<XServer>, wid: u32) {
    let info = if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            Some((w.width, w.height, w.background_pixel.unwrap_or(0), w.children.clone()))
        } else { None }
    } else { None };

    if let Some((width, height, bg, children)) = info {
        dispatch_render_commands(server, wid, vec![
            crate::display::RenderCommand::ClearArea {
                x: 0, y: 0, width, height, bg_color: bg,
            }
        ]);
        // Clear mapped children too
        for child_id in children {
            if let Some(res) = server.resources.get(&child_id) {
                if let super::resources::Resource::Window(win) = res.value() {
                    let w = win.read();
                    if w.mapped {
                        let bg = w.background_pixel.unwrap_or(0);
                        let cw = w.width;
                        let ch = w.height;
                        drop(w);
                        drop(res);
                        dispatch_render_commands(server, child_id, vec![
                            crate::display::RenderCommand::ClearArea {
                                x: 0, y: 0, width: cw, height: ch, bg_color: bg,
                            }
                        ]);
                    }
                }
            }
        }
    }
}

/// Clip a ClearArea rectangle by mapped child window rectangles (ClipByChildren).
/// Returns a list of ClearArea commands that cover only the parent's own area.
fn clip_clear_by_children(
    x: i16, y: i16, width: u16, height: u16, bg_color: u32,
    children: &[(i16, i16, u16, u16)],
) -> Vec<crate::display::RenderCommand> {
    use crate::display::RenderCommand;
    let mut rects: Vec<(i16, i16, u16, u16)> = vec![(x, y, width, height)];

    // For each child, subtract its rectangle from all current rects
    for &(cx, cy, cw, ch) in children {
        let cx2 = cx as i32 + cw as i32;
        let cy2 = cy as i32 + ch as i32;
        let mut new_rects = Vec::new();

        for (rx, ry, rw, rh) in rects {
            let rx2 = rx as i32 + rw as i32;
            let ry2 = ry as i32 + rh as i32;

            // Check if child overlaps this rect
            if cx as i32 >= rx2 || cx2 <= rx as i32 || cy as i32 >= ry2 || cy2 <= ry as i32 {
                // No overlap — keep the rect
                new_rects.push((rx, ry, rw, rh));
                continue;
            }

            // Subtract: split into up to 4 strips
            // Top strip
            if (cy as i32) > (ry as i32) {
                let h = (cy as i32 - ry as i32).min(rh as i32) as u16;
                if h > 0 { new_rects.push((rx, ry, rw, h)); }
            }
            // Bottom strip
            if cy2 < ry2 {
                let top = cy2.max(ry as i32) as i16;
                let h = (ry2 - cy2).min(rh as i32) as u16;
                if h > 0 { new_rects.push((rx, top, rw, h)); }
            }
            // Left strip (between top and bottom)
            let strip_top = (cy as i32).max(ry as i32) as i16;
            let strip_bot = cy2.min(ry2) as i32;
            let strip_h = (strip_bot - strip_top as i32).max(0) as u16;
            if strip_h > 0 {
                if (cx as i32) > (rx as i32) {
                    let w = (cx as i32 - rx as i32).min(rw as i32) as u16;
                    if w > 0 { new_rects.push((rx, strip_top, w, strip_h)); }
                }
                // Right strip
                if cx2 < rx2 {
                    let left = cx2.max(rx as i32) as i16;
                    let w = (rx2 - cx2).min(rw as i32) as u16;
                    if w > 0 { new_rects.push((left, strip_top, w, strip_h)); }
                }
            }
        }
        rects = new_rects;
    }

    rects.into_iter().map(|(rx, ry, rw, rh)| {
        RenderCommand::ClearArea { x: rx, y: ry, width: rw, height: rh, bg_color }
    }).collect()
}

fn post_render_to_mailbox(server: &Arc<XServer>, win_id: u64, commands: Vec<crate::display::RenderCommand>) {
    let mut entry = server.render_mailbox.entry(win_id).or_default();
    entry.extend(commands);
}

fn dispatch_render_commands(
    server: &Arc<XServer>,
    drawable: u32,
    commands: Vec<crate::display::RenderCommand>,
) {
    // First pass: extract window info without holding DashMap ref during child lookups
    enum DispatchTarget {
        NativeWindow { handle_id: u64, children: Vec<u32> },
        ChildWindow { x: i16, y: i16, parent: u32 },
        Pixmap,
        None,
    }

    let target = if let Some(res) = server.resources.get(&drawable) {
        match res.value() {
            super::resources::Resource::Window(win) => {
                let w = win.read();
                if let Some(ref handle) = w.native_window {
                    DispatchTarget::NativeWindow {
                        handle_id: handle.id,
                        children: w.children.clone(),
                    }
                } else {
                    DispatchTarget::ChildWindow {
                        x: w.x, y: w.y, parent: w.parent,
                    }
                }
            }
            super::resources::Resource::Pixmap(_) => DispatchTarget::Pixmap,
            _ => DispatchTarget::None,
        }
    } else {
        return;
    };

    match target {
        DispatchTarget::NativeWindow { handle_id, children } => {
            // X11 ClipByChildren: ClearArea on parent must not cover child windows
            let has_clear = commands.iter().any(|c| matches!(c, crate::display::RenderCommand::ClearArea { .. }));
            let commands = if !children.is_empty() && has_clear {
                // Gather mapped child rectangles (DashMap ref is NOT held here)
                log::debug!("ClipByChildren: drawable=0x{:08X} children={} has_clear={}", drawable, children.len(), has_clear);
                let mut child_rects: Vec<(i16, i16, u16, u16)> = Vec::new();
                for cid in &children {
                    if let Some(cres) = server.resources.get(cid) {
                        if let super::resources::Resource::Window(cwin) = cres.value() {
                            let cw = cwin.read();
                            if cw.mapped {
                                child_rects.push((cw.x, cw.y, cw.width, cw.height));
                            }
                        }
                    }
                }
                log::debug!("  mapped child_rects: {:?}", child_rects);
                if child_rects.is_empty() {
                    commands
                } else {
                    // Clip ClearArea commands by child rects
                    use crate::display::RenderCommand;
                    let mut clipped = Vec::with_capacity(commands.len());
                    for cmd in commands {
                        match cmd {
                            RenderCommand::ClearArea { x, y, width, height, bg_color } => {
                                clipped.extend(
                                    clip_clear_by_children(x, y, width, height, bg_color, &child_rects)
                                );
                            }
                            other => clipped.push(other),
                        }
                    }
                    clipped
                }
            } else {
                commands
            };
            post_render_to_mailbox(server, handle_id, commands);
        }
        DispatchTarget::ChildWindow { x: child_x, y: child_y, parent: parent_id } => {
            // Child window without its own native surface — find ancestor
            // with a native_window and offset coordinates
            let mut offset_x = child_x as i16;
            let mut offset_y = child_y as i16;
            let mut current = parent_id;

            // Walk up ancestor chain to find the window with a native surface
            for _ in 0..32 { // safety limit
                if let Some(pres) = server.resources.get(&current) {
                    if let super::resources::Resource::Window(pwin) = pres.value() {
                        let pw = pwin.read();
                        if let Some(ref handle) = pw.native_window {
                            // Found the native ancestor — offset all commands
                            let adjusted = offset_render_commands(commands, offset_x, offset_y);
                            post_render_to_mailbox(server, handle.id, adjusted);
                            return;
                        }
                        offset_x += pw.x as i16;
                        offset_y += pw.y as i16;
                        current = pw.parent;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            debug!("Window 0x{:08X}: no ancestor with native surface found", drawable);
        }
        DispatchTarget::Pixmap => {
            if let Some(res) = server.resources.get(&drawable) {
                if let super::resources::Resource::Pixmap(pix) = res.value() {
                    let mut p = pix.write();
                    let w = p.width as u32;
                    let h = p.height as u32;
                    let stride = w * 4;
                    for cmd in &commands {
                        crate::display::renderer::render_to_buffer(&mut p.data, w, h, stride, cmd);
                    }
                }
            }
        }
        DispatchTarget::None => {}
    }
}

/// Offset all render commands by (dx, dy) for child-to-parent coordinate translation.
fn offset_render_commands(
    commands: Vec<crate::display::RenderCommand>,
    dx: i16,
    dy: i16,
) -> Vec<crate::display::RenderCommand> {
    use crate::display::RenderCommand;
    commands.into_iter().map(|cmd| {
        match cmd {
            RenderCommand::FillRectangle { x, y, width, height, color } =>
                RenderCommand::FillRectangle { x: x + dx, y: y + dy, width, height, color },
            RenderCommand::ClearArea { x, y, width, height, bg_color } =>
                RenderCommand::ClearArea { x: x + dx, y: y + dy, width, height, bg_color },
            RenderCommand::DrawLine { x1, y1, x2, y2, color, line_width } =>
                RenderCommand::DrawLine { x1: x1 + dx, y1: y1 + dy, x2: x2 + dx, y2: y2 + dy, color, line_width },
            RenderCommand::FillArc { x, y, width, height, angle1, angle2, color } =>
                RenderCommand::FillArc { x: x + dx, y: y + dy, width, height, angle1, angle2, color },
            RenderCommand::DrawArc { x, y, width, height, angle1, angle2, color, line_width } =>
                RenderCommand::DrawArc { x: x + dx, y: y + dy, width, height, angle1, angle2, color, line_width },
            RenderCommand::DrawRectangle { x, y, width, height, color, line_width } =>
                RenderCommand::DrawRectangle { x: x + dx, y: y + dy, width, height, color, line_width },
            RenderCommand::PutImage { x, y, width, height, depth, format, data } =>
                RenderCommand::PutImage { x: x + dx, y: y + dy, width, height, depth, format, data },
            RenderCommand::DrawText { x, y, text, font_id, color, bg_color } =>
                RenderCommand::DrawText { x: x + dx, y: y + dy, text, font_id, color, bg_color },
            RenderCommand::CopyArea { src_x, src_y, dst_x, dst_y, width, height } =>
                RenderCommand::CopyArea { src_x: src_x + dx, src_y: src_y + dy, dst_x: dst_x + dx, dst_y: dst_y + dy, width, height },
            RenderCommand::FillPolygon { points, color } =>
                RenderCommand::FillPolygon { points: points.into_iter().map(|(x, y)| (x + dx, y + dy)).collect(), color },
            other => other, // Any other commands pass through unchanged
        }
    }).collect()
}

async fn handle_poly_fill_rectangle<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }

    let drawable = read_u32(conn, &data[4..8]);
    let gcid = read_u32(conn, &data[8..12]);

    // Get foreground color from GC
    let fg_color = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            gc.read().foreground
        } else { 0 }
    } else { 0 };

    // Parse rectangles (each is 8 bytes: x, y, width, height)
    let mut commands = Vec::new();
    let mut offset = 12;
    while offset + 8 <= data.len() {
        let x = read_i16(conn, &data[offset..offset+2]);
        let y = read_i16(conn, &data[offset+2..offset+4]);
        let width = read_u16(conn, &data[offset+4..offset+6]);
        let height = read_u16(conn, &data[offset+6..offset+8]);
        offset += 8;

        commands.push(crate::display::RenderCommand::FillRectangle {
            x, y, width, height, color: fg_color,
        });
    }

    dispatch_render_commands(server, drawable, commands);
    Ok(())
}

async fn handle_clear_area<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 16 {
        return Err(ServerError::Protocol);
    }

    let exposures = data[1] != 0;
    let wid = read_u32(conn, &data[4..8]);
    let x = read_i16(conn, &data[8..10]);
    let y = read_i16(conn, &data[10..12]);
    let mut width = read_u16(conn, &data[12..14]);
    let mut height = read_u16(conn, &data[14..16]);

    log::debug!("ClearArea: wid=0x{:08X} ({},{} {}x{}) exposures={}", wid, x, y, width, height, exposures);

    // Extract window info without holding locks across await
    let (bg_color, is_window) = if let Some(res) = server.resources.get(&wid) {
        match res.value() {
            super::resources::Resource::Window(win) => {
                let w = win.read();
                if width == 0 { width = w.width.saturating_sub(x as u16); }
                if height == 0 { height = w.height.saturating_sub(y as u16); }
                (w.background_pixel.unwrap_or(0), true)
            }
            super::resources::Resource::Pixmap(pix) => {
                let p = pix.read();
                if width == 0 { width = p.width.saturating_sub(x as u16); }
                if height == 0 { height = p.height.saturating_sub(y as u16); }
                (0u32, false)
            }
            _ => return Ok(()),
        }
    } else {
        return Ok(());
    };

    let command = crate::display::RenderCommand::ClearArea {
        x, y, width, height, bg_color,
    };
    dispatch_render_commands(server, wid, vec![command]);

    // X11 spec: ClearArea with exposures=true generates Expose events.
    // This tells the client to redraw the cleared region.
    if exposures && is_window && width > 0 && height > 0 {
        let seq = conn.current_request_sequence();
        let mut expose = super::events::build_expose_event(
            conn, wid,
            x as u16, y as u16, width, height,
            0,
        );
        set_event_sequence(conn, &mut expose, seq);
        stream.write_all(&expose).await?;
    }
    Ok(())
}

async fn handle_copy_area<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 28 {
        return Err(ServerError::Protocol);
    }

    let src_drawable = read_u32(conn, &data[4..8]);
    let dst_drawable = read_u32(conn, &data[8..12]);
    let gcid = read_u32(conn, &data[12..16]);
    let src_x = read_i16(conn, &data[16..18]);
    let src_y = read_i16(conn, &data[18..20]);
    let dst_x = read_i16(conn, &data[20..22]);
    let dst_y = read_i16(conn, &data[22..24]);
    let width = read_u16(conn, &data[24..26]);
    let height = read_u16(conn, &data[26..28]);

    debug!("CopyArea: src=0x{:08X} dst=0x{:08X} gc=0x{:08X} ({},{}) -> ({},{}) {}x{}",
           src_drawable, dst_drawable, gcid, src_x, src_y, dst_x, dst_y, width, height);

    // Check if GC has graphics_exposures set
    let graphics_exposures = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            gc.read().graphics_exposures
        } else { false }
    } else { false };

    // Read pixels from source drawable
    let src_data = {
        if let Some(res) = server.resources.get(&src_drawable) {
            match res.value() {
                super::resources::Resource::Pixmap(pix) => {
                    let p = pix.read();
                    let pw = p.width as u32;
                    let mut pixels = vec![0u8; width as usize * height as usize * 4];
                    for row in 0..height as u32 {
                        let sy = src_y as u32 + row;
                        if sy >= p.height as u32 { break; }
                        let src_off = (sy * pw * 4 + src_x as u32 * 4) as usize;
                        let dst_off = (row * width as u32 * 4) as usize;
                        let copy_w = (width as u32).min(pw.saturating_sub(src_x as u32)) as usize;
                        let src_end = src_off + copy_w * 4;
                        let dst_end = dst_off + copy_w * 4;
                        if src_end <= p.data.len() && dst_end <= pixels.len() {
                            pixels[dst_off..dst_end].copy_from_slice(&p.data[src_off..src_end]);
                        }
                    }
                    Some(pixels)
                }
                super::resources::Resource::Window(_win) => {
                    // CopyArea from window to window — dispatch as a render command
                    // The renderer will handle it as an in-buffer copy (for scrolling)
                    let command = crate::display::RenderCommand::CopyArea {
                        src_x, src_y, dst_x, dst_y, width, height,
                    };
                    dispatch_render_commands(server, dst_drawable, vec![command]);

                    // Send NoExposure event if graphics_exposures is set in the GC.
                    // X11 spec: CopyArea with no obscured regions + graphics_exposures=True
                    // requires a NoExposure event. Without this, clients like xterm block.
                    if graphics_exposures {
                        let seq = conn.current_request_sequence();
                        let mut evt = vec![0u8; 32];
                        evt[0] = super::protocol::event_type::NO_EXPOSURE; // 14
                        // byte 1 unused
                        evt[2] = (seq & 0xFF) as u8;
                        evt[3] = ((seq >> 8) & 0xFF) as u8;
                        write_u32_at(conn, &mut evt, 4, dst_drawable); // drawable
                        write_u16_at(conn, &mut evt, 8, 0); // minor-opcode (0 for core CopyArea)
                        evt[10] = 62; // major-opcode: CopyArea
                        stream.write_all(&evt).await?;
                    }
                    return Ok(());
                }
                _ => None,
            }
        } else { None }
    };

    if let Some(pixels) = src_data {
        let command = crate::display::RenderCommand::PutImage {
            x: dst_x,
            y: dst_y,
            width,
            height,
            depth: 24,
            format: 2, // ZPixmap
            data: pixels,
        };
        dispatch_render_commands(server, dst_drawable, vec![command]);
    }

    // Send NoExposure for pixmap-to-anything copies too
    if graphics_exposures {
        let seq = conn.current_request_sequence();
        let mut evt = vec![0u8; 32];
        evt[0] = super::protocol::event_type::NO_EXPOSURE;
        evt[2] = (seq & 0xFF) as u8;
        evt[3] = ((seq >> 8) & 0xFF) as u8;
        write_u32_at(conn, &mut evt, 4, dst_drawable);
        write_u16_at(conn, &mut evt, 8, 0);
        evt[10] = 62;
        stream.write_all(&evt).await?;
    }
    Ok(())
}

async fn handle_poly_point<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }

    let coordinate_mode = data[1]; // 0=Origin, 1=Previous
    let drawable = read_u32(conn, &data[4..8]);
    let gcid = read_u32(conn, &data[8..12]);

    let fg_color = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            gc.read().foreground
        } else { 0 }
    } else { 0 };

    let mut commands = Vec::new();
    let mut prev_x: i16 = 0;
    let mut prev_y: i16 = 0;
    let mut offset = 12;
    while offset + 4 <= data.len() {
        let mut px = read_i16(conn, &data[offset..offset+2]);
        let mut py = read_i16(conn, &data[offset+2..offset+4]);
        offset += 4;
        if coordinate_mode == 1 {
            px += prev_x;
            py += prev_y;
        }
        prev_x = px;
        prev_y = py;
        // Draw a single pixel as a 1x1 rectangle
        commands.push(crate::display::RenderCommand::FillRectangle {
            x: px, y: py, width: 1, height: 1, color: fg_color,
        });
    }

    dispatch_render_commands(server, drawable, commands);
    Ok(())
}

async fn handle_poly_rectangle<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }

    let drawable = read_u32(conn, &data[4..8]);
    let gcid = read_u32(conn, &data[8..12]);

    let (fg_color, line_width) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.line_width)
        } else { (0xFFFFFF, 0) }
    } else { (0xFFFFFF, 0) };

    let mut commands = Vec::new();
    let mut offset = 12;
    while offset + 8 <= data.len() {
        let x = read_i16(conn, &data[offset..offset+2]);
        let y = read_i16(conn, &data[offset+2..offset+4]);
        let w = read_u16(conn, &data[offset+4..offset+6]);
        let h = read_u16(conn, &data[offset+6..offset+8]);
        offset += 8;

        commands.push(crate::display::RenderCommand::DrawRectangle {
            x, y, width: w, height: h, color: fg_color, line_width,
        });
    }

    dispatch_render_commands(server, drawable, commands);
    Ok(())
}

async fn handle_poly_line<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }

    let coordinate_mode = data[1]; // 0=Origin, 1=Previous
    let drawable = read_u32(conn, &data[4..8]);
    let gcid = read_u32(conn, &data[8..12]);

    let (fg_color, line_width) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.line_width)
        } else { (0xFFFFFF, 0) }
    } else { (0xFFFFFF, 0) };

    let mut points = Vec::new();
    let mut offset = 12;
    while offset + 4 <= data.len() {
        let px = read_i16(conn, &data[offset..offset+2]);
        let py = read_i16(conn, &data[offset+2..offset+4]);
        offset += 4;
        points.push((px, py));
    }

    // Convert relative coordinates to absolute
    if coordinate_mode == 1 && points.len() > 1 {
        for i in 1..points.len() {
            points[i].0 += points[i-1].0;
            points[i].1 += points[i-1].1;
        }
    }

    let mut commands = Vec::new();
    for i in 0..points.len().saturating_sub(1) {
        commands.push(crate::display::RenderCommand::DrawLine {
            x1: points[i].0, y1: points[i].1,
            x2: points[i+1].0, y2: points[i+1].1,
            color: fg_color, line_width,
        });
    }

    dispatch_render_commands(server, drawable, commands);
    Ok(())
}

async fn handle_poly_segment<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }

    let drawable = read_u32(conn, &data[4..8]);
    let gcid = read_u32(conn, &data[8..12]);

    let (fg_color, line_width) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.line_width)
        } else { (0xFFFFFF, 0) }
    } else { (0xFFFFFF, 0) };

    let mut commands = Vec::new();
    let mut offset = 12;
    while offset + 8 <= data.len() {
        let x1 = read_i16(conn, &data[offset..offset+2]);
        let y1 = read_i16(conn, &data[offset+2..offset+4]);
        let x2 = read_i16(conn, &data[offset+4..offset+6]);
        let y2 = read_i16(conn, &data[offset+6..offset+8]);
        offset += 8;
        commands.push(crate::display::RenderCommand::DrawLine {
            x1, y1, x2, y2, color: fg_color, line_width,
        });
    }

    dispatch_render_commands(server, drawable, commands);
    Ok(())
}

async fn handle_fill_poly<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 16 {
        return Err(ServerError::Protocol);
    }

    let drawable = read_u32(conn, &data[4..8]);
    let gcid = read_u32(conn, &data[8..12]);
    let _shape = data[12];
    let coordinate_mode = data[13];

    let fg_color = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            gc.read().foreground
        } else { 0xFFFFFF }
    } else { 0xFFFFFF };

    let mut points: Vec<(i16, i16)> = Vec::new();
    let mut offset = 16;
    while offset + 4 <= data.len() {
        let px = read_i16(conn, &data[offset..offset+2]);
        let py = read_i16(conn, &data[offset+2..offset+4]);
        offset += 4;
        points.push((px, py));
    }

    if coordinate_mode == 1 && points.len() > 1 {
        for i in 1..points.len() {
            points[i].0 += points[i-1].0;
            points[i].1 += points[i-1].1;
        }
    }

    // Emit a filled polygon render command
    let commands = vec![crate::display::RenderCommand::FillPolygon {
        points,
        color: fg_color,
    }];

    dispatch_render_commands(server, drawable, commands);
    Ok(())
}

async fn handle_poly_arc<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }

    let drawable = read_u32(conn, &data[4..8]);
    let gcid = read_u32(conn, &data[8..12]);

    let (fg_color, line_width) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.line_width)
        } else { (0xFFFFFF, 0) }
    } else { (0xFFFFFF, 0) };

    // Each arc is 12 bytes: x(2), y(2), width(2), height(2), angle1(2), angle2(2)
    let mut commands = Vec::new();
    let mut offset = 12;
    while offset + 12 <= data.len() {
        let x = read_i16(conn, &data[offset..offset+2]);
        let y = read_i16(conn, &data[offset+2..offset+4]);
        let width = read_u16(conn, &data[offset+4..offset+6]);
        let height = read_u16(conn, &data[offset+6..offset+8]);
        let angle1 = read_i16(conn, &data[offset+8..offset+10]);
        let angle2 = read_i16(conn, &data[offset+10..offset+12]);
        offset += 12;

        commands.push(crate::display::RenderCommand::DrawArc {
            x, y, width, height, angle1, angle2,
            color: fg_color, line_width,
        });
    }

    dispatch_render_commands(server, drawable, commands);
    Ok(())
}

async fn handle_poly_fill_arc<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }

    let drawable = read_u32(conn, &data[4..8]);
    let gcid = read_u32(conn, &data[8..12]);

    let fg_color = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            gc.read().foreground
        } else { 0xFFFFFF }
    } else { 0xFFFFFF };

    // Each arc is 12 bytes: x(2), y(2), width(2), height(2), angle1(2), angle2(2)
    let mut commands = Vec::new();
    let mut offset = 12;
    while offset + 12 <= data.len() {
        let x = read_i16(conn, &data[offset..offset+2]);
        let y = read_i16(conn, &data[offset+2..offset+4]);
        let width = read_u16(conn, &data[offset+4..offset+6]);
        let height = read_u16(conn, &data[offset+6..offset+8]);
        let angle1 = read_i16(conn, &data[offset+8..offset+10]);
        let angle2 = read_i16(conn, &data[offset+10..offset+12]);
        offset += 12;

        commands.push(crate::display::RenderCommand::FillArc {
            x, y, width, height, angle1, angle2,
            color: fg_color,
        });
    }

    dispatch_render_commands(server, drawable, commands);
    Ok(())
}

async fn handle_put_image<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 24 {
        return Err(ServerError::Protocol);
    }

    let format = data[1]; // 0=Bitmap, 1=XYPixmap, 2=ZPixmap
    let drawable = read_u32(conn, &data[4..8]);
    let _gcid = read_u32(conn, &data[8..12]);
    let width = read_u16(conn, &data[12..14]);
    let height = read_u16(conn, &data[14..16]);
    let dst_x = read_i16(conn, &data[16..18]);
    let dst_y = read_i16(conn, &data[18..20]);
    let _left_pad = data[20];
    let depth = data[21];

    debug!("PutImage: drawable=0x{:08X} {}x{} at ({},{}) depth={} format={}", drawable, width, height, dst_x, dst_y, depth, format);
    let image_data = data[24..].to_vec();

    let command = crate::display::RenderCommand::PutImage {
        x: dst_x,
        y: dst_y,
        width,
        height,
        depth,
        format,
        data: image_data,
    };

    dispatch_render_commands(server, drawable, vec![command]);
    Ok(())
}

async fn handle_poly_text8<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 16 {
        return Err(ServerError::Protocol);
    }

    let drawable = read_u32(conn, &data[4..8]);
    let gcid = read_u32(conn, &data[8..12]);
    let x = read_i16(conn, &data[12..14]);
    let y = read_i16(conn, &data[14..16]);

    let (fg_color, font_id) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.font)
        } else { (0xFFFFFF, 0) }
    } else { (0xFFFFFF, 0) };

    // Parse text items from offset 16
    let mut text_bytes = Vec::new();
    let mut offset = 16;
    while offset < data.len() {
        let len = data[offset] as usize;
        if len == 0 || len == 255 { break; }
        let _delta = data[offset + 1] as i8;
        offset += 2;
        if offset + len <= data.len() {
            text_bytes.extend_from_slice(&data[offset..offset + len]);
        }
        offset += len;
    }

    if !text_bytes.is_empty() {
        let command = crate::display::RenderCommand::DrawText {
            x, y, text: text_bytes, font_id, color: fg_color, bg_color: None,
        };
        dispatch_render_commands(server, drawable, vec![command]);
    }
    Ok(())
}

async fn handle_image_text8<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 16 {
        return Err(ServerError::Protocol);
    }

    let str_len = data[1] as usize;
    let drawable = read_u32(conn, &data[4..8]);
    let gcid = read_u32(conn, &data[8..12]);
    let x = read_i16(conn, &data[12..14]);
    let y = read_i16(conn, &data[14..16]);

    let (fg_color, bg_color, font_id) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, Some(gc.background), gc.font)
        } else { (0xFFFFFF, Some(0), 0) }
    } else { (0xFFFFFF, Some(0), 0) };

    let text_end = (16 + str_len).min(data.len());
    let text = data[16..text_end].to_vec();

    if !text.is_empty() {
        let command = crate::display::RenderCommand::DrawText {
            x, y, text, font_id, color: fg_color, bg_color,
        };
        dispatch_render_commands(server, drawable, vec![command]);
    }
    Ok(())
}

async fn handle_query_extension<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 8 {
        return Err(ServerError::Protocol);
    }

    let name_len = read_u16(conn, &data[4..6]) as usize;
    if data.len() < 8 + name_len {
        return Err(ServerError::Protocol);
    }

    let name = std::str::from_utf8(&data[8..8 + name_len]).unwrap_or("");
    debug!("QueryExtension: '{}'", name);

    let seq = conn.current_request_sequence();

    // For now, report no extensions present
    // TODO: implement BIG-REQUESTS, RENDER, etc.
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data
    reply.push(0); // present (false)
    reply.push(0); // major opcode
    reply.push(0); // first event
    reply.push(0); // first error
    reply.extend(std::iter::repeat(0).take(20)); // padding

    stream.write_all(&reply).await?;
    Ok(())
}

// --- Pixmap, Font, and additional handlers ---

async fn handle_create_pixmap<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 16 {
        return Err(ServerError::Protocol);
    }
    let depth = data[1];
    let pid = read_u32(conn, &data[4..8]);
    let _drawable = read_u32(conn, &data[8..12]);
    let width = read_u16(conn, &data[12..14]);
    let height = read_u16(conn, &data[14..16]);
    debug!("CreatePixmap: 0x{:08X} {}x{} depth={}", pid, width, height, depth);

    let pixmap = super::resources::PixmapState {
        id: pid,
        drawable: _drawable,
        width,
        height,
        depth,
        data: vec![0u8; (width as usize) * (height as usize) * 4],
    };
    server.resources.insert(pid, super::resources::Resource::Pixmap(
        Arc::new(parking_lot::RwLock::new(pixmap)),
    ));
    Ok(())
}

async fn handle_free_pixmap<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    let pid = read_u32(conn, &data[4..8]);
    server.resources.remove(&pid);
    Ok(())
}

async fn handle_open_font<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }
    let fid = read_u32(conn, &data[4..8]);
    let name_len = read_u16(conn, &data[8..10]) as usize;
    let name = if data.len() >= 12 + name_len {
        std::str::from_utf8(&data[12..12 + name_len]).unwrap_or("unknown").to_string()
    } else {
        "unknown".to_string()
    };
    debug!("OpenFont: 0x{:08X} '{}'", fid, name);

    let font = super::resources::FontState {
        id: fid,
        name,
        ascent: 10,
        descent: 3,
        default_char: 0,
        min_char_width: 6,
        max_char_width: 6,
    };
    server.resources.insert(fid, super::resources::Resource::Font(
        Arc::new(parking_lot::RwLock::new(font)),
    ));
    Ok(())
}

async fn handle_close_font<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    let fid = read_u32(conn, &data[4..8]);
    server.resources.remove(&fid);
    Ok(())
}

async fn handle_query_font<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let fid = read_u32(conn, &data[4..8]);
    let seq = conn.current_request_sequence();

    // Build reply with font metrics
    let (ascent, descent, min_char, max_char, max_width, default_char) = {
        if let Some(res) = server.resources.get(&fid) {
            if let super::resources::Resource::Font(font) = res.value() {
                let f = font.read();
                (f.ascent, f.descent, 32u16, 126u16, f.max_char_width, f.default_char)
            } else {
                (12, 3, 32u16, 126u16, 8, 0)
            }
        } else {
            (12, 3, 32u16, 126u16, 8, 0)
        }
    };

    let num_chars = (max_char - min_char + 1) as u32;
    let num_properties = 0u16;
    // Each CharInfo is 12 bytes
    let char_info_size = num_chars * 12;
    let _additional_len = (8 + char_info_size) / 4;

    let mut reply = Vec::with_capacity(60 + char_info_size as usize);
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 7 + num_properties as u32 + num_chars * 3); // additional data (in 4-byte units)

    // min-bounds (CharInfo: 12 bytes)
    write_i16_to(conn, &mut reply, 0);  // left-side-bearing
    write_i16_to(conn, &mut reply, 0);  // right-side-bearing
    write_i16_to(conn, &mut reply, max_width as i16); // character-width
    write_i16_to(conn, &mut reply, ascent as i16);    // ascent
    write_i16_to(conn, &mut reply, -(descent as i16)); // descent
    write_u16_to(conn, &mut reply, 0);  // attributes

    // 4 bytes unused
    write_u32_to(conn, &mut reply, 0);

    // max-bounds (CharInfo: 12 bytes)
    write_i16_to(conn, &mut reply, 0);
    write_i16_to(conn, &mut reply, max_width as i16);
    write_i16_to(conn, &mut reply, max_width as i16);
    write_i16_to(conn, &mut reply, ascent as i16);
    write_i16_to(conn, &mut reply, -(descent as i16));
    write_u16_to(conn, &mut reply, 0);

    // 4 bytes unused
    write_u32_to(conn, &mut reply, 0);

    write_u16_to(conn, &mut reply, min_char); // min-char-or-byte2
    write_u16_to(conn, &mut reply, max_char); // max-char-or-byte2
    write_u16_to(conn, &mut reply, default_char); // default-char
    write_u16_to(conn, &mut reply, num_properties);
    reply.push(0); // draw-direction (LeftToRight)
    reply.push(0); // min-byte1
    reply.push(0); // max-byte1
    reply.push(1); // all-chars-exist (true)
    write_i16_to(conn, &mut reply, ascent as i16);  // font-ascent
    write_i16_to(conn, &mut reply, descent as i16); // font-descent
    write_u32_to(conn, &mut reply, num_chars); // number of CharInfos

    // CharInfo for each character
    for _i in 0..num_chars {
        write_i16_to(conn, &mut reply, 0);                // left-side-bearing
        write_i16_to(conn, &mut reply, max_width as i16); // right-side-bearing
        write_i16_to(conn, &mut reply, max_width as i16); // character-width
        write_i16_to(conn, &mut reply, ascent as i16);    // ascent
        write_i16_to(conn, &mut reply, -(descent as i16)); // descent
        write_u16_to(conn, &mut reply, 0);                // attributes
    }

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_map_subwindows<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    let wid = read_u32(conn, &data[4..8]);
    debug!("MapSubwindows: 0x{:08X}", wid);

    // Get children list
    let children = {
        if let Some(res) = server.resources.get(&wid) {
            if let super::resources::Resource::Window(win) = res.value() {
                win.read().children.clone()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    };

    // Map each child
    for child_id in children {
        if let Some(res) = server.resources.get(&child_id) {
            if let super::resources::Resource::Window(win) = res.value() {
                let mut w = win.write();
                w.mapped = true;
                w.viewable = true;
            }
        }
    }
    Ok(())
}

async fn handle_delete_property<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    let wid = read_u32(conn, &data[4..8]);
    let property = read_u32(conn, &data[8..12]);
    if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            win.write().delete_property(property);
        }
    }
    Ok(())
}

async fn handle_set_input_focus<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    let revert_to = data[1]; // 0=None, 1=PointerRoot, 2=Parent
    let focus = read_u32(conn, &data[4..8]); // 0=None, 1=PointerRoot, else=window
    info!("SetInputFocus: focus=0x{:08X} revert_to={}", focus, revert_to);
    server.focus_window.store(focus, std::sync::atomic::Ordering::Relaxed);
    server.focus_revert_to.store(revert_to as u32, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

// --- Additional request handlers for xdpyinfo compatibility ---

async fn handle_query_best_size<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let _class = data[1]; // 0=Cursor, 1=Tile, 2=Stipple
    let width = read_u16(conn, &data[8..10]);
    let height = read_u16(conn, &data[10..12]);
    let seq = conn.current_request_sequence();

    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data
    write_u16_to(conn, &mut reply, width); // best width
    write_u16_to(conn, &mut reply, height); // best height
    reply.extend(std::iter::repeat(0).take(20)); // padding
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_list_extensions<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    // Return empty extension list for now
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // number of extensions
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data length
    reply.extend(std::iter::repeat(0).take(24)); // padding
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_get_keyboard_mapping<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    // X11 GetKeyboardMapping: [0]=opcode, [1]=unused, [2-3]=length, [4]=first-keycode, [5]=count
    let first_keycode = data[4] as u32;
    let count = data[5] as u32;
    let keysyms_per_keycode: u32 = 4;
    let seq = conn.current_request_sequence();
    log::debug!("GetKeyboardMapping: first_keycode={} count={} seq={}", first_keycode, count, seq);

    // Generate basic keysym mapping
    let total_keysyms = count * keysyms_per_keycode;
    let additional_len = total_keysyms; // in 4-byte units

    let mut reply = Vec::with_capacity(32 + (total_keysyms as usize * 4));
    reply.push(1); // reply
    reply.push(keysyms_per_keycode as u8);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, additional_len);
    reply.extend(std::iter::repeat(0).take(24)); // padding

    // Generate keysym data: X11 keycode = macOS keycode + 8
    // So X11 keycode 8 = macOS key 0 (A), keycode 9 = macOS key 1 (S), etc.
    for i in 0..count {
        let keycode = first_keycode + i;
        let mac_key = keycode.wrapping_sub(8); // reverse the +8 offset

        // macOS virtual keycode → (normal keysym, shifted keysym)
        let (keysym, shifted) = macos_keycode_to_keysym(mac_key);

        write_u32_to(conn, &mut reply, keysym);
        write_u32_to(conn, &mut reply, shifted);
        write_u32_to(conn, &mut reply, 0); // NoSymbol (mode switch)
        write_u32_to(conn, &mut reply, 0); // NoSymbol (mode+shift)
    }

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_change_keyboard_mapping<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // No reply needed for ChangeKeyboardMapping
    Ok(())
}

/// Map macOS virtual keycode to X11 keysym (normal, shifted).
/// macOS keycodes: https://developer.apple.com/documentation/carbon/1543661-summary_of_virtual_key_codes
fn macos_keycode_to_keysym(mac_key: u32) -> (u32, u32) {
    match mac_key {
        // Letters (macOS layout: ANSI US)
        0  => (0x0061, 0x0041), // a A
        1  => (0x0073, 0x0053), // s S
        2  => (0x0064, 0x0044), // d D
        3  => (0x0066, 0x0046), // f F
        4  => (0x0068, 0x0048), // h H
        5  => (0x0067, 0x0047), // g G
        6  => (0x007A, 0x005A), // z Z
        7  => (0x0078, 0x0058), // x X
        8  => (0x0063, 0x0043), // c C
        9  => (0x0076, 0x0056), // v V
        11 => (0x0062, 0x0042), // b B
        12 => (0x0071, 0x0051), // q Q
        13 => (0x0077, 0x0057), // w W
        14 => (0x0065, 0x0045), // e E
        15 => (0x0072, 0x0052), // r R
        16 => (0x0079, 0x0059), // y Y
        17 => (0x0074, 0x0054), // t T
        18 => (0x0031, 0x0021), // 1 !
        19 => (0x0032, 0x0040), // 2 @
        20 => (0x0033, 0x0023), // 3 #
        21 => (0x0034, 0x0024), // 4 $
        22 => (0x0036, 0x005E), // 6 ^
        23 => (0x0035, 0x0025), // 5 %
        24 => (0x003D, 0x002B), // = +
        25 => (0x0039, 0x0028), // 9 (
        26 => (0x0037, 0x0026), // 7 &
        27 => (0x002D, 0x005F), // - _
        28 => (0x0038, 0x002A), // 8 *
        29 => (0x0030, 0x0029), // 0 )
        30 => (0x005D, 0x007D), // ] }
        31 => (0x006F, 0x004F), // o O
        32 => (0x0075, 0x0055), // u U
        33 => (0x005B, 0x007B), // [ {
        34 => (0x0069, 0x0049), // i I
        35 => (0x0070, 0x0050), // p P
        37 => (0x006C, 0x004C), // l L
        38 => (0x006A, 0x004A), // j J
        39 => (0x0027, 0x0022), // ' "
        40 => (0x006B, 0x004B), // k K
        41 => (0x003B, 0x003A), // ; :
        42 => (0x005C, 0x007C), // \ |
        43 => (0x002C, 0x003C), // , <
        44 => (0x002F, 0x003F), // / ?
        45 => (0x006E, 0x004E), // n N
        46 => (0x006D, 0x004D), // m M
        47 => (0x002E, 0x003E), // . >
        50 => (0x0060, 0x007E), // ` ~

        // Special keys
        36 => (0xFF0D, 0xFF0D), // Return
        48 => (0xFF09, 0xFF09), // Tab
        49 => (0x0020, 0x0020), // Space
        51 => (0xFF08, 0xFF08), // Backspace (Delete)
        53 => (0xFF1B, 0xFF1B), // Escape
        55 => (0xFFE7, 0xFFE7), // Left Command → Meta_L
        56 => (0xFFE1, 0xFFE1), // Left Shift
        57 => (0xFFE5, 0xFFE5), // Caps Lock
        58 => (0xFFE9, 0xFFE9), // Left Option → Alt_L
        59 => (0xFFE3, 0xFFE3), // Left Control
        60 => (0xFFE2, 0xFFE2), // Right Shift
        61 => (0xFFEA, 0xFFEA), // Right Option → Alt_R
        62 => (0xFFE4, 0xFFE4), // Right Control

        // Function keys
        122 => (0xFFBE, 0xFFBE), // F1
        120 => (0xFFBF, 0xFFBF), // F2
        99  => (0xFFC0, 0xFFC0), // F3
        118 => (0xFFC1, 0xFFC1), // F4
        96  => (0xFFC2, 0xFFC2), // F5
        97  => (0xFFC3, 0xFFC3), // F6
        98  => (0xFFC4, 0xFFC4), // F7
        100 => (0xFFC5, 0xFFC5), // F8
        101 => (0xFFC6, 0xFFC6), // F9
        109 => (0xFFC7, 0xFFC7), // F10
        103 => (0xFFC8, 0xFFC8), // F11
        111 => (0xFFC9, 0xFFC9), // F12

        // Arrow keys
        123 => (0xFF51, 0xFF51), // Left
        124 => (0xFF53, 0xFF53), // Right
        125 => (0xFF54, 0xFF54), // Down
        126 => (0xFF52, 0xFF52), // Up

        // Navigation
        115 => (0xFF50, 0xFF50), // Home
        119 => (0xFF57, 0xFF57), // End
        116 => (0xFF55, 0xFF55), // Page Up
        121 => (0xFF56, 0xFF56), // Page Down
        117 => (0xFFFF, 0xFFFF), // Forward Delete

        // Keypad
        65  => (0xFFAE, 0xFFAE), // KP_Decimal
        67  => (0xFFAA, 0xFFAA), // KP_Multiply
        69  => (0xFFAB, 0xFFAB), // KP_Add
        75  => (0xFFAF, 0xFFAF), // KP_Divide
        76  => (0xFF8D, 0xFF8D), // KP_Enter
        78  => (0xFFAD, 0xFFAD), // KP_Subtract
        82  => (0xFFB0, 0xFFB0), // KP_0
        83  => (0xFFB1, 0xFFB1), // KP_1
        84  => (0xFFB2, 0xFFB2), // KP_2
        85  => (0xFFB3, 0xFFB3), // KP_3
        86  => (0xFFB4, 0xFFB4), // KP_4
        87  => (0xFFB5, 0xFFB5), // KP_5
        88  => (0xFFB6, 0xFFB6), // KP_6
        89  => (0xFFB7, 0xFFB7), // KP_7
        91  => (0xFFB8, 0xFFB8), // KP_8
        92  => (0xFFB9, 0xFFB9), // KP_9

        _ => (0, 0), // NoSymbol
    }
}

async fn handle_get_keyboard_control<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(52);
    reply.push(1); // reply
    reply.push(0); // global auto-repeat (Off)
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 5); // additional data (5 words)
    write_u32_to(conn, &mut reply, 0); // LED mask
    reply.push(50); // key click percent
    reply.push(50); // bell percent
    write_u16_to(conn, &mut reply, 400); // bell pitch
    write_u16_to(conn, &mut reply, 100); // bell duration
    write_u16_to(conn, &mut reply, 0); // padding
    // Auto-repeats: 32 bytes bitmap (all keys repeat)
    reply.extend(std::iter::repeat(0xFF).take(32));

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_get_screen_saver<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data
    write_u16_to(conn, &mut reply, 0); // timeout
    write_u16_to(conn, &mut reply, 0); // interval
    reply.push(0); // prefer-blanking (No)
    reply.push(0); // allow-exposures (No)
    reply.extend(std::iter::repeat(0).take(18)); // padding
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_set_close_down_mode<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // No reply needed
    Ok(())
}

async fn handle_get_modifier_mapping<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let keycodes_per_modifier = 2u8;
    let total_bytes = 8 * keycodes_per_modifier as usize; // 8 modifiers
    let additional_len = total_bytes / 4;

    let mut reply = Vec::with_capacity(32 + total_bytes);
    reply.push(1); // reply
    reply.push(keycodes_per_modifier);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, additional_len as u32);
    reply.extend(std::iter::repeat(0).take(24)); // padding

    // Modifier mapping: 8 modifiers x keycodes_per_modifier keycodes
    // Shift
    reply.push(50); // Shift_L
    reply.push(62); // Shift_R
    // Lock
    reply.push(66); // Caps_Lock
    reply.push(0);
    // Control
    reply.push(37); // Control_L
    reply.push(105); // Control_R
    // Mod1 (Alt)
    reply.push(64); // Alt_L
    reply.push(108); // Alt_R
    // Mod2 (Num Lock)
    reply.push(77); // Num_Lock
    reply.push(0);
    // Mod3
    reply.push(0);
    reply.push(0);
    // Mod4 (Super)
    reply.push(133); // Super_L
    reply.push(134); // Super_R
    // Mod5
    reply.push(0);
    reply.push(0);

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_grab_server<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // No reply needed
    Ok(())
}

async fn handle_ungrab_server<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // No reply needed
    Ok(())
}

async fn handle_list_fonts<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    // Return empty font list for now
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data
    write_u16_to(conn, &mut reply, 0); // number of names
    reply.extend(std::iter::repeat(0).take(22)); // padding
    stream.write_all(&reply).await?;
    Ok(())
}

// --- Pointer, Selection, and Input handlers ---

async fn handle_grab_pointer<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    // GrabPointer reply: status = Success (0)
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // status: Success
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    reply.extend(std::iter::repeat(0).take(24));
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_ungrab_pointer<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    Ok(())
}

async fn handle_grab_keyboard<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1);
    reply.push(0); // status: Success
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    reply.extend(std::iter::repeat(0).take(24));
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_ungrab_keyboard<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    Ok(())
}

async fn handle_set_selection_owner<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // SetSelectionOwner: [4..8]=owner [8..12]=selection [12..16]=time
    if data.len() < 16 { return Err(ServerError::Protocol); }
    let owner = read_u32(conn, &data[4..8]);
    let selection = read_u32(conn, &data[8..12]);
    let time = read_u32(conn, &data[12..16]);
    debug!("SetSelectionOwner: selection={} owner=0x{:08x} time={}", selection, owner, time);
    let ts = if time == 0 { server.startup_time } else { time };
    if owner == 0 {
        server.selections.remove(&selection);
    } else {
        server.selections.insert(selection, (owner, ts));
    }
    Ok(())
}

async fn handle_get_selection_owner<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let selection = if data.len() >= 8 { read_u32(conn, &data[4..8]) } else { 0 };
    let owner = server.selections.get(&selection).map(|e| e.0).unwrap_or(0);
    debug!("GetSelectionOwner: selection={} -> owner=0x{:08x}", selection, owner);
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, owner);
    reply.extend(std::iter::repeat(0).take(20));
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_convert_selection<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // ConvertSelection: no-op for now
    Ok(())
}

async fn handle_send_event<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // SendEvent: [1]=propagate [4..8]=destination [8..12]=event_mask [12..44]=event(32 bytes)
    if data.len() < 44 { return Err(ServerError::Protocol); }
    let propagate = data[1] != 0;
    let destination = read_u32(conn, &data[4..8]);
    let event_mask = read_u32(conn, &data[8..12]);
    let event_data = &data[12..44];

    debug!("SendEvent: dest=0x{:08x} mask=0x{:08x} propagate={}", destination, event_mask, propagate);

    // Resolve destination: 0=PointerWindow, 1=InputFocus, else=window ID
    let target_window = match destination {
        0 | 1 => {
            // For now, use the first visible window
            server.resources.iter()
                .find_map(|e| if let super::resources::Resource::Window(_) = e.value() { Some(*e.key()) } else { None })
                .unwrap_or(1)
        }
        _ => destination,
    };

    // Mark event as synthetic (bit 7 of event code)
    let mut synthetic_event = [0u8; 32];
    synthetic_event.copy_from_slice(event_data);
    synthetic_event[0] |= 0x80; // Set synthetic bit

    // Find which connection owns the target window and send via event channel
    for entry in server.connections.iter() {
        let target_conn = entry.value();
        if target_conn.resource_id_base <= target_window
            && target_window < target_conn.resource_id_base + 0x00200000
        {
            let _ = target_conn.event_tx.send(synthetic_event.to_vec());
            break;
        }
    }

    Ok(())
}

async fn handle_set_pointer_mapping<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    // SetPointerMapping reply: status = Success (0)
    let _nmap = data[1] as usize;
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // status: Success
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    reply.extend(std::iter::repeat(0).take(24));
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_get_pointer_mapping<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    // GetPointerMapping: return 1:1 mapping for 3 buttons
    let seq = conn.current_request_sequence();
    let map = [1u8, 2, 3]; // 3-button mouse
    let pad = (4 - (map.len() % 4)) % 4;
    let mut reply = Vec::with_capacity(32 + map.len() + pad);
    reply.push(1); // reply
    reply.push(map.len() as u8); // nElts
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, ((map.len() + pad) / 4) as u32);
    reply.extend(std::iter::repeat(0).take(24));
    reply.extend_from_slice(&map);
    reply.extend(std::iter::repeat(0).take(pad));
    stream.write_all(&reply).await?;
    Ok(())
}

/// Walk up the window tree from `wid` to root, accumulating x/y offsets
/// to compute the window's absolute screen position.
fn window_to_root_coords(server: &Arc<XServer>, wid: u32) -> (i32, i32) {
    let root = server.screens[0].root_window;
    let mut abs_x: i32 = 0;
    let mut abs_y: i32 = 0;
    let mut current = wid;
    // Walk up to root (max 32 levels to prevent infinite loop)
    for _ in 0..32 {
        if current == root || current == 0 {
            break;
        }
        if let Some(res) = server.resources.get(&current) {
            if let super::resources::Resource::Window(win) = res.value() {
                let w = win.read();
                abs_x += w.x as i32;
                abs_y += w.y as i32;
                current = w.parent;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    (abs_x, abs_y)
}

async fn handle_query_pointer<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let root = server.screens[0].root_window;
    let window = if data.len() >= 8 { read_u32(conn, &data[4..8]) } else { root };

    let root_x = server.pointer_x.load(std::sync::atomic::Ordering::Relaxed) as i16;
    let root_y = server.pointer_y.load(std::sync::atomic::Ordering::Relaxed) as i16;

    // Compute window-relative coordinates.
    // Use stored per-window MotionNotify coordinates for accuracy,
    // since macOS window position may not match X11 stored position.
    let (win_x, win_y) = if window == root {
        (root_x, root_y)
    } else {
        // Find the top-level ancestor that receives MotionNotify from macOS
        let mut toplevel = window;
        for _ in 0..32 {
            if let Some(res) = server.resources.get(&toplevel) {
                if let super::resources::Resource::Window(win) = res.value() {
                    let w = win.read();
                    if w.parent == root {
                        break; // toplevel found
                    }
                    toplevel = w.parent;
                } else { break; }
            } else { break; }
        }
        if let Some(entry) = server.window_pointer.get(&toplevel) {
            let (top_x, top_y) = *entry;
            if window == toplevel {
                (top_x, top_y)
            } else {
                // Subtract child's offset relative to top-level
                let mut off_x: i32 = 0;
                let mut off_y: i32 = 0;
                let mut cur = window;
                for _ in 0..32 {
                    if cur == toplevel || cur == root || cur == 0 { break; }
                    if let Some(res) = server.resources.get(&cur) {
                        if let super::resources::Resource::Window(win) = res.value() {
                            let w = win.read();
                            off_x += w.x as i32;
                            off_y += w.y as i32;
                            cur = w.parent;
                        } else { break; }
                    } else { break; }
                }
                (top_x - off_x as i16, top_y - off_y as i16)
            }
        } else {
            // No MotionNotify received yet; fall back to root coord calculation
            let (abs_x, abs_y) = window_to_root_coords(server, window);
            (root_x - abs_x as i16, root_y - abs_y as i16)
        }
    };

    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(1); // same-screen: True
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, root); // root
    write_u32_to(conn, &mut reply, 0); // child: None
    write_i16_to(conn, &mut reply, root_x); // root-x
    write_i16_to(conn, &mut reply, root_y); // root-y
    write_i16_to(conn, &mut reply, win_x); // win-x
    write_i16_to(conn, &mut reply, win_y); // win-y
    write_u16_to(conn, &mut reply, 0); // mask
    reply.extend(std::iter::repeat(0).take(6));
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_translate_coordinates<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 16 {
        return Err(ServerError::Protocol);
    }
    let src_window = read_u32(conn, &data[4..8]);
    let dst_window = read_u32(conn, &data[8..12]);
    let src_x = read_i16(conn, &data[12..14]);
    let src_y = read_i16(conn, &data[14..16]);

    // Convert src coords to root coords, then to dst coords
    let (src_abs_x, src_abs_y) = window_to_root_coords(server, src_window);
    let (dst_abs_x, dst_abs_y) = window_to_root_coords(server, dst_window);
    let dst_x = (src_x as i32 + src_abs_x - dst_abs_x) as i16;
    let dst_y = (src_y as i32 + src_abs_y - dst_abs_y) as i16;

    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(1); // same-screen
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, 0); // child: None
    write_i16_to(conn, &mut reply, dst_x);
    write_i16_to(conn, &mut reply, dst_y);
    reply.extend(std::iter::repeat(0).take(16));
    stream.write_all(&reply).await?;
    Ok(())
}

// --- Colormap handlers (TrueColor — all operations are trivial decompositions) ---

async fn handle_create_colormap<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // CreateColormap: alloc=data[1], mid=data[4..8], window=data[8..12], visual=data[12..16]
    // For TrueColor, colormaps are read-only — just accept and ignore.
    if data.len() < 16 {
        return Err(ServerError::Protocol);
    }
    let mid = read_u32(conn, &data[4..8]);
    debug!("CreateColormap: id=0x{:08X}", mid);
    Ok(())
}

async fn handle_free_colormap<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 8 {
        return Err(ServerError::Protocol);
    }
    let cmap = read_u32(conn, &data[4..8]);
    debug!("FreeColormap: 0x{:08X}", cmap);
    Ok(())
}

async fn handle_alloc_color<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    // AllocColor: cmap=data[4..8], red=data[8..10], green=data[10..12], blue=data[12..14]
    if data.len() < 14 {
        return Err(ServerError::Protocol);
    }
    let red = read_u16(conn, &data[8..10]);
    let green = read_u16(conn, &data[10..12]);
    let blue = read_u16(conn, &data[12..14]);

    // TrueColor: pixel = (R8 << 16) | (G8 << 8) | B8
    let r8 = (red >> 8) as u32;
    let g8 = (green >> 8) as u32;
    let b8 = (blue >> 8) as u32;
    let pixel = (r8 << 16) | (g8 << 8) | b8;

    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data length
    write_u16_to(conn, &mut reply, red & 0xFF00); // exact red
    write_u16_to(conn, &mut reply, green & 0xFF00); // exact green
    write_u16_to(conn, &mut reply, blue & 0xFF00); // exact blue
    reply.extend_from_slice(&[0; 2]); // padding
    write_u32_to(conn, &mut reply, pixel);
    reply.extend(std::iter::repeat(0).take(12)); // padding to 32 bytes

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_query_colors<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    // QueryColors: cmap=data[4..8], pixels=data[8..]
    if data.len() < 8 {
        return Err(ServerError::Protocol);
    }
    let n_pixels = (data.len() - 8) / 4;

    let seq = conn.current_request_sequence();
    // Reply: 32 bytes header + n_pixels * 8 bytes (each RGB entry is 8 bytes)
    let reply_data_len = n_pixels * 8;
    let mut reply = Vec::with_capacity(32 + reply_data_len);
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, (reply_data_len / 4) as u32); // additional data in 4-byte units
    write_u16_to(conn, &mut reply, n_pixels as u16); // nColors
    reply.extend(std::iter::repeat(0).take(22)); // padding to 32 bytes

    // For TrueColor: decompose each pixel into RGB16
    for i in 0..n_pixels {
        let offset = 8 + i * 4;
        if offset + 4 > data.len() { break; }
        let pixel = read_u32(conn, &data[offset..offset + 4]);
        let r = ((pixel >> 16) & 0xFF) as u16;
        let g = ((pixel >> 8) & 0xFF) as u16;
        let b = (pixel & 0xFF) as u16;
        // X11 expects 16-bit color values (scale 8-bit to 16-bit)
        write_u16_to(conn, &mut reply, r << 8 | r);
        write_u16_to(conn, &mut reply, g << 8 | g);
        write_u16_to(conn, &mut reply, b << 8 | b);
        write_u16_to(conn, &mut reply, 0); // padding per entry
    }

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_lookup_color<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    // LookupColor: cmap=data[4..8], name_len=data[8..10], name=data[12..]
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }
    let name_len = read_u16(conn, &data[8..10]) as usize;
    if data.len() < 12 + name_len {
        return Err(ServerError::Protocol);
    }
    let name = std::str::from_utf8(&data[12..12 + name_len])
        .unwrap_or("")
        .to_lowercase();

    // Basic X11 color name lookup
    let (r, g, b) = match name.as_str() {
        "white" => (0xFFFF_u16, 0xFFFF_u16, 0xFFFF_u16),
        "black" => (0, 0, 0),
        "red" => (0xFFFF, 0, 0),
        "green" => (0, 0xFFFF, 0),
        "blue" => (0, 0, 0xFFFF),
        "yellow" => (0xFFFF, 0xFFFF, 0),
        "cyan" => (0, 0xFFFF, 0xFFFF),
        "magenta" => (0xFFFF, 0, 0xFFFF),
        "grey" | "gray" => (0xBEBE, 0xBEBE, 0xBEBE),
        _ => {
            debug!("LookupColor: unknown color '{}'", name);
            // Return Name error (opcode 15)
            return Err(ServerError::Protocol);
        }
    };

    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data
    write_u16_to(conn, &mut reply, r); // exact red
    write_u16_to(conn, &mut reply, g); // exact green
    write_u16_to(conn, &mut reply, b); // exact blue
    write_u16_to(conn, &mut reply, r); // visual red
    write_u16_to(conn, &mut reply, g); // visual green
    write_u16_to(conn, &mut reply, b); // visual blue
    reply.extend(std::iter::repeat(0).take(8)); // padding to 32

    stream.write_all(&reply).await?;
    Ok(())
}

/// Set the sequence number (bytes 2-3) in a 32-byte X11 event.
fn set_event_sequence(conn: &ClientConnection, event: &mut [u8], seq: u16) {
    let bytes = match conn.byte_order {
        ByteOrder::BigEndian => seq.to_be_bytes(),
        ByteOrder::LittleEndian => seq.to_le_bytes(),
    };
    event[2] = bytes[0];
    event[3] = bytes[1];
}

// --- Byte order aware read/write helpers ---

fn read_u16(conn: &ClientConnection, data: &[u8]) -> u16 {
    match conn.byte_order {
        ByteOrder::BigEndian => u16::from_be_bytes([data[0], data[1]]),
        ByteOrder::LittleEndian => u16::from_le_bytes([data[0], data[1]]),
    }
}

fn read_i16(conn: &ClientConnection, data: &[u8]) -> i16 {
    match conn.byte_order {
        ByteOrder::BigEndian => i16::from_be_bytes([data[0], data[1]]),
        ByteOrder::LittleEndian => i16::from_le_bytes([data[0], data[1]]),
    }
}

fn read_u32(conn: &ClientConnection, data: &[u8]) -> u32 {
    match conn.byte_order {
        ByteOrder::BigEndian => u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
        ByteOrder::LittleEndian => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
    }
}

fn write_u16_to(conn: &ClientConnection, buf: &mut Vec<u8>, val: u16) {
    match conn.byte_order {
        ByteOrder::BigEndian => buf.extend_from_slice(&val.to_be_bytes()),
        ByteOrder::LittleEndian => buf.extend_from_slice(&val.to_le_bytes()),
    }
}

// --- Cursor handlers ---

async fn handle_create_cursor<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // CreateCursor: opcode 93
    // data[4..8] = cursor ID, [8..12] = source pixmap, [12..16] = mask pixmap
    // [16..18] = fore-red, [18..20] = fore-green, [20..22] = fore-blue
    // [22..24] = back-red, [24..26] = back-green, [26..28] = back-blue
    // [28..30] = x, [30..32] = y
    if data.len() < 32 { return Ok(()); }
    let cid = read_u32(conn, &data[4..8]);
    let fore_red = read_u16(conn, &data[16..18]);
    let fore_green = read_u16(conn, &data[18..20]);
    let fore_blue = read_u16(conn, &data[20..22]);
    let back_red = read_u16(conn, &data[22..24]);
    let back_green = read_u16(conn, &data[24..26]);
    let back_blue = read_u16(conn, &data[26..28]);

    let cursor = super::resources::CursorState {
        id: cid,
        source_font: 0,
        source_char: 0,
        mask_font: 0,
        mask_char: 0,
        fore_red,
        fore_green,
        fore_blue,
        back_red,
        back_green,
        back_blue,
    };
    server.resources.insert(cid, super::resources::Resource::Cursor(Arc::new(cursor)));
    debug!("CreateCursor: id=0x{:08X}", cid);
    Ok(())
}

async fn handle_create_glyph_cursor<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // CreateGlyphCursor: opcode 94
    // data[4..8] = cursor ID, [8..12] = source font, [12..14] = source char
    // [14..18] = mask font, [18..20] = mask char
    // [20..22] = fore-red, [22..24] = fore-green, [24..26] = fore-blue
    // [26..28] = back-red, [28..30] = back-green, [30..32] = back-blue
    if data.len() < 32 { return Ok(()); }
    let cid = read_u32(conn, &data[4..8]);
    let source_font = read_u32(conn, &data[8..12]);
    let source_char = read_u16(conn, &data[12..14]);
    let mask_font = read_u32(conn, &data[14..18]);
    let mask_char = read_u16(conn, &data[18..20]);
    let fore_red = read_u16(conn, &data[20..22]);
    let fore_green = read_u16(conn, &data[22..24]);
    let fore_blue = read_u16(conn, &data[24..26]);
    let back_red = read_u16(conn, &data[26..28]);
    let back_green = read_u16(conn, &data[28..30]);
    let back_blue = read_u16(conn, &data[30..32]);

    let cursor = super::resources::CursorState {
        id: cid,
        source_font,
        source_char,
        mask_font,
        mask_char,
        fore_red,
        fore_green,
        fore_blue,
        back_red,
        back_green,
        back_blue,
    };
    server.resources.insert(cid, super::resources::Resource::Cursor(Arc::new(cursor)));
    debug!("CreateGlyphCursor: id=0x{:08X} font=0x{:08X} char={}", cid, source_font, source_char);
    Ok(())
}

async fn handle_free_cursor<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // FreeCursor: opcode 95
    if data.len() < 8 { return Ok(()); }
    let cid = read_u32(conn, &data[4..8]);
    server.resources.remove(&cid);
    debug!("FreeCursor: id=0x{:08X}", cid);
    Ok(())
}

async fn handle_recolor_cursor<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // RecolorCursor: opcode 96
    // data[4..8] = cursor, [8..10] = fore-red, ..., [18..20] = back-blue
    if data.len() < 20 { return Ok(()); }
    let cid = read_u32(conn, &data[4..8]);
    debug!("RecolorCursor: id=0x{:08X} (no-op)", cid);
    // We just ignore recoloring for now
    Ok(())
}

fn write_i16_to(conn: &ClientConnection, buf: &mut Vec<u8>, val: i16) {
    match conn.byte_order {
        ByteOrder::BigEndian => buf.extend_from_slice(&val.to_be_bytes()),
        ByteOrder::LittleEndian => buf.extend_from_slice(&val.to_le_bytes()),
    }
}

fn write_u32_to(conn: &ClientConnection, buf: &mut Vec<u8>, val: u32) {
    match conn.byte_order {
        ByteOrder::BigEndian => buf.extend_from_slice(&val.to_be_bytes()),
        ByteOrder::LittleEndian => buf.extend_from_slice(&val.to_le_bytes()),
    }
}

fn write_u32_at(conn: &ClientConnection, buf: &mut [u8], offset: usize, val: u32) {
    let bytes = match conn.byte_order {
        ByteOrder::BigEndian => val.to_be_bytes(),
        ByteOrder::LittleEndian => val.to_le_bytes(),
    };
    buf[offset..offset + 4].copy_from_slice(&bytes);
}

fn write_u16_at(conn: &ClientConnection, buf: &mut [u8], offset: usize, val: u16) {
    let bytes = match conn.byte_order {
        ByteOrder::BigEndian => val.to_be_bytes(),
        ByteOrder::LittleEndian => val.to_le_bytes(),
    };
    buf[offset..offset + 2].copy_from_slice(&bytes);
}
