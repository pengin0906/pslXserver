// XIM (X Input Method) protocol implementation.
// This provides inline preedit display for GTK/Electron apps that use XIM
// instead of processing raw KeyPress events (which get garbled by ibus).
//
// Transport: ClientMessage-based (_XIM_XCONNECT, _XIM_PROTOCOL, _XIM_MOREDATA)
// Reference: https://www.x.org/releases/X11R7.6/doc/libX11/specs/XIM/xim.html

use std::sync::atomic::{AtomicU16, Ordering};

use dashmap::DashMap;
use log::{debug, info, warn};

use crate::display::Xid;
use super::atoms::AtomTable;
use super::connection::{ClientConnection, ByteOrder};
use super::XServer;

// XIM protocol opcodes
const XIM_CONNECT: u8 = 1;
const XIM_CONNECT_REPLY: u8 = 2;
const XIM_DISCONNECT: u8 = 3;
const XIM_DISCONNECT_REPLY: u8 = 4;
const XIM_OPEN: u8 = 30;
const XIM_OPEN_REPLY: u8 = 31;
const XIM_CLOSE: u8 = 32;
const XIM_CLOSE_REPLY: u8 = 33;
const XIM_QUERY_EXTENSION: u8 = 40;
const XIM_QUERY_EXTENSION_REPLY: u8 = 41;
const XIM_ENCODING_NEGOTIATION: u8 = 38;
const XIM_ENCODING_NEGOTIATION_REPLY: u8 = 39;
const XIM_SET_IM_VALUES: u8 = 42;
const XIM_SET_IM_VALUES_REPLY: u8 = 43;
const XIM_GET_IM_VALUES: u8 = 44;
const XIM_GET_IM_VALUES_REPLY: u8 = 45;
const XIM_CREATE_IC: u8 = 50;
const XIM_CREATE_IC_REPLY: u8 = 51;
const XIM_DESTROY_IC: u8 = 52;
const XIM_DESTROY_IC_REPLY: u8 = 53;
const XIM_SET_IC_VALUES: u8 = 54;
const XIM_SET_IC_VALUES_REPLY: u8 = 55;
const XIM_GET_IC_VALUES: u8 = 56;
const XIM_GET_IC_VALUES_REPLY: u8 = 57;
const XIM_SET_IC_FOCUS: u8 = 58;
const XIM_UNSET_IC_FOCUS: u8 = 59;
const XIM_FORWARD_EVENT: u8 = 60;
const XIM_SYNC: u8 = 61;
const XIM_SYNC_REPLY: u8 = 62;
const XIM_COMMIT: u8 = 63;
const XIM_RESET_IC: u8 = 64;
const XIM_RESET_IC_REPLY: u8 = 65;
const XIM_SET_EVENT_MASK: u8 = 70;
const XIM_PREEDIT_START: u8 = 73;
const XIM_PREEDIT_START_REPLY: u8 = 74;
const XIM_PREEDIT_DRAW: u8 = 75;
const XIM_PREEDIT_CARET: u8 = 76;
const XIM_PREEDIT_DONE: u8 = 78;

// XIM commit flags
const XIM_LOOKUP_CHARS: u16 = 0x0002;
const XIM_LOOKUP_KEYSYM: u16 = 0x0004;

// Input styles
const XIM_PREEDIT_CALLBACKS: u32 = 0x0002;
const XIM_PREEDIT_NOTHING: u32 = 0x0008;
const XIM_STATUS_NOTHING: u32 = 0x0400;

// IC attribute IDs we assign
const IC_ATTR_INPUT_STYLE: u16 = 0;
const IC_ATTR_CLIENT_WINDOW: u16 = 1;
const IC_ATTR_FOCUS_WINDOW: u16 = 2;
const IC_ATTR_SPOT_LOCATION: u16 = 3;

// IM attribute IDs
const IM_ATTR_QUERY_INPUT_STYLE: u16 = 0;

/// Atoms needed for XIM transport.
pub struct XimAtoms {
    pub xim_servers: u32,      // XIM_SERVERS
    pub xim_xconnect: u32,     // _XIM_XCONNECT
    pub xim_protocol: u32,     // _XIM_PROTOCOL
    pub xim_moredata: u32,     // _XIM_MOREDATA
    pub server_atom: u32,      // @server=pslx
    pub locales: u32,          // LOCALES
    pub transport: u32,        // TRANSPORT
}

/// Per-client XIM connection state.
struct XimConnection {
    im_id: u16,
    client_comm_window: Xid,
    /// Connection ID in our XServer.connections table.
    conn_id: u32,
    byte_order: ByteOrder,
    /// Input contexts for this connection.
    contexts: DashMap<u16, XimInputContext>,
}

/// XIM Input Context.
struct XimInputContext {
    ic_id: u16,
    im_id: u16,
    client_window: Xid,
    focus_window: Xid,
    input_style: u32,
}

/// The XIM server.
pub struct XimServer {
    pub atoms: XimAtoms,
    /// IM connections keyed by im_id.
    connections: DashMap<u16, XimConnection>,
    /// Map from client comm window to im_id (for routing incoming ClientMessages).
    comm_to_im: DashMap<Xid, u16>,
    /// Our server communication window (we use the root window).
    pub server_comm_window: Xid,
    next_im_id: AtomicU16,
    next_ic_id: AtomicU16,
}

impl XimServer {
    pub fn new(atoms: &AtomTable, root_window: Xid) -> Self {
        let xim_atoms = XimAtoms {
            xim_servers: atoms.intern_or_create("XIM_SERVERS"),
            xim_xconnect: atoms.intern_or_create("_XIM_XCONNECT"),
            xim_protocol: atoms.intern_or_create("_XIM_PROTOCOL"),
            xim_moredata: atoms.intern_or_create("_XIM_MOREDATA"),
            server_atom: atoms.intern_or_create("@server=pslx"),
            locales: atoms.intern_or_create("LOCALES"),
            transport: atoms.intern_or_create("TRANSPORT"),
        };

        info!("XIM server initialized: XIM_SERVERS={}, _XIM_XCONNECT={}, _XIM_PROTOCOL={}, @server=pslx={}",
            xim_atoms.xim_servers, xim_atoms.xim_xconnect, xim_atoms.xim_protocol, xim_atoms.server_atom);

        XimServer {
            atoms: xim_atoms,
            connections: DashMap::new(),
            comm_to_im: DashMap::new(),
            server_comm_window: root_window,
            next_im_id: AtomicU16::new(1),
            next_ic_id: AtomicU16::new(1),
        }
    }

