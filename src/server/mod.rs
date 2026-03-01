pub mod atoms;
pub mod connection;
pub mod events;
pub mod extensions;
pub mod protocol;
pub mod resources;

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use dashmap::DashMap;
use log::info;
use thiserror::Error;
use tokio::net::UnixListener;

use crate::display::{DisplayCommand, DisplayEvent, Xid};
use crate::util::coord::CoordContext;

use self::atoms::AtomTable;
use self::resources::Resource;

/// Timestamp type for X11 events.
pub type Timestamp = u32;

/// X11 server errors.
#[derive(Error, Debug)]
pub enum ServerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Protocol error")]
    Protocol,
    #[error("Resource not found: {0}")]
    ResourceNotFound(Xid),
    #[error("Atom not found")]
    AtomNotFound,
    #[error("Not implemented")]
    NotImplemented,
}

/// Visual type information for the X11 connection setup.
pub struct Visual {
    pub id: u32,
    pub class: u8,       // TrueColor = 4
    pub bits_per_rgb: u8,
    pub colormap_entries: u16,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
}

/// Screen information.
pub struct Screen {
    pub root_window: Xid,
    pub default_colormap: Xid,
    pub white_pixel: u32,
    pub black_pixel: u32,
    pub width_in_pixels: u16,
    pub height_in_pixels: u16,
    pub width_in_mm: u16,
    pub height_in_mm: u16,
    pub root_depth: u8,
    pub root_visual: Visual,
    pub coord_context: CoordContext,
}

/// The central X11 server state, shared across all client connections.
pub struct XServer {
    /// Per-client connection states, keyed by connection ID.
    pub connections: DashMap<u32, Arc<connection::ClientConnection>>,
    /// Global resource table: XID -> Resource.
    pub resources: DashMap<Xid, Resource>,
    /// Atom intern table.
    pub atoms: AtomTable,
    /// Screen(s) — typically one for single-monitor.
    pub screens: Vec<Screen>,
    /// Channel to send display commands to the macOS main thread.
    pub display_cmd_tx: Sender<DisplayCommand>,
    /// Next connection ID.
    pub next_conn_id: AtomicU32,
    /// Next available XID base for new connections.
    pub next_resource_id_base: AtomicU32,
    /// Server startup timestamp (milliseconds).
    pub startup_time: Timestamp,
    /// Display number.
    pub display_number: u32,
    /// Selection ownership: atom -> (owner_window, timestamp).
    pub selections: DashMap<u32, (Xid, Timestamp)>,
    /// Current pointer position (root coordinates).
    pub pointer_x: AtomicI32,
    pub pointer_y: AtomicI32,
    /// Per-window pointer position (window-relative coordinates from MotionNotify).
    /// Key is X11 window ID of top-level window.
    pub window_pointer: DashMap<Xid, (i16, i16)>,
    /// Render mailbox: native_window_id -> pending render commands.
    /// Protocol handlers append here; display thread drains each frame.
    pub render_mailbox: crate::display::RenderMailbox,
    /// Current keyboard focus window (0 = None, 1 = PointerRoot).
    pub focus_window: AtomicU32,
    /// Focus revert-to mode (0=None, 1=PointerRoot, 2=Parent).
    pub focus_revert_to: AtomicU32,
    /// Virtual keysyms for IME input. Keycodes 200..200+len are mapped to these keysyms.
    /// Written by send_ime_text, read by handle_get_keyboard_mapping.
    pub virtual_keysyms: parking_lot::RwLock<Vec<u32>>,
}

