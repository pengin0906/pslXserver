// macOS display backend — NSWindow/NSView management, NSTextInputClient for IME
// This module only compiles on macOS.

#![cfg(target_os = "macos")]

use crossbeam_channel::{Receiver, Sender};
use log::{debug, info};

use crate::display::{DisplayCommand, DisplayEvent};

/// Run the Cocoa application on the main thread.
/// This function blocks — it IS the main run loop.
pub fn run_cocoa_app(
    cmd_rx: Receiver<DisplayCommand>,
    evt_tx: Sender<DisplayEvent>,
) {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;

    let mtm = MainThreadMarker::new()
        .expect("Must be called from the main thread");

    let app = NSApplication::sharedApplication(mtm);

    info!("Cocoa application initialized");

    // TODO: Set up NSApplication delegate
    // TODO: Create a timer/source to poll cmd_rx and dispatch DisplayCommands
    // TODO: Create X11View NSView subclass with NSTextInputClient

    // For now, just run the application (blocks forever)
    unsafe {
        app.run();
    }
}
