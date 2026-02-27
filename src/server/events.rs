use crate::display::Xid;
use super::connection::{ClientConnection, ByteOrder};

/// Build a 32-byte X11 event in the client's byte order.
pub struct EventBuilder<'a> {
    conn: &'a ClientConnection,
    buf: Vec<u8>,
}

impl<'a> EventBuilder<'a> {
    pub fn new(conn: &'a ClientConnection, event_type: u8) -> Self {
        let mut buf = vec![0u8; 32];
        buf[0] = event_type;
        Self { conn, buf }
    }

    pub fn set_u8(&mut self, offset: usize, val: u8) -> &mut Self {
        self.buf[offset] = val;
        self
    }

    pub fn set_u16(&mut self, offset: usize, val: u16) -> &mut Self {
        let bytes = match self.conn.byte_order {
            ByteOrder::BigEndian => val.to_be_bytes(),
            ByteOrder::LittleEndian => val.to_le_bytes(),
        };
        self.buf[offset] = bytes[0];
        self.buf[offset + 1] = bytes[1];
        self
    }

    pub fn set_i16(&mut self, offset: usize, val: i16) -> &mut Self {
        let bytes = match self.conn.byte_order {
            ByteOrder::BigEndian => val.to_be_bytes(),
            ByteOrder::LittleEndian => val.to_le_bytes(),
        };
        self.buf[offset] = bytes[0];
        self.buf[offset + 1] = bytes[1];
        self
    }

    pub fn set_u32(&mut self, offset: usize, val: u32) -> &mut Self {
        let bytes = match self.conn.byte_order {
            ByteOrder::BigEndian => val.to_be_bytes(),
            ByteOrder::LittleEndian => val.to_le_bytes(),
        };
        self.buf[offset] = bytes[0];
        self.buf[offset + 1] = bytes[1];
        self.buf[offset + 2] = bytes[2];
        self.buf[offset + 3] = bytes[3];
        self
    }

    pub fn build(self) -> Vec<u8> {
        self.buf
    }
}

/// Build a KeyPress or KeyRelease event (type 2 or 3).
pub fn build_key_event(
    conn: &ClientConnection,
    event_type: u8, // 2=KeyPress, 3=KeyRelease
    keycode: u8,
    time: u32,
    root: Xid,
    event_window: Xid,
    child: Xid,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
    state: u16,
    same_screen: bool,
) -> Vec<u8> {
    let mut eb = EventBuilder::new(conn, event_type);
    eb.set_u8(1, keycode)
      .set_u16(2, 0) // sequence (filled by sender)
      .set_u32(4, time)
      .set_u32(8, root)
      .set_u32(12, event_window)
      .set_u32(16, child)
      .set_i16(20, root_x)
      .set_i16(22, root_y)
      .set_i16(24, event_x)
      .set_i16(26, event_y)
      .set_u16(28, state)
      .set_u8(30, if same_screen { 1 } else { 0 });
    eb.build()
}

/// Build an Expose event (type 12).
pub fn build_expose_event(
    conn: &ClientConnection,
    window: Xid,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    count: u16,
) -> Vec<u8> {
    let mut eb = EventBuilder::new(conn, super::protocol::event_type::EXPOSE);
    eb.set_u32(4, window)
      .set_u16(8, x)
      .set_u16(10, y)
      .set_u16(12, width)
      .set_u16(14, height)
      .set_u16(16, count);
    eb.build()
}

/// Build a ConfigureNotify event (type 22).
pub fn build_configure_notify(
    conn: &ClientConnection,
    event_window: Xid,
    window: Xid,
    above_sibling: Xid,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    override_redirect: bool,
) -> Vec<u8> {
    let mut eb = EventBuilder::new(conn, super::protocol::event_type::CONFIGURE_NOTIFY);
    eb.set_u32(4, event_window)
      .set_u32(8, window)
      .set_u32(12, above_sibling)
      .set_i16(16, x)
      .set_i16(18, y)
      .set_u16(20, width)
      .set_u16(22, height)
      .set_u16(24, border_width)
      .set_u8(26, if override_redirect { 1 } else { 0 });
    eb.build()
}

/// Build a MapNotify event (type 19).
pub fn build_map_notify(
    conn: &ClientConnection,
    event_window: Xid,
    window: Xid,
    override_redirect: bool,
) -> Vec<u8> {
    let mut eb = EventBuilder::new(conn, super::protocol::event_type::MAP_NOTIFY);
    eb.set_u32(4, event_window)
      .set_u32(8, window)
      .set_u8(12, if override_redirect { 1 } else { 0 });
    eb.build()
}

/// Build a ClientMessage event (type 33).
pub fn build_client_message(
    conn: &ClientConnection,
    format: u8,
    window: Xid,
    message_type: u32,
    data: &[u32; 5],
) -> Vec<u8> {
    let mut eb = EventBuilder::new(conn, super::protocol::event_type::CLIENT_MESSAGE);
    eb.set_u8(1, format)
      .set_u32(4, window)
      .set_u32(8, message_type)
      .set_u32(12, data[0])
      .set_u32(16, data[1])
      .set_u32(20, data[2])
      .set_u32(24, data[3])
      .set_u32(28, data[4]);
    eb.build()
}
