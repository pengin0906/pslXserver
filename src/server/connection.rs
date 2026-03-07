use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use bytes::BytesMut;
use log::{debug, info, warn};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::{ServerError, XServer};
use crate::display::Xid;

/// Byte order of the client connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    BigEndian,
    LittleEndian,
}

/// X11 event data — stack-allocated for the common 32-byte case.
#[derive(Clone)]
pub enum EventData {
    /// Standard 32-byte X11 event (most events).
    Fixed([u8; 32]),
    /// Extended event (XI2 GenericEvent, >32 bytes).
    Extended(Vec<u8>),
}

impl EventData {
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        match self {
            EventData::Fixed(ref buf) => buf,
            EventData::Extended(ref vec) => vec,
        }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            EventData::Fixed(ref mut buf) => buf,
            EventData::Extended(ref mut vec) => vec,
        }
    }
}

impl From<[u8; 32]> for EventData {
    #[inline]
    fn from(buf: [u8; 32]) -> Self {
        EventData::Fixed(buf)
    }
}

impl From<Vec<u8>> for EventData {
    #[inline]
    fn from(vec: Vec<u8>) -> Self {
        EventData::Extended(vec)
    }
}

/// Per-client connection state.
pub struct ClientConnection {
    pub id: u32,
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub byte_order: ByteOrder,
    pub sequence_number: AtomicU16,
    /// Channel for sending X11 events to this client's connection task.
    pub event_tx: mpsc::UnboundedSender<EventData>,
    /// Incremented each time client processes GetKeyboardMapping. Used to
    /// synchronize MappingNotify → GetKeyboardMapping round-trip without fixed sleep.
    pub mapping_gen: std::sync::atomic::AtomicU32,
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

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventData>();

    let conn = Arc::new(ClientConnection {
        id: conn_id,
        resource_id_base,
        resource_id_mask,
        byte_order,
        sequence_number: AtomicU16::new(1),
        event_tx,
        mapping_gen: std::sync::atomic::AtomicU32::new(0),
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
                    let (request_len, is_big_request) = {
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
                            (ext_len as usize * 4, true)
                        } else {
                            (len as usize * 4, false)
                        }
                    };

                    if pending.len() < request_len { break; }

                    let _seq = conn.next_sequence();
                    let opcode = pending[0];
                    info!("Request seq={} opcode={} len={} bytes", conn.current_request_sequence(), opcode, request_len);
                    let seq = conn.current_request_sequence();

                    let raw_data = pending.split_to(request_len);
                    // BIG-REQUESTS: bytes 4-7 are extended length, shift data
                    // so handlers see the same layout as normal requests.
                    // Reconstruct: header (bytes 0-3) + body (bytes 8+)
                    let request_data: bytes::Bytes = if is_big_request {
                        let mut normalized = bytes::BytesMut::with_capacity(4 + raw_data.len() - 8);
                        normalized.extend_from_slice(&raw_data[..4]);
                        normalized.extend_from_slice(&raw_data[8..]);
                        normalized.freeze()
                    } else {
                        raw_data.freeze()
                    };

                    match handle_request(&server, &conn, opcode, &request_data, &mut stream).await {
                        Ok(()) => {}
                        Err(ServerError::NotImplemented) => {
                            info!("Unimplemented opcode: {} — sending Implementation error", opcode);
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
                            let error_code = e.x11_error_code();
                            warn!("Request error (opcode {}): {} -> X11 error {}", opcode, e, error_code);
                            err_buf = [0u8; 32];
                            err_buf[0] = 0;
                            err_buf[1] = error_code;
                            err_buf[2] = (seq & 0xFF) as u8;
                            err_buf[3] = ((seq >> 8) & 0xFF) as u8;
                            err_buf[8] = opcode;
                            err_buf[10] = opcode;
                            let _ = stream.write_all(&err_buf).await;
                        }
                    }
                }

