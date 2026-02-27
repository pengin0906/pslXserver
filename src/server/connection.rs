use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use log::{debug, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

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
pub async fn handle_connection(
    server: Arc<XServer>,
    mut stream: UnixStream,
    conn_id: u32,
) -> Result<(), ServerError> {
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

    let conn = Arc::new(ClientConnection {
        id: conn_id,
        resource_id_base,
        resource_id_mask,
        byte_order,
        sequence_number: AtomicU16::new(1),
    });

    // Register the connection
    server.connections.insert(conn_id, Arc::clone(&conn));

    // Build and send the connection setup reply
    let reply = build_setup_reply(&server, &conn);
    stream.write_all(&reply).await?;

    info!("Connection {} setup complete (resource base: 0x{:08X})", conn_id, resource_id_base);

    // Phase 2: Main request processing loop
    let mut buf = vec![0u8; 65536];
    let mut pending = Vec::new();

    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            // Connection closed
            break;
        }

        pending.extend_from_slice(&buf[..n]);

        // Process all complete requests in the buffer
        while pending.len() >= 4 {
            let request_len = {
                let len_bytes = &pending[2..4];
                let len = match byte_order {
                    ByteOrder::BigEndian => u16::from_be_bytes([len_bytes[0], len_bytes[1]]),
                    ByteOrder::LittleEndian => u16::from_le_bytes([len_bytes[0], len_bytes[1]]),
                };
                // Length is in 4-byte units
                if len == 0 {
                    // BIG-REQUESTS: length 0 means next 4 bytes are actual length
                    if pending.len() < 8 {
                        break; // Need more data
                    }
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

            if pending.len() < request_len {
                break; // Need more data for this request
            }

            // Extract the complete request
            let request_data: Vec<u8> = pending.drain(..request_len).collect();
            let _seq = conn.next_sequence();

            // Dispatch the request
            let opcode = request_data[0];
            debug!("Request opcode={} len={} bytes", opcode, request_data.len());
            let seq = conn.current_request_sequence();
            match handle_request(&server, &conn, opcode, &request_data, &mut stream).await {
                Ok(()) => {}
                Err(ServerError::NotImplemented) => {
                    debug!("Unimplemented opcode: {} — sending Implementation error", opcode);
                    // Send X11 error reply: Implementation (17)
                    let mut err = vec![0u8; 32];
                    err[0] = 0; // Error
                    err[1] = 17; // Implementation error code
                    err[2] = (seq & 0xFF) as u8;
                    err[3] = ((seq >> 8) & 0xFF) as u8;
                    // bytes 4-7: resource ID (0)
                    err[8] = (opcode as u16 & 0xFF) as u8; // minor opcode
                    err[9] = 0;
                    err[10] = opcode; // major opcode
                    let _ = stream.write_all(&err).await;
                }
                Err(e) => {
                    warn!("Request error (opcode {}): {}", opcode, e);
                }
            }
        }
    }

    // Clean up
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
async fn handle_request(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    opcode: u8,
    data: &[u8],
    stream: &mut UnixStream,
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
        38 => handle_grab_server(server, conn, data, stream).await,
        39 => handle_ungrab_server(server, conn, data, stream).await,
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
        62 => handle_poly_fill_rectangle(server, conn, data, stream).await,
        72 => handle_put_image(server, conn, data, stream).await,
        97 => handle_query_best_size(server, conn, data, stream).await,
        98 => handle_query_extension(server, conn, data, stream).await,
        99 => handle_list_extensions(server, conn, data, stream).await,
        101 => handle_get_keyboard_mapping(server, conn, data, stream).await,
        102 => handle_change_keyboard_mapping(server, conn, data, stream).await,
        103 => handle_get_keyboard_control(server, conn, data, stream).await,
        108 => handle_get_screen_saver(server, conn, data, stream).await,
        115 => handle_set_close_down_mode(server, conn, data, stream).await,
        119 => handle_get_modifier_mapping(server, conn, data, stream).await,
        _ => {
            debug!("Unimplemented opcode: {}", opcode);
            Err(ServerError::NotImplemented)
        }
    }
}

// --- Request Handlers ---

async fn handle_intern_atom(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_get_atom_name(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_create_window(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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

async fn handle_change_window_attributes(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }
    let wid = read_u32(conn, &data[4..8]);
    let value_mask = read_u32(conn, &data[8..12]);

    if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let mut w = win.write();
            let mut offset = 12;

            if value_mask & 0x0002 != 0 { // BackPixel
                w.background_pixel = Some(read_u32(conn, &data[offset..offset+4]));
                offset += 4;
            }
            if value_mask & 0x0008 != 0 { // BorderPixel
                w.border_pixel = Some(read_u32(conn, &data[offset..offset+4]));
                offset += 4;
            }
            if value_mask & 0x0800 != 0 { // EventMask
                let mask = read_u32(conn, &data[offset..offset+4]);
                // Update or add this client's event selection
                if let Some(entry) = w.event_selections.iter_mut().find(|(cid, _)| *cid == conn.id) {
                    entry.1 = mask;
                } else {
                    w.event_selections.push((conn.id, mask));
                }
                let _ = offset;
            }
            // TODO: parse remaining value_mask bits
        }
    }
    Ok(())
}

async fn handle_get_window_attributes(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut UnixStream,
) -> Result<(), ServerError> {
    let wid = read_u32(conn, &data[4..8]);
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

async fn handle_destroy_window(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    let wid = read_u32(conn, &data[4..8]);
    debug!("DestroyWindow: 0x{:08X}", wid);
    server.resources.remove(&wid);
    Ok(())
}

async fn handle_map_window(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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
        }
    }
    Ok(())
}

