pub mod atoms;
pub mod connection;
pub mod events;
pub mod extensions;
pub mod protocol;
pub mod resources;
pub mod xim;

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use dashmap::DashMap;
use log::{info, debug};
use unicode_width::UnicodeWidthChar;
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

impl ServerError {
    /// Map this error to the appropriate X11 error code.
    pub fn x11_error_code(&self) -> u8 {
        match self {
            ServerError::Protocol => 1,            // BadRequest
            ServerError::ResourceNotFound(_) => 9,  // BadDrawable (generic for missing resources)
            ServerError::AtomNotFound => 5,         // BadAtom
            ServerError::NotImplemented => 17,      // BadImplementation
            ServerError::Io(_) => 17,               // BadImplementation
        }
    }
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
    /// Flag: server is waiting for X11 selection data to copy to macOS clipboard.
    pub pending_clipboard_copy: AtomicBool,
    /// Current keyboard modifier state (updated on each key event).
    pub modifier_state: AtomicU16,
    /// Available font names (XLFD) loaded from fonts.dir/fonts.alias files.
    pub font_names: Vec<String>,
    /// Pre-lowercased font names for fast pattern matching.
    pub font_names_lower: Vec<String>,
    /// XIM (X Input Method) server for inline preedit in GTK/Electron apps.
    pub xim: xim::XimServer,
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
            xi2_event_selections: Vec::new(),
            native_window: None,
            backing_buffer: None,
            ime_spot: None,
            ime_focus: false,
        };
        resources.insert(
            root_window_id,
            resources::Resource::Window(Arc::new(parking_lot::RwLock::new(root_win))),
        );

        let xim = xim::XimServer::new(&atoms, root_window_id);

        let mut server = Self {
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
            virtual_keysyms: parking_lot::RwLock::new({
                // Pre-populate hiragana keysyms (86 chars) — leaves 33 slots for kanji
                let mut v = Vec::with_capacity(119);
                // All hiragana U+3041-U+3096 (86 chars: ぁ-ゖ)
                for cp in 0x3041u32..=0x3096u32 {
                    v.push(0x01000000 | cp);
                }
                v
            }),
            pending_clipboard_copy: AtomicBool::new(false),
            modifier_state: AtomicU16::new(0),
            font_names_lower: Vec::new(), // set below
            font_names: Self::load_font_names(),
            xim,
        };
        server.font_names_lower = server.font_names.iter().map(|n| n.to_lowercase()).collect();
        server
    }

    /// Return the minimal set of font names pslXserver advertises.
    /// We use CoreText for all rendering, so these are just names clients can OpenFont.
    fn load_font_names() -> Vec<String> {
        // Only advertise fonts we actually handle. 4600+ system fonts cause
        // huge ListFonts responses that slow xterm startup significantly.
        // All fonts are iso10646 (Unicode BMP). No iso8859 — pslXserver uses
        // CoreText for all rendering, so every font can display CJK.
        let names: &[&str] = &[
            "fixed",
            "cursor",
            "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso10646-1",
            "-misc-fixed-medium-r-semicondensed--13-120-75-75-c-60-iso10646-1",
            "-misc-fixed-medium-r-normal--14-130-75-75-c-70-iso10646-1",
            "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso10646-1",
        ];
        names.iter().map(|s| s.to_string()).collect()
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
    compress_port: Option<u16>,
    evt_rx: Receiver<DisplayEvent>,
    cmd_tx: Sender<DisplayCommand>,
    screen_width: u16,
    screen_height: u16,
    render_mailbox: crate::display::RenderMailbox,
) -> Result<(), ServerError> {
    let mut server = XServer::new(display_number, cmd_tx, screen_width, screen_height);
    server.render_mailbox = render_mailbox;

    // Set up XIM server advertisement:
    // 1. Set XIM_SERVERS property on root window with @server=pslx atom
    // 2. Own the @server=pslx selection
    {
        let root_id = server.screens[0].root_window;
        let xim_servers_atom = server.xim.atoms.xim_servers;
        let server_atom = server.xim.atoms.server_atom;

        if let Some(mut res) = server.resources.get_mut(&root_id) {
            if let resources::Resource::Window(ref w) = res.value() {
                let mut w = w.write();
                // XIM_SERVERS property: array of ATOM containing @server=pslx
                w.properties.push(resources::Property {
                    name: xim_servers_atom,
                    type_atom: 4, // ATOM
                    format: 32,
                    data: server_atom.to_le_bytes().to_vec(),
                });
            }
        }
        // Own the @server=pslx selection
        server.selections.insert(server_atom, (root_id, server.startup_time));
        info!("XIM: advertised @server=pslx on root window, owned selection atom={}", server_atom);
    }

    let server = Arc::new(server);

    // Spawn event dispatch task: routes DisplayEvents from macOS to X11 clients
    {
        let server_clone = Arc::clone(&server);
        tokio::spawn(async move {
            dispatch_events(server_clone, evt_rx).await;
        });
    }

    // Create Unix domain socket (skip on iOS — no /tmp access)
    #[cfg(not(target_os = "ios"))]
    let listener = {
        let socket_dir = "/tmp/.X11-unix";
        let socket_path = format!("{}/X{}", socket_dir, display_number);

        // Ensure the socket directory exists
        std::fs::create_dir_all(socket_dir).ok();

        // Remove stale socket file
        std::fs::remove_file(&socket_path).ok();

        let listener = UnixListener::bind(&socket_path)?;
        info!("Listening on {}", socket_path);

        // Set socket permissions to world-readable/writable
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o777))?;
        }
        Some(listener)
    };
    #[cfg(target_os = "ios")]
    let listener: Option<UnixListener> = {
        info!("iOS: skipping Unix domain socket, TCP only");
        None
    };

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
                        let _ = stream.set_nodelay(true);
                        // Enlarge TCP receive buffer for large PutImage transfers (4MB)
                        unsafe {
                            use std::os::unix::io::AsRawFd;
                            let fd = stream.as_raw_fd();
                            let buf_size: i32 = 32 * 1024 * 1024;
                            // SOL_SOCKET=0xffff, SO_RCVBUF=0x1002 on macOS
                            extern "C" { fn setsockopt(socket: i32, level: i32, option_name: i32, option_value: *const std::ffi::c_void, option_len: u32) -> i32; }
                            setsockopt(fd, 0xffff, 0x1002, &buf_size as *const _ as *const std::ffi::c_void, 4);
                        }
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

    // Optional zstd-compressed TCP listener
    if let Some(cport) = compress_port {
        let tcp_listener = tokio::net::TcpListener::bind(("0.0.0.0", cport)).await?;
        info!("Listening for zstd-compressed connections on TCP port {}", cport);

        let server_clone = Arc::clone(&server);
        tokio::spawn(async move {
            loop {
                match tcp_listener.accept().await {
                    Ok((stream, addr)) => {
                        let _ = stream.set_nodelay(true);
                        // Large receive buffer
                        unsafe {
                            use std::os::unix::io::AsRawFd;
                            let fd = stream.as_raw_fd();
                            let buf_size: i32 = 32 * 1024 * 1024;
                            extern "C" { fn setsockopt(socket: i32, level: i32, option_name: i32, option_value: *const std::ffi::c_void, option_len: u32) -> i32; }
                            setsockopt(fd, 0xffff, 0x1002, &buf_size as *const _ as *const std::ffi::c_void, 4);
                        }
                        let server = Arc::clone(&server_clone);
                        let conn_id = server.next_conn_id();
                        info!("New zstd-compressed client (id={}) from {}", conn_id, addr);
                        tokio::spawn(async move {
                            if let Err(e) = connection::handle_compressed_connection(server, stream, conn_id).await {
                                log::error!("Compressed connection {} error: {}", conn_id, e);
                            }
                            info!("Compressed connection {} closed", conn_id);
                        });
                    }
                    Err(e) => log::error!("Compressed TCP accept error: {}", e),
                }
            }
        });
    }

    // Create reverse mapping: native_window_id -> X11 window ID
    // This is needed to route display events back to the right X11 window.
    // For simplicity, we add a helper to XServer.

    // Accept Unix socket connections (or just keep alive on iOS where there's no Unix socket)
    if let Some(listener) = listener {
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
    } else {
        // iOS: no Unix socket, just keep the server alive via TCP
        info!("Running in TCP-only mode (no Unix socket)");
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }
}

