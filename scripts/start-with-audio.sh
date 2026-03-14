#!/bin/bash
# Start Xserver with PulseAudio TCP forwarding for remote browser audio
#
# Usage: ./scripts/start-with-audio.sh [--tcp]
#
# Remote clients should set:
#   export PULSE_SERVER=tcp:<macOS-IP>:4713
# before launching Chrome/Firefox to get audio output through macOS.
#
# Docker clients:
#   docker exec -d <container> bash -c "DISPLAY=192.168.5.2:0 PULSE_SERVER=tcp:192.168.5.2:4713 chromium ..."
#
# SSH clients:
#   ssh user@host "DISPLAY=<mac>:0 PULSE_SERVER=tcp:<mac>:4713 google-chrome-stable ..."

set -e

# Ensure PulseAudio is running with TCP module
if ! pactl info >/dev/null 2>&1; then
    echo "Starting PulseAudio..."
    pulseaudio --start --log-target=syslog
    sleep 1
fi

# Check if TCP module is loaded
if ! pactl list modules short 2>/dev/null | grep -q "module-native-protocol-tcp"; then
    echo "Loading PulseAudio TCP module..."
    pactl load-module module-native-protocol-tcp auth-anonymous=1
fi

echo "PulseAudio TCP ready on port 4713"
echo "Remote clients: export PULSE_SERVER=tcp:$(hostname -s).local:4713"

# Start Xserver
exec ./target/release/Xserver "$@"
