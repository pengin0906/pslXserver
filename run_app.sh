#!/bin/bash
# Build and run pslXserver as a proper .app bundle
# This ensures macOS treats it as a real GUI app, enabling proper focus management

set -e
~/.cargo/bin/cargo build --release

# Update .app bundle binary
mkdir -p pslXserver.app/Contents/MacOS
cp target/release/pslXserver pslXserver.app/Contents/MacOS/

# Kill old instance if running
pkill -f pslXserver.app || true
sleep 0.5

# Launch via 'open' — macOS handles activation policy correctly this way
open pslXserver.app --args "$@"
