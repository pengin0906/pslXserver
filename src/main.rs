#![allow(dead_code)]

mod display;
mod input;
mod server;
mod util;
mod wm;
mod clipboard;
mod cursor;
mod font;

use clap::Parser;
use log::info;

#[derive(Parser)]
#[command(name = "pslXserver", about = "Native macOS X11 Server — XQuartz alternative")]
struct Cli {
    /// Display number (e.g., 0 for :0)
    #[arg(short = 'd', long, default_value = "0")]
    display: u32,

    /// Listen on TCP as well as Unix socket
    #[arg(long)]
    tcp: bool,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn main() {
    let cli = Cli::parse();

    // Initialize logging
    std::env::set_var("RUST_LOG", &cli.log_level);
    env_logger::init();

    info!("pslXserver starting on display :{}", cli.display);

    // Create channels for communication between Cocoa main thread and tokio thread
    #[allow(unused_variables)]
    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<display::DisplayCommand>(256);
    #[allow(unused_variables)]
    let (evt_tx, evt_rx) = crossbeam_channel::bounded::<display::DisplayEvent>(256);

    let display_num = cli.display;
    let listen_tcp = cli.tcp;

    // Spawn tokio runtime on a background thread
    // macOS requires the main thread for Cocoa/AppKit
    let tokio_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(async {
            if let Err(e) = server::run_server(display_num, listen_tcp, evt_rx, cmd_tx).await {
                log::error!("X11 server error: {}", e);
            }
        });
    });

    // Run Cocoa application on the main thread (macOS requirement)
    #[cfg(target_os = "macos")]
    display::macos::run_cocoa_app(cmd_rx, evt_tx);

    #[cfg(not(target_os = "macos"))]
    {
        log::warn!("Non-macOS platform: running in headless mode (no display backend)");
        // On non-macOS, just run the server without display
        // Useful for protocol testing
        tokio_handle.join().expect("Tokio thread panicked");
    }

    #[cfg(target_os = "macos")]
    tokio_handle.join().expect("Tokio thread panicked");
}
