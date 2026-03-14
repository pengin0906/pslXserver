// pslXserver iOS — Swift entry point
// Calls into the Rust static library to start the X11 server.

import UIKit

// Rust entry point (defined in src/lib.rs)
@_silgen_name("pslx_start")
func pslx_start(_ display_num: UInt32, _ tcp_port: UInt16)

// This is a minimal wrapper. The Rust library handles everything:
// - UIApplicationMain is called from Rust (display/ios.rs)
// - UIWindow, UIView, touch handling, rendering — all in Rust
// - X11 protocol server runs on background threads via tokio

// We call pslx_start which calls UIApplicationMain and never returns.
pslx_start(0, 0) // display :0, TCP port 6000