    /// Check if a SelectionRequest is for our XIM selection and handle it.
    /// Returns true if handled.
    pub fn handle_selection_request(
        &self,
        _owner: Xid,
        requestor: Xid,
        selection: u32,
        target: u32,
        property: u32,
        conn: &ClientConnection,
        server: &XServer,
    ) -> bool {
        if selection != self.atoms.server_atom {
            return false;
        }

        debug!("XIM: SelectionRequest for @server=pslx, target={}, requestor=0x{:08x}", target, requestor);

        if target == self.atoms.locales {
            // Reply with supported locales
            let locale_data = b"@locale=C,en_US.UTF-8,ja_JP.UTF-8,ja_JP.utf8\0";
            self.set_property(server, requestor, property, self.atoms.locales, locale_data);
        } else if target == self.atoms.transport {
            // Reply with transport — only X/ (ClientMessage based)
            let transport_data = b"@transport=X/\0";
            self.set_property(server, requestor, property, self.atoms.transport, transport_data);
        } else {
            // Unknown target, set property to empty
            self.set_property(server, requestor, property, target, b"");
        }

        // Send SelectionNotify
        self.send_selection_notify(server, requestor, selection, target, property, conn);
        true
    }

    /// Handle _XIM_XCONNECT ClientMessage (initial XIM transport handshake).
    pub fn handle_xconnect(
        &self,
        client_comm_window: Xid,
        conn: &ClientConnection,
        _server: &XServer,
    ) {
        info!("XIM: _XIM_XCONNECT from client comm window 0x{:08x}, conn_id={}", client_comm_window, conn.id);

        let im_id = self.next_im_id.fetch_add(1, Ordering::Relaxed);

        self.connections.insert(im_id, XimConnection {
            im_id,
            client_comm_window,
            conn_id: conn.id,
            byte_order: conn.byte_order,
            contexts: DashMap::new(),
        });
        self.comm_to_im.insert(client_comm_window, im_id);

        // Reply with _XIM_XCONNECT ClientMessage to client:
        // data.l[0] = server_comm_window
        // data.l[1] = major transport version (0)
        // data.l[2] = minor transport division (0) — means only-CM transport
        let reply = build_client_message_32(
            self.atoms.xim_xconnect,
            client_comm_window,
            &[self.server_comm_window, 0, 0, 0, 0],
            &conn.byte_order,
        );
        let _ = conn.event_tx.send(reply.into());
        info!("XIM: sent _XIM_XCONNECT reply to 0x{:08x}", client_comm_window);
    }

    /// Handle _XIM_PROTOCOL routed by conn_id (xterm sets ClientMessage window=server_comm_window, not client_comm).
    pub fn handle_protocol_message_by_conn(
        &self,
        conn_id: u32,
        event_data: &[u8],
        conn: &ClientConnection,
        server: &XServer,
    ) {
        // Find the XIM connection for this X11 connection
        let im_id = match self.connections.iter().find(|e| e.conn_id == conn_id) {
            Some(e) => e.im_id,
            None => {
                debug!("XIM: protocol message from conn {} with no XIM connection", conn_id);
                return;
            }
        };
        self.dispatch_xim_message(im_id, event_data, conn, server);
    }

    /// Handle _XIM_PROTOCOL or _XIM_MOREDATA ClientMessage.
    pub fn handle_protocol_message(
        &self,
        client_comm_window: Xid,
        _message_type: u32,
        event_data: &[u8],
        conn: &ClientConnection,
        server: &XServer,
    ) {
        let im_id = match self.comm_to_im.get(&client_comm_window) {
            Some(id) => *id,
            None => {
                debug!("XIM: protocol message from unknown comm window 0x{:08x}", client_comm_window);
                return;
            }
        };

        // For ClientMessage format=8, the 20-byte data is the XIM message directly.
        // For multi-message transport, _XIM_MOREDATA is followed by _XIM_PROTOCOL.
        // For now, handle single-ClientMessage messages (up to 20 bytes).

        // The XIM protocol message format:
        // byte 0: major opcode
        // byte 1: minor opcode
        // bytes 2-3: length in 4-byte units (of data following this header)
        // bytes 4+: data

        self.dispatch_xim_message(im_id, event_data, conn, server);
    }

    fn dispatch_xim_message(&self, im_id: u16, event_data: &[u8], conn: &ClientConnection, server: &XServer) {
        if event_data.len() < 4 {
            warn!("XIM: protocol message too short ({} bytes)", event_data.len());
            return;
        }

        let major_opcode = event_data[0];
        let _minor_opcode = event_data[1];
        let _length = u16::from_le_bytes([event_data[2], event_data[3]]);

        debug!("XIM: opcode={} from im_id={}", major_opcode, im_id);

        match major_opcode {
            XIM_CONNECT => self.handle_xim_connect(im_id, event_data, conn),
            XIM_DISCONNECT => self.handle_xim_disconnect(im_id, conn),
            XIM_OPEN => self.handle_xim_open(im_id, event_data, conn),
            XIM_CLOSE => self.handle_xim_close(im_id, event_data, conn),
            XIM_QUERY_EXTENSION => self.handle_query_extension(im_id, event_data, conn),
            XIM_ENCODING_NEGOTIATION => self.handle_encoding_negotiation(im_id, event_data, conn),
            XIM_SET_IM_VALUES => self.handle_set_im_values(im_id, event_data, conn),
            XIM_GET_IM_VALUES => self.handle_get_im_values(im_id, event_data, conn),
            XIM_CREATE_IC => self.handle_create_ic(im_id, event_data, conn, server),
            XIM_DESTROY_IC => self.handle_destroy_ic(im_id, event_data, conn),
            XIM_SET_IC_VALUES => self.handle_set_ic_values(im_id, event_data, conn),
            XIM_GET_IC_VALUES => self.handle_get_ic_values(im_id, event_data, conn),
            XIM_SET_IC_FOCUS => self.handle_set_ic_focus(im_id, event_data),
            XIM_UNSET_IC_FOCUS => { debug!("XIM: UnsetICFocus im_id={}", im_id); }
            XIM_FORWARD_EVENT => self.handle_forward_event(im_id, event_data, conn, server),
            XIM_SYNC_REPLY => { debug!("XIM: SyncReply im_id={}", im_id); }
            XIM_RESET_IC => self.handle_reset_ic(im_id, event_data, conn),
            XIM_PREEDIT_START_REPLY => { debug!("XIM: PreeditStartReply im_id={}", im_id); }
            _ => {
                info!("XIM: unhandled opcode {} from im_id={}", major_opcode, im_id);
            }
        }
    }