                // After processing client requests, also deliver any pending events.
                // Without this, biased select! would starve event delivery during
                // heavy client output (e.g., ls -lR in xterm) because the read branch
                // is always ready and events in event_rx never get selected.
                while let Ok(mut event_data) = event_rx.try_recv() {
                    let cur_seq = conn.current_request_sequence();
                    set_event_sequence(&conn, event_data.as_mut_slice(), cur_seq);
                    if stream.write_all(event_data.as_slice()).await.is_err() { break; }
                }
            }

            // Events from the server's event dispatcher
            Some(mut event_data) = event_rx.recv() => {
                // Re-stamp sequence number to current request sequence.
                // X11 spec: event seq = last request processed by server.
                // Events queued via event_tx may have stale seq numbers that
                // would violate xcb's monotonic sequence requirement.
                let cur_seq = conn.current_request_sequence();
                set_event_sequence(&conn, event_data.as_mut_slice(), cur_seq);
                let evs = event_data.as_slice();
                if evs[0] == 2 || evs[0] == 3 || evs[0] == 4 || evs[0] == 5 || evs[0] == 34 {
                    let detail = evs[1];
                    info!("Writing event to conn {}: type={} detail={} seq={}", conn_id, evs[0], detail, cur_seq);
                }
                match stream.write_all(event_data.as_slice()).await {
                    Ok(()) => {
                        // Drain any additional queued events before going back to select
                        // This prevents event backlog from starving the read branch
                        let mut extra = 0;
                        while let Ok(mut more) = event_rx.try_recv() {
                            let cur_seq = conn.current_request_sequence();
                            set_event_sequence(&conn, more.as_mut_slice(), cur_seq);
                            let ms = more.as_slice();
                            if ms[0] == 2 || ms[0] == 3 || ms[0] == 4 || ms[0] == 5 || ms[0] == 34 {
                                info!("Writing event (drain) to conn {}: type={} detail={} seq={}", conn_id, ms[0], ms[1], cur_seq);
                            }
                            if stream.write_all(more.as_slice()).await.is_err() { break; }
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

    // Clean up: find and destroy top-level windows owned by this connection
    let owned_windows: Vec<Xid> = server.resources.iter()
        .filter(|entry| (*entry.key() & !mask) == (base & !mask))
        .filter_map(|entry| {
            if let super::resources::Resource::Window(win) = entry.value() {
                let w = win.read();
                // Only destroy top-level (parent=root) to trigger recursive destruction
                if w.parent == server.screens[0].root_window {
                    return Some(*entry.key());
                }
            }
            None
        })
        .collect();

    for wid in owned_windows {
        destroy_window_recursive(&server, wid);
    }

    // Remove remaining non-window resources (pixmaps, GCs, fonts, etc.)
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
    reply.push(2); // number of pixmap formats

    reply.push(0); // image-byte-order (LSBFirst)
    reply.push(0); // bitmap-bit-order (LSBFirst)
    reply.push(8);  // bitmap-scanline-unit
    reply.push(32); // bitmap-scanline-pad
    reply.push(8);  // min-keycode
    reply.push(255); // max-keycode

    write_u32(&mut reply, 0); // unused (4 bytes)

    // Vendor string
    reply.extend_from_slice(vendor);
    reply.extend(std::iter::repeat(0).take(vendor_pad));

    // Pixmap format 1: depth 1 (bitmaps), bpp 1, scanline-pad 32
    reply.push(1);  // depth
    reply.push(1);  // bits-per-pixel
    reply.push(32); // scanline-pad
    reply.extend(std::iter::repeat(0).take(5)); // unused padding

    // Pixmap format 2: depth 24, bpp 32, scanline-pad 32
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
        7 => handle_reparent_window(server, conn, data, stream).await,
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
        21 => handle_list_properties(server, conn, data, stream).await,
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
        41 => handle_warp_pointer(server, conn, data, stream).await,
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
        75 => handle_poly_text16(server, conn, data, stream).await,
        76 => handle_image_text8(server, conn, data, stream).await,
        77 => handle_image_text16(server, conn, data, stream).await,
        78 => handle_create_colormap(server, conn, data, stream).await,
        79 => handle_free_colormap(server, conn, data, stream).await,
        80 => handle_get_font_path(server, conn, data, stream).await,
        81 => { Ok(()) }, // SetFontPath — no-op, we use system fonts
        84 => handle_alloc_color(server, conn, data, stream).await,
        85 => handle_alloc_named_color(server, conn, data, stream).await,
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
        118 => handle_set_modifier_mapping(server, conn, data, stream).await,
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

            // Phase 1: gather info from resources (no await while holding locks)
            enum GetImageSource {
                Window { depth: u8, visual: u32, bg: u32, native: Option<crate::display::NativeWindowHandle> },
                Pixmap { depth: u8, pixels: Vec<u8> },
                Error,
            }
            let source = if let Some(res) = server.resources.get(&drawable) {
                match res.value() {
                    super::resources::Resource::Window(win) => {
                        let w = win.read();
                        let depth = w.depth;
                        let visual = w.visual;
                        let bg = w.background_pixel.unwrap_or(0);
                        drop(w);
                        let native = find_native_handle(server, drawable);
                        drop(res);
                        GetImageSource::Window { depth, visual, bg, native }
                    }
                    super::resources::Resource::Pixmap(pix) => {
                        let pix = pix.read();
                        let depth = pix.depth;
                        let src_w = pix.width as usize;
                        let src_h = pix.height as usize;
                        let px = x.max(0) as usize;
                        let py = y.max(0) as usize;
                        let w = width as usize;
                        let h = height as usize;
                        let mut pixels = vec![0u8; w * h * 4];
                        for row in 0..h {
                            let sy = py + row;
                            if sy >= src_h { break; }
                            for col in 0..w {
                                let sx = px + col;
                                if sx >= src_w { continue; }
                                let src_off = (sy * src_w + sx) * 4;
                                let dst_off = (row * w + col) * 4;
                                if src_off + 4 <= pix.data.len() {
                                    pixels[dst_off..dst_off + 4].copy_from_slice(&pix.data[src_off..src_off + 4]);
                                }
                            }
                        }
                        GetImageSource::Pixmap { depth, pixels }
                    }
                    _ => GetImageSource::Error,
                }
            } else {
                GetImageSource::Error
            };

            // Phase 2: resolve (may await for IOSurface read)
            let (depth, visual, img_data) = match source {
                GetImageSource::Window { depth, visual, bg, native } => {
                    if let Some(handle) = native {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let _ = server.display_cmd_tx.send(
                            crate::display::DisplayCommand::ReadPixels {
                                handle, x, y, width, height, reply: tx,
                            });
                        match rx.await {
                            Ok(Some(pixels)) => (depth, visual, pixels),
                            _ => {
                                let pixel_count = width as usize * height as usize;
                                let bg_bytes = bg.to_le_bytes();
                                let mut pixels = vec![0u8; pixel_count * 4];
                                for i in 0..pixel_count {
                                    pixels[i*4..i*4+4].copy_from_slice(&bg_bytes);
                                }
                                (depth, visual, pixels)
                            }
                        }
                    } else {
                        let pixel_count = width as usize * height as usize;
                        let bg_bytes = bg.to_le_bytes();
                        let mut pixels = vec![0u8; pixel_count * 4];
                        for i in 0..pixel_count {
                            pixels[i*4..i*4+4].copy_from_slice(&bg_bytes);
                        }
                        (depth, visual, pixels)
                    }
                }
                GetImageSource::Pixmap { depth, pixels } => (depth, 0, pixels),
                GetImageSource::Error => {
                    let mut err = [0u8; 32];
                    err[0] = 0;
                    err[1] = 9; // BadDrawable
                    err[2] = (seq & 0xFF) as u8;
                    err[3] = ((seq >> 8) & 0xFF) as u8;
                    err[8] = opcode;
                    stream.write_all(&err).await?;
                    return Ok(());
                }
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
        86 => { // AllocColorCells — TrueColor: always fail with BadAlloc (read-only colormap)
            let seq = conn.current_request_sequence();
            let mut err = [0u8; 32];
            err[0] = 0; // error
            err[1] = 11; // BadAlloc
            err[2] = (seq & 0xFF) as u8;
            err[3] = ((seq >> 8) & 0xFF) as u8;
            err[8] = opcode;
            err[10] = opcode;
            stream.write_all(&err).await?;
            Ok(())
        }
        87 => { // AllocColorPlanes — TrueColor: always fail with BadAlloc (read-only colormap)
            let seq = conn.current_request_sequence();
            let mut err = [0u8; 32];
            err[0] = 0; // error
            err[1] = 11; // BadAlloc
            err[2] = (seq & 0xFF) as u8;
            err[3] = ((seq >> 8) & 0xFF) as u8;
            err[8] = opcode;
            err[10] = opcode;
            stream.write_all(&err).await?;
            Ok(())
        }
        39 => { // GetMotionEvents — return empty list
            let seq = conn.current_request_sequence();
            let mut reply = vec![1u8, 0];
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0); // additional data length
            write_u32_to(conn, &mut reply, 0); // nEvents = 0
            reply.extend(std::iter::repeat(0).take(20)); // padding to 32
            stream.write_all(&reply).await?;
            Ok(())
        }
        50 => { // ListFontsWithInfo — return end-of-list marker immediately
            let seq = conn.current_request_sequence();
            let mut reply = vec![1u8, 0]; // reply, name-length=0 (end marker)
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 7); // additional data: 28 bytes = 7 words
            // min-bounds (12 bytes) + max-bounds (12 bytes) padded
            reply.extend(std::iter::repeat(0).take(24)); // padding to fill header to 32
            reply.extend(std::iter::repeat(0).take(28)); // 7 words of font info
            stream.write_all(&reply).await?;
            Ok(())
        }
        133 => { // BIG-REQUESTS: BigReqEnable
            let seq = conn.current_request_sequence();
            let mut reply = Vec::with_capacity(32);
            reply.push(1); // reply
            reply.push(0); // unused
            write_u16_to(conn, &mut reply, seq);
            write_u32_to(conn, &mut reply, 0); // additional data length
            // maximum-request-length in 4-byte units: 16MB worth
            write_u32_to(conn, &mut reply, 4194304); // 4194304 * 4 = 16MB
            reply.extend(std::iter::repeat(0).take(20)); // padding to 32
            stream.write_all(&reply).await?;
            info!("BIG-REQUESTS enabled: max request length = 16MB");
            Ok(())
        }
        // No-op for common requests that don't need replies
        5 | 6 | 11 | 13 | 28 | 29 | 30 | 33 | 34 | 51 | 57 | 58 | 59 |
        63 | 83 | 88 | 89 | 90 | 100 | 104 | 105 | 107 | 109 | 111 | 112 | 113 | 114 => {
            debug!("Stubbed opcode: {} (no-op)", opcode);
            Ok(())
        }
        // Extension opcodes
        131 => super::extensions::xinput2::handle_xinput2_request(server, conn, data, stream).await,
        132 => super::extensions::xtest::handle_xtest_request(server, conn, data, stream).await,
        134 => super::extensions::shape::handle_shape_request(server, conn, data, stream).await,
        135 => super::extensions::xkb::handle_xkb_request(server, conn, data, stream).await,
        139 => super::extensions::render::handle_render_request(server, conn, data, stream).await,
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
    let seq = conn.current_request_sequence();

    let name = match server.atoms.get_name(atom) {
        Some(n) => n,
        None => {
            // BadAtom error (error code 5)
            let mut err = [0u8; 32];
            err[0] = 0; // error
            err[1] = 5; // BadAtom
            err[2] = (seq & 0xFF) as u8;
            err[3] = ((seq >> 8) & 0xFF) as u8;
            // bytes 4-7: bad resource id (the invalid atom)
            err[4] = (atom & 0xFF) as u8;
            err[5] = ((atom >> 8) & 0xFF) as u8;
            err[6] = ((atom >> 16) & 0xFF) as u8;
            err[7] = ((atom >> 24) & 0xFF) as u8;
            err[8] = 0; // minor opcode
            err[9] = 0;
            err[10] = 17; // major opcode (GetAtomName)
            stream.write_all(&err).await?;
            return Ok(());
        }
    };

    let name_bytes = name.as_bytes();
    let pad = (4 - (name_bytes.len() % 4)) % 4;
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

    // CopyFromParent: inherit visual from parent
    let effective_visual = if visual == 0 {
        if let Some(res) = server.resources.get(&parent) {
            if let super::resources::Resource::Window(win) = res.value() {
                win.read().visual
            } else { 0x21 }
        } else { 0x21 }
    } else { visual };

    let mut window = super::resources::WindowState::new(
        wid,
        parent,
        x, y,
        width, height,
        border_width,
        effective_depth,
        super::resources::WindowClass::from(class),
        effective_visual,
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

    let mut bg_pixel_for_native: Option<(u32, Option<crate::display::NativeWindowHandle>)> = None;

    if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let mut w = win.write();
            let mut offset = 12;

            if value_mask & 0x0001 != 0 { // BackPixmap
                offset += 4;
            }
            if value_mask & 0x0002 != 0 { // BackPixel
                let bg = read_u32(conn, &data[offset..offset+4]);
                w.background_pixel = Some(bg);
                // Save for propagation to native window (done outside lock)
                bg_pixel_for_native = Some((bg, w.native_window.clone()));
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
                let cursor_id = read_u32(conn, &data[offset..offset+4]);
                w.cursor = cursor_id;
                offset += 4;

                // Look up pre-computed macOS cursor type
                let cursor_type = if cursor_id == 0 {
                    0 // None/default = arrow
                } else if let Some(cres) = server.resources.get(&cursor_id) {
                    if let super::resources::Resource::Cursor(cursor) = cres.value() {
                        cursor.macos_type
                    } else { 0 }
                } else { 0 };
                debug!("ChangeWindowAttributes cursor: win=0x{:08x} cursor_id=0x{:08X} → macos_type={}", wid, cursor_id, cursor_type);

                // Find native window and send cursor update
                let native = w.native_window.clone();
                drop(w); // Release lock before sending command
                let handle = if let Some(h) = native {
                    Some(h)
                } else {
                    find_native_handle(server, wid)
                };
                if let Some(handle) = handle {
                    let _ = server.display_cmd_tx.send(
                        crate::display::DisplayCommand::SetWindowCursor { handle, cursor_type }
                    );
                }
                // Early return since we dropped the lock
                return Ok(());
            }
            let _ = offset;
        }
    }

    // Send background pixel to native window (outside of lock)
    if let Some((bg, native)) = bg_pixel_for_native {
        let handle = native.or_else(|| find_native_handle(server, wid));
        if let Some(handle) = handle {
            let _ = server.display_cmd_tx.send(
                crate::display::DisplayCommand::SetWindowBackgroundPixel { handle, pixel: bg }
            );
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
    destroy_window_recursive(server, wid);
    Ok(())
}

fn destroy_window_recursive(server: &Arc<XServer>, wid: Xid) {
    // Get parent and children before removing
    let (parent, children, native_window) = if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            (w.parent, w.children.clone(), w.native_window.clone())
        } else {
            return;
        }
    } else {
        return;
    };

    // Recursively destroy children first
    for child in &children {
        destroy_window_recursive(server, *child);
    }

    // Remove from parent's children list
    if parent != 0 {
        if let Some(res) = server.resources.get(&parent) {
            if let super::resources::Resource::Window(win) = res.value() {
                win.write().children.retain(|&c| c != wid);
            }
        }
    }

    // Close native window if exists
    if let Some(handle) = native_window {
        let _ = server.display_cmd_tx.send(
            crate::display::DisplayCommand::DestroyWindow { handle },
        );
    }

    // Remove from resource table
    server.resources.remove(&wid);
    debug!("Destroyed window 0x{:08X}", wid);
}

async fn handle_reparent_window<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 16 {
        return Err(ServerError::Protocol);
    }
    let wid = read_u32(conn, &data[4..8]);
    let new_parent = read_u32(conn, &data[8..12]);
    let x = read_i16(conn, &data[12..14]);
    let y = read_i16(conn, &data[14..16]);
    debug!("ReparentWindow: 0x{:08X} -> parent 0x{:08X} at ({},{})", wid, new_parent, x, y);

    // Update window's parent and position
    if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let mut w = win.write();
            let old_parent = w.parent;
            w.parent = new_parent;
            w.x = x;
            w.y = y;
            drop(w);

            // Remove from old parent's children list
            if let Some(old_res) = server.resources.get(&old_parent) {
                if let super::resources::Resource::Window(old_win) = old_res.value() {
                    old_win.write().children.retain(|&c| c != wid);
                }
            }
            // Add to new parent's children list
            if let Some(new_res) = server.resources.get(&new_parent) {
                if let super::resources::Resource::Window(new_win) = new_res.value() {
                    new_win.write().children.push(wid);
                }
            }
        }
    }
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
            // Re-acquire lock to store handle and check for deferred WM_NAME
            let (deferred_title, bg_pixel_opt) = if let Some(res) = server.resources.get(&wid) {
                if let super::resources::Resource::Window(win) = res.value() {
                    let mut w = win.write();
                    w.native_window = Some(handle.clone());
                    // If WM_NAME was already set before MapWindow, apply it now
                    let title = w.get_property(crate::server::atoms::predefined::WM_NAME)
                        .and_then(|p| std::str::from_utf8(&p.data).ok().map(|s| s.to_string()));
                    // If _NET_WM_ICON was already set before MapWindow, apply it now
                    let icon_atom = server.atoms.get_id("_NET_WM_ICON");
                    if let Some(atom) = icon_atom {
                        if w.get_property(atom).is_some() {
                            send_icon_from_property(server, conn, &w, atom, &handle);
                        }
                    }
                    (title, w.background_pixel)
                } else { (None, None) }
            } else { (None, None) };
            if let Some(title) = deferred_title {
                let _ = server.display_cmd_tx.send(
                    crate::display::DisplayCommand::SetWindowTitle {
                        handle: handle.clone(),
                        title,
                    },
                );
            }
            // Only set background pixel for resize fill if explicitly defined
            if let Some(bg_pixel) = bg_pixel_opt {
                let _ = server.display_cmd_tx.send(
                    crate::display::DisplayCommand::SetWindowBackgroundPixel {
                        handle: handle.clone(),
                        pixel: bg_pixel,
                    },
                );
            }
            // Determine visibility: show the first window per connection,
            // transient (dialog) windows, and override_redirect windows.
            // Other windows stay hidden (render-only IOSurface buffers).
            let visible = {
                let has_transient = if let Some(res) = server.resources.get(&wid) {
                    if let super::resources::Resource::Window(win) = res.value() {
                        win.read().get_property(crate::server::atoms::predefined::WM_TRANSIENT_FOR).is_some()
                    } else { false }
                } else { false };
                if override_redirect || has_transient {
                    true
                } else {
                    // Check if this connection already has a visible native window
                    let resource_base = wid & 0xFFE00000;
                    !server.resources.iter().any(|r| {
                        let rid = *r.key();
                        if (rid & 0xFFE00000) == resource_base && rid != wid {
                            if let super::resources::Resource::Window(w2) = r.value() {
                                w2.read().native_window.is_some()
                            } else { false }
                        } else { false }
                    })
                }
            };
            let _ = server.display_cmd_tx.send(
                crate::display::DisplayCommand::ShowWindow { handle, visible },
            );

            // Auto-focus visible windows (transient dialogs, popups) so keyboard
            // input routes to them when macOS makes them the key window.
            if visible {
                let old_focus = server.focus_window.load(std::sync::atomic::Ordering::Relaxed);
                if old_focus != wid {
                    server.focus_window.store(wid, std::sync::atomic::Ordering::Relaxed);
                    if old_focus > 1 {
                        crate::server::send_focus_event(server, 10, old_focus); // FocusOut
                    }
                    crate::server::send_focus_event(server, 9, wid); // FocusIn
                    info!("MapWindow 0x{:08X}: auto-focused (visible window)", wid);
                }
            }
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
        let _ = conn.event_tx.send(expose.into());
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
                if w.mapped { Some((w.width, w.height)) } else { None }
            } else { None }
        } else { None };
        if let Some((cw, ch)) = child_info {
            if cw > 0 && ch > 0 {
                let mut child_expose = super::events::build_expose_event(conn, child_id, 0, 0, cw, ch, 0);
                set_event_sequence(conn, &mut child_expose, seq);
                let _ = conn.event_tx.send(child_expose.into());
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

    // If unmapping the currently focused window, revert focus to PointerRoot
    // so key events route to whatever window the pointer is over.
    let current_focus = server.focus_window.load(std::sync::atomic::Ordering::Relaxed);
    if current_focus == wid {
        server.focus_window.store(1, std::sync::atomic::Ordering::Relaxed); // PointerRoot
        crate::server::send_focus_event(server, 10, wid); // FocusOut
        info!("UnmapWindow 0x{:08X}: reverted focus to PointerRoot", wid);
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

            info!("ConfigureWindow: 0x{:08X} mask=0x{:04X} => {}x{} at ({},{}) border={} old={}x{}",
                wid, value_mask, w.width, w.height, w.x, w.y, w.border_width, old_width, old_height);

            // Update native window if exists
            if let Some(ref handle) = w.native_window {
                // Suppress setFrameSize events — ConfigureWindow handler sends them itself
                if w.width != old_width || w.height != old_height {
                    crate::display::SUPPRESS_RESIZE_EVENTS.store(true, std::sync::atomic::Ordering::Relaxed);
                }
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
            Some((w.x, w.y, w.width, w.height, w.border_width, w.override_redirect, w.parent, size_changed, wants_structure_notify, old_width, old_height))
        } else { None }
    } else { None };

    // Send ConfigureNotify event inline (must maintain sequence order)
    if let Some((x, y, width, height, border_width, override_redirect, _parent, size_changed, wants_structure_notify, old_width, old_height)) = configure_info {
        debug!("ConfigureWindow 0x{:08X}: size_changed={} wants_struct_notify={}", wid, size_changed, wants_structure_notify);
        if wants_structure_notify {
            let seq = conn.current_request_sequence();
            let mut config_notify = super::events::build_configure_notify(
                conn, wid, wid, 0, x, y, width, height, border_width, override_redirect,
            );
            set_event_sequence(conn, &mut config_notify, seq);
            stream.write_all(&config_notify).await?;
        }
        // When a window is resized, send full Expose so app redraws at new size.
        // Many apps (xterm, xclock, xeyes) depend on full Expose to relayout.
        if size_changed {
            let seq = conn.current_request_sequence();
            let mut expose = super::events::build_expose_event(conn, wid, 0, 0, width, height, 0);
            set_event_sequence(conn, &mut expose, seq);
            stream.write_all(&expose).await?;

            // Parent size = direct child size: resize direct children to same dimensions
            let children = if let Some(res) = server.resources.get(&wid) {
                if let super::resources::Resource::Window(win) = res.value() {
                    win.read().children.clone()
                } else { Vec::new() }
            } else { Vec::new() };
            for child_id in children {
                if let Some(res) = server.resources.get(&child_id) {
                    if let super::resources::Resource::Window(win) = res.value() {
                        let mut w = win.write();
                        w.width = width;
                        w.height = height;
                    }
                }
                // Send ConfigureNotify + Expose to child via event queue
                let child_border = if let Some(res) = server.resources.get(&child_id) {
                    if let super::resources::Resource::Window(win) = res.value() {
                        let w = win.read();
                        w.border_width
                    } else { 0 }
                } else { 0 };
                let mut child_config = super::events::build_configure_notify(
                    conn, child_id, child_id, 0, 0, 0, width, height, child_border, false,
                );
                set_event_sequence(conn, &mut child_config, seq);
                let _ = conn.event_tx.send(child_config.into());

                let mut child_expose = super::events::build_expose_event(conn, child_id, 0, 0, width, height, 0);
                set_event_sequence(conn, &mut child_expose, seq);
                let _ = conn.event_tx.send(child_expose.into());
            }
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

    // Resolve property and type atom names for logging
    let prop_name = server.atoms.get_name(property).unwrap_or_default();
    let _type_name = server.atoms.get_name(type_atom).unwrap_or_default();
    info!("ChangeProperty: wid=0x{:08X} prop={} ({}) type={} fmt={} len={}", wid, prop_name, property, _type_name, format, num_elements);

    // Check if this is a clipboard copy response (property = _PSLX_CLIP on root window)
    if server.pending_clipboard_copy.load(std::sync::atomic::Ordering::Relaxed)
        && prop_name == "_PSLX_CLIP"
    {
        server.pending_clipboard_copy.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Ok(text) = std::str::from_utf8(&prop_data) {
            if !text.is_empty() {
                info!("Clipboard copy: got {} bytes from X11 selection, writing to macOS clipboard", text.len());
                // Write to macOS clipboard via pbcopy
                if let Ok(mut child) = std::process::Command::new("pbcopy")
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                {
                    if let Some(ref mut stdin) = child.stdin {
                        use std::io::Write;
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    let _ = child.wait();
                }
            }
        }
    }

    if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let mut w = win.write();
            w.set_property(super::resources::Property {
                name: property,
                type_atom,
                format,
                data: prop_data,
            });

            // Check for WM_NAME or _NET_WM_NAME changes to update native window title
            if property == crate::server::atoms::predefined::WM_NAME || prop_name == "_NET_WM_NAME" {
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

            // Check for _NET_WM_ICON to update native window icon
            if prop_name == "_NET_WM_ICON" && format == 32 {
                if let Some(ref handle) = w.native_window {
                    send_icon_from_property(server, conn, &w, property, handle);
                }
                // If native_window not yet set, icon will be applied in handle_map_window
            }
        }
    }
    Ok(())
}

/// Extract _NET_WM_ICON data from a window property and send SetWindowIcon command.
fn send_icon_from_property(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    w: &super::resources::WindowState,
    property: u32,
    handle: &crate::display::NativeWindowHandle,
) {
    if let Some(prop) = w.get_property(property) {
        let icon_data: Vec<u32> = prop.data.chunks_exact(4)
            .map(|c| read_u32(conn, c))
            .collect();
        let mut best_w = 0u32;
        let mut best_h = 0u32;
        let mut best_offset = 0usize;
        let mut best_score = u32::MAX;
        let mut offset = 0usize;
        while offset + 2 < icon_data.len() {
            let iw = icon_data[offset];
            let ih = icon_data[offset + 1];
            let pixel_count = (iw as usize) * (ih as usize);
            if offset + 2 + pixel_count > icon_data.len() { break; }
            let score = ((iw as i32 - 48).unsigned_abs())
                .saturating_add((ih as i32 - 48).unsigned_abs());
            if score < best_score {
                best_score = score;
                best_w = iw;
                best_h = ih;
                best_offset = offset + 2;
            }
            offset += 2 + pixel_count;
        }
        if best_w > 0 && best_h > 0 {
            let pixel_count = (best_w as usize) * (best_h as usize);
            let argb_data = icon_data[best_offset..best_offset + pixel_count].to_vec();
            info!("SetWindowIcon: {}x{} for handle {}", best_w, best_h, handle.id);
            let _ = server.display_cmd_tx.send(
                crate::display::DisplayCommand::SetWindowIcon {
                    handle: handle.clone(),
                    width: best_w,
                    height: best_h,
                    argb_data,
                },
            );
        }
    }
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

    let (parent, children) = if let Some(res) = server.resources.get(&wid) {
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
        match res.value() {
            super::resources::Resource::Window(win) => {
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
            }
            super::resources::Resource::Pixmap(pix) => {
                let p = pix.read();
                let mut reply = Vec::with_capacity(32);
                reply.push(1); // reply
                reply.push(p.depth);
                write_u16_to(conn, &mut reply, seq);
                write_u32_to(conn, &mut reply, 0);
                write_u32_to(conn, &mut reply, server.screens[0].root_window); // root
                write_i16_to(conn, &mut reply, 0); // x
                write_i16_to(conn, &mut reply, 0); // y
                write_u16_to(conn, &mut reply, p.width);
                write_u16_to(conn, &mut reply, p.height);
                write_u16_to(conn, &mut reply, 0); // border_width
                reply.extend(std::iter::repeat(0).take(10));
                Some(reply)
            }
            _ => None,
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

    // Parse GC value list — all 23 bits (0-22) must be processed in order.
    // Each set bit corresponds to a 4-byte value in the data.
    parse_gc_values(conn, data, 16, value_mask, &mut gc);

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
            parse_gc_values(conn, data, 12, value_mask, &mut g);
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
/// X11 spec: Only clear windows that have a defined background (background_pixel set).
/// Windows with background=None have undefined contents — server must NOT fill them.
fn clear_window_tree(server: &Arc<XServer>, wid: u32) {
    let info = if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            Some((w.width, w.height, w.background_pixel, w.children.clone()))
        } else { None }
    } else { None };

    if let Some((width, height, bg_opt, children)) = info {
        // Only clear if background_pixel is explicitly set
        if let Some(bg) = bg_opt {
            dispatch_render_commands(server, wid, vec![
                crate::display::RenderCommand::ClearArea {
                    x: 0, y: 0, width, height, bg_color: bg,
                }
            ]);
        }
        // Clear mapped children that have a defined background
        for child_id in children {
            if let Some(res) = server.resources.get(&child_id) {
                if let super::resources::Resource::Window(win) = res.value() {
                    let w = win.read();
                    if w.mapped {
                        if let Some(bg) = w.background_pixel {
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
}

/// Parse GC value list attributes (bits 0-22) in correct order.
/// X11 GC value list packs values for set bits in ascending bit order.
/// All 23 possible bits must be processed to maintain correct offsets.
fn parse_gc_values(
    conn: &Arc<ClientConnection>,
    data: &[u8],
    start_offset: usize,
    value_mask: u32,
    gc: &mut super::resources::GContextState,
) {
    let mut offset = start_offset;
    // Process all 23 GC attribute bits in order (0-22).
    // For each set bit, read the 4-byte value and advance offset.
    for bit in 0..23u32 {
        if value_mask & (1 << bit) == 0 { continue; }
        if offset + 4 > data.len() { break; }
        let val = read_u32(conn, &data[offset..offset+4]);
        offset += 4;
        match bit {
            0  => gc.function = super::resources::GcFunction::from(val as u8),
            1  => gc.plane_mask = val,
            2  => gc.foreground = val,
            3  => gc.background = val,
            4  => gc.line_width = val as u16,
            5  => gc.line_style = val as u8,
            14 => gc.font = val,
            16 => gc.graphics_exposures = val != 0,
            _  => { /* skip unhandled attributes (cap-style, join-style, fill-style, etc.) */ }
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

/// Update the global IME cursor spot position.
/// Walks up from the drawable to find the top-level native window,
/// accumulating coordinate offsets, and stores the result in global atomics.
fn update_ime_spot(server: &Arc<XServer>, drawable: u32, x: i16, y: i16) {
    use std::sync::atomic::Ordering;
    let (_, abs_x, abs_y) = find_native_position(server, drawable, x, y);
    crate::display::IME_SPOT_X.store(abs_x as i32, Ordering::Relaxed);
    crate::display::IME_SPOT_Y.store(abs_y as i32, Ordering::Relaxed);
}

/// Find the native window for a drawable and compute position in native window coordinates.
/// Find the native window handle for any X11 window (walks up to top-level).
fn find_native_handle(server: &Arc<XServer>, wid: u32) -> Option<crate::display::NativeWindowHandle> {
    if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            if let Some(ref handle) = w.native_window {
                return Some(handle.clone());
            }
            let mut current = w.parent;
            drop(w);
            for _ in 0..32 {
                if let Some(pres) = server.resources.get(&current) {
                    if let super::resources::Resource::Window(pwin) = pres.value() {
                        let pw = pwin.read();
                        if let Some(ref handle) = pw.native_window {
                            return Some(handle.clone());
                        }
                        current = pw.parent;
                    } else { break; }
                } else { break; }
            }
        }
    }
    None
}

/// Returns (native_window_handle_id, adjusted_x, adjusted_y).
fn find_native_position(server: &Arc<XServer>, drawable: u32, x: i16, y: i16) -> (u64, i16, i16) {
    if let Some(res) = server.resources.get(&drawable) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            if let Some(ref handle) = w.native_window {
                return (handle.id, x, y);
            }
            // Child window — walk up ancestor chain
            let mut ax = x + w.x;
            let mut ay = y + w.y;
            let mut current = w.parent;
            drop(w);
            for _ in 0..32 {
                if let Some(pres) = server.resources.get(&current) {
                    if let super::resources::Resource::Window(pwin) = pres.value() {
                        let pw = pwin.read();
                        if let Some(ref handle) = pw.native_window {
                            return (handle.id, ax, ay);
                        }
                        ax += pw.x;
                        ay += pw.y;
                        current = pw.parent;
                    } else { break; }
                } else { break; }
            }
        }
    }
    (0, 0, 0)
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
                            debug!("ChildWindow dispatch: drawable=0x{:08X} offset=({},{}) -> native win {} ({} cmds)",
                                  drawable, offset_x, offset_y, handle.id, commands.len());
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
            RenderCommand::FillRectangle { x, y, width, height, color, gc_function } =>
                RenderCommand::FillRectangle { x: x + dx, y: y + dy, width, height, color, gc_function },
            RenderCommand::ClearArea { x, y, width, height, bg_color } =>
                RenderCommand::ClearArea { x: x + dx, y: y + dy, width, height, bg_color },
            RenderCommand::DrawLine { x1, y1, x2, y2, color, line_width, gc_function } =>
                RenderCommand::DrawLine { x1: x1 + dx, y1: y1 + dy, x2: x2 + dx, y2: y2 + dy, color, line_width, gc_function },
            RenderCommand::FillArc { x, y, width, height, angle1, angle2, color, gc_function } =>
                RenderCommand::FillArc { x: x + dx, y: y + dy, width, height, angle1, angle2, color, gc_function },
            RenderCommand::DrawArc { x, y, width, height, angle1, angle2, color, line_width, gc_function } =>
                RenderCommand::DrawArc { x: x + dx, y: y + dy, width, height, angle1, angle2, color, line_width, gc_function },
            RenderCommand::DrawRectangle { x, y, width, height, color, line_width, gc_function } =>
                RenderCommand::DrawRectangle { x: x + dx, y: y + dy, width, height, color, line_width, gc_function },
            RenderCommand::PutImage { x, y, width, height, depth, format, data, gc_function } =>
                RenderCommand::PutImage { x: x + dx, y: y + dy, width, height, depth, format, data, gc_function },
            RenderCommand::DrawText { x, y, text, font_id, color, bg_color, gc_function } =>
                RenderCommand::DrawText { x: x + dx, y: y + dy, text, font_id, color, bg_color, gc_function },
            RenderCommand::CopyArea { src_x, src_y, dst_x, dst_y, width, height, gc_function } =>
                RenderCommand::CopyArea { src_x: src_x + dx, src_y: src_y + dy, dst_x: dst_x + dx, dst_y: dst_y + dy, width, height, gc_function },
            RenderCommand::FillPolygon { points, color, gc_function } =>
                RenderCommand::FillPolygon { points: points.into_iter().map(|(x, y)| (x + dx, y + dy)).collect(), color, gc_function },
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

    // Get foreground color and GC function from GC
    let (fg_color, gc_fn) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let g = gc.read();
            (g.foreground, g.function as u8)
        } else { (0, 3) }
    } else { (0, 3) }; // 3 = GXcopy

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
            x, y, width, height, color: fg_color, gc_function: gc_fn,
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
    // X11 spec: If background is None, ClearArea does nothing (no painting).
    let (bg_color_opt, is_window) = if let Some(res) = server.resources.get(&wid) {
        match res.value() {
            super::resources::Resource::Window(win) => {
                let w = win.read();
                if width == 0 { width = w.width.saturating_sub(x as u16); }
                if height == 0 { height = w.height.saturating_sub(y as u16); }
                (w.background_pixel, true)
            }
            super::resources::Resource::Pixmap(pix) => {
                let p = pix.read();
                if width == 0 { width = p.width.saturating_sub(x as u16); }
                if height == 0 { height = p.height.saturating_sub(y as u16); }
                (Some(0u32), false)
            }
            _ => return Ok(()),
        }
    } else {
        return Ok(());
    };

    // X11 spec: If background is None, ClearArea has no effect on window contents.
    // If background is defined, fill with it. Either way, Expose events still fire below.
    if let Some(bg_color) = bg_color_opt {
        let command = crate::display::RenderCommand::ClearArea {
            x, y, width, height, bg_color,
        };
        dispatch_render_commands(server, wid, vec![command]);
    }

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

    // debug!("CopyArea: src=0x{:08X} dst=0x{:08X} gc=0x{:08X} ({},{}) -> ({},{}) {}x{}",
    //        src_drawable, dst_drawable, gcid, src_x, src_y, dst_x, dst_y, width, height);

    // Read GC properties
    let (graphics_exposures, gc_function) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.graphics_exposures, gc.function as u8)
        } else { (false, 3) }
    } else { (false, 3) };

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
                        src_x, src_y, dst_x, dst_y, width, height, gc_function,
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
            gc_function,
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

    let (fg_color, gc_function) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.function as u8)
        } else { (0, 3) }
    } else { (0, 3) };

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
            x: px, y: py, width: 1, height: 1, color: fg_color, gc_function,
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

    let (fg_color, line_width, gc_function) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.line_width, gc.function as u8)
        } else { (0xFFFFFF, 0, 3) }
    } else { (0xFFFFFF, 0, 3) };

    let mut commands = Vec::new();
    let mut offset = 12;
    while offset + 8 <= data.len() {
        let x = read_i16(conn, &data[offset..offset+2]);
        let y = read_i16(conn, &data[offset+2..offset+4]);
        let w = read_u16(conn, &data[offset+4..offset+6]);
        let h = read_u16(conn, &data[offset+6..offset+8]);
        offset += 8;

        commands.push(crate::display::RenderCommand::DrawRectangle {
            x, y, width: w, height: h, color: fg_color, line_width, gc_function,
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

    let (fg_color, line_width, gc_fn) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.line_width, gc.function as u8)
        } else { (0xFFFFFF, 0, 3) }
    } else { (0xFFFFFF, 0, 3) };

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
            color: fg_color, line_width, gc_function: gc_fn,
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

    let (fg_color, line_width, gc_fn) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.line_width, gc.function as u8)
        } else { (0xFFFFFF, 0, 3) }
    } else { (0xFFFFFF, 0, 3) };

    let mut commands = Vec::new();
    let mut offset = 12;
    while offset + 8 <= data.len() {
        let x1 = read_i16(conn, &data[offset..offset+2]);
        let y1 = read_i16(conn, &data[offset+2..offset+4]);
        let x2 = read_i16(conn, &data[offset+4..offset+6]);
        let y2 = read_i16(conn, &data[offset+6..offset+8]);
        offset += 8;
        commands.push(crate::display::RenderCommand::DrawLine {
            x1, y1, x2, y2, color: fg_color, line_width, gc_function: gc_fn,
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

    let (fg_color, gc_fn) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let g = gc.read();
            (g.foreground, g.function as u8)
        } else { (0xFFFFFF, 3) }
    } else { (0xFFFFFF, 3) };

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
        gc_function: gc_fn,
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

    let (fg_color, line_width, gc_fn) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.line_width, gc.function as u8)
        } else { (0xFFFFFF, 0, 3) }
    } else { (0xFFFFFF, 0, 3) };

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
            color: fg_color, line_width, gc_function: gc_fn,
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

    let (fg_color, gc_fn) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let g = gc.read();
            (g.foreground, g.function as u8)
        } else { (0xFFFFFF, 3) }
    } else { (0xFFFFFF, 3) };

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

        // debug!("PolyFillArc: drawable=0x{:08X} gc=0x{:08X} ({},{} {}x{}) a1={} a2={} color=0x{:06X}",
        //       drawable, gcid, x, y, width, height, angle1, angle2, fg_color);

        commands.push(crate::display::RenderCommand::FillArc {
            x, y, width, height, angle1, angle2,
            color: fg_color, gc_function: gc_fn,
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
    let gcid = read_u32(conn, &data[8..12]);
    let width = read_u16(conn, &data[12..14]);
    let height = read_u16(conn, &data[14..16]);
    let dst_x = read_i16(conn, &data[16..18]);
    let dst_y = read_i16(conn, &data[18..20]);
    let _left_pad = data[20];
    let depth = data[21];

    let gc_function = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            gc.read().function as u8
        } else { 3 }
    } else { 3 };

    if width > 100 && height > 100 {
        info!("PutImage: drawable=0x{:08X} {}x{} at ({},{}) depth={} format={}", drawable, width, height, dst_x, dst_y, depth, format);
    }
    let image_data = data[24..].to_vec();

    // Track PutImage position as IME cursor hint (xterm Xft renders glyphs via PutImage)
    // Heuristic: glyph-sized images (height 8-40, width 4-30) likely represent text
    if height >= 8 && height <= 40 && width >= 4 && width <= 30 {
        update_ime_spot(server, drawable, dst_x + width as i16, dst_y);
    }

    let command = crate::display::RenderCommand::PutImage {
        x: dst_x,
        y: dst_y,
        width,
        height,
        depth,
        format,
        data: image_data,
        gc_function,
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

    let (fg_color, font_id, gc_function) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.font, gc.function as u8)
        } else { (0xFFFFFF, 0, 3) }
    } else { (0xFFFFFF, 0, 3) };

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
            x, y, text: text_bytes, font_id, color: fg_color, bg_color: None, gc_function,
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
        // Update IME cursor position: end of drawn text in native window coords
        let cursor_x = x + (str_len as i16) * 6; // GLYPH_W = 6
        update_ime_spot(server, drawable, cursor_x, y);

        let command = crate::display::RenderCommand::DrawText {
            x, y, text, font_id, color: fg_color, bg_color, gc_function: 3, // ImageText always GXcopy
        };
        dispatch_render_commands(server, drawable, vec![command]);
    }
    Ok(())
}