impl XServer {
    pub fn new(
        display_number: u32,
        display_cmd_tx: Sender<DisplayCommand>,
        screen_width: u16,
        screen_height: u16,
    ) -> Self {
        let atoms = AtomTable::new();

        let scale = 1.0;

        let root_visual = Visual {
            id: 0x21, // arbitrary visual ID
            class: 4, // TrueColor
            bits_per_rgb: 8,
            colormap_entries: 256,
            red_mask: 0x00FF0000,
            green_mask: 0x0000FF00,
            blue_mask: 0x000000FF,
        };

        let coord_context = CoordContext::new(
            screen_height as f64 / scale,
            scale,
        );

        let root_window_id: Xid = 0x00000001;
        let default_colormap_id: Xid = 0x00000002;

        let screen = Screen {
            root_window: root_window_id,
            default_colormap: default_colormap_id,
            white_pixel: 0x00FFFFFF,
            black_pixel: 0x00000000,
            width_in_pixels: screen_width,
            height_in_pixels: screen_height,
            width_in_mm: (screen_width as u32 * 254 / 960) as u16, // ~96dpi
            height_in_mm: (screen_height as u32 * 254 / 960) as u16,
            root_depth: 24,
            root_visual,
            coord_context,
        };

        let startup_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;

        let resources = DashMap::new();

        // Register root window as a resource
        let root_win = resources::WindowState {
            id: root_window_id,
            parent: 0,
            x: 0,
            y: 0,
            width: screen_width,
            height: screen_height,
            border_width: 0,
            depth: 24,
            class: resources::WindowClass::InputOutput,
            visual: 0x21,
            background_pixel: Some(0),
            border_pixel: Some(0),
            bit_gravity: 0,
            win_gravity: 0,
            backing_store: 0,
            event_mask: 0,
            do_not_propagate_mask: 0,
            override_redirect: false,
            colormap: default_colormap_id,
            cursor: 0,
            mapped: true,
            viewable: true,
            children: Vec::new(),
            properties: Vec::new(),
            event_selections: Vec::new(),
            native_window: None,
            backing_buffer: None,
            ime_spot: None,
            ime_focus: false,
        };
        resources.insert(
            root_window_id,
            resources::Resource::Window(Arc::new(parking_lot::RwLock::new(root_win))),
        );

        Self {
            connections: DashMap::new(),
            resources,
            atoms,
            screens: vec![screen],
            display_cmd_tx,
            next_conn_id: AtomicU32::new(1),
            next_resource_id_base: AtomicU32::new(0x00200000),
            startup_time,
            display_number,
            selections: DashMap::new(),
            pointer_x: AtomicI32::new(0),
            pointer_y: AtomicI32::new(0),
            window_pointer: DashMap::new(),
            render_mailbox: std::sync::Arc::new(DashMap::new()),
            focus_window: AtomicU32::new(1), // PointerRoot by default
            focus_revert_to: AtomicU32::new(1), // PointerRoot
            virtual_keysyms: parking_lot::RwLock::new(Vec::new()),
        }
    }

    /// Allocate a resource ID base for a new connection.
    pub fn alloc_resource_id_base(&self) -> u32 {
        self.next_resource_id_base.fetch_add(0x00200000, Ordering::Relaxed)
    }

    /// Get the next connection ID.
    pub fn next_conn_id(&self) -> u32 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Get current timestamp.
    pub fn current_time(&self) -> Timestamp {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;
        now.wrapping_sub(self.startup_time)
    }
}

/// Main server entry point. Called from the tokio background thread.
pub async fn run_server(
    display_number: u32,
    listen_tcp: bool,
    evt_rx: Receiver<DisplayEvent>,
    cmd_tx: Sender<DisplayCommand>,
    screen_width: u16,
    screen_height: u16,
    render_mailbox: crate::display::RenderMailbox,
) -> Result<(), ServerError> {
    let mut server = XServer::new(display_number, cmd_tx, screen_width, screen_height);
    server.render_mailbox = render_mailbox;
    let server = Arc::new(server);

    // Spawn event dispatch task: routes DisplayEvents from macOS to X11 clients
    {
        let server_clone = Arc::clone(&server);
        tokio::spawn(async move {
            dispatch_events(server_clone, evt_rx).await;
        });
    }

    // Create Unix domain socket
    let socket_dir = "/tmp/.X11-unix";
    let socket_path = format!("{}/X{}", socket_dir, display_number);

    // Ensure the socket directory exists
    std::fs::create_dir_all(socket_dir).ok();

    // Remove stale socket file
    std::fs::remove_file(&socket_path).ok();

    let listener = UnixListener::bind(&socket_path)?;
    info!("Listening on {}", socket_path);

    // Set socket permissions to world-readable/writable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o777))?;
    }

    // Optional TCP listener
    if listen_tcp {
        let tcp_port = 6000 + display_number as u16;
        let tcp_listener = tokio::net::TcpListener::bind(("0.0.0.0", tcp_port)).await?;
        info!("Also listening on TCP port {}", tcp_port);

        let server_clone = Arc::clone(&server);
        tokio::spawn(async move {
            loop {
                match tcp_listener.accept().await {
                    Ok((stream, addr)) => {
                        let server = Arc::clone(&server_clone);
                        let conn_id = server.next_conn_id();
                        info!("New X11 TCP client connection (id={}) from {}", conn_id, addr);
                        tokio::spawn(async move {
                            if let Err(e) = connection::handle_connection(server, stream, conn_id).await {
                                log::error!("TCP connection {} error: {}", conn_id, e);
                            }
                            info!("TCP connection {} closed", conn_id);
                        });
                    }
                    Err(e) => log::error!("TCP accept error: {}", e),
                }
            }
        });
    }

    // Create reverse mapping: native_window_id -> X11 window ID
    // This is needed to route display events back to the right X11 window.
    // For simplicity, we add a helper to XServer.

    // Accept Unix socket connections
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let server = Arc::clone(&server);
                let conn_id = server.next_conn_id();
                info!("New X11 client connection (id={})", conn_id);

                tokio::spawn(async move {
                    if let Err(e) = connection::handle_connection(server, stream, conn_id).await {
                        log::error!("Connection {} error: {}", conn_id, e);
                    }
                    info!("Connection {} closed", conn_id);
                });
            }
            Err(e) => {
                log::error!("Accept error: {}", e);
            }
        }
    }
}