    // --- XIM message handlers ---

    fn handle_xim_connect(&self, im_id: u16, data: &[u8], conn: &ClientConnection) {
        // XIM_CONNECT: [0]=1 [1]=0 [2-3]=len
        // [4]=byte_order(0x42='B',0x6c='l') [5]=unused
        // [6-7]=client_major [8-9]=client_minor
        // [10-11]=num_auth
        info!("XIM: XIM_CONNECT from im_id={}", im_id);

        if data.len() > 4 {
            let byte_order_flag = data[4];
            debug!("XIM: client byte order: 0x{:02x}", byte_order_flag);
            // Update connection byte order based on XIM client preference
            if let Some(mut xim_conn) = self.connections.get_mut(&im_id) {
                xim_conn.byte_order = if byte_order_flag == 0x42 {
                    ByteOrder::BigEndian
                } else {
                    ByteOrder::LittleEndian
                };
            }
        }

        // XIM_CONNECT_REPLY: server_major=1, server_minor=0
        let reply = [
            XIM_CONNECT_REPLY, 0, // major, minor opcode
            1, 0, // length = 1 (4 bytes)
            1, 0, // server_major_protocol_version = 1
            0, 0, // server_minor_protocol_version = 0
        ];
        self.send_xim_message(im_id, &reply, conn);
    }

    fn handle_xim_disconnect(&self, im_id: u16, conn: &ClientConnection) {
        info!("XIM: XIM_DISCONNECT from im_id={}", im_id);
        let reply = [XIM_DISCONNECT_REPLY, 0, 0, 0];
        self.send_xim_message(im_id, &reply, conn);
        // Clean up
        if let Some((_, xim_conn)) = self.connections.remove(&im_id) {
            self.comm_to_im.remove(&xim_conn.client_comm_window);
        }
    }

    fn handle_xim_open(&self, im_id: u16, data: &[u8], conn: &ClientConnection) {
        // XIM_OPEN: [4]=locale_name_length [5..]=locale name
        let locale = if data.len() > 5 {
            let len = data[4] as usize;
            let end = std::cmp::min(5 + len, data.len());
            String::from_utf8_lossy(&data[5..end]).to_string()
        } else {
            String::from("C")
        };
        info!("XIM: XIM_OPEN locale='{}' im_id={}", locale, im_id);

        // XIM_OPEN_REPLY:
        // [0-3] header: opcode=31, minor=0, length
        // [4-5] input-method-ID
        // [6-7] length of IM attributes supported (LISTofXIMATTR)
        // IM attributes: InputStyle (id=0, type=XimType_XIMStyles=15, name="queryInputStyle")
        // Then IC attributes list

        let im_attr_name = b"queryInputStyle";
        // XIMAttr: [0-1]=attr_id [2-3]=type [4-5]=name_len [6..]=name + pad
        // The TOTAL entry (6 + name_len + pad) must be 4-byte aligned.
        let im_attr_pad = (4 - (6 + im_attr_name.len()) % 4) % 4;
        let im_attr_entry_len = 6 + im_attr_name.len() + im_attr_pad;
        let im_attr_list_len = im_attr_entry_len;

        // IC attributes we support
        // Type values from libX11 imExt.h: CARD32=0x000A, Window=3, XPoint=6, XIMStyles=0x000D
        let ic_attrs: &[(u16, u16, &[u8])] = &[
            (IC_ATTR_INPUT_STYLE, 0x000A, b"inputStyle"),     // XimType_CARD32
            (IC_ATTR_CLIENT_WINDOW, 3, b"clientWindow"),       // XimType_Window
            (IC_ATTR_FOCUS_WINDOW, 3, b"focusWindow"),         // XimType_Window
            (IC_ATTR_SPOT_LOCATION, 6, b"spotLocation"),       // XimType_XPoint
        ];

        let mut ic_attr_data = Vec::new();
        for (id, type_id, name) in ic_attrs {
            // Total entry (6 + name_len + pad) must be 4-byte aligned
            let pad = (4 - (6 + name.len()) % 4) % 4;
            ic_attr_data.extend_from_slice(&id.to_le_bytes());
            ic_attr_data.extend_from_slice(&type_id.to_le_bytes());
            ic_attr_data.extend_from_slice(&(name.len() as u16).to_le_bytes());
            ic_attr_data.extend_from_slice(name);
            for _ in 0..pad {
                ic_attr_data.push(0);
            }
        }

        let body_len = 2 + 2 + im_attr_list_len + 2 + 2 + ic_attr_data.len();
        let padded_body = (body_len + 3) & !3;
        let length_words = padded_body / 4;

        let mut reply = Vec::with_capacity(4 + padded_body);
        reply.push(XIM_OPEN_REPLY);
        reply.push(0);
        reply.extend_from_slice(&(length_words as u16).to_le_bytes());

        // input-method-ID
        reply.extend_from_slice(&im_id.to_le_bytes());

        // IM attributes list length (in bytes)
        reply.extend_from_slice(&(im_attr_list_len as u16).to_le_bytes());

        // IM attribute: queryInputStyle
        reply.extend_from_slice(&IM_ATTR_QUERY_INPUT_STYLE.to_le_bytes()); // attr_id = 0
        reply.extend_from_slice(&0x000Du16.to_le_bytes()); // type = XimType_XIMStyles
        reply.extend_from_slice(&(im_attr_name.len() as u16).to_le_bytes());
        reply.extend_from_slice(im_attr_name);
        for _ in 0..im_attr_pad {
            reply.push(0);
        }

        // IC attributes list length (in bytes)
        reply.extend_from_slice(&(ic_attr_data.len() as u16).to_le_bytes());
        // pad after length
        reply.extend_from_slice(&[0, 0]);

        // IC attributes
        reply.extend_from_slice(&ic_attr_data);

        // Pad to 4-byte boundary
        while reply.len() % 4 != 0 {
            reply.push(0);
        }

        self.send_xim_message(im_id, &reply, conn);
        info!("XIM: sent XIM_OPEN_REPLY im_id={}", im_id);
    }