/// Dispatch events from the macOS display backend to X11 clients.
async fn dispatch_events(server: Arc<XServer>, evt_rx: Receiver<DisplayEvent>) {
    info!("Event dispatch task started");
    let mut focused_window: Xid = 0;
    let mut entered_window: Xid = 0;
    let mut preedit_char_count: usize = 0; // char count for ImeCommit/Done BS
    let mut preedit_col_count: usize = 0;  // col count for ImePreeditDraw BS
    let mut preedit_text = String::new();   // current preedit text for incremental diff
    let mut preedit_injected: bool = false; // whether preedit was sent as raw keys (xterm only)
    // Implicit pointer grab: when a button is pressed, the target window
    // receives all subsequent MotionNotify/ButtonRelease until all buttons are released.
    // This is essential for xterm text selection + scroll-back.
    let mut grab_window: Xid = 0;       // 0 = no grab
    let mut grab_offset_x: i16 = 0;     // top-level coord - grab window coord
    let mut grab_offset_y: i16 = 0;
    let mut buttons_pressed: u8 = 0;    // count of currently pressed buttons
    let mut last_cursor_type: u8 = 0;   // track current macOS cursor to avoid redundant updates
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
                // Find deepest child window at the click point
                let (target, cx, cy) = find_child_at_point(&server, window, x, y);
                // Establish implicit pointer grab on first button press
                if buttons_pressed == 0 {
                    grab_window = target;
                    grab_offset_x = x - cx;
                    grab_offset_y = y - cy;
                    log::debug!("Implicit grab: window=0x{:08x} offset=({},{})", target, grab_offset_x, grab_offset_y);
                }
                buttons_pressed = buttons_pressed.saturating_add(1);
                // Send LeaveNotify to old window, EnterNotify to new window
                if entered_window != target {
                    if entered_window != 0 {
                        send_enter_leave_event(&server, protocol::event_type::LEAVE_NOTIFY,
                            entered_window, 0, 0, root_x, root_y, state, time);
                    }
                    send_enter_leave_event(&server, protocol::event_type::ENTER_NOTIFY,
                        target, cx, cy, root_x, root_y, state, time);
                    entered_window = target;
                }
                // Sync local focused_window with global (may have changed via SetInputFocus/MapWindow)
                let global_focus = server.focus_window.load(Ordering::Relaxed);
                if global_focus > 1 {
                    focused_window = global_focus;
                }
                // Only change focus when clicking on a different top-level window.
                // Within the same top-level, let the app manage focus via SetInputFocus.
                let target_toplevel = find_toplevel(&server, target);
                let focused_toplevel = if focused_window > 1 { find_toplevel(&server, focused_window) } else { 0 };
                if focused_toplevel != target_toplevel {
                    if focused_window > 1 {
                        send_focus_event(&server, protocol::event_type::FOCUS_OUT, focused_window);
                    }
                    send_focus_event(&server, protocol::event_type::FOCUS_IN, target_toplevel);
                    focused_window = target_toplevel;
                    // Update global focus so send_key_event routes to clicked window
                    server.focus_window.store(target_toplevel, Ordering::Relaxed);
                }
                // Update cursor on click (in case MotionNotify didn't fire first)
                let desired_cursor = determine_cursor(&server, target, cx, cy);
                if desired_cursor != last_cursor_type {
                    last_cursor_type = desired_cursor;
                    if let Some(handle) = find_native_handle_for_event(&server, window) {
                        let _ = server.display_cmd_tx.send(
                            DisplayCommand::SetWindowCursor { handle, cursor_type: desired_cursor }
                        );
                    }
                }

                debug!("BTN_PRESS: src=0x{:08x} target=0x{:08x} btn={} ({},{}) root=({},{}) state=0x{:04x} grab=0x{:08x}", window, target, button, cx, cy, root_x, root_y, state, grab_window);
                send_button_event(&server, protocol::event_type::BUTTON_PRESS,
                    target, button, cx, cy, root_x, root_y, state, time);
            }
            DisplayEvent::ButtonRelease { window, button, x, y, root_x, root_y, state, time } => {
                // During implicit grab, send to grab window with adjusted coords
                let (target, cx, cy) = if grab_window != 0 {
                    (grab_window, x - grab_offset_x, y - grab_offset_y)
                } else {
                    find_child_at_point(&server, window, x, y)
                };
                debug!("BTN_RELEASE: src=0x{:08x} target=0x{:08x} btn={} ({},{}) root=({},{}) state=0x{:04x}", window, target, button, cx, cy, root_x, root_y, state);
                send_button_event(&server, protocol::event_type::BUTTON_RELEASE,
                    target, button, cx, cy, root_x, root_y, state, time);
                // Release implicit grab when all buttons are released
                buttons_pressed = buttons_pressed.saturating_sub(1);
                if buttons_pressed == 0 {
                    log::debug!("Implicit grab released (was 0x{:08x})", grab_window);
                    grab_window = 0;
                }
            }
            DisplayEvent::MotionNotify { window, x, y, root_x, root_y, state, time } => {
                // Update stored pointer position for QueryPointer
                server.pointer_x.store(root_x as i32, Ordering::Relaxed);
                server.pointer_y.store(root_y as i32, Ordering::Relaxed);
                // Watchdog: if implicit grab is active but macOS reports no buttons pressed,
                // force-release the grab (handles any missed ButtonRelease events)
                if grab_window != 0 && (state & 0x1F00) == 0 {
                    log::info!("Implicit grab watchdog: releasing stuck grab on 0x{:08x} (no buttons in state=0x{:04x})", grab_window, state);
                    grab_window = 0;
                    buttons_pressed = 0;
                }
                // During implicit grab, send directly to grab window
                let (target, cx, cy) = if grab_window != 0 {
                    let cx = x - grab_offset_x;
                    let cy = y - grab_offset_y;
                    if cy < 0 || cx < 0 {
                        log::debug!("Grab motion OOB: win=0x{:08x} cx={} cy={} state=0x{:04x}", grab_window, cx, cy, state);
                    }
                    (grab_window, cx, cy)
                } else {
                    find_child_at_point(&server, window, x, y)
                };
                server.window_pointer.insert(target, (cx, cy));
                if entered_window != target {
                    if entered_window != 0 {
                        send_enter_leave_event(&server, protocol::event_type::LEAVE_NOTIFY,
                            entered_window, 0, 0, root_x, root_y, state, time);
                    }
                    send_enter_leave_event(&server, protocol::event_type::ENTER_NOTIFY,
                        target, cx, cy, root_x, root_y, state, time);
                    entered_window = target;
                }
                // Update macOS cursor based on window cursor attribute + edge zone
                let desired_cursor = determine_cursor(&server, target, cx, cy);
                if desired_cursor != last_cursor_type {
                    last_cursor_type = desired_cursor;
                    if let Some(handle) = find_native_handle_for_event(&server, window) {
                        let _ = server.display_cmd_tx.send(
                            DisplayCommand::SetWindowCursor { handle, cursor_type: desired_cursor }
                        );
                    }
                }
                send_motion_event(&server, target, cx, cy, root_x, root_y, state, time);
            }
            DisplayEvent::KeyPress { window, keycode, state, time } => {
                server.modifier_state.store(state, Ordering::Relaxed);
                send_key_event(&server, protocol::event_type::KEY_PRESS, window, keycode, state, time);
            }
            DisplayEvent::KeyRelease { window, keycode, state, time } => {
                server.modifier_state.store(state, Ordering::Relaxed);
                send_key_event(&server, protocol::event_type::KEY_RELEASE, window, keycode, state, time);
            }
            DisplayEvent::ImeCommit { window, text } => {
                // Try XIM first (for GTK/Electron apps with XIM connections)
                let focus = server.focus_window.load(Ordering::Relaxed);
                let target = if focus > 1 { focus } else { window };
                eprintln!("ImeCommit: focus=0x{:x} window=0x{:x} target=0x{:x} text='{}' preedit_injected={}", focus, window, target, text, preedit_injected);
                if server.xim.has_xim_client(&server, target) {
                    server.xim.send_preedit_done(&server, target);
                    server.xim.send_commit(&server, target, &text);
                    info!("ImeCommit via XIM: '{}' to window 0x{:08x}", text, target);
                } else if preedit_injected {
                    // xterm: preedit was injected inline — erase it and send committed text
                    if !preedit_text.is_empty() && text.starts_with(&*preedit_text) {
                        // Incremental: only send the new suffix (preedit already in xterm)
                        let suffix = &text[preedit_text.len()..];
                        if !suffix.is_empty() {
                            send_ime_text(&server, target, suffix).await;
                        }
                    } else {
                        if preedit_char_count > 0 {
                            send_backspaces(&server, target, preedit_char_count);
                        }
                        send_ime_text(&server, target, &text).await;
                    }
                } else {
                    // Non-xterm (Chrome etc.): preedit was NOT injected — send full committed text
                    send_ime_text(&server, target, &text).await;
                }
                preedit_text.clear();
                preedit_char_count = 0;
                preedit_col_count = 0;
                preedit_injected = false;
            }
            DisplayEvent::ImePreeditDraw { window, text, .. } => {
                let new_count = text.chars().count();
                let focus = server.focus_window.load(Ordering::Relaxed);
                let target = if focus > 1 { focus } else { window };

                if server.xim.has_xim_client(&server, target) {
                    // Send preedit via XIM protocol
                    if preedit_char_count == 0 && new_count > 0 {
                        server.xim.send_preedit_start(&server, target);
                    }
                    if new_count > 0 {
                        server.xim.send_preedit_draw(&server, target, &text, new_count as u32);
                    } else if preedit_char_count > 0 {
                        server.xim.send_preedit_done(&server, target);
                    }
                } else {
                    // Non-XIM clients: only inject preedit inline into xterm.
                    // Chrome/browsers use macOS native floating IME for display.
                    // Injecting preedit chars into Chrome URL bar triggers autocomplete,
                    // breaking BackSpace and cursor behaviour.
                    if window_is_xterm(&server, target) {
                        // xterm: inline preedit via BS + KeyPress
                        // Incremental: if new text extends old, only send the appended suffix
                        if !preedit_text.is_empty() && text.starts_with(&*preedit_text) {
                            let suffix = &text[preedit_text.len()..];
                            if !suffix.is_empty() {
                                send_ime_text(&server, target, suffix).await;
                            }
                        } else {
                            // Full erase + resend (conversion, deletion, or first char)
                            if preedit_char_count > 0 {
                                send_backspaces(&server, target, preedit_char_count);
                            }
                            if !text.is_empty() {
                                send_ime_text(&server, target, &text).await;
                            }
                        }
                        preedit_char_count = new_count;
                        preedit_injected = true;
                    } else {
                        // Non-xterm: track state only, no raw key injection
                        preedit_injected = false;
                    }
                }

                preedit_col_count = preedit_display_cols(&text);
                preedit_text = text;
            }
            DisplayEvent::ImeReplace { window, erase_chars, text } => {
                // Reconversion: erase original committed text, then insert converted text.
                let focus = server.focus_window.load(Ordering::Relaxed);
                let target = if focus > 1 { focus } else { window };
                if erase_chars > 0 {
                    send_backspaces(&server, target, erase_chars);
                }
                send_ime_text(&server, target, &text).await;
                preedit_text.clear();
                preedit_char_count = 0;
                preedit_col_count = 0;
                preedit_injected = false;
            }
            DisplayEvent::ImePreeditDone { window } => {
                let focus = server.focus_window.load(Ordering::Relaxed);
                let target = if focus > 1 { focus } else { window };
                if server.xim.has_xim_client(&server, target) && preedit_char_count > 0 {
                    server.xim.send_preedit_done(&server, target);
                } else if preedit_injected && preedit_char_count > 0 {
                    // xterm: erase injected preedit on cancel
                    send_backspaces(&server, target, preedit_char_count);
                }
                preedit_text.clear();
                preedit_char_count = 0;
                preedit_col_count = 0;
                preedit_injected = false;
            }
            DisplayEvent::Expose { window, x, y, width, height, count } => {
                send_expose_event(&server, window, x, y, width, height, count);
                // Also propagate Expose to all descendant windows with ExposureMask
                fn expose_descendants(server: &XServer, parent: Xid, x: u16, y: u16, width: u16, height: u16) {
                    let children = if let Some(res) = server.resources.get(&parent) {
                        if let Resource::Window(win) = res.value() {
                            win.read().children.clone()
                        } else { Vec::new() }
                    } else { Vec::new() };
                    for child_id in &children {
                        send_expose_event(server, *child_id, x, y, width, height, 0);
                        expose_descendants(server, *child_id, x, y, width, height);
                    }
                }
                expose_descendants(&server, window, x, y, width, height);
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

                // macOS window size = top-level X11 window size = direct children size
                // Only resize direct children (1 level), app manages its own descendants
                for child_id in &children {
                    if let Some(res) = server.resources.get(child_id) {
                        if let Resource::Window(win) = res.value() {
                            let mut w = win.write();
                            w.width = width;
                            w.height = height;
                        }
                    }
                    send_configure_notify_event(&server, *child_id, 0, 0, width, height);
                    send_expose_event(&server, *child_id, 0, 0, width, height, 0);
                }
            }
            DisplayEvent::GlobalPointerUpdate { root_x, root_y } => {
                server.pointer_x.store(root_x as i32, Ordering::Relaxed);
                server.pointer_y.store(root_y as i32, Ordering::Relaxed);
            }
            DisplayEvent::ClipboardCopyRequest { window: _ } => {
                // Cmd+C: grab X11 PRIMARY selection data and copy to macOS clipboard
                use crate::server::atoms::predefined;
                let primary = predefined::PRIMARY;
                if let Some(entry) = server.selections.get(&primary) {
                    let (owner, _ts) = *entry;
                    info!("ClipboardCopyRequest: PRIMARY owner=0x{:08x}, requesting data", owner);
                    // Find the connection that owns this window
                    for conn_entry in server.connections.iter() {
                        let c = conn_entry.value();
                        if (owner & !c.resource_id_mask) == (c.resource_id_base & !c.resource_id_mask) {
                            // Intern UTF8_STRING and _PSLX_CLIP atoms
                            let utf8_atom = server.atoms.intern("UTF8_STRING", false).unwrap_or(31);
                            let clip_prop = server.atoms.intern("_PSLX_CLIP", false).unwrap_or(100);
                            let root = server.screens[0].root_window;

                            // Send SelectionRequest to the owner
                            let time = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u32;
                            let mut event = [0u8; 32];
                            event[0] = 30; // SelectionRequest
                            crate::server::connection::write_u32_at(c, &mut event, 4, time);
                            crate::server::connection::write_u32_at(c, &mut event, 8, owner);
                            crate::server::connection::write_u32_at(c, &mut event, 12, root); // requestor = root
                            crate::server::connection::write_u32_at(c, &mut event, 16, primary);
                            crate::server::connection::write_u32_at(c, &mut event, 20, utf8_atom); // target
                            crate::server::connection::write_u32_at(c, &mut event, 24, clip_prop); // property
                            let _ = c.event_tx.send(event.into());

                            // Mark that we're waiting for selection data
                            server.pending_clipboard_copy.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                } else {
                    info!("ClipboardCopyRequest: no PRIMARY owner");
                }
            }
            DisplayEvent::FocusIn { window } => {
                // macOS window became key — update X11 focus to match
                let old_focus = server.focus_window.load(Ordering::Relaxed);
                let new_focus = find_toplevel(&server, window);
                if new_focus > 1 && new_focus != old_focus {
                    if old_focus > 1 {
                        send_focus_event(&server, protocol::event_type::FOCUS_OUT, old_focus);
                    }
                    server.focus_window.store(new_focus, Ordering::Relaxed);
                    send_focus_event(&server, protocol::event_type::FOCUS_IN, new_focus);
                    info!("FocusIn from macOS: focus_window -> 0x{:08x}", new_focus);
                }
            }
            _ => {
                log::debug!("Unhandled display event: {:?}", evt);
            }
        }
    }
}