/// Dispatch events from the macOS display backend to X11 clients.
async fn dispatch_events(server: Arc<XServer>, evt_rx: Receiver<DisplayEvent>) {
    info!("Event dispatch task started");
    let mut focused_window: Xid = 0;
    let mut entered_window: Xid = 0;
    let mut preedit_char_count: usize = 0;
    loop {
        // Use spawn_blocking to avoid blocking the tokio runtime
        let evt = {
            let rx = evt_rx.clone();
            match tokio::task::spawn_blocking(move || rx.recv()).await {
                Ok(Ok(evt)) => evt,
                _ => break,
            }
        };

        log::debug!("Dispatching event: {:?}", evt);
        match evt {
            DisplayEvent::ButtonPress { window, button, x, y, root_x, root_y, state, time } => {
                // Send EnterNotify if we haven't entered this window yet
                if entered_window != window {
                    send_enter_leave_event(&server, protocol::event_type::ENTER_NOTIFY,
                        window, x, y, root_x, root_y, state, time);
                    entered_window = window;
                }
                // Send FocusIn if this window doesn't have focus yet
                if focused_window != window {
                    send_focus_event(&server, protocol::event_type::FOCUS_IN, window);
                    focused_window = window;
                }
                send_button_event(&server, protocol::event_type::BUTTON_PRESS,
                    window, button, x, y, root_x, root_y, state, time);
            }
            DisplayEvent::ButtonRelease { window, button, x, y, root_x, root_y, state, time } => {
                send_button_event(&server, protocol::event_type::BUTTON_RELEASE,
                    window, button, x, y, root_x, root_y, state, time);
            }
            DisplayEvent::MotionNotify { window, x, y, root_x, root_y, state, time } => {
                // Update stored pointer position for QueryPointer
                server.pointer_x.store(root_x as i32, Ordering::Relaxed);
                server.pointer_y.store(root_y as i32, Ordering::Relaxed);
                // Store per-window relative coordinates for child window QueryPointer
                server.window_pointer.insert(window, (x, y));
                if entered_window != window {
                    send_enter_leave_event(&server, protocol::event_type::ENTER_NOTIFY,
                        window, x, y, root_x, root_y, state, time);
                    entered_window = window;
                }
                send_motion_event(&server, window, x, y, root_x, root_y, state, time);
            }
            DisplayEvent::KeyPress { window, keycode, state, time } => {
                send_key_event(&server, protocol::event_type::KEY_PRESS, window, keycode, state, time);
            }
            DisplayEvent::KeyRelease { window, keycode, state, time } => {
                send_key_event(&server, protocol::event_type::KEY_RELEASE, window, keycode, state, time);
            }
            DisplayEvent::ImeCommit { window, text } => {
                // Erase inline preedit before inserting committed text
                if preedit_char_count > 0 {
                    info!("ImeCommit: erasing {} preedit chars first", preedit_char_count);
                    send_backspaces(&server, window, preedit_char_count);
                    preedit_char_count = 0;
                }
                send_ime_text(&server, window, &text);
            }
            DisplayEvent::ImePreeditDraw { window, text, .. } => {
                // Erase old preedit, then send new preedit as temporary characters
                if preedit_char_count > 0 {
                    send_backspaces(&server, window, preedit_char_count);
                }
                let new_count = text.chars().count();
                if !text.is_empty() {
                    info!("ImePreeditDraw: showing '{}' ({} chars, was {})", text, new_count, preedit_char_count);
                    send_ime_text(&server, window, &text);
                }
                preedit_char_count = new_count;
            }
            DisplayEvent::ImePreeditDone { window } => {
                if preedit_char_count > 0 {
                    info!("ImePreeditDone: erasing {} preedit chars", preedit_char_count);
                    send_backspaces(&server, window, preedit_char_count);
                    preedit_char_count = 0;
                }
            }
            DisplayEvent::Expose { window, x, y, width, height, count } => {
                send_expose_event(&server, window, x, y, width, height, count);
            }
            DisplayEvent::ConfigureNotify { window, x, y, width, height } => {
                // Update the X11 window state with the new dimensions AND position
                let children = if let Some(res) = server.resources.get(&window) {
                    if let Resource::Window(win) = res.value() {
                        let mut w = win.write();
                        let old_w = w.width;
                        let old_h = w.height;
                        w.x = x;
                        w.y = y;
                        w.width = width;
                        w.height = height;
                        info!("Window 0x{:08X} configure: ({},{}) {}x{} -> ({},{}) {}x{}",
                              window, 0, 0, old_w, old_h, x, y, width, height);
                        w.children.clone()
                    } else { Vec::new() }
                } else { Vec::new() };

                // Send ConfigureNotify to clients that selected StructureNotify
                send_configure_notify_event(&server, window, x, y, width, height);
                // Send Expose for full window so client redraws
                send_expose_event(&server, window, 0, 0, width, height, 0);

                // Don't force-resize child windows here. The client (e.g. xterm)
                // is responsible for resizing its own children via ConfigureWindow
                // after receiving the parent's ConfigureNotify.
                let _ = children;
            }
            DisplayEvent::GlobalPointerUpdate { root_x, root_y } => {
                server.pointer_x.store(root_x as i32, Ordering::Relaxed);
                server.pointer_y.store(root_y as i32, Ordering::Relaxed);
                // MotionNotify is now sent directly from macOS side with accurate
                // window-relative coords (see process_commands in macos.rs).
            }
            _ => {
                log::debug!("Unhandled display event: {:?}", evt);
            }
        }
    }
}