/// PolyText16 (opcode 75): 16-bit text drawing (no background fill).
/// Each character is 2 bytes (big-endian). Convert to UTF-8 for rendering.
async fn handle_poly_text16<S: AsyncRead + AsyncWrite + Unpin>(
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

    let (fg_color, font_id, gc_function) = if let Some(res) = server.resources.get(&gcid) {
        if let super::resources::Resource::GContext(gc) = res.value() {
            let gc = gc.read();
            (gc.foreground, gc.font, gc.function as u8)
        } else { (0xFFFFFF, 0, 3) }
    } else { (0xFFFFFF, 0, 3) };

    // Parse text items: each item has len (in 2-byte chars), delta, then len*2 bytes
    let mut text_bytes = Vec::new();
    let mut offset = 16;
    while offset < data.len() {
        let len = data[offset] as usize;
        if len == 0 || len == 255 { break; }
        let _delta = data[offset + 1] as i8;
        offset += 2;
        let byte_len = len * 2;
        if offset + byte_len <= data.len() {
            // Convert 16-bit big-endian chars to UTF-8 bytes
            for j in 0..len {
                let hi = data[offset + j * 2] as u16;
                let lo = data[offset + j * 2 + 1] as u16;
                let codepoint = (hi << 8) | lo;
                if let Some(ch) = char::from_u32(codepoint as u32) {
                    let mut buf = [0u8; 4];
                    let s = ch.encode_utf8(&mut buf);
                    text_bytes.extend_from_slice(s.as_bytes());
                }
            }
        }
        offset += byte_len;
    }

    if !text_bytes.is_empty() {
        let command = crate::display::RenderCommand::DrawText {
            x, y, text: text_bytes, font_id, color: fg_color, bg_color: None, gc_function,
        };
        dispatch_render_commands(server, drawable, vec![command]);
    }
    Ok(())
}

