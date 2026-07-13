#!/bin/bash
# Phase 2 end-to-end check for the real add-on: one AirPlay-receive source
# (shairport-sync) manually linked to one sendspin output, via the bridge
# daemon's own REST API — not a spike script standing in for it.
#
#   cliraop (real AirPlay sender) -> shairport-sync -> ALSA/PipeWire ->
#   POST /api/links (bridge daemon's real API) ->
#   sendspin-out-<name> sink (the sendspin adapter's own capture point) ->
#   independent pw-record capture -> real-signal verification
#
# Deliberately does NOT use --network host / real mDNS: a custom bridge
# network is enough to prove the mechanism, and avoids the failure mode
# hit during development where a real ESP32 device on the LAN
# auto-discovered and connected to a test container advertising the same
# sendspin service Music Assistant uses (see memory/
# pipewire_router_bt_bridge_dev_device.md for the incident). Direct
# container-IP addressing sidesteps needing mDNS to work at all, same
# pattern already used for RAOP testing.
set -euo pipefail

ADDON_DIR="$(dirname "$0")/../pipewire_audio_router"
IMAGE="${IMAGE:-pipewire_audio_router:dev}"
CONTAINER_NAME="pw-addon-phase2-test"
NETWORK_NAME="pw-addon-phase2-net"
DATA_DIR="$(mktemp -d)"
HOST_PORT="${HOST_PORT:-18090}"
WAV_HOST_PATH="${WAV_HOST_PATH:-/usr/share/sounds/speech-dispatcher/pipe.wav}"
CLIRAOP_PATH="${CLIRAOP_PATH:-$(dirname "$0")/../../music-assistant-server/music_assistant/providers/airplay/bin/cliraop-linux-x86_64}"

if [ ! -x "$CLIRAOP_PATH" ]; then
  echo "SKIP: cliraop not found at $CLIRAOP_PATH — set CLIRAOP_PATH to run this test."
  echo "      (It needs Music Assistant's proprietary AirPlay sender, which isn't in this repo / CI.)"
  exit 0
fi

TEMP_WAV=""
cleanup() {
  docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
  docker network rm "$NETWORK_NAME" >/dev/null 2>&1 || true
  rm -rf "$DATA_DIR"
  [ -n "$TEMP_WAV" ] && rm -f "$TEMP_WAV"
}
trap cleanup EXIT

echo "--- building add-on image ---"
docker build -t "$IMAGE" "$ADDON_DIR"

# No options.json seeding — the sendspin output and AirPlay source are created
# at runtime via the API once the daemon is up (below).
docker network create "$NETWORK_NAME" >/dev/null 2>&1 || true
docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER_NAME" --network "$NETWORK_NAME" \
  -v "$DATA_DIR:/data" -p "$HOST_PORT:8099" "$IMAGE" >/dev/null

echo "--- waiting for bridge-daemon HTTP API ---"
READY=""
for _ in $(seq 1 30); do
  if curl -sf "http://localhost:$HOST_PORT/health" >/dev/null 2>&1; then
    READY=1
    break
  fi
  sleep 1
done
if [ -z "$READY" ]; then
  echo "FAIL: bridge-daemon never became healthy"
  docker logs "$CONTAINER_NAME"
  exit 1
fi

echo "--- creating the sendspin output + AirPlay source via the API ---"
curl -s -X POST "http://localhost:$HOST_PORT/api/sendspin_outputs" -H 'Content-Type: application/json' \
  -d '{"name":"Kitchen"}' | grep -q '"ok":true' \
  || { echo "FAIL: POST /api/sendspin_outputs failed"; docker logs "$CONTAINER_NAME"; exit 1; }
curl -s -X PUT "http://localhost:$HOST_PORT/api/source/airplay" -H 'Content-Type: application/json' \
  -d '{"name":"PipeWire Router"}' | grep -q '"ok":true' \
  || { echo "FAIL: PUT /api/source/airplay failed"; docker logs "$CONTAINER_NAME"; exit 1; }

