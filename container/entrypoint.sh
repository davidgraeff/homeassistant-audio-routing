#!/bin/bash
set -eu

mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

# pipewire needs a D-Bus session for module-portal etc.; a private bus is
# enough for our headless, network-router use case.
eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS

pipewire &
PIPEWIRE_PID=$!

# Give the daemon a moment to create its socket before wireplumber connects.
for i in $(seq 1 50); do
    [ -S "$XDG_RUNTIME_DIR/pipewire-0" ] && break
    sleep 0.1
done

wireplumber &
WIREPLUMBER_PID=$!

trap 'kill "$PIPEWIRE_PID" "$WIREPLUMBER_PID" 2>/dev/null' TERM INT

wait -n "$PIPEWIRE_PID" "$WIREPLUMBER_PID"