/// ImageText16 (opcode 77): 16-bit text drawing with background fill.
/// Each character is 2 bytes (big-endian). Convert to UTF-8 for rendering.
async fn handle_image_text16<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 16 {
        return Err(ServerError::Protocol);
    }

    let str_len = data[1] as usize; // number of 2-byte characters
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

    let byte_len = str_len * 2;
    let text_end = (16 + byte_len).min(data.len());
    let raw = &data[16..text_end];

    // Convert 16-bit big-endian chars to UTF-8
    let mut text_bytes = Vec::new();
    let mut char_count = 0usize;
    for i in 0..str_len {
        if i * 2 + 1 >= raw.len() { break; }
        let hi = raw[i * 2] as u16;
        let lo = raw[i * 2 + 1] as u16;
        let codepoint = (hi << 8) | lo;
        if let Some(ch) = char::from_u32(codepoint as u32) {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            text_bytes.extend_from_slice(s.as_bytes());
            char_count += 1;
        }
    }

    if !text_bytes.is_empty() {
        // Update IME cursor position
        let cursor_x = x + (char_count as i16) * 12; // CJK chars are typically double-width (2 * GLYPH_W)
        update_ime_spot(server, drawable, cursor_x, y);

        let command = crate::display::RenderCommand::DrawText {
            x, y, text: text_bytes, font_id, color: fg_color, bg_color, gc_function: 3, // ImageText always GXcopy
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

    let extensions = super::extensions::supported_extensions();
    let found = extensions.iter().find(|e| e.name == name);

    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data
    if let Some(ext) = found {
        debug!("QueryExtension: '{}' -> present, opcode={}", name, ext.major_opcode);
        reply.push(1); // present (true)
        reply.push(ext.major_opcode);
        reply.push(ext.first_event);
        reply.push(ext.first_error);
    } else {
        reply.push(0); // present (false)
        reply.push(0); // major opcode
        reply.push(0); // first event
        reply.push(0); // first error
    }
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
    info!("OpenFont: 0x{:08X} '{}' is_2byte={}", fid, name, name.contains("iso10646"));

    let is_2byte = name.contains("iso10646");
    let (ascent, descent, char_width) = parse_xlfd_metrics(&name);
    let font = super::resources::FontState {
        id: fid,
        name,
        ascent,
        descent,
        default_char: 0,
        min_char_width: char_width,
        max_char_width: char_width,
        is_2byte,
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
    let (ascent, descent, max_width, default_char, is_2byte) = {
        if let Some(res) = server.resources.get(&fid) {
            if let super::resources::Resource::Font(font) = res.value() {
                let f = font.read();
                (f.ascent, f.descent, f.max_char_width, f.default_char, f.is_2byte)
            } else {
                (12, 3, 8, 0, false)
            }
        } else {
            (12, 3, 8, 0, false)
        }
    };

    // 2-byte fonts (iso10646): report full BMP coverage
    let (min_byte2, max_byte2, min_byte1, max_byte1) = if is_2byte {
        (0u16, 255u16, 0u8, 255u8)
    } else {
        (32u16, 126u16, 0u8, 0u8)
    };

    // For 2-byte fonts, set num_chars=0 when min-bounds == max-bounds
    // (X11 spec: "If min-bounds equals max-bounds, zero char-infos are returned")
    // This avoids sending 65536×12=786KB of identical CharInfo data.
    let num_chars = if is_2byte {
        0u32
    } else {
        (max_byte2 - min_byte2 + 1) as u32
    };

    let num_properties = 0u16;

    let mut reply = Vec::with_capacity(60 + (num_chars as usize) * 12);
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 7 + num_properties as u32 + num_chars * 3); // additional data (in 4-byte units)

    // min-bounds = max-bounds (same metrics for all characters)
    let lsb: i16 = 0;
    let rsb: i16 = max_width as i16;
    let cw: i16 = max_width as i16;
    let asc: i16 = ascent as i16;
    let desc: i16 = -(descent as i16);

    // min-bounds (CharInfo: 12 bytes)
    write_i16_to(conn, &mut reply, lsb);
    write_i16_to(conn, &mut reply, rsb);
    write_i16_to(conn, &mut reply, cw);
    write_i16_to(conn, &mut reply, asc);
    write_i16_to(conn, &mut reply, desc);
    write_u16_to(conn, &mut reply, 0);  // attributes

    // 4 bytes unused
    write_u32_to(conn, &mut reply, 0);

    // max-bounds (CharInfo: 12 bytes) — same as min-bounds
    write_i16_to(conn, &mut reply, lsb);
    write_i16_to(conn, &mut reply, rsb);
    write_i16_to(conn, &mut reply, cw);
    write_i16_to(conn, &mut reply, asc);
    write_i16_to(conn, &mut reply, desc);
    write_u16_to(conn, &mut reply, 0);

    // 4 bytes unused
    write_u32_to(conn, &mut reply, 0);

    write_u16_to(conn, &mut reply, min_byte2); // min-char-or-byte2
    write_u16_to(conn, &mut reply, max_byte2); // max-char-or-byte2
    write_u16_to(conn, &mut reply, default_char); // default-char
    write_u16_to(conn, &mut reply, num_properties);
    reply.push(0); // draw-direction (LeftToRight)
    reply.push(min_byte1); // min-byte1
    reply.push(max_byte1); // max-byte1
    reply.push(1); // all-chars-exist (true)
    write_i16_to(conn, &mut reply, ascent as i16);  // font-ascent
    write_i16_to(conn, &mut reply, descent as i16); // font-descent
    write_u32_to(conn, &mut reply, num_chars); // number of CharInfos

    // CharInfo for each character (0 for 2-byte fonts since min==max bounds)
    for _i in 0..num_chars {
        write_i16_to(conn, &mut reply, lsb);
        write_i16_to(conn, &mut reply, rsb);
        write_i16_to(conn, &mut reply, cw);
        write_i16_to(conn, &mut reply, asc);
        write_i16_to(conn, &mut reply, desc);
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

async fn handle_list_properties<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let wid = read_u32(conn, &data[4..8]);
    let seq = conn.current_request_sequence();

    let atoms: Vec<u32> = if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            win.read().properties.iter().map(|p| p.name).collect()
        } else {
            Vec::new()
        }
    } else {
        // Window not found — send BadWindow error
        let mut err = vec![0u8; 32];
        err[0] = 0; // error
        err[1] = 3; // BadWindow
        write_u16_to(conn, &mut err[2..4].to_vec(), seq);
        // Need to write seq into the buffer properly
        let mut reply = Vec::with_capacity(32);
        reply.push(0); // error
        reply.push(3); // BadWindow
        write_u16_to(conn, &mut reply, seq);
        write_u32_to(conn, &mut reply, wid); // bad resource id
        write_u16_to(conn, &mut reply, 0); // minor opcode
        reply.push(21); // major opcode
        reply.extend(std::iter::repeat(0).take(32 - reply.len()));
        stream.write_all(&reply).await?;
        return Ok(());
    };

    let num_atoms = atoms.len() as u16;
    let data_len = atoms.len() as u32; // each atom is 4 bytes = 1 word
    let mut reply = Vec::with_capacity(32 + atoms.len() * 4);
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, data_len);
    write_u16_to(conn, &mut reply, num_atoms);
    reply.extend(std::iter::repeat(0).take(22)); // padding
    for atom in &atoms {
        write_u32_to(conn, &mut reply, *atom);
    }
    stream.write_all(&reply).await?;
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

    // Send FocusOut to the old focus window, FocusIn to the new one.
    // Chrome/Electron require these events to enable keyboard input in focused widgets.
    let old_focus = server.focus_window.load(std::sync::atomic::Ordering::Relaxed);
    server.focus_window.store(focus, std::sync::atomic::Ordering::Relaxed);
    server.focus_revert_to.store(revert_to as u32, std::sync::atomic::Ordering::Relaxed);

    if old_focus > 1 && old_focus != focus {
        crate::server::send_focus_event(server, 10, old_focus); // FocusOut=10
    }
    if focus > 1 && focus != old_focus {
        crate::server::send_focus_event(server, 9, focus); // FocusIn=9
    }
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
    let extensions = super::extensions::supported_extensions();

    // Build names body: each name is 1-byte length + name bytes
    let mut names_body = Vec::new();
    for ext in &extensions {
        names_body.push(ext.name.len() as u8);
        names_body.extend_from_slice(ext.name.as_bytes());
    }
    // Pad to 4-byte boundary
    while names_body.len() % 4 != 0 {
        names_body.push(0);
    }

    let mut reply = Vec::with_capacity(32 + names_body.len());
    reply.push(1); // reply
    reply.push(extensions.len() as u8); // number of STRs
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, (names_body.len() / 4) as u32);
    reply.extend(std::iter::repeat(0).take(24)); // padding
    reply.extend_from_slice(&names_body);
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_get_keyboard_mapping<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    // X11 GetKeyboardMapping: [0]=opcode, [1]=unused, [2-3]=length, [4]=first-keycode, [5]=count
    let first_keycode = data[4] as u32;
    let count = data[5] as u32;
    let keysyms_per_keycode: u32 = 4;
    let seq = conn.current_request_sequence();
    log::info!("GetKeyboardMapping: first_keycode={} count={} (range {}-{}) seq={}", first_keycode, count, first_keycode, first_keycode + count - 1, seq);

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

        if keycode >= 136 {
            // Virtual keycodes 136+ for IME input (Unicode keysyms)
            let vk = server.virtual_keysyms.read();
            let idx = (keycode - 136) as usize;
            let ks = if idx < vk.len() { vk[idx] } else { 0 };
            if ks != 0 {
                log::info!("GetKeyboardMapping: keycode={} → keysym 0x{:08x} (U+{:04X})",
                    keycode, ks, ks & 0x00FFFFFF);
            }
            write_u32_to(conn, &mut reply, ks);
            write_u32_to(conn, &mut reply, 0);
            write_u32_to(conn, &mut reply, 0);
            write_u32_to(conn, &mut reply, 0);
        } else {
            let mac_key = keycode.wrapping_sub(8); // reverse the +8 offset
            // macOS virtual keycode → (normal keysym, shifted keysym)
            let (keysym, shifted) = macos_keycode_to_keysym(mac_key);
            write_u32_to(conn, &mut reply, keysym);
            write_u32_to(conn, &mut reply, shifted);
            write_u32_to(conn, &mut reply, 0); // NoSymbol (mode switch)
            write_u32_to(conn, &mut reply, 0); // NoSymbol (mode+shift)
        }
    }

    stream.write_all(&reply).await?;
    // Increment generation so send_ime_text knows client has fetched updated keymap
    conn.mapping_gen.fetch_add(1, std::sync::atomic::Ordering::Release);
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
/// Consults the UCKeyTranslate-derived KEYBOARD_MAP first (correct for JIS/all layouts),
/// falling back to the static table for special keys (arrows, F-keys, etc.).
pub(crate) fn macos_keycode_to_keysym(mac_key: u32) -> (u32, u32) {
    // Special keys (function keys, modifiers, arrows, JIS IME keys like 英数/かな) must use
    // the static table — UCKeyTranslate returns garbage for these on JIS keyboards.
    let static_entry = macos_keycode_to_keysym_static(mac_key);
    if static_entry.0 >= 0xFF00 {
        return static_entry;
    }
    // For printable keys, prefer UCKeyTranslate map (handles JIS/UK/AZERTY layout differences).
    if let Some(map) = crate::display::KEYBOARD_MAP.get() {
        if (mac_key as usize) < 128 {
            let entry = map[mac_key as usize];
            if entry != (0, 0) {
                return entry;
            }
        }
    }
    // Fallback to static table.
    static_entry
}