fn send_button_event(
    server: &XServer, event_type: u8,
    window: Xid, button: u8,
    x: i16, y: i16, root_x: i16, root_y: i16,
    state: u16, time: u32,
) {
    let mask_bit = match event_type {
        protocol::event_type::BUTTON_PRESS => protocol::event_mask::BUTTON_PRESS,
        _ => protocol::event_mask::BUTTON_RELEASE,
    };

    log::debug!("send_button_event: window=0x{:08x} button={} resource_exists={}",
        window, button, server.resources.contains_key(&window));

    if let Some(res) = server.resources.get(&window) {
        if let Resource::Window(win) = res.value() {
            let w = win.read();
            log::debug!("  event_selections: {:?}", w.event_selections.iter()
                .map(|(c, m)| format!("conn{}:0x{:08x}", c, m)).collect::<Vec<_>>());
            for &(conn_id, emask) in &w.event_selections {
                if (emask & mask_bit) != 0 {
                    log::debug!("  -> Delivering to conn {} (mask match 0x{:08x} & 0x{:08x})", conn_id, emask, mask_bit);
                    if let Some(conn_ref) = server.connections.get(&conn_id) {
                        let conn = conn_ref.value();
                        let mut evt = events::EventBuilder::new(conn, event_type);
                        evt.set_u8(1, button)
                           .set_u32(4, time)
                           .set_u32(8, server.screens[0].root_window)
                           .set_u32(12, window)
                           .set_u32(16, 0) // child
                           .set_i16(20, root_x)
                           .set_i16(22, root_y)
                           .set_i16(24, x)
                           .set_i16(26, y)
                           .set_u16(28, state)
                           .set_u8(30, 1); // same-screen
                        let _ = conn.event_tx.send(evt.build());
                    }
                }
            }
        }
    } else {
        log::debug!("  Window 0x{:08x} not found in resources", window);
    }
}

fn send_motion_event(
    server: &XServer, window: Xid,
    x: i16, y: i16, root_x: i16, root_y: i16,
    state: u16, time: u32,
) {
    if let Some(res) = server.resources.get(&window) {
        if let Resource::Window(win) = res.value() {
            let w = win.read();
            for &(conn_id, emask) in &w.event_selections {
                if (emask & protocol::event_mask::POINTER_MOTION) != 0
                    || (emask & protocol::event_mask::BUTTON_MOTION) != 0
                {
                    if let Some(conn_ref) = server.connections.get(&conn_id) {
                        let conn = conn_ref.value();
                        let mut evt = events::EventBuilder::new(conn, protocol::event_type::MOTION_NOTIFY);
                        evt.set_u8(1, 0) // detail: Normal
                           .set_u32(4, time)
                           .set_u32(8, server.screens[0].root_window)
                           .set_u32(12, window)
                           .set_u32(16, 0) // child
                           .set_i16(20, root_x)
                           .set_i16(22, root_y)
                           .set_i16(24, x)
                           .set_i16(26, y)
                           .set_u16(28, state)
                           .set_u8(30, 1); // same-screen
                        let _ = conn.event_tx.send(evt.build());
                    }
                }
            }
        }
    }
}