pub fn send_button_event(
    server: &XServer, event_type: u8,
    window: Xid, button: u8,
    x: i16, y: i16, root_x: i16, root_y: i16,
    state: u16, time: u32,
) {
    let mask_bit = match event_type {
        protocol::event_type::BUTTON_PRESS => protocol::event_mask::BUTTON_PRESS,
        _ => protocol::event_mask::BUTTON_RELEASE,
    };

    log::debug!("send_button_event: window=0x{:08x} button={} type={} mask_bit=0x{:08x}",
        window, button, event_type, mask_bit);

    // X11 spec: button events propagate up the window hierarchy until a client
    // has selected the event type on the window.
    let mut current = window;
    let mut event_x = x;
    let mut event_y = y;
    let mut child: Xid = 0; // child of event window that contains the source
    for _ in 0..32 {
        if current == 0 { break; }
        if let Some(res) = server.resources.get(&current) {
            if let Resource::Window(win) = res.value() {
                let w = win.read();
                for &(conn_id, emask) in &w.event_selections {
                    if (emask & mask_bit) != 0 {
                        debug!("  BTN_DELIVER: conn={} win=0x{:08x} from=0x{:08x} type={}", conn_id, current, window, event_type);
                        if let Some(conn_ref) = server.connections.get(&conn_id) {
                            let conn = conn_ref.value();
                            let mut evt = events::EventBuilder::new(conn, event_type);
                            evt.set_u8(1, button)
                               .set_u32(4, time)
                               .set_u32(8, server.screens[0].root_window)
                               .set_u32(12, current)
                               .set_u32(16, child)
                               .set_i16(20, root_x)
                               .set_i16(22, root_y)
                               .set_i16(24, event_x)
                               .set_i16(26, event_y)
                               .set_u16(28, state)
                               .set_u8(30, 1); // same-screen
                            let _ = conn.event_tx.send(evt.build().into());
                        }
                        return;
                    }
                }
                // Not selected here — propagate to parent, adjusting coordinates
                child = current;
                event_x += w.x as i16;
                event_y += w.y as i16;
                current = w.parent;
            } else { break; }
        } else { break; }
    }
    debug!("  BTN_NOT_DELIVERED: mask=0x{:08x} last_win=0x{:08x} orig_win=0x{:08x}", mask_bit, current, window);
}