fn macos_keycode_to_keysym_static(mac_key: u32) -> (u32, u32) {
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

        // JIS keyboard keys
        102 => (0xFF30, 0xFF30), // JIS_Eisuu → Eisu_toggle (英数)
        104 => (0xFF27, 0xFF27), // JIS_Kana → Hiragana_Katakana (かな)

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
    reply.push(1); // global auto-repeat (On)
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

async fn handle_get_font_path<S: AsyncRead + AsyncWrite + Unpin>(
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
    write_u16_to(conn, &mut reply, 0); // number of STRs (empty path)
    reply.extend(std::iter::repeat(0).take(22)); // padding
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
    // Keycodes MUST match actual key events: X11 keycode = macOS keycode + 8
    // Shift (macOS 56=LShift, 60=RShift → X11 64, 68)
    reply.push(64); // Shift_L (macOS 56 + 8)
    reply.push(68); // Shift_R (macOS 60 + 8)
    // Lock (macOS 57=CapsLock → X11 65)
    reply.push(65); // Caps_Lock (macOS 57 + 8)
    reply.push(0);
    // Control (macOS 59=LCtrl, 62=RCtrl → X11 67, 70)
    reply.push(67); // Control_L (macOS 59 + 8)
    reply.push(70); // Control_R (macOS 62 + 8)
    // Mod1 (Alt/Option: macOS 58=LOpt, 61=ROpt → X11 66, 69)
    reply.push(66); // Alt_L (macOS 58 + 8)
    reply.push(69); // Alt_R (macOS 61 + 8)
    // Mod2 (Num Lock) - not typically present on Mac keyboards
    reply.push(0);
    reply.push(0);
    // Mod3
    reply.push(0);
    reply.push(0);
    // Mod4 (Super/Command: macOS 55=LCmd → X11 63)
    reply.push(63); // Super_L/Meta_L (macOS 55 + 8)
    reply.push(0);
    // Mod5
    reply.push(0);
    reply.push(0);

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_set_modifier_mapping<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    // SetModifierMapping requires a reply.
    // Reply with MappingSuccess (0) — we accept but ignore the client's mapping.
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // status: MappingSuccess
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data length
    reply.extend(std::iter::repeat(0).take(24)); // padding
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
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    if data.len() < 8 {
        return Err(ServerError::Protocol);
    }
    let max_names = read_u16(conn, &data[4..6]) as usize;
    let pattern_len = read_u16(conn, &data[6..8]) as usize;
    let pattern = if data.len() >= 8 + pattern_len {
        std::str::from_utf8(&data[8..8 + pattern_len]).unwrap_or("*")
    } else {
        "*"
    };
    let pattern_lower = pattern.to_lowercase();
    info!("ListFonts: pattern='{}' max_names={}", pattern, max_names);

    // Collect matching font names (use pre-lowercased names to avoid per-name allocation)
    let mut matched: Vec<&str> = Vec::new();
    for (name, name_lower) in server.font_names.iter().zip(server.font_names_lower.iter()) {
        if matched.len() >= max_names {
            break;
        }
        if x11_font_pattern_match(&pattern_lower, name_lower) {
            matched.push(name);
        }
    }

    // Build the names data: each name is 1-byte length + name bytes
    let mut names_data = Vec::new();
    for name in &matched {
        let name_bytes = name.as_bytes();
        names_data.push(name_bytes.len() as u8);
        names_data.extend_from_slice(name_bytes);
    }
    // Pad to 4-byte boundary
    while names_data.len() % 4 != 0 {
        names_data.push(0);
    }

    let mut reply = Vec::with_capacity(32 + names_data.len());
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, (names_data.len() / 4) as u32);
    write_u16_to(conn, &mut reply, matched.len() as u16);
    reply.extend(std::iter::repeat(0).take(22)); // padding
    reply.extend_from_slice(&names_data);
    info!("ListFonts: pattern='{}' matched {} fonts", pattern, matched.len());
    stream.write_all(&reply).await?;
    Ok(())
}