echo "--- waiting for the sendspin sink to be discovered ---"
FOUND=""
for _ in $(seq 1 15); do
  if curl -sf "http://localhost:$HOST_PORT/api/nodes" 2>/dev/null | grep -q "sendspin-out-kitchen"; then
    FOUND=1
    break
  fi
  sleep 1
done
if [ -z "$FOUND" ]; then
  echo "FAIL: sendspin-out-kitchen node never appeared"
  docker logs "$CONTAINER_NAME"
  exit 1
fi
echo "OK: sendspin sink present, bridge daemon healthy"

CONTAINER_IP=$(docker inspect "$CONTAINER_NAME" --format "{{(index .NetworkSettings.Networks \"$NETWORK_NAME\").IPAddress}}")
echo "--- container IP: $CONTAINER_IP ---"

# cliraop sends well ahead of real-time and exits within ~1-2s regardless
# of clip length (see spikes/shairport-sync-source.md) — loop the sample
# out to ~8s so the linking retry loop below has a comfortable window.
if command -v ffmpeg >/dev/null 2>&1; then
  TEMP_WAV="$(mktemp --suffix=.wav)"
  ffmpeg -y -loglevel error -stream_loop 9 -i "$WAV_HOST_PATH" -c copy "$TEMP_WAV"
fi
PLAYBACK_WAV="${TEMP_WAV:-$WAV_HOST_PATH}"

echo "--- capturing from the sendspin sink while sending a real AirPlay stream ---"
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" \
  bash -c 'timeout 6 pw-record --target sendspin-out-kitchen --rate 48000 --channels 2 --format s16 /tmp/captured.raw' &
RECORD_PID=$!
sleep 0.3

"$CLIRAOP_PATH" -et 0 "$CONTAINER_IP" "$PLAYBACK_WAV" > /tmp/phase2_cliraop.log 2>&1 &
CLIRAOP_PID=$!

echo "--- linking via the bridge daemon's real POST /api/links (tight retry, node is short-lived) ---"
LINKED=""
for _ in $(seq 1 60); do
  RESP_FL=$(curl -s -X POST "http://localhost:$HOST_PORT/api/links" -H 'Content-Type: application/json' \
    -d '{"from_port":"alsa_playback.shairport-sync:output_FL","to_port":"sendspin-out-kitchen:playback_FL"}')
  RESP_FR=$(curl -s -X POST "http://localhost:$HOST_PORT/api/links" -H 'Content-Type: application/json' \
    -d '{"from_port":"alsa_playback.shairport-sync:output_FR","to_port":"sendspin-out-kitchen:playback_FR"}')
  if echo "$RESP_FL" | grep -q '"ok":true' && echo "$RESP_FR" | grep -q '"ok":true'; then
    LINKED=1
    break
  fi
  sleep 0.03
done
if [ -z "$LINKED" ]; then
  echo "FAIL: could not link via the API — last responses: $RESP_FL / $RESP_FR"
  exit 1
fi
echo "OK: linked via POST /api/links"

wait "$CLIRAOP_PID" || { echo "FAIL: cliraop exited with an error"; cat /tmp/phase2_cliraop.log; exit 1; }
wait "$RECORD_PID" || true
docker cp "$CONTAINER_NAME:/tmp/captured.raw" /tmp/phase2_captured.raw

echo "--- signal analysis (peak/RMS — confirms real audio, not silence) ---"
ffmpeg -f s16le -ar 48000 -ac 2 -i /tmp/phase2_captured.raw -af astats -f null - 2>&1 \
  | grep -E "Peak level dB|RMS level dB" | head -2
rm -f /tmp/phase2_captured.raw /tmp/phase2_cliraop.log

echo "PASS: AirPlay source -> bridge-daemon API link -> sendspin sink, full chain verified through the real add-on code"