    fn handle_xim_close(&self, im_id: u16, _data: &[u8], conn: &ClientConnection) {
        info!("XIM: XIM_CLOSE im_id={}", im_id);
        let mut reply = vec![XIM_CLOSE_REPLY, 0, 1, 0]; // length=1 word
        reply.extend_from_slice(&im_id.to_le_bytes());
        reply.extend_from_slice(&[0, 0]); // pad
        self.send_xim_message(im_id, &reply, conn);
    }

    fn handle_query_extension(&self, im_id: u16, _data: &[u8], conn: &ClientConnection) {
        debug!("XIM: QueryExtension im_id={}", im_id);
        // Reply with no extensions
        let mut reply = vec![XIM_QUERY_EXTENSION_REPLY, 0, 1, 0]; // length=1 word
        reply.extend_from_slice(&im_id.to_le_bytes());
        reply.extend_from_slice(&0u16.to_le_bytes()); // 0 extensions
        self.send_xim_message(im_id, &reply, conn);
    }

    fn handle_encoding_negotiation(&self, im_id: u16, _data: &[u8], conn: &ClientConnection) {
        debug!("XIM: EncodingNegotiation im_id={}", im_id);
        // Reply: use encoding index 0 (COMPOUND_TEXT) and category 0
        let mut reply = vec![XIM_ENCODING_NEGOTIATION_REPLY, 0, 2, 0]; // length=2 words
        reply.extend_from_slice(&im_id.to_le_bytes());
        reply.extend_from_slice(&0u16.to_le_bytes()); // category = 0 (name)
        reply.extend_from_slice(&0i16.to_le_bytes()); // encoding index = 0
        reply.extend_from_slice(&[0, 0]); // pad
        self.send_xim_message(im_id, &reply, conn);
    }

    fn handle_set_im_values(&self, im_id: u16, _data: &[u8], conn: &ClientConnection) {
        debug!("XIM: SetIMValues im_id={}", im_id);
        let mut reply = vec![XIM_SET_IM_VALUES_REPLY, 0, 1, 0];
        reply.extend_from_slice(&im_id.to_le_bytes());
        reply.extend_from_slice(&[0, 0]); // pad
        self.send_xim_message(im_id, &reply, conn);
    }

    fn handle_get_im_values(&self, im_id: u16, _data: &[u8], conn: &ClientConnection) {
        debug!("XIM: GetIMValues im_id={}", im_id);

        // Parse requested attribute IDs
        // [4-5] = im_id, [6-7] = byte length of attr ID list, [8..] = attr IDs (u16 each)
        // Build reply with queryInputStyle if requested

        // Supported input styles
        let styles: &[u32] = &[
            XIM_PREEDIT_CALLBACKS | XIM_STATUS_NOTHING,  // 0x0402
            XIM_PREEDIT_NOTHING | XIM_STATUS_NOTHING,    // 0x0408
        ];

        // Build the XIMStyles value:
        // [0-1] attr_id=0, [2-3] value_length, [4-5] num_styles, [6-7] pad, [8..] styles
        let styles_data_len = 4 + styles.len() * 4; // count(2) + pad(2) + styles
        let mut attr_value = Vec::new();
        attr_value.extend_from_slice(&IM_ATTR_QUERY_INPUT_STYLE.to_le_bytes());
        attr_value.extend_from_slice(&(styles_data_len as u16).to_le_bytes());
        attr_value.extend_from_slice(&(styles.len() as u16).to_le_bytes());
        attr_value.extend_from_slice(&[0, 0]); // pad
        for &style in styles {
            attr_value.extend_from_slice(&style.to_le_bytes());
        }

        let body_len = 2 + 2 + attr_value.len();
        let padded = (body_len + 3) & !3;

        let mut reply = Vec::with_capacity(4 + padded);
        reply.push(XIM_GET_IM_VALUES_REPLY);
        reply.push(0);
        reply.extend_from_slice(&((padded / 4) as u16).to_le_bytes());
        reply.extend_from_slice(&im_id.to_le_bytes());
        reply.extend_from_slice(&(attr_value.len() as u16).to_le_bytes());
        reply.extend_from_slice(&attr_value);
        while reply.len() % 4 != 0 {
            reply.push(0);
        }

        self.send_xim_message(im_id, &reply, conn);
    }

