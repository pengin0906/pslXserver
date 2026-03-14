// iOS static library wrapper for Xserver.
// This crate symlinks to the main Xserver source and builds as a staticlib for iOS.

#![allow(dead_code)]

// Include main crate source via path attributes
#[path = "../../../src/display/mod.rs"]
pub mod display;
#[path = "../../../src/input/mod.rs"]
pub mod input;
#[path = "../../../src/server/mod.rs"]
pub mod server;
#[path = "../../../src/util/mod.rs"]
pub mod util;
#[path = "../../../src/wm/mod.rs"]
pub mod wm;
#[path = "../../../src/clipboard/mod.rs"]
pub mod clipboard;
#[path = "../../../src/cursor/mod.rs"]
pub mod cursor;
#[path = "../../../src/font/mod.rs"]
pub mod font;

/// C entry point for iOS app.
#[cfg(target_os = "ios")]
#[no_mangle]
pub extern "C" fn pslx_start(display_num: u32, tcp_port: u16) {
    use log::info;

    std::env::set_var("RUST_LOG", "warn");
    env_logger::init();

    info!("Xserver (iOS) starting on display :{}", display_num);

    let (screen_width, screen_height) = display::hidpi::get_screen_dimensions_pixels();

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<display::DisplayCommand>();
    let (evt_tx, evt_rx) = crossbeam_channel::unbounded::<display::DisplayEvent>();
    let render_mailbox: display::RenderMailbox = std::sync::Arc::new(dashmap::DashMap::new());
    let render_mailbox_display = render_mailbox.clone();

    let listen_tcp = true;
    let compress_port = if tcp_port > 0 { Some(tcp_port) } else { None };

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(async {
            if let Err(e) = server::run_server(
                display_num, listen_tcp, compress_port,
                evt_rx, cmd_tx, screen_width, screen_height, render_mailbox,
            ).await {
                log::error!("X11 server error: {}", e);
            }
        });
    });

    display::ios::run_ios_app(cmd_rx, evt_tx, render_mailbox_display);
}
