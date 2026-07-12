#!/bin/bash
# Spike 1 (PLAN.md Section 7, spikes/01-headless-pipewire.md): verify
# PipeWire + WirePlumber boot headless in the container and that a virtual
# source can be linked to a virtual sink via pw-link — the core routing
# mechanism the whole project depends on.
set -euo pipefail

IMAGE="${1:-pw-audio-router:dev}"

docker run --rm --entrypoint bash "$IMAGE" -c '
set -e
export XDG_RUNTIME_DIR=/run/pipewire
mkdir -p "$XDG_RUNTIME_DIR"
eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS

pipewire > /tmp/pw.log 2>&1 &
PWPID=$!
sleep 1
wireplumber > /tmp/wp.log 2>&1 &
WPPID=$!
sleep 3

if ! kill -0 "$PWPID" 2>/dev/null; then
  echo "FAIL: pipewire exited"; cat /tmp/pw.log; exit 1
fi
if ! kill -0 "$WPPID" 2>/dev/null; then
  echo "FAIL: wireplumber exited (check bluetooth.lua/logind config override)"; cat /tmp/wp.log; exit 1
fi
echo "OK: pipewire and wireplumber both stayed up"

pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=test-sink media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }" >/dev/null
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=test-source media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }" >/dev/null
sleep 1

pw-link test-source:capture_FL test-sink:playback_FL
pw-link test-source:capture_FR test-sink:playback_FR

LINKS=$(pw-link -l | grep -c "test-source" || true)
if [ "$LINKS" -lt 2 ]; then
  echo "FAIL: expected links between test-source and test-sink not found"
  pw-link -l
  exit 1
fi
echo "PASS: virtual source linked to virtual sink"
'