    fn handle_create_ic(
        &self,
        im_id: u16,
        data: &[u8],
        conn: &ClientConnection,
        _server: &XServer,
    ) {
        // XIM_CREATE_IC: [4-5]=im_id [6-7]=byte_len_of_ic_attrs [8..]=IC attribute list
        let ic_id = self.next_ic_id.fetch_add(1, Ordering::Relaxed);

        let mut input_style: u32 = XIM_PREEDIT_NOTHING | XIM_STATUS_NOTHING;
        let mut client_window: Xid = 0;
        let mut focus_window: Xid = 0;

        // Parse IC attributes: each is [attr_id:u16][value_len:u16][value:...][pad]
        if data.len() > 8 {
            let attr_list_len = u16::from_le_bytes([data[6], data[7]]) as usize;
            let attr_data = &data[8..std::cmp::min(8 + attr_list_len, data.len())];
            let mut pos = 0;
            while pos + 4 <= attr_data.len() {
                let attr_id = u16::from_le_bytes([attr_data[pos], attr_data[pos + 1]]);
                let val_len = u16::from_le_bytes([attr_data[pos + 2], attr_data[pos + 3]]) as usize;
                let val_start = pos + 4;
                let val_end = std::cmp::min(val_start + val_len, attr_data.len());
                let val = &attr_data[val_start..val_end];

                match attr_id {
                    IC_ATTR_INPUT_STYLE if val.len() >= 4 => {
                        input_style = u32::from_le_bytes([val[0], val[1], val[2], val[3]]);
                    }
                    IC_ATTR_CLIENT_WINDOW if val.len() >= 4 => {
                        client_window = u32::from_le_bytes([val[0], val[1], val[2], val[3]]);
                    }
                    IC_ATTR_FOCUS_WINDOW if val.len() >= 4 => {
                        focus_window = u32::from_le_bytes([val[0], val[1], val[2], val[3]]);
                    }
                    _ => {}
                }

                let padded_val = (val_len + 3) & !3;
                pos = val_start + padded_val;
            }
        }

        if focus_window == 0 {
            focus_window = client_window;
        }

        info!("XIM: CreateIC im_id={} ic_id={} input_style=0x{:04x} client=0x{:08x} focus=0x{:08x}",
            im_id, ic_id, input_style, client_window, focus_window);

        if let Some(xim_conn) = self.connections.get(&im_id) {
            xim_conn.contexts.insert(ic_id, XimInputContext {
                ic_id,
                im_id,
                client_window,
                focus_window,
                input_style,
            });
        }

        // XIM_CREATE_IC_REPLY: [4-5]=im_id [6-7]=ic_id
        let mut reply = vec![XIM_CREATE_IC_REPLY, 0, 1, 0]; // length=1 word
        reply.extend_from_slice(&im_id.to_le_bytes());
        reply.extend_from_slice(&ic_id.to_le_bytes());
        self.send_xim_message(im_id, &reply, conn);

        // Send XIM_SET_EVENT_MASK to tell the client which events we want.
        // forward_event_mask = 0 (we don't want the client to forward any key events to us)
        // synchronous_event_mask = 0
        let mut mask_msg = vec![XIM_SET_EVENT_MASK, 0, 3, 0]; // length=3 words (12 bytes)
        mask_msg.extend_from_slice(&im_id.to_le_bytes());
        mask_msg.extend_from_slice(&ic_id.to_le_bytes());
        mask_msg.extend_from_slice(&0u32.to_le_bytes()); // forward_event_mask = 0
        mask_msg.extend_from_slice(&0u32.to_le_bytes()); // synchronous_event_mask = 0
        self.send_xim_message(im_id, &mask_msg, conn);
    }

    fn handle_destroy_ic(&self, im_id: u16, data: &[u8], conn: &ClientConnection) {
        let ic_id = if data.len() > 7 {
            u16::from_le_bytes([data[6], data[7]])
        } else { 0 };
        info!("XIM: DestroyIC im_id={} ic_id={}", im_id, ic_id);

        if let Some(xim_conn) = self.connections.get(&im_id) {
            xim_conn.contexts.remove(&ic_id);
        }

        let mut reply = vec![XIM_DESTROY_IC_REPLY, 0, 1, 0];
        reply.extend_from_slice(&im_id.to_le_bytes());
        reply.extend_from_slice(&ic_id.to_le_bytes());
        self.send_xim_message(im_id, &reply, conn);
    }

    fn handle_set_ic_values(&self, im_id: u16, data: &[u8], conn: &ClientConnection) {
        let ic_id = if data.len() > 7 {
            u16::from_le_bytes([data[6], data[7]])
        } else { 0 };
        debug!("XIM: SetICValues im_id={} ic_id={}", im_id, ic_id);

        // Parse and apply IC attribute values (spot location, focus window, etc.)
        if data.len() > 12 {
            let attr_list_len = u16::from_le_bytes([data[8], data[9]]) as usize;
            let attr_start = 12; // after im_id(2) + ic_id(2) + byte_len(2) + pad(2)
            let attr_data = &data[attr_start..std::cmp::min(attr_start + attr_list_len, data.len())];
            let mut pos = 0;
            while pos + 4 <= attr_data.len() {
                let attr_id = u16::from_le_bytes([attr_data[pos], attr_data[pos + 1]]);
                let val_len = u16::from_le_bytes([attr_data[pos + 2], attr_data[pos + 3]]) as usize;
                let val_start = pos + 4;
                let val_end = std::cmp::min(val_start + val_len, attr_data.len());
                let val = &attr_data[val_start..val_end];

                if let Some(xim_conn) = self.connections.get(&im_id) {
                    if let Some(mut ctx) = xim_conn.contexts.get_mut(&ic_id) {
                        match attr_id {
                            IC_ATTR_FOCUS_WINDOW if val.len() >= 4 => {
                                ctx.focus_window = u32::from_le_bytes([val[0], val[1], val[2], val[3]]);
                            }
                            IC_ATTR_CLIENT_WINDOW if val.len() >= 4 => {
                                ctx.client_window = u32::from_le_bytes([val[0], val[1], val[2], val[3]]);
                            }
                            _ => {}
                        }
                    }
                }

                let padded_val = (val_len + 3) & !3;
                pos = val_start + padded_val;
            }
        }

        let mut reply = vec![XIM_SET_IC_VALUES_REPLY, 0, 1, 0];
        reply.extend_from_slice(&im_id.to_le_bytes());
        reply.extend_from_slice(&ic_id.to_le_bytes());
        self.send_xim_message(im_id, &reply, conn);
    }

