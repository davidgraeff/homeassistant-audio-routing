#!/bin/bash
# Spike 2 (PLAN.md Section 7 + 5.4a): verify PipeWire loads a *static*
# libpipewire-module-raop-sink config (container/etc-pipewire/pipewire.conf.d/
# 10-raop-static.conf) without crashing, and that it shows up as an ordinary
# PipeWire sink node that other nodes can link to.
#
# This only proves the config/plumbing works — it targets an unroutable
# placeholder IP (192.0.2.1, RFC 5737 TEST-NET-1) so no real device is
# needed. It does NOT prove real audio delivery to a Pioneer/Yamaha; that
# requires re-running against a real raop.ip on the home LAN (see
# spikes/02-raop-static-sink.md for the manual validation steps).
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
sleep 2

if ! kill -0 "$PWPID" 2>/dev/null; then
  echo "FAIL: pipewire exited after loading raop-sink config"
  cat /tmp/pw.log
  exit 1
fi
if ! kill -0 "$WPPID" 2>/dev/null; then
  echo "FAIL: wireplumber exited"
  cat /tmp/wp.log
  exit 1
fi

echo "--- modules loaded ---"
pw-cli ls Module 2>&1 | grep -i raop || { echo "FAIL: no raop module loaded"; exit 1; }

echo "--- sink node present ---"
pw-cli ls Node 2>&1 | grep -A3 "raop-spike-test-placeholder" || { echo "FAIL: raop sink node not found"; exit 1; }

echo "--- can link a virtual source into it ---"
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=spike-source media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }" >/dev/null
sleep 1
pw-link spike-source:capture_FL raop-spike-test-placeholder:send_FL
pw-link spike-source:capture_FR raop-spike-test-placeholder:send_FR
pw-link -l | grep -A1 spike-source

echo "PASS: static raop-sink config loads, creates a linkable sink node"
'
