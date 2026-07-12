#!/bin/bash
# Spike 4 addendum: measures native pipewire-rs (official freedesktop.org
# Rust bindings) link create+destroy latency, for direct comparison against
# test_spike04_graph_control_latency.sh's shell-based measurement (~16ms).
# See spikes/04-graph-control.md for the full language comparison
# (Python/shell vs Rust vs Go vs C++) this feeds into.
set -euo pipefail

IMAGE="${1:-pw-audio-router:dev}"
BUILD_DIR="$(dirname "$0")/spike04_rust_poc"

docker build -t pw-rust-spike:dev --build-arg BASE_IMAGE="$IMAGE" -f "$BUILD_DIR/Dockerfile" "$BUILD_DIR"

docker run --rm --entrypoint bash pw-rust-spike:dev -c '
set -e
export XDG_RUNTIME_DIR=/run/pipewire
mkdir -p "$XDG_RUNTIME_DIR"
eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS
pipewire > /tmp/pw.log 2>&1 &
sleep 1
wireplumber > /tmp/wp.log 2>&1 &
sleep 2
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=rust-spike-source media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }" >/dev/null
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=rust-spike-sink media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }" >/dev/null
sleep 1
/spike/target/release/pw_graph_spike
'
