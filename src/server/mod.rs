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

                // Resize child windows, accounting for their position offsets.
                // Each child's new size = parent's new size - child's (x,y) offset.
                for child_id in &children {
                    if let Some(cres) = server.resources.get(child_id) {
                        if let Resource::Window(cwin) = cres.value() {
                            let mut cw = cwin.write();
                            let child_x = cw.x.max(0) as u16;
                            let child_y = cw.y.max(0) as u16;
                            let new_cw = width.saturating_sub(child_x);
                            let new_ch = height.saturating_sub(child_y);
                            if new_cw > 0 && new_ch > 0 {
                                let old_cw = cw.width;
                                let old_ch = cw.height;
                                cw.width = new_cw;
                                cw.height = new_ch;
                                info!("  Child 0x{:08X} (at {},{}) resize: {}x{} -> {}x{}",
                                      child_id, child_x, child_y, old_cw, old_ch, new_cw, new_ch);
                                drop(cw);
                                send_configure_notify_event(&server, *child_id, child_x as i16, child_y as i16, new_cw, new_ch);
                                send_expose_event(&server, *child_id, 0, 0, new_cw, new_ch, 0);
                            }
                        }
                    }
                }
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