/// X11 font pattern matching: '*' matches any sequence, '?' matches any single character.
/// Case-insensitive (caller should lowercase both strings).
fn x11_font_pattern_match(pattern: &str, text: &str) -> bool {
    // XLFD names are ASCII-only, so byte-level matching is safe and avoids Vec<char> allocation
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0, 0);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
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
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // ConvertSelection: [4..8]=requestor [8..12]=selection [12..16]=target [16..20]=property [20..24]=time
    if data.len() < 24 { return Err(ServerError::Protocol); }
    let requestor = read_u32(conn, &data[4..8]);
    let selection = read_u32(conn, &data[8..12]);
    let target = read_u32(conn, &data[12..16]);
    let property = read_u32(conn, &data[16..20]);
    let time = read_u32(conn, &data[20..24]);

    let sel_name = server.atoms.get_name(selection).unwrap_or_default();
    let tgt_name = server.atoms.get_name(target).unwrap_or_default();
    info!("ConvertSelection: requestor=0x{:08x} selection={}({}) target={}({}) property={} time={}",
        requestor, selection, sel_name, target, tgt_name, property, time);

    // Find the owner of the selection
    let owner_opt = server.selections.get(&selection).map(|e| (e.0, e.1));

    if let Some((owner, _ts)) = owner_opt {
        // Check if this is an XIM selection owned by our server
        if server.xim.handle_selection_request(owner, requestor, selection, target, property, conn, server) {
            return Ok(());
        }

        // Build SelectionRequest event (type=30) and send to the owner's connection
        let mut event = [0u8; 32];
        event[0] = 30; // SelectionRequest
        // event[1] = 0; // unused
        // seq will be stamped by the connection loop
        write_u32_at(conn, &mut event, 4, time);
        write_u32_at(conn, &mut event, 8, owner);
        write_u32_at(conn, &mut event, 12, requestor);
        write_u32_at(conn, &mut event, 16, selection);
        write_u32_at(conn, &mut event, 20, target);
        write_u32_at(conn, &mut event, 24, property);

        // Find which connection owns the owner window
        let mut sent = false;
        for entry in server.connections.iter() {
            let target_conn = entry.value();
            if (owner & !target_conn.resource_id_mask) == (target_conn.resource_id_base & !target_conn.resource_id_mask) {
                let _ = target_conn.event_tx.send(event.into());
                sent = true;
                info!("ConvertSelection: sent SelectionRequest to conn {} (owner=0x{:08x})", target_conn.id, owner);
                break;
            }
        }

        if !sent {
            // Owner not found — send SelectionNotify with property=None to requestor
            send_selection_notify(server, conn, requestor, selection, target, 0, time);
        }
    } else {
        // No owner — send SelectionNotify with property=None (0) to indicate failure
        info!("ConvertSelection: no owner for selection {}, sending failure SelectionNotify", selection);
        send_selection_notify(server, conn, requestor, selection, target, 0, time);
    }

    Ok(())
}

