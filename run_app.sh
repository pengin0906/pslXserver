#!/bin/bash
# Build and run Xerver as a proper .app bundle
# This ensures macOS treats it as a real GUI app, enabling proper focus management

set -e
~/.cargo/bin/cargo build --release

# Update .app bundle binary
mkdir -p Xerver.app/Contents/MacOS
cp target/release/Xerver Xerver.app/Contents/MacOS/

# Kill old instance if running
pkill -f Xerver.app || true
sleep 0.5

# Launch via 'open' — macOS handles activation policy correctly this way
open Xerver.app --args "$@"