fn send_key_event(
    server: &XServer, event_type: u8,
    window: Xid, keycode: u8, state: u16, time: u32,
) {
    let mask_bit = match event_type {
        protocol::event_type::KEY_PRESS => protocol::event_mask::KEY_PRESS,
        _ => protocol::event_mask::KEY_RELEASE,
    };

    // Determine target window based on focus.
    // focus_window: 0=None, 1=PointerRoot, else=specific window ID.
    let focus = server.focus_window.load(Ordering::Relaxed);
    let target = if focus > 1 {
        // Explicit focus window set by SetInputFocus
        focus
    } else if focus == 1 {
        // PointerRoot: find the deepest child under the pointer in the event window
        find_deepest_child(server, window)
    } else {
        return; // focus=None, discard key events
    };

    // Walk up from the target window, looking for a window that selected key events.
    // X11 spec: key events propagate from focus window up to root.
    let mut current = target;
    for _ in 0..32 {
        if current == 0 { break; }
        let (found_conn, parent) = if let Some(res) = server.resources.get(&current) {
            if let Resource::Window(win) = res.value() {
                let w = win.read();
                let mut found = None;
                for &(conn_id, emask) in &w.event_selections {
                    if (emask & mask_bit) != 0 {
                        found = Some(conn_id);
                        break;
                    }
                }
                (found, w.parent)
            } else { (None, 0) }
        } else { (None, 0) };

        if let Some(conn_id) = found_conn {
            if let Some(conn_ref) = server.connections.get(&conn_id) {
                let conn = conn_ref.value();
                let evt_data = events::build_key_event(
                    conn, event_type, keycode, time,
                    server.screens[0].root_window, current, 0,
                    0, 0, 0, 0, state, true,
                );
                let _ = conn.event_tx.send(evt_data);
            }
            return;
        }
        current = parent;
    }
}