/// Send a SelectionNotify event (type=31) to the requestor
fn send_selection_notify(
    server: &XServer,
    _conn: &Arc<ClientConnection>,
    requestor: u32,
    selection: u32,
    target: u32,
    property: u32, // 0 = None (failure)
    time: u32,
) {
    // Find the connection that owns the requestor window
    for entry in server.connections.iter() {
        let c = entry.value();
        if (requestor & !c.resource_id_mask) == (c.resource_id_base & !c.resource_id_mask) {
            let mut event = [0u8; 32];
            event[0] = 31; // SelectionNotify
            // seq will be stamped by connection loop
            write_u32_at(c, &mut event, 4, time);
            write_u32_at(c, &mut event, 8, requestor);
            write_u32_at(c, &mut event, 12, selection);
            write_u32_at(c, &mut event, 16, target);
            write_u32_at(c, &mut event, 20, property);
            let _ = c.event_tx.send(event.into());
            break;
        }
    }
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

    // Check if this is a ClientMessage for XIM protocol
    let event_type = event_data[0] & 0x7F; // strip synthetic bit
    if event_type == 33 { // ClientMessage
        let format = event_data[1];
        // Read the message_type (atom) at offset 8-11 of the event data
        let msg_type = read_u32(conn, &event_data[8..12]);
        let msg_window = read_u32(conn, &event_data[4..8]);

        if msg_type == server.xim.atoms.xim_xconnect {
            // XIM transport handshake — the client_comm_window is in data[12..16] (format=32)
            let client_comm_window = if format == 32 {
                read_u32(conn, &event_data[12..16])
            } else {
                msg_window
            };
            info!("SendEvent: intercepted _XIM_XCONNECT from conn {} client_comm=0x{:08x}",
                conn.id, client_comm_window);
            server.xim.handle_xconnect(client_comm_window, conn, server);
            return Ok(());
        } else if msg_type == server.xim.atoms.xim_protocol || msg_type == server.xim.atoms.xim_moredata {
            // XIM protocol message
            let xim_data = &event_data[12..32]; // 20 bytes of data
            server.xim.handle_protocol_message(msg_window, msg_type, xim_data, conn, server);
            return Ok(());
        }
    }

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
            let _ = target_conn.event_tx.send(synthetic_event.into());
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

// --- WarpPointer (opcode 41) ---

async fn handle_warp_pointer<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    if data.len() < 24 {
        return Err(ServerError::Protocol);
    }
    let _src_window = read_u32(_conn, &data[4..8]);
    let dst_window = read_u32(_conn, &data[8..12]);
    let _src_x = read_i16(_conn, &data[12..14]);
    let _src_y = read_i16(_conn, &data[14..16]);
    let _src_width = read_u16(_conn, &data[16..18]);
    let _src_height = read_u16(_conn, &data[18..20]);
    let dst_x = read_i16(_conn, &data[20..22]);
    let dst_y = read_i16(_conn, &data[22..24]);

    // Convert destination coords to root coords
    let (abs_x, abs_y) = if dst_window != 0 {
        let (wx, wy) = window_to_root_coords(server, dst_window);
        (wx + dst_x as i32, wy + dst_y as i32)
    } else {
        (dst_x as i32, dst_y as i32)
    };

    info!("WarpPointer: dst_win=0x{:08x} dst=({},{}) abs=({},{})", dst_window, dst_x, dst_y, abs_x, abs_y);

    // Update stored pointer position
    server.pointer_x.store(abs_x, std::sync::atomic::Ordering::Relaxed);
    server.pointer_y.store(abs_y, std::sync::atomic::Ordering::Relaxed);

    // Generate MotionNotify directly to clients registered on child windows
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;
    let root = server.screens[0].root_window;

    // Walk top-level windows and their children to find matching event registrations
    fn send_warp_motion(server: &XServer, wid: u32, wx: i16, wy: i16, root_x: i16, root_y: i16, time: u32, depth: u8) {
        if depth > 8 { return; }
        if let Some(res) = server.resources.get(&wid) {
            if let super::resources::Resource::Window(win) = res.value() {
                let w = win.read();
                if !w.mapped { return; }
                // Deliver to clients registered on this window
                for &(conn_id, emask) in &w.event_selections {
                    if (emask & super::protocol::event_mask::POINTER_MOTION) != 0 {
                        if let Some(conn_ref) = server.connections.get(&conn_id) {
                            let conn = conn_ref.value();
                            let mut evt = super::events::EventBuilder::new(conn, super::protocol::event_type::MOTION_NOTIFY);
                            evt.set_u8(1, 0)
                               .set_u32(4, time)
                               .set_u32(8, server.screens[0].root_window)
                               .set_u32(12, wid)
                               .set_u32(16, 0)
                               .set_i16(20, root_x)
                               .set_i16(22, root_y)
                               .set_i16(24, wx)
                               .set_i16(26, wy)
                               .set_u16(28, 0)
                               .set_u8(30, 1);
                            let _ = conn.event_tx.send(evt.build().into());
                        }
                    }
                }
                // Recurse into children
                for &child_id in &w.children {
                    if let Some(cres) = server.resources.get(&child_id) {
                        if let super::resources::Resource::Window(cwin) = cres.value() {
                            let cw = cwin.read();
                            let cx = wx - cw.x as i16;
                            let cy = wy - cw.y as i16;
                            drop(cw);
                            drop(cres);
                            send_warp_motion(server, child_id, cx, cy, root_x, root_y, time, depth + 1);
                        }
                    }
                }
            }
        }
    }

    if let Some(res) = server.resources.get(&root) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            for &child_id in &w.children {
                if let Some(cres) = server.resources.get(&child_id) {
                    if let super::resources::Resource::Window(cwin) = cres.value() {
                        let cw = cwin.read();
                        if cw.mapped {
                            let wx = abs_x as i16 - cw.x as i16;
                            let wy = abs_y as i16 - cw.y as i16;
                            drop(cw);
                            drop(cres);
                            send_warp_motion(server, child_id, wx, wy, abs_x as i16, abs_y as i16, time, 0);
                        }
                    }
                }
            }
        }
    }

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

    // Return exact colors: scale 8-bit back to 16-bit (e.g. 0xFF -> 0xFFFF)
    let exact_r = (r8 as u16) << 8 | r8 as u16;
    let exact_g = (g8 as u16) << 8 | g8 as u16;
    let exact_b = (b8 as u16) << 8 | b8 as u16;

    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data length
    write_u16_to(conn, &mut reply, exact_r); // exact red
    write_u16_to(conn, &mut reply, exact_g); // exact green
    write_u16_to(conn, &mut reply, exact_b); // exact blue
    reply.extend_from_slice(&[0; 2]); // padding
    write_u32_to(conn, &mut reply, pixel);
    reply.extend(std::iter::repeat(0).take(12)); // padding to 32 bytes

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_alloc_named_color<S: AsyncRead + AsyncWrite + Unpin>(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut S,
) -> Result<(), ServerError> {
    // AllocNamedColor: cmap=data[4..8], name_len=data[8..10], name=data[12..]
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

    let (r, g, b) = lookup_x11_color(&name).unwrap_or_else(|| {
        debug!("AllocNamedColor: unknown color '{}', defaulting to black", name);
        (0, 0, 0)
    });

    // TrueColor: pixel = (R8 << 16) | (G8 << 8) | B8
    let r8 = (r >> 8) as u32;
    let g8 = (g >> 8) as u32;
    let b8 = (b >> 8) as u32;
    let pixel = (r8 << 16) | (g8 << 8) | b8;

    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(0); // unused
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0); // additional data length
    write_u32_to(conn, &mut reply, pixel); // pixel
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

    // X11 color name lookup using rgb.txt database
    let (r, g, b) = lookup_x11_color(&name).unwrap_or_else(|| {
        debug!("LookupColor: unknown color '{}', defaulting to black", name);
        (0, 0, 0)
    });

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
    reply.extend(std::iter::repeat(0).take(12)); // padding to 32

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