    fn handle_get_ic_values(&self, im_id: u16, data: &[u8], conn: &ClientConnection) {
        let ic_id = if data.len() > 7 {
            u16::from_le_bytes([data[6], data[7]])
        } else { 0 };
        debug!("XIM: GetICValues im_id={} ic_id={}", im_id, ic_id);

        // For now, return empty IC attributes
        let mut reply = vec![XIM_GET_IC_VALUES_REPLY, 0, 2, 0]; // length=2 words
        reply.extend_from_slice(&im_id.to_le_bytes());
        reply.extend_from_slice(&ic_id.to_le_bytes());
        reply.extend_from_slice(&0u16.to_le_bytes()); // 0 bytes of attributes
        reply.extend_from_slice(&[0, 0]); // pad
        self.send_xim_message(im_id, &reply, conn);
    }

    fn handle_set_ic_focus(&self, im_id: u16, data: &[u8]) {
        let ic_id = if data.len() > 7 {
            u16::from_le_bytes([data[6], data[7]])
        } else { 0 };
        debug!("XIM: SetICFocus im_id={} ic_id={}", im_id, ic_id);
    }

    fn handle_forward_event(
        &self,
        im_id: u16,
        data: &[u8],
        conn: &ClientConnection,
        _server: &XServer,
    ) {
        // XIM_FORWARD_EVENT: [4-5]=im_id [6-7]=ic_id [8-9]=flag [10-11]=serial [12..44]=event
        if data.len() < 44 {
            debug!("XIM: ForwardEvent too short");
            return;
        }

        let ic_id = u16::from_le_bytes([data[6], data[7]]);
        let flag = u16::from_le_bytes([data[8], data[9]]);
        let event_data = &data[12..44];

        debug!("XIM: ForwardEvent im_id={} ic_id={} flag={} event_type={}", im_id, ic_id, flag, event_data[0]);

        // Forward the key event to the client's event queue directly
        let mut event_buf = [0u8; 32];
        event_buf.copy_from_slice(event_data);
        let _ = conn.event_tx.send(event_buf.into());

        // Send sync reply if synchronous flag is set
        if flag & 0x0001 != 0 {
            let mut sync = vec![XIM_SYNC_REPLY, 0, 1, 0];
            sync.extend_from_slice(&im_id.to_le_bytes());
            sync.extend_from_slice(&ic_id.to_le_bytes());
            self.send_xim_message(im_id, &sync, conn);
        }
    }

    fn handle_reset_ic(&self, im_id: u16, data: &[u8], conn: &ClientConnection) {
        let ic_id = if data.len() > 7 {
            u16::from_le_bytes([data[6], data[7]])
        } else { 0 };
        debug!("XIM: ResetIC im_id={} ic_id={}", im_id, ic_id);

        // Reply with empty committed string
        let mut reply = vec![XIM_RESET_IC_REPLY, 0, 2, 0]; // length=2 words
        reply.extend_from_slice(&im_id.to_le_bytes());
        reply.extend_from_slice(&ic_id.to_le_bytes());
        reply.extend_from_slice(&0u16.to_le_bytes()); // committed string length = 0
        reply.extend_from_slice(&[0, 0]); // pad
        self.send_xim_message(im_id, &reply, conn);
    }

    // --- Public API for sending committed text and preedit to XIM clients ---

    /// Send committed text to all XIM clients that have ICs for the given window.
    /// Returns true if text was sent to at least one XIM client.
    pub fn send_commit(&self, server: &XServer, focus_window: Xid, text: &str) -> bool {
        let encoded = encode_compound_text(text);
        let mut sent = false;

        for conn_entry in self.connections.iter() {
            let xim_conn = conn_entry.value();
            for ctx_entry in xim_conn.contexts.iter() {
                let ctx = ctx_entry.value();
                // Match by focus_window or client_window
                if ctx.focus_window == focus_window || ctx.client_window == focus_window
                    || self.window_is_descendant(server, focus_window, ctx.client_window)
                {
                    if let Some(conn) = server.connections.get(&xim_conn.conn_id) {
                        self.send_xim_commit(
                            xim_conn.im_id,
                            ctx.ic_id,
                            &encoded,
                            &conn,
                        );
                        sent = true;
                    }
                }
            }
        }

        if sent {
            info!("XIM: sent commit '{}' ({} bytes CTEXT) to focus=0x{:08x}", text, encoded.len(), focus_window);
        }
        sent
    }

    /// Send preedit start to matching XIM clients.
    pub fn send_preedit_start(&self, server: &XServer, focus_window: Xid) {
        self.for_each_matching_ic(server, focus_window, |xim_conn, ctx, conn| {
            let mut msg = vec![XIM_PREEDIT_START, 0, 1, 0];
            msg.extend_from_slice(&xim_conn.im_id.to_le_bytes());
            msg.extend_from_slice(&ctx.ic_id.to_le_bytes());
            self.send_xim_message(xim_conn.im_id, &msg, conn);
        });
    }

