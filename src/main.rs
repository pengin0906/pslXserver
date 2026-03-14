#![allow(dead_code)]

// Use the library crate's modules (defined in lib.rs)
use Xserver::display;
use Xserver::server;

use clap::Parser;
use log::info;

#[derive(Parser)]
#[command(name = "Xserver", about = "Native macOS X11 Server — XQuartz alternative")]
struct Cli {
    /// Display number (e.g., 0 for :0)
    #[arg(short = 'd', long, default_value = "0")]
    display: u32,

    /// Listen on TCP as well as Unix socket
    #[arg(long)]
    tcp: bool,

    /// Screen resolution (e.g., "1920x1080"). Defaults to actual macOS screen size.
    #[arg(long)]
    screen: Option<String>,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "warn")]
    log_level: String,

    /// Listen on this port for zstd-compressed TCP connections (e.g., 6100)
    #[arg(long)]
    compress_port: Option<u16>,
}

fn main() {
    let cli = Cli::parse();

    // Initialize logging
    std::env::set_var("RUST_LOG", &cli.log_level);
    env_logger::init();

    info!("Xserver starting on display :{}", cli.display);

    // Detect screen resolution: CLI override or actual macOS screen size
    let (screen_width, screen_height) = if let Some(ref s) = cli.screen {
        let parts: Vec<&str> = s.split('x').collect();
        if parts.len() == 2 {
            let w: u16 = parts[0].parse().expect("Invalid screen width");
            let h: u16 = parts[1].parse().expect("Invalid screen height");
            (w, h)
        } else {
            panic!("Invalid --screen format. Use WIDTHxHEIGHT (e.g., 1920x1080)");
        }
    } else {
        display::hidpi::get_screen_dimensions_pixels()
    };
    info!("Screen resolution: {}x{}", screen_width, screen_height);

    // Create channels for communication between Cocoa main thread and tokio thread
    #[allow(unused_variables)]
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<display::DisplayCommand>();
    #[allow(unused_variables)]
    let (evt_tx, evt_rx) = crossbeam_channel::unbounded::<display::DisplayEvent>();

    // Shared render mailbox — protocol threads write, display thread reads
    let render_mailbox: display::RenderMailbox = std::sync::Arc::new(dashmap::DashMap::new());
    let render_mailbox_display = render_mailbox.clone();

    let display_num = cli.display;
    let listen_tcp = cli.tcp;
    let compress_port = cli.compress_port;

    // Spawn tokio runtime on a background thread
    // macOS requires the main thread for Cocoa/AppKit
    let tokio_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(async {
            // Spawn PulseAudio TCP server (port 4713)
            tokio::spawn(Xserver::audio::start_pulse_server(4713));

            if let Err(e) = server::run_server(display_num, listen_tcp, compress_port, evt_rx, cmd_tx, screen_width, screen_height, render_mailbox).await {
                log::error!("X11 server error: {}", e);
            }
        });
    });

    // Run UI application on the main thread (Apple platform requirement)
    #[cfg(target_os = "macos")]
    display::macos::run_cocoa_app(cmd_rx, evt_tx, render_mailbox_display);

    #[cfg(target_os = "ios")]
    display::ios::run_ios_app(cmd_rx, evt_tx, render_mailbox_display);

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        log::warn!("Non-Apple platform: running in headless mode (no display backend)");
        tokio_handle.join().expect("Tokio thread panicked");
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    tokio_handle.join().expect("Tokio thread panicked");
}
