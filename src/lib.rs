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
pub mod audio;

/// C entry point for iOS app — called from Swift/ObjC wrapper.
#[cfg(target_os = "ios")]
#[no_mangle]
pub extern "C" fn pslx_start(display_num: u32, tcp_port: u16) {
    use log::info;

    // iOS: log to stderr (console output visible via devicectl --console)
    std::env::set_var("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_string()));
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    // Set locale for correct font selection (Japanese, etc.)
    // iOS simulator doesn't propagate locale env vars automatically.
    std::env::set_var("LANG", "ja_JP.UTF-8");
    std::env::set_var("LC_ALL", "ja_JP.UTF-8");

    info!("Xserver (iOS) starting on display :{}", display_num);

    let (screen_width, screen_height) = display::hidpi::get_screen_dimensions_pixels();

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<display::DisplayCommand>();
    let (evt_tx, evt_rx) = crossbeam_channel::unbounded::<display::DisplayEvent>();
    let render_mailbox: display::RenderMailbox = std::sync::Arc::new(dashmap::DashMap::new());
    let render_mailbox_display = render_mailbox.clone();

    let listen_tcp = tcp_port > 0 || true; // Always listen on TCP for iOS
    let compress_port: Option<u16> = None; // No zstd compression on iOS

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(async {
            // Spawn PulseAudio TCP server (port 4713)
            tokio::spawn(audio::start_pulse_server(4713));

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