    /// Send preedit draw (update preedit text) to matching XIM clients.
    pub fn send_preedit_draw(&self, server: &XServer, focus_window: Xid, text: &str, caret: u32) {
        let encoded = encode_compound_text(text);

        self.for_each_matching_ic(server, focus_window, |xim_conn, ctx, conn| {
            // XIM_PREEDIT_DRAW:
            // [4-5] im_id [6-7] ic_id
            // [8-11] caret
            // [12-15] chg_first (0 = replace all)
            // [16-19] chg_length (0 = replace all)
            // [20-23] status (0 = no feedback)
            // [24-25] length of preedit string
            // [26..] string + pad
            // then feedback array (4 bytes per char)
            let char_count = text.chars().count();
            let str_padded = (encoded.len() + 3) & !3;
            let feedback_len = char_count * 4;
            let body_len = 4 + 4 + 4 + 4 + 4 + 2 + str_padded + feedback_len;
            let padded_body = (body_len + 3) & !3;

            let mut msg = Vec::with_capacity(4 + padded_body);
            msg.push(XIM_PREEDIT_DRAW);
            msg.push(0);
            msg.extend_from_slice(&((padded_body / 4) as u16).to_le_bytes());
            msg.extend_from_slice(&xim_conn.im_id.to_le_bytes());
            msg.extend_from_slice(&ctx.ic_id.to_le_bytes());
            msg.extend_from_slice(&caret.to_le_bytes());       // caret
            msg.extend_from_slice(&0u32.to_le_bytes());         // chg_first
            msg.extend_from_slice(&0u32.to_le_bytes());         // chg_length (replace all)
            msg.extend_from_slice(&0u32.to_le_bytes());         // status
            msg.extend_from_slice(&(encoded.len() as u16).to_le_bytes()); // string length
            msg.extend_from_slice(&encoded);
            // Pad string
            for _ in 0..(str_padded - encoded.len()) {
                msg.push(0);
            }
            // Feedback: XIMUnderline (1) for each character
            for _ in 0..char_count {
                msg.extend_from_slice(&1u32.to_le_bytes()); // XIMUnderline
            }
            while msg.len() % 4 != 0 {
                msg.push(0);
            }

            self.send_xim_message(xim_conn.im_id, &msg, conn);
        });
    }

    /// Send preedit done to matching XIM clients.
    pub fn send_preedit_done(&self, server: &XServer, focus_window: Xid) {
        self.for_each_matching_ic(server, focus_window, |xim_conn, ctx, conn| {
            let mut msg = vec![XIM_PREEDIT_DONE, 0, 1, 0];
            msg.extend_from_slice(&xim_conn.im_id.to_le_bytes());
            msg.extend_from_slice(&ctx.ic_id.to_le_bytes());
            self.send_xim_message(xim_conn.im_id, &msg, conn);
        });
    }

    /// Check if any XIM client has an IC matching the given window.
    pub fn has_xim_client(&self, server: &XServer, focus_window: Xid) -> bool {
        for conn_entry in self.connections.iter() {
            for ctx_entry in conn_entry.value().contexts.iter() {
                let ctx = ctx_entry.value();
                if ctx.focus_window == focus_window || ctx.client_window == focus_window
                    || self.window_is_descendant(server, focus_window, ctx.client_window)
                {
                    return true;
                }
            }
        }
        false
    }

    /// Check if a window belongs to an app that has an XIM connection (even without IC).
    /// Such apps expect XIM-based IME and cannot handle raw Unicode keysyms.
    pub fn has_xim_connection(&self, _window: Xid) -> bool {
        !self.connections.is_empty()
    }

    /// Check if a specific connection ID has an XIM connection.
    pub fn has_xim_connection_by_conn_id(&self, conn_id: u32) -> bool {
        for conn_entry in self.connections.iter() {
            if conn_entry.value().conn_id == conn_id {
                return true;
            }
        }
        false
    }

    // --- Internal helpers ---

    fn for_each_matching_ic<F>(&self, server: &XServer, focus_window: Xid, mut f: F)
    where
        F: FnMut(&XimConnection, &XimInputContext, &ClientConnection),
    {
        for conn_entry in self.connections.iter() {
            let xim_conn = conn_entry.value();
            for ctx_entry in xim_conn.contexts.iter() {
                let ctx = ctx_entry.value();
                if ctx.focus_window == focus_window || ctx.client_window == focus_window
                    || self.window_is_descendant(server, focus_window, ctx.client_window)
                {
                    if let Some(conn) = server.connections.get(&xim_conn.conn_id) {
                        f(xim_conn, &ctx, &conn);
                    }
                }
            }
        }
    }

    fn send_xim_commit(&self, im_id: u16, ic_id: u16, encoded_text: &[u8], conn: &ClientConnection) {
        // XIM_COMMIT:
        // [4-5] im_id [6-7] ic_id
        // [8-9] flag (XIM_LOOKUP_CHARS=0x0002)
        // [10-11] byte length of committed string
        // [12..] string + pad
        let str_padded = (encoded_text.len() + 3) & !3;
        let body_len = 4 + 2 + 2 + str_padded;
        let padded_body = (body_len + 3) & !3;

        let mut msg = Vec::with_capacity(4 + padded_body);
        msg.push(XIM_COMMIT);
        msg.push(0);
        msg.extend_from_slice(&((padded_body / 4) as u16).to_le_bytes());
        msg.extend_from_slice(&im_id.to_le_bytes());
        msg.extend_from_slice(&ic_id.to_le_bytes());
        msg.extend_from_slice(&XIM_LOOKUP_CHARS.to_le_bytes()); // flag
        msg.extend_from_slice(&(encoded_text.len() as u16).to_le_bytes());
        msg.extend_from_slice(encoded_text);
        // Pad
        for _ in 0..(str_padded - encoded_text.len()) {
            msg.push(0);
        }
        while msg.len() % 4 != 0 {
            msg.push(0);
        }

        self.send_xim_message(im_id, &msg, conn);
    }