pub(crate) fn read_u16(conn: &ClientConnection, data: &[u8]) -> u16 {
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

pub(crate) fn read_u32(conn: &ClientConnection, data: &[u8]) -> u32 {
    match conn.byte_order {
        ByteOrder::BigEndian => u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
        ByteOrder::LittleEndian => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
    }
}

pub(crate) fn write_u16_to(conn: &ClientConnection, buf: &mut Vec<u8>, val: u16) {
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
    // [28..30] = x (hotspot), [30..32] = y (hotspot)
    if data.len() < 32 { return Ok(()); }
    let cid = read_u32(conn, &data[4..8]);
    let source_pixmap = read_u32(conn, &data[8..12]);
    let fore_red = read_u16(conn, &data[16..18]);
    let fore_green = read_u16(conn, &data[18..20]);
    let fore_blue = read_u16(conn, &data[20..22]);
    let back_red = read_u16(conn, &data[22..24]);
    let back_green = read_u16(conn, &data[24..26]);
    let back_blue = read_u16(conn, &data[26..28]);
    let hotspot_x = read_u16(conn, &data[28..30]);
    let hotspot_y = read_u16(conn, &data[30..32]);

    // Infer macOS cursor type from source pixmap dimensions + hotspot
    let macos_type = infer_pixmap_cursor_type(server, source_pixmap, hotspot_x, hotspot_y);

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
        macos_type,
    };
    server.resources.insert(cid, super::resources::Resource::Cursor(Arc::new(cursor)));
    debug!("CreateCursor: id=0x{:08X} pixmap=0x{:08X} hotspot=({},{}) macos_type={}", cid, source_pixmap, hotspot_x, hotspot_y, macos_type);
    Ok(())
}

/// Infer macOS cursor type from pixmap cursor dimensions + hotspot.
/// Heuristic based on cursor image shape and hotspot position.
fn infer_pixmap_cursor_type(server: &Arc<XServer>, source_pixmap: u32, hotspot_x: u16, hotspot_y: u16) -> u8 {
    // Get pixmap dimensions
    let (w, h) = if let Some(res) = server.resources.get(&source_pixmap) {
        if let super::resources::Resource::Pixmap(pix) = res.value() {
            let p = pix.read();
            (p.width as i32, p.height as i32)
        } else { return 0; }
    } else { return 0; };

    let hx = hotspot_x as i32;
    let hy = hotspot_y as i32;
    let cx = w / 2;
    let cy = h / 2;

    // IBeam: typically narrow and tall (width < height/2), hotspot near center
    if w > 0 && h > 0 && w * 3 < h && (hx - cx).abs() <= 2 {
        return 1; // IBeam
    }

    // Hotspot near center → resize or move cursor
    let center_threshold = w.max(h) / 4;
    let hotspot_centered = (hx - cx).abs() <= center_threshold && (hy - cy).abs() <= center_threshold;

    if hotspot_centered && w > 2 && h > 2 {
        // Wide cursor with centered hotspot → horizontal resize
        if w > h + h / 3 {
            return 6; // ResizeLeftRight
        }
        // Tall cursor with centered hotspot → vertical resize
        if h > w + w / 3 {
            return 7; // ResizeUpDown
        }
        // Square with centered hotspot: could be crosshair, move, or resize
        // Check if it's a move/fleur cursor (centered hotspot, square-ish)
        if (w - h).abs() <= 4 && w >= 16 {
            return 4; // OpenHand (move)
        }
        return 2; // Crosshair (generic centered)
    }

    // Hotspot at top-left quadrant → arrow or hand
    if hx < cx && hy < cy {
        // Hand cursor: hotspot slightly offset from top-left corner
        if hx > 2 && hy <= 4 && w >= 16 {
            return 3; // PointingHand
        }
        return 0; // Arrow
    }

    0 // Default: Arrow
}

async fn handle_create_glyph_cursor<S: AsyncRead + AsyncWrite + Unpin>(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut S,
) -> Result<(), ServerError> {
    // CreateGlyphCursor: opcode 94
    // X11 protocol layout:
    // data[4..8] = cursor ID, [8..12] = source font, [12..16] = mask font
    // [16..18] = source char, [18..20] = mask char
    // [20..22] = fore-red, [22..24] = fore-green, [24..26] = fore-blue
    // [26..28] = back-red, [28..30] = back-green, [30..32] = back-blue
    if data.len() < 32 { return Ok(()); }
    let cid = read_u32(conn, &data[4..8]);
    let source_font = read_u32(conn, &data[8..12]);
    let mask_font = read_u32(conn, &data[12..16]);
    let source_char = read_u16(conn, &data[16..18]);
    let mask_char = read_u16(conn, &data[18..20]);
    let fore_red = read_u16(conn, &data[20..22]);
    let fore_green = read_u16(conn, &data[22..24]);
    let fore_blue = read_u16(conn, &data[24..26]);
    let back_red = read_u16(conn, &data[26..28]);
    let back_green = read_u16(conn, &data[28..30]);
    let back_blue = read_u16(conn, &data[30..32]);

    let macos_type = crate::cursor::x11_cursor_glyph_to_macos(source_char) as u8;
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
        macos_type,
    };
    server.resources.insert(cid, super::resources::Resource::Cursor(Arc::new(cursor)));
    debug!("CreateGlyphCursor: id=0x{:08X} font=0x{:08X} char={} macos_type={}", cid, source_font, source_char, macos_type);
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

pub(crate) fn write_u32_to(conn: &ClientConnection, buf: &mut Vec<u8>, val: u32) {
    match conn.byte_order {
        ByteOrder::BigEndian => buf.extend_from_slice(&val.to_be_bytes()),
        ByteOrder::LittleEndian => buf.extend_from_slice(&val.to_le_bytes()),
    }
}

pub fn write_u32_at(conn: &ClientConnection, buf: &mut [u8], offset: usize, val: u32) {
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

/// Look up an X11 color name from the system rgb.txt database.
/// Returns 16-bit (r, g, b) values suitable for X11 LookupColor/AllocNamedColor replies.
fn lookup_x11_color(name: &str) -> Option<(u16, u16, u16)> {
    use std::sync::OnceLock;
    use std::collections::HashMap;

    static COLOR_DB: OnceLock<HashMap<String, (u16, u16, u16)>> = OnceLock::new();

    let db = COLOR_DB.get_or_init(|| {
        let mut map = HashMap::new();
        let paths = [
            "/opt/X11/share/X11/rgb.txt",
            "/usr/share/X11/rgb.txt",
            "/etc/X11/rgb.txt",
        ];
        for path in &paths {
            if let Ok(content) = std::fs::read_to_string(path) {
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('!') || line.starts_with('#') {
                        continue;
                    }
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 4 {
                        if let (Ok(r), Ok(g), Ok(b)) = (
                            parts[0].parse::<u8>(),
                            parts[1].parse::<u8>(),
                            parts[2].parse::<u8>(),
                        ) {
                            let color_name = parts[3..].join(" ").to_lowercase();
                            // X11 colors are 16-bit: scale 8-bit to 16-bit
                            let r16 = (r as u16) << 8 | r as u16;
                            let g16 = (g as u16) << 8 | g as u16;
                            let b16 = (b as u16) << 8 | b as u16;
                            map.insert(color_name, (r16, g16, b16));
                        }
                    }
                }
                if !map.is_empty() {
                    break;
                }
            }
        }
        map
    });

    db.get(name).copied()
}

fn parse_xlfd_metrics(name: &str) -> (i16, i16, i16) {
    let default = (10i16, 3i16, 6i16);
    if !name.starts_with('-') {
        return default;
    }
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() < 14 {
        return default;
    }
    let pixel_size: i16 = parts[7].parse().unwrap_or(13);
    let avg_width: i16 = parts[12].parse().unwrap_or(60);
    let char_width = (avg_width + 5) / 10;
    let ascent = (pixel_size * 3 + 2) / 4;
    let descent = pixel_size - ascent;
    (ascent, descent, char_width)
}

