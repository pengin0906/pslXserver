#!/bin/bash
# Build and run Xserver as a proper .app bundle
# This ensures macOS treats it as a real GUI app, enabling proper focus management
# IMPORTANT: Must launch via 'open Xserver.app' for correct screen resolution detection.
#            Direct binary execution (nohup target/release/Xserver) causes wrong resolution.

set -e
cd "$(dirname "$0")"
~/.cargo/bin/cargo build --release

# Update .app bundle binary
mkdir -p Xserver.app/Contents/MacOS
cp target/release/Xserver Xserver.app/Contents/MacOS/

# Kill old instance if running
pkill -f Xserver.app || true
sleep 0.5

# Launch via 'open' — macOS handles activation policy correctly this way
open Xserver.app --args "$@"