/// Send IME committed text as X11 key events using Unicode keysyms.
///
/// This is the same approach XQuartz uses:
/// - ASCII chars use their normal X11 keycodes directly
/// - Non-ASCII chars use Unicode keysyms (0x01000000 + Unicode codepoint)
/// - XLookupString returns 0 bytes for Unicode keysyms (keysym > 0xFF)
/// - xterm in UTF-8 mode (OPT_WIDE_CHARS) detects nbytes==0 && keysym >= 0x01000100
///   and converts the Unicode keysym directly to UTF-8
///
/// Requires xterm to be launched with UTF-8 locale (LANG=ja_JP.UTF-8).
fn send_ime_text(server: &XServer, window: Xid, text: &str) {
    const VIRTUAL_BASE: u8 = 200;
    info!("send_ime_text: window=0x{:08x} text='{}'", window, text);

    let focus = server.focus_window.load(Ordering::Relaxed);
    let target = if focus > 1 {
        focus
    } else if focus == 1 {
        find_deepest_child(server, window)
    } else {
        info!("send_ime_text: focus=None, discarding");
        return;
    };

    // Find the connection that selected key events on target (or ancestors)
    let (conn_id, event_window) = {
        let mut current = target;
        let mut result = None;
        for _ in 0..32 {
            if current == 0 { break; }
            if let Some(res) = server.resources.get(&current) {
                if let Resource::Window(win) = res.value() {
                    let w = win.read();
                    for &(cid, emask) in &w.event_selections {
                        if (emask & protocol::event_mask::KEY_PRESS) != 0 {
                            result = Some((cid, current));
                            break;
                        }
                    }
                    if result.is_some() { break; }
                    current = w.parent;
                } else { break; }
            } else { break; }
        }
        match result {
            Some(r) => r,
            None => {
                info!("send_ime_text: no conn with KEY_PRESS mask found for target=0x{:08x}", target);
                return;
            },
        }
    };

    info!("send_ime_text: target=0x{:08x} conn={} event_window=0x{:08x}", target, conn_id, event_window);

    let conn_ref = match server.connections.get(&conn_id) {
        Some(c) => c,
        None => return,
    };
    let conn = conn_ref.value();

    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;

    // Unicode keysym approach: for each non-ASCII character, use keysym 0x01000000 | codepoint
    // on virtual keycodes 200+. Send MappingNotify first, then KeyPress events.
    let chars: Vec<char> = text.chars().collect();
    let mut virtual_idx = 0usize;
    let mut char_keys: Vec<(u8, u16)> = Vec::with_capacity(chars.len());
    const MAX_VIRTUAL: usize = 55;

    {
        let mut vk = server.virtual_keysyms.write();
        vk.clear();
        for &ch in &chars {
            let ch = if ch >= '\u{FF01}' && ch <= '\u{FF5E}' {
                char::from_u32(ch as u32 - 0xFF01 + 0x0021).unwrap_or(ch)
            } else if ch == '\u{3000}' {
                ' '
            } else {
                ch
            };

            if ch.is_ascii() {
                let keycode = ascii_to_x11_keycode(ch as u32);
                let state = if needs_shift(ch) { 0x0001u16 } else { 0u16 };
                char_keys.push((keycode, state));
            } else if virtual_idx < MAX_VIRTUAL {
                let keysym = 0x01000000 | (ch as u32);
                info!("send_ime_text: char '{}' U+{:04X} → keysym 0x{:08X} on keycode {}",
                    ch, ch as u32, keysym, VIRTUAL_BASE + virtual_idx as u8);
                vk.push(keysym);
                char_keys.push((VIRTUAL_BASE + virtual_idx as u8, 0));
                virtual_idx += 1;
            }
        }
    }

    // Send MappingNotify covering all virtual keycodes used
    if virtual_idx > 0 {
        info!("send_ime_text: {} Unicode keysyms on virtual keycodes (200..{})", virtual_idx, 200 + virtual_idx);
        for conn_entry in server.connections.iter() {
            let c = conn_entry.value();
            let mut mapping_notify = [0u8; 32];
            mapping_notify[0] = 34; // MappingNotify
            mapping_notify[4] = 1;  // request = Keyboard
            mapping_notify[5] = VIRTUAL_BASE;
            mapping_notify[6] = virtual_idx as u8;
            let _ = c.event_tx.send(mapping_notify.to_vec());
        }
    }

    // Send KeyPress + KeyRelease for each character
    for &(keycode, state) in &char_keys {
        let press = events::build_key_event(
            conn, protocol::event_type::KEY_PRESS, keycode, time,
            server.screens[0].root_window, event_window, 0,
            0, 0, 0, 0, state, true,
        );
        let _ = conn.event_tx.send(press);

        let release = events::build_key_event(
            conn, protocol::event_type::KEY_RELEASE, keycode, time,
            server.screens[0].root_window, event_window, 0,
            0, 0, 0, 0, state, true,
        );
        let _ = conn.event_tx.send(release);
    }
}

/// Send `count` BackSpace key events to erase preedit text.
/// BackSpace = macOS keycode 51 + 8 = X11 keycode 59.
fn send_backspaces(server: &XServer, window: Xid, count: usize) {
    if count == 0 { return; }
    const BACKSPACE_KEYCODE: u8 = 59; // macOS 51 + 8

    let focus = server.focus_window.load(Ordering::Relaxed);
    let target = if focus > 1 {
        focus
    } else if focus == 1 {
        find_deepest_child(server, window)
    } else {
        return;
    };

    // Find connection with KEY_PRESS mask (same logic as send_ime_text)
    let (conn_id, event_window) = {
        let mut current = target;
        let mut result = None;
        for _ in 0..32 {
            if current == 0 { break; }
            if let Some(res) = server.resources.get(&current) {
                if let Resource::Window(win) = res.value() {
                    let w = win.read();
                    for &(cid, emask) in &w.event_selections {
                        if (emask & protocol::event_mask::KEY_PRESS) != 0 {
                            result = Some((cid, current));
                            break;
                        }
                    }
                    if result.is_some() { break; }
                    current = w.parent;
                } else { break; }
            } else { break; }
        }
        match result {
            Some(r) => r,
            None => return,
        }
    };

    let conn_ref = match server.connections.get(&conn_id) {
        Some(c) => c,
        None => return,
    };
    let conn = conn_ref.value();
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;

    info!("send_backspaces: {} backspaces to conn={} window=0x{:08x}", count, conn_id, event_window);

    for _ in 0..count {
        let press = events::build_key_event(
            conn, protocol::event_type::KEY_PRESS, BACKSPACE_KEYCODE, time,
            server.screens[0].root_window, event_window, 0,
            0, 0, 0, 0, 0, true,
        );
        let _ = conn.event_tx.send(press);

        let release = events::build_key_event(
            conn, protocol::event_type::KEY_RELEASE, BACKSPACE_KEYCODE, time,
            server.screens[0].root_window, event_window, 0,
            0, 0, 0, 0, 0, true,
        );
        let _ = conn.event_tx.send(release);
    }
}

