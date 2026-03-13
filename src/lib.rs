// Library target — enables this crate to be used as a dependency and as a staticlib for iOS.
// On macOS, both the binary (main.rs) and this library are built; the binary links the library.

#![allow(dead_code)]

pub mod display;
pub mod input;
pub mod server;
pub mod util;
pub mod wm;
pub mod clipboard;
pub mod cursor;
pub mod font;

/// C entry point for iOS app — called from Swift/ObjC wrapper.
#[cfg(target_os = "ios")]
#[no_mangle]
pub extern "C" fn pslx_start(display_num: u32, tcp_port: u16) {
    use log::info;

    // Redirect env_logger to a file for iOS debugging (line-buffered for immediate flush)
    std::env::set_var("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()));
    let log_file = std::fs::OpenOptions::new()
        .create(true).append(true).open("/tmp/pslx_server.log")
        .expect("Failed to open log file");
    let line_buffered = std::io::LineWriter::new(log_file);
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Pipe(Box::new(line_buffered)))
        .init();

    info!("pslXserver (iOS) starting on display :{}", display_num);

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