async fn handle_unmap_window(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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

async fn handle_configure_window(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    if data.len() < 12 {
        return Err(ServerError::Protocol);
    }
    let wid = read_u32(conn, &data[4..8]);
    let value_mask = read_u16(conn, &data[8..10]);

    if let Some(res) = server.resources.get(&wid) {
        if let super::resources::Resource::Window(win) = res.value() {
            let mut w = win.write();
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
        }
    }
    Ok(())
}

async fn handle_change_property(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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

async fn handle_get_property(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_query_tree(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_get_geometry(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut UnixStream,
) -> Result<(), ServerError> {
    let drawable = read_u32(conn, &data[4..8]);
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

async fn handle_get_input_focus(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut UnixStream,
) -> Result<(), ServerError> {
    let seq = conn.current_request_sequence();
    let mut reply = Vec::with_capacity(32);
    reply.push(1); // reply
    reply.push(1); // revert-to (PointerRoot)
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, 0);
    write_u32_to(conn, &mut reply, 1); // focus window (PointerRoot)
    reply.extend(std::iter::repeat(0).take(20));
    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_create_gc(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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

async fn handle_change_gc(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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

async fn handle_free_gc(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    let gcid = read_u32(conn, &data[4..8]);
    server.resources.remove(&gcid);
    Ok(())
}

async fn handle_poly_fill_rectangle(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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

    // Send render commands to the display backend
    if let Some(res) = server.resources.get(&drawable) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            if let Some(ref handle) = w.native_window {
                let _ = server.display_cmd_tx.send(
                    crate::display::DisplayCommand::RenderBatch {
                        handle: handle.clone(),
                        commands,
                    },
                );
            }
        }
    }
    Ok(())
}

async fn handle_put_image(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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

    if let Some(res) = server.resources.get(&drawable) {
        if let super::resources::Resource::Window(win) = res.value() {
            let w = win.read();
            if let Some(ref handle) = w.native_window {
                let _ = server.display_cmd_tx.send(
                    crate::display::DisplayCommand::RenderBatch {
                        handle: handle.clone(),
                        commands: vec![command],
                    },
                );
            }
        }
    }
    Ok(())
}

async fn handle_query_extension(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_create_pixmap(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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

async fn handle_free_pixmap(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    let pid = read_u32(conn, &data[4..8]);
    server.resources.remove(&pid);
    Ok(())
}

async fn handle_open_font(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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
        ascent: 12,
        descent: 3,
        default_char: 0,
        min_char_width: 6,
        max_char_width: 8,
    };
    server.resources.insert(fid, super::resources::Resource::Font(
        Arc::new(parking_lot::RwLock::new(font)),
    ));
    Ok(())
}

async fn handle_close_font(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    let fid = read_u32(conn, &data[4..8]);
    server.resources.remove(&fid);
    Ok(())
}

async fn handle_query_font(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_map_subwindows(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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
                win.write().mapped = true;
                win.write().viewable = true;
            }
        }
    }
    Ok(())
}

async fn handle_delete_property(
    server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    _stream: &mut UnixStream,
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

async fn handle_set_input_focus(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    // No reply needed
    Ok(())
}

// --- Additional request handlers for xdpyinfo compatibility ---

async fn handle_query_best_size(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_list_extensions(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_get_keyboard_mapping(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    data: &[u8],
    stream: &mut UnixStream,
) -> Result<(), ServerError> {
    let first_keycode = data[1] as u32;
    let count = data[4] as u32;
    let keysyms_per_keycode: u32 = 4;
    let seq = conn.current_request_sequence();

    // Generate basic keysym mapping
    let total_keysyms = count * keysyms_per_keycode;
    let additional_len = total_keysyms; // in 4-byte units

    let mut reply = Vec::with_capacity(32 + (total_keysyms as usize * 4));
    reply.push(1); // reply
    reply.push(keysyms_per_keycode as u8);
    write_u16_to(conn, &mut reply, seq);
    write_u32_to(conn, &mut reply, additional_len);
    reply.extend(std::iter::repeat(0).take(24)); // padding

    // Generate simple keysym data
    for i in 0..count {
        let keycode = first_keycode + i;
        // Map keycodes to basic Latin keysyms (keycode 8 = physical key 0)
        let keysym = match keycode {
            10..=19 => (keycode - 10 + 0x31) as u32, // 1-0
            24..=33 => (keycode - 24 + 0x71) as u32, // q-p
            38..=46 => (keycode - 38 + 0x61) as u32, // a-l
            52..=58 => (keycode - 52 + 0x7A) as u32, // z-m (approximate)
            65 => 0x0020,    // space
            9 => 0xFF1B,     // Escape
            36 => 0xFF0D,    // Return
            22 => 0xFF08,    // BackSpace
            23 => 0xFF09,    // Tab
            _ => 0, // NoSymbol
        };

        // keysym, shifted keysym, mode switch, mode+shift
        write_u32_to(conn, &mut reply, keysym);
        write_u32_to(conn, &mut reply, keysym); // shifted (simplified)
        write_u32_to(conn, &mut reply, 0); // NoSymbol
        write_u32_to(conn, &mut reply, 0); // NoSymbol
    }

    stream.write_all(&reply).await?;
    Ok(())
}

async fn handle_change_keyboard_mapping(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    // No reply needed for ChangeKeyboardMapping
    Ok(())
}

async fn handle_get_keyboard_control(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_get_screen_saver(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_set_close_down_mode(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    // No reply needed
    Ok(())
}

async fn handle_get_modifier_mapping(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut UnixStream,
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

async fn handle_grab_server(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    // No reply needed
    Ok(())
}

async fn handle_ungrab_server(
    _server: &Arc<XServer>,
    _conn: &Arc<ClientConnection>,
    _data: &[u8],
    _stream: &mut UnixStream,
) -> Result<(), ServerError> {
    // No reply needed
    Ok(())
}

async fn handle_list_fonts(
    _server: &Arc<XServer>,
    conn: &Arc<ClientConnection>,
    _data: &[u8],
    stream: &mut UnixStream,
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