/// Map ASCII character to X11 keycode (reverse of macos_keycode_to_keysym).
/// Returns the X11 keycode (macOS keycode + 8).
fn ascii_to_x11_keycode(ascii: u32) -> u8 {
    // Map of ASCII keysym -> macOS keycode (ANSI US layout)
    let mac_key = match ascii {
        0x61 => 0,  // a
        0x73 => 1,  // s
        0x64 => 2,  // d
        0x66 => 3,  // f
        0x68 => 4,  // h
        0x67 => 5,  // g
        0x7A => 6,  // z
        0x78 => 7,  // x
        0x63 => 8,  // c
        0x76 => 9,  // v
        0x62 => 11, // b
        0x71 => 12, // q
        0x77 => 13, // w
        0x65 => 14, // e
        0x72 => 15, // r
        0x79 => 16, // y
        0x74 => 17, // t
        0x31 => 18, // 1
        0x32 => 19, // 2
        0x33 => 20, // 3
        0x34 => 21, // 4
        0x36 => 22, // 6
        0x35 => 23, // 5
        0x3D => 24, // =
        0x39 => 25, // 9
        0x37 => 26, // 7
        0x2D => 27, // -
        0x38 => 28, // 8
        0x30 => 29, // 0
        0x5D => 30, // ]
        0x6F => 31, // o
        0x75 => 32, // u
        0x5B => 33, // [
        0x69 => 34, // i
        0x70 => 35, // p
        0x6C => 37, // l
        0x6A => 38, // j
        0x27 => 39, // '
        0x6B => 40, // k
        0x3B => 41, // ;
        0x5C => 42, // backslash
        0x2C => 43, // ,
        0x2F => 44, // /
        0x6E => 45, // n
        0x6D => 46, // m
        0x2E => 47, // .
        0x60 => 50, // `
        0x20 => 49, // space
        0x0D | 0x0A => 36, // Return/Enter
        0x09 => 48, // Tab
        0x08 | 0x7F => 51, // Backspace/Delete
        0x1B => 53, // Escape
        // Uppercase -> same key as lowercase
        0x41..=0x5A => return ascii_to_x11_keycode(ascii + 0x20),
        // Shifted symbols -> find base key
        0x21 => 18, // ! (shift+1)
        0x40 => 19, // @ (shift+2)
        0x23 => 20, // # (shift+3)
        0x24 => 21, // $ (shift+4)
        0x5E => 22, // ^ (shift+6)
        0x25 => 23, // % (shift+5)
        0x2B => 24, // + (shift+=)
        0x28 => 25, // ( (shift+9)
        0x26 => 26, // & (shift+7)
        0x5F => 27, // _ (shift+-)
        0x2A => 28, // * (shift+8)
        0x29 => 29, // ) (shift+0)
        0x7D => 30, // } (shift+])
        0x7B => 33, // { (shift+[)
        0x22 => 39, // " (shift+')
        0x3A => 41, // : (shift+;)
        0x7C => 42, // | (shift+\)
        0x3C => 43, // < (shift+,)
        0x3F => 44, // ? (shift+/)
        0x3E => 47, // > (shift+.)
        0x7E => 50, // ~ (shift+`)
        _ => 49, // fallback to space
    };
    (mac_key as u8).wrapping_add(8) // macOS keycode + 8 = X11 keycode
}

/// Returns true if the ASCII character requires Shift modifier to produce.
fn needs_shift(ch: char) -> bool {
    ch.is_ascii_uppercase() || matches!(ch,
        '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '(' | ')' |
        '_' | '+' | '{' | '}' | '|' | ':' | '"' | '<' | '>' | '?' | '~'
    )
}

/// Find the deepest mapped child window under the pointer in the given window tree.
fn find_deepest_child(server: &XServer, window: Xid) -> Xid {
    let mut current = window;
    for _ in 0..16 {
        let children = if let Some(res) = server.resources.get(&current) {
            if let Resource::Window(win) = res.value() {
                let w = win.read();
                if w.mapped {
                    w.children.clone()
                } else { Vec::new() }
            } else { Vec::new() }
        } else { Vec::new() };

        // Find a mapped child that contains the pointer
        let mut found_child = None;
        for &child_id in &children {
            if let Some(res) = server.resources.get(&child_id) {
                if let Resource::Window(win) = res.value() {
                    let w = win.read();
                    if w.mapped {
                        found_child = Some(child_id);
                        break;
                    }
                }
            }
        }

        if let Some(child) = found_child {
            current = child;
        } else {
            break;
        }
    }
    current
}

