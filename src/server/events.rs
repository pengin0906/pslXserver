use crate::display::Xid;
use super::connection::{ClientConnection, ByteOrder};

/// Build a 32-byte X11 event in the client's byte order.
pub struct EventBuilder<'a> {
    conn: &'a ClientConnection,
    buf: [u8; 32],
}

impl<'a> EventBuilder<'a> {
    pub fn new(conn: &'a ClientConnection, event_type: u8) -> Self {
        let mut buf = [0u8; 32];
        buf[0] = event_type;
        // Set sequence number at offset 2-3
        let seq = conn.current_request_sequence();
        let seq_bytes = match conn.byte_order {
            ByteOrder::BigEndian => (seq as u16).to_be_bytes(),
            ByteOrder::LittleEndian => (seq as u16).to_le_bytes(),
        };
        buf[2] = seq_bytes[0];
        buf[3] = seq_bytes[1];
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

    pub fn build(self) -> [u8; 32] {
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
) -> [u8; 32] {
    let mut eb = EventBuilder::new(conn, event_type);
    eb.set_u8(1, keycode)
      // sequence at offset 2-3 already set by EventBuilder::new
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
) -> [u8; 32] {
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
) -> [u8; 32] {
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
) -> [u8; 32] {
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
) -> [u8; 32] {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU16;
    use tokio::sync::mpsc;

    fn make_test_conn(byte_order: ByteOrder, seq: u16) -> ClientConnection {
        let (tx, _rx) = mpsc::unbounded_channel();
        let conn = ClientConnection {
            id: 1,
            resource_id_base: 0x00200000,
            resource_id_mask: 0x001FFFFF,
            byte_order,
            sequence_number: AtomicU16::new(seq),
            event_tx: tx,
            mapping_gen: std::sync::atomic::AtomicU32::new(0),
        };
        conn
    }

    #[test]
    fn test_event_builder_size() {
        let conn = make_test_conn(ByteOrder::LittleEndian, 5);
        let event = EventBuilder::new(&conn, 12).build(); // Expose
        assert_eq!(event.len(), 32); // All X11 events are exactly 32 bytes
    }

    #[test]
    fn test_event_builder_type_and_sequence() {
        let conn = make_test_conn(ByteOrder::LittleEndian, 42);
        let event = EventBuilder::new(&conn, 12).build();
        assert_eq!(event[0], 12); // event type
        // Sequence is current_request_sequence() = 42 - 1 = 41
        assert_eq!(u16::from_le_bytes([event[2], event[3]]), 41);
    }

    #[test]
    fn test_event_builder_big_endian() {
        let conn = make_test_conn(ByteOrder::BigEndian, 42);
        let event = EventBuilder::new(&conn, 2).build();
        assert_eq!(event[0], 2); // event type
        assert_eq!(u16::from_be_bytes([event[2], event[3]]), 41);
    }

    #[test]
    fn test_key_event_little_endian() {
        let conn = make_test_conn(ByteOrder::LittleEndian, 10);
        let event = build_key_event(
            &conn, 2, 38, // KeyPress, keycode 38 ('a')
            12345, 0x80, 0x100, 0, // time, root, event_window, child
            100, 200, 50, 75, 0x0001, true, // root_x/y, event_x/y, state, same_screen
        );
        assert_eq!(event.len(), 32);
        assert_eq!(event[0], 2); // KeyPress
        assert_eq!(event[1], 38); // keycode
        assert_eq!(u32::from_le_bytes([event[4], event[5], event[6], event[7]]), 12345); // time
        assert_eq!(u32::from_le_bytes([event[8], event[9], event[10], event[11]]), 0x80); // root
        assert_eq!(u32::from_le_bytes([event[12], event[13], event[14], event[15]]), 0x100); // event
        assert_eq!(i16::from_le_bytes([event[20], event[21]]), 100); // root_x
        assert_eq!(i16::from_le_bytes([event[22], event[23]]), 200); // root_y
        assert_eq!(i16::from_le_bytes([event[24], event[25]]), 50); // event_x
        assert_eq!(i16::from_le_bytes([event[26], event[27]]), 75); // event_y
        assert_eq!(u16::from_le_bytes([event[28], event[29]]), 0x0001); // state
        assert_eq!(event[30], 1); // same_screen
    }

    #[test]
    fn test_expose_event() {
        let conn = make_test_conn(ByteOrder::LittleEndian, 5);
        let event = build_expose_event(&conn, 0x100, 10, 20, 640, 480, 0);
        assert_eq!(event[0], 12); // Expose type
        assert_eq!(u32::from_le_bytes([event[4], event[5], event[6], event[7]]), 0x100);
        assert_eq!(u16::from_le_bytes([event[8], event[9]]), 10); // x
        assert_eq!(u16::from_le_bytes([event[10], event[11]]), 20); // y
        assert_eq!(u16::from_le_bytes([event[12], event[13]]), 640); // width
        assert_eq!(u16::from_le_bytes([event[14], event[15]]), 480); // height
        assert_eq!(u16::from_le_bytes([event[16], event[17]]), 0); // count
    }

    #[test]
    fn test_configure_notify_event() {
        let conn = make_test_conn(ByteOrder::LittleEndian, 5);
        let event = build_configure_notify(
            &conn, 0x100, 0x100, 0, 50, 75, 800, 600, 0, false
        );
        assert_eq!(event[0], 22); // ConfigureNotify type
        assert_eq!(u32::from_le_bytes([event[4], event[5], event[6], event[7]]), 0x100); // event
        assert_eq!(u32::from_le_bytes([event[8], event[9], event[10], event[11]]), 0x100); // window
        assert_eq!(i16::from_le_bytes([event[16], event[17]]), 50); // x
        assert_eq!(i16::from_le_bytes([event[18], event[19]]), 75); // y
        assert_eq!(u16::from_le_bytes([event[20], event[21]]), 800); // width
        assert_eq!(u16::from_le_bytes([event[22], event[23]]), 600); // height
        assert_eq!(event[26], 0); // override_redirect = false
    }

    #[test]
    fn test_map_notify_event() {
        let conn = make_test_conn(ByteOrder::LittleEndian, 1);
        let event = build_map_notify(&conn, 0x80, 0x100, true);
        assert_eq!(event[0], 19); // MapNotify type
        assert_eq!(u32::from_le_bytes([event[4], event[5], event[6], event[7]]), 0x80);
        assert_eq!(u32::from_le_bytes([event[8], event[9], event[10], event[11]]), 0x100);
        assert_eq!(event[12], 1); // override_redirect = true
    }

    #[test]
    fn test_client_message_event() {
        let conn = make_test_conn(ByteOrder::LittleEndian, 1);
        let data = [0xDEADBEEF_u32, 0xCAFEBABE, 0, 0, 0];
        let event = build_client_message(&conn, 32, 0x100, 42, &data);
        assert_eq!(event[0], 33); // ClientMessage type
        assert_eq!(event[1], 32); // format
        assert_eq!(u32::from_le_bytes([event[4], event[5], event[6], event[7]]), 0x100); // window
        assert_eq!(u32::from_le_bytes([event[8], event[9], event[10], event[11]]), 42); // type
        assert_eq!(u32::from_le_bytes([event[12], event[13], event[14], event[15]]), 0xDEADBEEF);
    }

    #[test]
    fn test_event_big_endian_byte_order() {
        let conn = make_test_conn(ByteOrder::BigEndian, 10);
        let event = build_expose_event(&conn, 0x100, 10, 20, 640, 480, 0);
        // In big-endian, bytes should be big-endian
        assert_eq!(u32::from_be_bytes([event[4], event[5], event[6], event[7]]), 0x100);
        assert_eq!(u16::from_be_bytes([event[12], event[13]]), 640);
    }
}