    /// Send an XIM protocol message via ClientMessage transport.
    /// Messages <= 20 bytes fit in a single ClientMessage.
    /// Longer messages are split: N-1 _XIM_MOREDATA + 1 _XIM_PROTOCOL.
    fn send_xim_message(&self, im_id: u16, data: &[u8], conn: &ClientConnection) {
        let client_window = match self.connections.get(&im_id) {
            Some(c) => c.client_comm_window,
            None => {
                warn!("XIM: cannot send message, unknown im_id={}", im_id);
                return;
            }
        };

        let byte_order = match self.connections.get(&im_id) {
            Some(c) => c.byte_order,
            None => conn.byte_order,
        };

        if data.len() <= 20 {
            // Fits in one ClientMessage (format=8, 20 bytes data area)
            let msg = build_client_message_8(
                self.atoms.xim_protocol,
                client_window,
                data,
                &byte_order,
            );
            let _ = conn.event_tx.send(msg.into());
        } else {
            // Split into multiple ClientMessages:
            // All but the last use _XIM_MOREDATA, the last uses _XIM_PROTOCOL
            let chunks: Vec<&[u8]> = data.chunks(20).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                let is_last = i == chunks.len() - 1;
                let atom = if is_last {
                    self.atoms.xim_protocol
                } else {
                    self.atoms.xim_moredata
                };
                let msg = build_client_message_8(atom, client_window, chunk, &byte_order);
                let _ = conn.event_tx.send(msg.into());
            }
        }
    }

    /// Check if `window` is a descendant of `ancestor`.
    fn window_is_descendant(&self, server: &XServer, window: Xid, ancestor: Xid) -> bool {
        if window == 0 || ancestor == 0 || window == ancestor {
            return false;
        }
        let mut current = window;
        for _ in 0..20 { // max depth
            if let Some(res) = server.resources.get(&current) {
                if let super::resources::Resource::Window(w) = res.value() {
                    let w = w.read();
                    let parent = w.parent;
                    if parent == ancestor {
                        return true;
                    }
                    if parent == 0 || parent == 1 {
                        return false;
                    }
                    drop(w);
                    current = parent;
                } else {
                    return false;
                }
            } else {
                return false;
            }
        }
        false
    }

    fn set_property(&self, server: &XServer, window: Xid, property: u32, type_atom: u32, data: &[u8]) {
        if let Some(res) = server.resources.get(&window) {
            if let super::resources::Resource::Window(ref w) = res.value() {
                let mut w = w.write();
                // Replace existing or add new property
                if let Some(p) = w.properties.iter_mut().find(|p| p.name == property) {
                    p.type_atom = type_atom;
                    p.format = 8;
                    p.data = data.to_vec();
                } else {
                    w.properties.push(super::resources::Property {
                        name: property,
                        type_atom,
                        format: 8,
                        data: data.to_vec(),
                    });
                }
            }
        }
    }

    fn send_selection_notify(
        &self,
        server: &XServer,
        requestor: Xid,
        selection: u32,
        target: u32,
        property: u32,
        conn: &ClientConnection,
    ) {
        // Send SelectionNotify event (type 31)
        let mut event = [0u8; 32];
        event[0] = 31; // SelectionNotify

        let time = server.current_time();
        let seq = conn.current_request_sequence();

        match conn.byte_order {
            ByteOrder::LittleEndian => {
                event[2..4].copy_from_slice(&(seq as u16).to_le_bytes());
                event[4..8].copy_from_slice(&time.to_le_bytes());
                event[8..12].copy_from_slice(&requestor.to_le_bytes());
                event[12..16].copy_from_slice(&selection.to_le_bytes());
                event[16..20].copy_from_slice(&target.to_le_bytes());
                event[20..24].copy_from_slice(&property.to_le_bytes());
            }
            ByteOrder::BigEndian => {
                event[2..4].copy_from_slice(&(seq as u16).to_be_bytes());
                event[4..8].copy_from_slice(&time.to_be_bytes());
                event[8..12].copy_from_slice(&requestor.to_be_bytes());
                event[12..16].copy_from_slice(&selection.to_be_bytes());
                event[16..20].copy_from_slice(&target.to_be_bytes());
                event[20..24].copy_from_slice(&property.to_be_bytes());
            }
        }

        let _ = conn.event_tx.send(event.into());
    }
}

/// Encode text as COMPOUND_TEXT with UTF-8 extension.
/// Format: ESC % G <UTF-8 bytes> ESC % @
fn encode_compound_text(text: &str) -> Vec<u8> {
    let utf8_bytes = text.as_bytes();
    // ESC % G = begin UTF-8, ESC % @ = end UTF-8
    let mut result = Vec::with_capacity(6 + utf8_bytes.len());
    result.push(0x1B); // ESC
    result.push(b'%');
    result.push(b'G');
    result.extend_from_slice(utf8_bytes);
    result.push(0x1B); // ESC
    result.push(b'%');
    result.push(b'@');
    result
}

/// Build a 32-byte ClientMessage event with format=32 (5 x u32 data).
fn build_client_message_32(
    message_type: u32,
    window: Xid,
    data: &[u32; 5],
    byte_order: &ByteOrder,
) -> [u8; 32] {
    let mut event = [0u8; 32];
    event[0] = 33; // ClientMessage
    event[1] = 32; // format
    // sequence at 2-3 = 0
    match byte_order {
        ByteOrder::LittleEndian => {
            event[4..8].copy_from_slice(&window.to_le_bytes());
            event[8..12].copy_from_slice(&message_type.to_le_bytes());
            for (i, &val) in data.iter().enumerate() {
                let off = 12 + i * 4;
                event[off..off + 4].copy_from_slice(&val.to_le_bytes());
            }
        }
        ByteOrder::BigEndian => {
            event[4..8].copy_from_slice(&window.to_be_bytes());
            event[8..12].copy_from_slice(&message_type.to_be_bytes());
            for (i, &val) in data.iter().enumerate() {
                let off = 12 + i * 4;
                event[off..off + 4].copy_from_slice(&val.to_be_bytes());
            }
        }
    }
    event
}

/// Build a 32-byte ClientMessage event with format=8 (up to 20 bytes data).
fn build_client_message_8(
    message_type: u32,
    window: Xid,
    data: &[u8],
    byte_order: &ByteOrder,
) -> [u8; 32] {
    let mut event = [0u8; 32];
    event[0] = 33; // ClientMessage
    event[1] = 8;  // format
    // sequence at 2-3 = 0
    match byte_order {
        ByteOrder::LittleEndian => {
            event[4..8].copy_from_slice(&window.to_le_bytes());
            event[8..12].copy_from_slice(&message_type.to_le_bytes());
        }
        ByteOrder::BigEndian => {
            event[4..8].copy_from_slice(&window.to_be_bytes());
            event[8..12].copy_from_slice(&message_type.to_be_bytes());
        }
    }
    // Copy data into bytes 12..32 (up to 20 bytes)
    let copy_len = std::cmp::min(data.len(), 20);
    event[12..12 + copy_len].copy_from_slice(&data[..copy_len]);
    event
}
