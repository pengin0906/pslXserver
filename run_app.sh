#!/bin/bash
# Build and run Xserver as a proper .app bundle
# This ensures macOS treats it as a real GUI app, enabling proper focus management

set -e
~/.cargo/bin/cargo build --release

# Update .app bundle binary
mkdir -p Xserver.app/Contents/MacOS
cp target/release/Xserver Xserver.app/Contents/MacOS/

# Kill old instance if running
pkill -f Xserver.app || true
sleep 0.5

# Launch via 'open' — macOS handles activation policy correctly this way
open Xserver.app --args "$@"
