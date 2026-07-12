#!/bin/bash
# Spike 2, real-hardware half: stream a short WAV from the container to a
# real RAOP receiver (Pioneer/Yamaha) on your LAN, fully automated (no
# manual docker cp / exec step). Run this ON THE HOST that's on the same
# network as the receiver (or with the container on a macvlan/host
# network so it can actually reach it — bridge networking's NAT may
# prevent the receiver's RTSP/UDP reply from reaching back).
#
# Uses the speech-dispatcher sample WAV that ships on most Linux desktops
# as the test clip (short, mono, easy to recognize by ear).
#
# IMPORTANT #1: requires the image built from the current Dockerfile
# (Ubuntu 26.04 base, PipeWire 1.6.2). The old Debian bookworm base
# (PipeWire 0.3.65) creates the sink node fine but NEVER performs the
# actual RTSP handshake — a silent, version-specific bug. Rebuild the
# image if you last built it before this fix (spikes/02-raop-static-sink.md).
#
# IMPORTANT #2: RAOP/AirPlay RTSP port is NOT reliably 5000. Real devices
# seen on this network (Pioneer VSX-934, Dusche) both advertise port 7000
# via mDNS — find yours with `avahi-browse -r _raop._tcp` or by checking
# an already-working host-side sink's node.name (the port is the trailing
# number, e.g. raop_sink.Pioneer-VSX-934-F11B89.local.192.168.178.35.7000).
#
# IMPORTANT #3: raop.encryption.type is NOT reliably "none". Both
# "none" and "RSA" got "403 Forbidden" on ANNOUNCE against real hardware
# here — these receivers require the Apple device-verification handshake
# ("auth_setup"), independent of whether the audio payload is encrypted.
# Confirmed working end-to-end (full RTSP lifecycle through RECORD and
# audible playback) with "auth_setup" — that's now the default.
#
# Usage: RAOP_IP=192.168.1.42 RAOP_PORT=7000 RAOP_NAME="Pioneer VSX-934" ./test_spike02_raop_real_device.sh
set -euo pipefail

RAOP_IP="${RAOP_IP:?Set RAOP_IP to the receiver LAN IP}"
RAOP_PORT="${RAOP_PORT:-7000}"
RAOP_ENCRYPTION="${RAOP_ENCRYPTION:-auth_setup}"
RAOP_NAME="${RAOP_NAME:-raop-real-test}"
IMAGE="${IMAGE:-pw-audio-router:dev}"
NODE_NAME="raop-real-test"
CONTAINER_NAME="pw-raop-spike"
WAV_HOST_PATH="${WAV_HOST_PATH:-/usr/share/sounds/speech-dispatcher/pipe.wav}"
WAV_CONTAINER_PATH="/tmp/test.wav"

if [ ! -f "$WAV_HOST_PATH" ]; then
  echo "FAIL: test WAV not found at $WAV_HOST_PATH — set WAV_HOST_PATH to an existing file"
  exit 1
fi

# The full RTSP lifecycle (OPTIONS, auth-setup, ANNOUNCE, SETUP, RECORD)
# takes several round-trips before audio actually starts flowing; the raw
# speech-dispatcher sample is only ~1.5s, which may not leave enough time
# to hear anything even on a fully working session. Loop it out to ~8s
# via ffmpeg if available; otherwise use it as-is.
PLAYBACK_WAV="$WAV_HOST_PATH"
TEMP_WAV=""
if command -v ffmpeg >/dev/null 2>&1; then
  TEMP_WAV="$(mktemp --suffix=.wav)"
  ffmpeg -y -loglevel error -stream_loop 9 -i "$WAV_HOST_PATH" -c copy "$TEMP_WAV"
  PLAYBACK_WAV="$TEMP_WAV"
else
  echo "NOTE: ffmpeg not found — using the WAV as-is (~1.5s), which may be too short for the full RTSP handshake to complete audibly."
fi

cleanup() {
  docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
  [ -n "$TEMP_WAV" ] && rm -f "$TEMP_WAV"
}
trap cleanup EXIT

docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true

docker run -d --network host --name "$CONTAINER_NAME" --entrypoint bash "$IMAGE" -c "
set -e
export XDG_RUNTIME_DIR=/run/pipewire
mkdir -p \"\$XDG_RUNTIME_DIR\"
eval \"\$(dbus-launch --sh-syntax)\"
export DBUS_SESSION_BUS_ADDRESS

# A module loaded via 'pw-cli load-module' lives in pw-cli's own transient
# client connection and disappears the instant pw-cli exits — it does NOT
# register in the daemon's own context. To get a sink node that persists
# for the container's lifetime, drop the config in pipewire.conf.d/ and
# let the daemon load it at its own startup, same as the static placeholder
# already baked into the image (Section 5.4a of PLAN.md).
cat > /etc/pipewire/pipewire.conf.d/99-real-device.conf <<CONF
context.modules = [
    { name = libpipewire-module-raop-sink
        args = {
            raop.ip = \"$RAOP_IP\"
            raop.port = $RAOP_PORT
            raop.name = \"$RAOP_NAME\"
            raop.transport = \"udp\"
            raop.encryption.type = \"$RAOP_ENCRYPTION\"
            audio.format = \"S16\"
            audio.rate = 44100
            audio.channels = 2
            node.name = \"$NODE_NAME\"
        }
        flags = [ nofail ]
    }
]
CONF

pipewire > /tmp/pw.log 2>&1 &
sleep 1
wireplumber > /tmp/wp.log 2>&1 &
sleep 2

sleep infinity
" >/dev/null

echo "--- waiting for sink node ($NODE_NAME) to appear ---"
SINK_READY=""
for _ in $(seq 1 15); do
  if docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" pw-cli ls Node 2>/dev/null | grep -q "$NODE_NAME"; then
    SINK_READY=1
    break
  fi
  sleep 1
done

if [ -z "$SINK_READY" ]; then
  echo "FAIL: sink node not created — check raop.port/raop.encryption.type or that the receiver is reachable"
  echo "--- pipewire log ---"
  docker exec "$CONTAINER_NAME" cat /tmp/pw.log 2>/dev/null || true
  exit 1
fi
echo "OK: sink node present"

echo "--- copying test WAV into container ---"
docker cp "$PLAYBACK_WAV" "$CONTAINER_NAME:$WAV_CONTAINER_PATH"

echo "--- playing $PLAYBACK_WAV to $NODE_NAME via raop.ip=$RAOP_IP raop.port=$RAOP_PORT raop.encryption.type=$RAOP_ENCRYPTION (listen at the receiver now) ---"
PWCAT_OK=1
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" pw-cat --target "$NODE_NAME" --playback "$WAV_CONTAINER_PATH" || PWCAT_OK=0

echo "--- raop/rtsp-related pipewire log lines (this is where a wrong port/encryption shows up, NOT in pw-cat's own exit code) ---"
docker exec "$CONTAINER_NAME" grep -i "raop\|rtsp" /tmp/pw.log 2>/dev/null || echo "(no raop/rtsp lines logged)"

if [ "$PWCAT_OK" = "1" ]; then
  echo "PASS: pw-cat finished playing the clip into the local PipeWire sink node without error"
  echo "This does NOT by itself confirm the RTSP session with the receiver succeeded — check the log"
  echo "lines above for errors, and manually confirm: did the receiver switch to an AirPlay input"
  echo "and play the clip audibly?"
else
  echo "FAIL: pw-cat exited with an error while playing into $NODE_NAME"
  exit 1
fi