fn send_enter_leave_event(
    server: &XServer, event_type: u8,
    window: Xid,
    x: i16, y: i16, root_x: i16, root_y: i16,
    state: u16, time: u32,
) {
    let mask_bit = match event_type {
        protocol::event_type::ENTER_NOTIFY => protocol::event_mask::ENTER_WINDOW,
        _ => protocol::event_mask::LEAVE_WINDOW,
    };

    if let Some(res) = server.resources.get(&window) {
        if let Resource::Window(win) = res.value() {
            let w = win.read();
            for &(conn_id, emask) in &w.event_selections {
                if (emask & mask_bit) != 0 {
                    if let Some(conn_ref) = server.connections.get(&conn_id) {
                        let conn = conn_ref.value();
                        let mut evt = events::EventBuilder::new(conn, event_type);
                        evt.set_u8(1, 0) // detail: Ancestor
                           .set_u32(4, time)
                           .set_u32(8, server.screens[0].root_window)
                           .set_u32(12, window)
                           .set_u32(16, 0) // child
                           .set_i16(20, root_x)
                           .set_i16(22, root_y)
                           .set_i16(24, x)
                           .set_i16(26, y)
                           .set_u16(28, state)
                           .set_u8(30, 0) // mode: Normal
                           .set_u8(31, 1); // same-screen + focus: yes
                        let _ = conn.event_tx.send(evt.build());
                    }
                }
            }
        }
    }
}

fn send_focus_event(
    server: &XServer, event_type: u8, window: Xid,
) {
    if let Some(res) = server.resources.get(&window) {
        if let Resource::Window(win) = res.value() {
            let w = win.read();
            for &(conn_id, emask) in &w.event_selections {
                if (emask & protocol::event_mask::FOCUS_CHANGE) != 0 {
                    if let Some(conn_ref) = server.connections.get(&conn_id) {
                        let conn = conn_ref.value();
                        let mut evt = events::EventBuilder::new(conn, event_type);
                        evt.set_u8(1, 0) // detail: Ancestor
                           .set_u32(4, window)
                           .set_u8(8, 0); // mode: Normal
                        let _ = conn.event_tx.send(evt.build());
                    }
                }
            }
        }
    }
}

fn send_configure_notify_event(
    server: &XServer, window: Xid,
    x: i16, y: i16, width: u16, height: u16,
) {
    let (border_width, override_redirect) = if let Some(res) = server.resources.get(&window) {
        if let Resource::Window(win) = res.value() {
            let w = win.read();
            (w.border_width, w.override_redirect)
        } else { (0, false) }
    } else { (0, false) };

    if let Some(res) = server.resources.get(&window) {
        if let Resource::Window(win) = res.value() {
            let w = win.read();
            info!("send_configure_notify: window=0x{:08X} {}x{} event_selections={}",
                  window, width, height, w.event_selections.len());
            let mut sent = false;
            for &(conn_id, emask) in &w.event_selections {
                info!("  conn={} mask=0x{:08X} struct_notify={}",
                      conn_id, emask, (emask & protocol::event_mask::STRUCTURE_NOTIFY) != 0);
                if (emask & protocol::event_mask::STRUCTURE_NOTIFY) != 0 {
                    if let Some(conn_ref) = server.connections.get(&conn_id) {
                        let conn = conn_ref.value();
                        let evt_data = events::build_configure_notify(
                            conn, window, window, 0,
                            x, y, width, height, border_width, override_redirect,
                        );
                        let _ = conn.event_tx.send(evt_data);
                        sent = true;
                    }
                }
            }
            if !sent {
                info!("  No client selected StructureNotify on this window");
            }
        }
    }
}

fn send_expose_event(
    server: &XServer, window: Xid,
    x: u16, y: u16, width: u16, height: u16, count: u16,
) {
    if let Some(res) = server.resources.get(&window) {
        if let Resource::Window(win) = res.value() {
            let w = win.read();
            for &(conn_id, emask) in &w.event_selections {
                if (emask & protocol::event_mask::EXPOSURE) != 0 {
                    if let Some(conn_ref) = server.connections.get(&conn_id) {
                        let conn = conn_ref.value();
                        let evt_data = events::build_expose_event(
                            conn, window, x, y, width, height, count,
                        );
                        let _ = conn.event_tx.send(evt_data);
                    }
                }
            }
        }
    }
}