pub fn send_motion_event(
    server: &XServer, window: Xid,
    x: i16, y: i16, root_x: i16, root_y: i16,
    state: u16, time: u32,
) {
    fn motion_mask_matches(emask: u32, state: u16) -> bool {
        (emask & protocol::event_mask::POINTER_MOTION) != 0
            || ((emask & protocol::event_mask::BUTTON_MOTION) != 0 && (state & 0x1f00) != 0)
            || ((emask & protocol::event_mask::BUTTON1_MOTION) != 0 && (state & 0x100) != 0)
            || ((emask & protocol::event_mask::BUTTON2_MOTION) != 0 && (state & 0x200) != 0)
            || ((emask & protocol::event_mask::BUTTON3_MOTION) != 0 && (state & 0x400) != 0)
            || ((emask & protocol::event_mask::BUTTON4_MOTION) != 0 && (state & 0x800) != 0)
            || ((emask & protocol::event_mask::BUTTON5_MOTION) != 0 && (state & 0x1000) != 0)
    }

    // X11 spec: motion events propagate up the window hierarchy until selected.
    let mut current = window;
    let mut event_x = x;
    let mut event_y = y;
    let mut child: Xid = 0;
    for _ in 0..32 {
        if current == 0 { break; }
        if let Some(res) = server.resources.get(&current) {
            if let Resource::Window(win) = res.value() {
                let w = win.read();
                for &(conn_id, emask) in &w.event_selections {
                    if motion_mask_matches(emask, state) {
                        if let Some(conn_ref) = server.connections.get(&conn_id) {
                            let conn = conn_ref.value();
                            let mut evt = events::EventBuilder::new(conn, protocol::event_type::MOTION_NOTIFY);
                            evt.set_u8(1, 0) // detail: Normal
                               .set_u32(4, time)
                               .set_u32(8, server.screens[0].root_window)
                               .set_u32(12, current)
                               .set_u32(16, child)
                               .set_i16(20, root_x)
                               .set_i16(22, root_y)
                               .set_i16(24, event_x)
                               .set_i16(26, event_y)
                               .set_u16(28, state)
                               .set_u8(30, 1); // same-screen
                            let _ = conn.event_tx.send(evt.build().into());
                        }
                        return;
                    }
                }
                // Not selected here — propagate to parent
                child = current;
                event_x += w.x as i16;
                event_y += w.y as i16;
                current = w.parent;
            } else { break; }
        } else { break; }
    }
}

