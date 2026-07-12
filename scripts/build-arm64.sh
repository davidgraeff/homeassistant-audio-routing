#!/bin/bash
# Periodic sanity check that the container still builds and boots for the
# real target (Raspberry Pi 4, aarch64). Local iteration/spikes happen on
# x86_64 via container/docker-compose.yml; run this occasionally, not on
# every change (QEMU emulation is slow).
set -euo pipefail
cd "$(dirname "$0")/../container"

docker buildx build --platform linux/arm64 -t pw-audio-router:arm64-test --load .

echo "--- smoke test: pipewire + wireplumber stay up under qemu ---"
docker run --rm --platform linux/arm64 --entrypoint bash pw-audio-router:arm64-test -c '
set -e
export XDG_RUNTIME_DIR=/run/pipewire
mkdir -p "$XDG_RUNTIME_DIR"
eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS
pipewire > /tmp/pw.log 2>&1 &
sleep 2
wireplumber > /tmp/wp.log 2>&1 &
WPID=$!
sleep 4
if kill -0 "$WPID" 2>/dev/null; then
  echo "OK: wireplumber still running under arm64/qemu"
else
  echo "FAIL: wireplumber exited"
  cat /tmp/wp.log
  exit 1
fi
'