pub fn send_key_event(
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
        // Explicit focus window set by SetInputFocus (or by macOS FocusIn).
        // Still use find_deepest_child so key events reach the child window
        // that actually selected KEY_PRESS (e.g. xterm's vt100 child).
        let deepest = find_deepest_child(server, window);
        // Only use deepest if it's a descendant of (or equal to) focus window
        if deepest != 0 { deepest } else { focus }
    } else if focus == 1 {
        // PointerRoot: find the deepest child under the pointer in the event window
        find_deepest_child(server, window)
    } else {
        return; // focus=None, discard key events
    };

    // Walk up from the target window, looking for a window that selected key events.
    // X11 spec: key events propagate from focus window up to root.
    // Track 'child' field: the direct child of the event window that is an ancestor of
    // (or is) the source/focus window.
    let mut current = target;
    let mut prev = 0u32; // previous window in walk-up chain (becomes 'child' field)
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

        // child = 0 if event delivered to the focus window itself,
        // otherwise the direct child of event_window on the path to focus window
        let child = if current == target { 0 } else { prev };
        if let Some(conn_id) = found_conn {
            log::info!("  -> Key delivered to conn {} event=0x{:08x} child=0x{:08x} target=0x{:08x}", conn_id, current, child, target);
            if let Some(conn_ref) = server.connections.get(&conn_id) {
                let conn = conn_ref.value();
                let evt_data = events::build_key_event(
                    conn, event_type, keycode, time,
                    server.screens[0].root_window, current, child,
                    0, 0, 0, 0, state, true,
                );
                let _ = conn.event_tx.send(evt_data.into());
            }
            return;
        }
        prev = current;
        current = parent;
    }
    log::debug!("  -> Key NOT delivered (no KEY_PRESS mask found)");
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
async fn send_ime_text(server: &XServer, window: Xid, text: &str) {
    const VIRTUAL_BASE: u8 = 136;
    info!("send_ime_text: window=0x{:08x} text='{}'", window, text);

    let focus = server.focus_window.load(Ordering::Relaxed);
    // Always find the deepest child under the pointer — focus may be set
    // to a top-level window, but KEY_PRESS mask is on the child (e.g. vt100).
    // If focus==0 (not yet set), use `window` directly (iOS: window from ImeCommit/ShowWindow).
    let target = find_deepest_child(server, if focus > 1 { focus } else { window });
    eprintln!("send_ime_text: focus=0x{:x} window=0x{:x} target=0x{:x} text='{}'",
        focus, window, target, text);

    // Find the connection that selected key events on target (or ancestors)
    // Track child field for proper event propagation
    let (conn_id, event_window, child) = {
        let mut current = target;
        let mut prev = 0u32;
        let mut result = None;
        for _ in 0..32 {
            if current == 0 { break; }
            if let Some(res) = server.resources.get(&current) {
                if let Resource::Window(win) = res.value() {
                    let w = win.read();
                    for &(cid, emask) in &w.event_selections {
                        if (emask & protocol::event_mask::KEY_PRESS) != 0 {
                            let child = if current == target { 0 } else { prev };
                            result = Some((cid, current, child));
                            break;
                        }
                    }
                    if result.is_some() { break; }
                    prev = current;
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

    info!("send_ime_text: target=0x{:08x} conn={} event=0x{:08x} child=0x{:08x}", target, conn_id, event_window, child);

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
    const MAX_VIRTUAL: usize = 119; // keycodes 136-254

    let new_keysyms_added;
    {
        let mut vk = server.virtual_keysyms.write();
        // If slots are full, reset kanji slots so new chars can be registered.
        // This triggers one MappingNotify+50ms, but only happens after 33 unique kanji.
        if vk.len() >= MAX_VIRTUAL {
            vk.truncate(86);
        }
        let prev_len = vk.len();
        for &ch in &chars {
            let ch = if ch >= '\u{FF01}' && ch <= '\u{FF5E}' {
                char::from_u32(ch as u32 - 0xFF01 + 0x0021).unwrap_or(ch)
            } else if ch == '\u{3000}' {
                ' '
            } else if ch == '\u{00A5}' {
                '\\' // Japanese keyboard ¥ → backslash
            } else {
                ch
            };

            if ch.is_ascii() {
                let keycode = ascii_to_x11_keycode(ch as u32);
                let state = if needs_shift(ch) { 0x0001u16 } else { 0u16 };
                char_keys.push((keycode, state));
            } else if vk.len() < MAX_VIRTUAL {
                let keysym = 0x01000000 | (ch as u32);
                // Reuse existing slot if this keysym was registered before
                let slot = if let Some(pos) = vk.iter().position(|&k| k == keysym) {
                    pos
                } else {
                    let pos = vk.len();
                    vk.push(keysym);
                    pos
                };
                info!("send_ime_text: char '{}' U+{:04X} → keysym 0x{:08X} on keycode {}",
                    ch, ch as u32, keysym, VIRTUAL_BASE + slot as u8);
                char_keys.push((VIRTUAL_BASE + slot as u8, 0));
                virtual_idx += 1;
            }
        }
        new_keysyms_added = vk.len() > prev_len;
    }
    let total_virtual = server.virtual_keysyms.read().len();

    // Send MappingNotify only when new keysyms were added (avoids unnecessary round-trips)
    if new_keysyms_added {
        info!("send_ime_text: {} total keysyms on virtual keycodes ({}..{})", total_virtual, VIRTUAL_BASE, VIRTUAL_BASE as usize + total_virtual);
        // Save generation before sending MappingNotify — we wait for it to increment,
        // meaning the client has sent GetKeyboardMapping and fetched the updated keymap.
        let gen_before = conn_ref.mapping_gen.load(std::sync::atomic::Ordering::Acquire);
        for conn_entry in server.connections.iter() {
            let c = conn_entry.value();
            let mut mapping_notify = [0u8; 32];
            mapping_notify[0] = 34; // MappingNotify type
            // [1] = 0 (unused per X11 wire protocol)
            // [2-3] = 0 (sequence number)
            mapping_notify[4] = 1;  // request = MappingKeyboard (1)
            mapping_notify[5] = VIRTUAL_BASE;        // first_keycode
            mapping_notify[6] = total_virtual as u8; // count
            let _ = c.event_tx.send(mapping_notify.into());
        }
        // Wait for client to process MappingNotify → GetKeyboardMapping round-trip.
        // Poll mapping_gen with 1ms intervals; timeout at 200ms to avoid hanging.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
        loop {
            if conn_ref.mapping_gen.load(std::sync::atomic::Ordering::Acquire) != gen_before {
                break; // client fetched new keymap
            }
            if tokio::time::Instant::now() >= deadline {
                // Timeout: client didn't respond, wait a bit anyway to be safe
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    // Send KeyPress + KeyRelease for each character
    for &(keycode, state) in &char_keys {
        let press = events::build_key_event(
            conn, protocol::event_type::KEY_PRESS, keycode, time,
            server.screens[0].root_window, event_window, child,
            0, 0, 0, 0, state, true,
        );
        let _ = conn.event_tx.send(press.into());

        let release = events::build_key_event(
            conn, protocol::event_type::KEY_RELEASE, keycode, time,
            server.screens[0].root_window, event_window, child,
            0, 0, 0, 0, state, true,
        );
        let _ = conn.event_tx.send(release.into());
    }
}

/// Compute the display column width of a preedit string.
/// Uses the unicode-width crate for correct East Asian Width handling.
fn preedit_display_cols(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Send `count` BackSpace key events to erase preedit text.
/// BackSpace = macOS keycode 51 + 8 = X11 keycode 59.
fn send_backspaces(server: &XServer, window: Xid, count: usize) {
    if count == 0 { return; }
    const BACKSPACE_KEYCODE: u8 = 59; // macOS 51 + 8

    let focus = server.focus_window.load(Ordering::Relaxed);
    let target = if focus >= 1 {
        // Always find deepest child — focus may be a top-level window, but
        // KEY_PRESS mask is on the child (e.g. xterm's vt100 widget).
        // Must match send_ime_text's target computation.
        find_deepest_child(server, window)
    } else {
        return;
    };

    // Find connection with KEY_PRESS mask, tracking child field for event propagation
    let (conn_id, event_window, child) = {
        let mut current = target;
        let mut prev = 0u32;
        let mut result = None;
        for _ in 0..32 {
            if current == 0 { break; }
            if let Some(res) = server.resources.get(&current) {
                if let Resource::Window(win) = res.value() {
                    let w = win.read();
                    for &(cid, emask) in &w.event_selections {
                        if (emask & protocol::event_mask::KEY_PRESS) != 0 {
                            let child = if current == target { 0 } else { prev };
                            result = Some((cid, current, child));
                            break;
                        }
                    }
                    if result.is_some() { break; }
                    prev = current;
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

    info!("send_backspaces: {} backspaces to conn={} event=0x{:08x} child=0x{:08x}", count, conn_id, event_window, child);

    for _ in 0..count {
        let press = events::build_key_event(
            conn, protocol::event_type::KEY_PRESS, BACKSPACE_KEYCODE, time,
            server.screens[0].root_window, event_window, child,
            0, 0, 0, 0, 0, true,
        );
        let _ = conn.event_tx.send(press.into());

        let release = events::build_key_event(
            conn, protocol::event_type::KEY_RELEASE, BACKSPACE_KEYCODE, time,
            server.screens[0].root_window, event_window, child,
            0, 0, 0, 0, 0, true,
        );
        let _ = conn.event_tx.send(release.into());
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
/// Find the deepest child window at (x, y) in the parent's coordinate space.
/// Returns (child_id, child_relative_x, child_relative_y).
fn find_child_at_point(server: &XServer, window: Xid, x: i16, y: i16) -> (Xid, i16, i16) {
    let mut current = window;
    let mut cx = x;
    let mut cy = y;
    for depth in 0..16 {
        let children = if let Some(res) = server.resources.get(&current) {
            if let Resource::Window(win) = res.value() {
                let w = win.read();
                w.children.clone()
            } else { Vec::new() }
        } else { Vec::new() };

        if depth == 0 && !children.is_empty() {
            debug!("  find_child_at_point: win=0x{:08x} ({},{}) children={:?}", current, cx, cy, children.iter().map(|c| format!("0x{:08x}", c)).collect::<Vec<_>>());
        }

        let mut found = None;
        // Check children in reverse order (top-most first)
        for &child_id in children.iter().rev() {
            if let Some(res) = server.resources.get(&child_id) {
                if let Resource::Window(win) = res.value() {
                    let w = win.read();
                    if w.mapped {
                        let x1 = w.x as i16;
                        let y1 = w.y as i16;
                        let x2 = x1 + w.width as i16;
                        let y2 = y1 + w.height as i16;
                        if cx >= x1 && cx < x2 && cy >= y1 && cy < y2 {
                            found = Some((child_id, cx - x1, cy - y1));
                            break;
                        }
                    }
                }
            }
        }

        if let Some((child, rx, ry)) = found {
            current = child;
            cx = rx;
            cy = ry;
        } else {
            break;
        }
    }
    (current, cx, cy)
}

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

/// Edge zone width in pixels for resize cursor detection.
const EDGE_ZONE: i16 = 5;

/// Get the effective macOS cursor type for a window, inheriting from parents if not set.
fn get_effective_cursor_type(server: &XServer, window_id: Xid) -> u8 {
    let mut current = window_id;
    for _ in 0..32 {
        if let Some(res) = server.resources.get(&current) {
            if let Resource::Window(win) = res.value() {
                let w = win.read();
                if w.cursor != 0 {
                    if let Some(cres) = server.resources.get(&w.cursor) {
                        if let Resource::Cursor(cursor) = cres.value() {
                            return cursor.macos_type;
                        }
                    }
                    return 0; // cursor ID set but resource gone
                }
                if w.parent == 0 {
                    break;
                }
                current = w.parent;
            } else { break; }
        } else { break; }
    }
    0 // default: arrow
}

/// Check if position is in the edge zone of a window. Returns resize cursor type if so.
/// Only applies to windows with children (container/parent windows) or top-level windows.
fn edge_resize_cursor(server: &XServer, window_id: Xid, x: i16, y: i16) -> Option<u8> {
    if let Some(res) = server.resources.get(&window_id) {
        if let Resource::Window(win) = res.value() {
            let w = win.read();
            let width = w.width as i16;
            let height = w.height as i16;
            // Only detect edges for windows large enough
            if width < EDGE_ZONE * 3 || height < EDGE_ZONE * 3 {
                return None;
            }
            let near_left = x < EDGE_ZONE;
            let near_right = x >= width - EDGE_ZONE;
            let near_top = y < EDGE_ZONE;
            let near_bottom = y >= height - EDGE_ZONE;

            if !near_left && !near_right && !near_top && !near_bottom {
                return None;
            }
            // Corner: use left-right as best approximation (macOS NSCursor has no diagonal)
            if (near_left || near_right) && (near_top || near_bottom) {
                return Some(6); // ResizeLeftRight
            }
            if near_left || near_right {
                return Some(6); // ResizeLeftRight
            }
            return Some(7); // ResizeUpDown
        }
    }
    None
}

/// Find the native window handle for a given X11 window (walks up ancestor chain).
fn find_native_handle_for_event(server: &XServer, wid: Xid) -> Option<crate::display::NativeWindowHandle> {
    let mut current = wid;
    for _ in 0..32 {
        if let Some(res) = server.resources.get(&current) {
            if let Resource::Window(win) = res.value() {
                let w = win.read();
                if let Some(ref handle) = w.native_window {
                    return Some(handle.clone());
                }
                if w.parent == 0 {
                    break;
                }
                current = w.parent;
            } else { break; }
        } else { break; }
    }
    None
}

/// Find the top-level X11 window (direct child of root) for a given window.
/// Check if a window (or its toplevel ancestor) is xterm-like by WM_CLASS.
/// Preedit is injected inline only into xterm; all other apps (Chrome, VS Code, etc.)
/// use the macOS floating IME overlay instead of raw key injection.
fn window_is_xterm(server: &XServer, window: Xid) -> bool {
    let toplevel = find_toplevel(server, window);
    if let Some(res) = server.resources.get(&toplevel) {
        if let Resource::Window(win) = res.value() {
            if let Some(prop) = win.read().get_property(crate::server::atoms::predefined::WM_CLASS) {
                // WM_CLASS data: "instance_name\0class_name\0"
                let data = &prop.data;
                let instance = data.split(|&b| b == 0).next().unwrap_or(&[]);
                return instance.eq_ignore_ascii_case(b"xterm");
            }
        }
    }
    false
}

fn find_toplevel(server: &XServer, wid: Xid) -> Xid {
    let root = server.screens[0].root_window;
    let mut current = wid;
    for _ in 0..32 {
        if current == 0 || current == root { return wid; }
        if let Some(res) = server.resources.get(&current) {
            if let Resource::Window(win) = res.value() {
                let parent = win.read().parent;
                if parent == root {
                    return current;
                }
                current = parent;
            } else { return wid; }
        } else { return wid; }
    }
    wid
}

/// Determine the cursor to display: edge resize cursor overrides window cursor.
fn determine_cursor(server: &XServer, window_id: Xid, x: i16, y: i16) -> u8 {
    // Check edge zone first (visual feedback for window borders)
    if let Some(resize_cursor) = edge_resize_cursor(server, window_id, x, y) {
        return resize_cursor;
    }
    // Fall back to the window's (or inherited) cursor
    get_effective_cursor_type(server, window_id)
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
                        let _ = conn.event_tx.send(evt.build().into());
                    }
                }
            }
        }
    }
}

pub(crate) fn send_focus_event(
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
                        let _ = conn.event_tx.send(evt.build().into());
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
                        let _ = conn.event_tx.send(evt_data.into());
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
            let mut sent = false;
            for &(conn_id, emask) in &w.event_selections {
                if (emask & protocol::event_mask::EXPOSURE) != 0 {
                    if let Some(conn_ref) = server.connections.get(&conn_id) {
                        let conn = conn_ref.value();
                        let evt_data = events::build_expose_event(
                            conn, window, x, y, width, height, count,
                        );
                        let _ = conn.event_tx.send(evt_data.into());
                        sent = true;
                    }
                }
            }
            info!("send_expose: window=0x{:08X} {}x{} sent={}", window, width, height, sent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_error_x11_codes() {
        assert_eq!(ServerError::Protocol.x11_error_code(), 1); // BadRequest
        assert_eq!(ServerError::ResourceNotFound(0x100).x11_error_code(), 9); // BadDrawable
        assert_eq!(ServerError::AtomNotFound.x11_error_code(), 5); // BadAtom
        assert_eq!(ServerError::NotImplemented.x11_error_code(), 17); // BadImplementation
    }

    #[test]
    fn test_server_creation() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let server = XServer::new(0, tx, 1920, 1080);

        // Check screen configuration
        assert_eq!(server.screens.len(), 1);
        assert_eq!(server.screens[0].width_in_pixels, 1920);
        assert_eq!(server.screens[0].height_in_pixels, 1080);
        assert_eq!(server.screens[0].root_depth, 24);
        assert_eq!(server.screens[0].root_visual.class, 4); // TrueColor
        assert_eq!(server.screens[0].white_pixel, 0x00FFFFFF);
        assert_eq!(server.screens[0].black_pixel, 0x00000000);
        assert_eq!(server.screens[0].root_visual.red_mask, 0x00FF0000);
        assert_eq!(server.screens[0].root_visual.green_mask, 0x0000FF00);
        assert_eq!(server.screens[0].root_visual.blue_mask, 0x000000FF);
    }

    #[test]
    fn test_root_window_exists() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let server = XServer::new(0, tx, 800, 600);

        let root_id = server.screens[0].root_window;
        assert!(server.resources.get(&root_id).is_some());

        // Extract values from the borrow scope, then assert outside
        let (width, height, mapped, viewable, depth, parent) = {
            let res = server.resources.get(&root_id).unwrap();
            if let Resource::Window(win) = res.value() {
                let w = win.read();
                (w.width, w.height, w.mapped, w.viewable, w.depth, w.parent)
            } else {
                panic!("Root resource is not a Window");
            }
        };
        assert_eq!(width, 800);
        assert_eq!(height, 600);
        assert!(mapped);
        assert!(viewable);
        assert_eq!(depth, 24);
        assert_eq!(parent, 0);
    }

    #[test]
    fn test_resource_id_allocation() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let server = XServer::new(0, tx, 800, 600);

        let base1 = server.alloc_resource_id_base();
        let base2 = server.alloc_resource_id_base();
        assert_ne!(base1, base2);
        assert_eq!(base2 - base1, 0x00200000);
    }

    #[test]
    fn test_connection_id_allocation() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let server = XServer::new(0, tx, 800, 600);

        let id1 = server.next_conn_id();
        let id2 = server.next_conn_id();
        assert_eq!(id2, id1 + 1);
    }

    #[test]
    fn test_display_number() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let server = XServer::new(42, tx, 800, 600);
        assert_eq!(server.display_number, 42);
    }

    #[test]
    fn test_truecolor_pixel_format() {
        // TrueColor pixel format: (R8 << 16) | (G8 << 8) | B8
        let red_pixel: u32 = 0xFF << 16;
        let green_pixel: u32 = 0xFF << 8;
        let blue_pixel: u32 = 0xFF;
        let white_pixel: u32 = red_pixel | green_pixel | blue_pixel;

        assert_eq!(red_pixel, 0x00FF0000);
        assert_eq!(green_pixel, 0x0000FF00);
        assert_eq!(blue_pixel, 0x000000FF);
        assert_eq!(white_pixel, 0x00FFFFFF);

        // Decompose back to 16-bit
        let r = ((white_pixel >> 16) & 0xFF) as u16;
        let g = ((white_pixel >> 8) & 0xFF) as u16;
        let b = (white_pixel & 0xFF) as u16;
        assert_eq!(r, 0xFF);
        assert_eq!(g, 0xFF);
        assert_eq!(b, 0xFF);

        // Scale to 16-bit (QueryColors format)
        assert_eq!(r << 8 | r, 0xFFFF);
        assert_eq!(g << 8 | g, 0xFFFF);
        assert_eq!(b << 8 | b, 0xFFFF);
    }

    #[test]
    fn test_truecolor_color_roundtrip() {
        // Simulate AllocColor -> QueryColors roundtrip
        let input_r16: u16 = 0xABCD;
        let input_g16: u16 = 0x1234;
        let input_b16: u16 = 0x5678;

        // AllocColor: 16-bit -> 8-bit -> pixel
        let r8 = (input_r16 >> 8) as u32; // 0xAB
        let g8 = (input_g16 >> 8) as u32; // 0x12
        let b8 = (input_b16 >> 8) as u32; // 0x56
        let pixel = (r8 << 16) | (g8 << 8) | b8;
        assert_eq!(pixel, 0x00AB1256);

        // QueryColors: pixel -> 16-bit
        let qr = ((pixel >> 16) & 0xFF) as u16;
        let qg = ((pixel >> 8) & 0xFF) as u16;
        let qb = (pixel & 0xFF) as u16;
        assert_eq!(qr << 8 | qr, 0xABAB);
        assert_eq!(qg << 8 | qg, 0x1212);
        assert_eq!(qb << 8 | qb, 0x5656);
    }

    #[test]
    fn test_focus_defaults() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let server = XServer::new(0, tx, 800, 600);

        // Default focus: PointerRoot (1)
        assert_eq!(server.focus_window.load(Ordering::Relaxed), 1);
        assert_eq!(server.focus_revert_to.load(Ordering::Relaxed), 1);
    }
}
