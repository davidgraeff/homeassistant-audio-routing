#!/bin/bash
# Phase 2 end-to-end check for the real add-on: one AirPlay-receive source
# routed to one sendspin output through the bridge daemon's own REST API — not a
# spike script standing in for it:
#
#   cliraop (real AirPlay sender) -> the daemon's native AirPlay receiver
#   (airplay_source.rs, node `airplay-in-<slug>`) ->
#   POST /api/routing/link (the real routing API) ->
#   sendspin-out-<name> sink (the sendspin capture point) ->
#   independent pw-record capture -> real-signal verification
#
# Two things changed under this test since it was written, both in the same
# direction — configuration became discovery plus a decision:
#   * There is no create API for a sendspin output anymore; devices are
#     discovered and the user *adds* (adopts) one. CI has no real sendspin
#     device, so a plain null-audio-sink named with the `sendspin-out-` prefix
#     stands in — the daemon treats it as an output by name (routing.rs
#     is_output_node) — and it is adopted through the real endpoint.
#   * The AirPlay receiver is native and multi-instance: `POST /api/sources`
#     instead of the old singular `PUT /api/source/airplay`, and its PipeWire
#     producer node exists from the moment the source is created. So the link is
#     made *before* the sender starts, instead of racing a short-lived node.
#
# Deliberately does NOT use --network host / real mDNS: a custom bridge
# network is enough to prove the mechanism, and avoids the failure mode
# hit during development where a real ESP32 device on the LAN
# auto-discovered and connected to a test container advertising the same
# sendspin service Music Assistant uses (see memory/
# pipewire_router_bt_bridge_dev_device.md for the incident). Direct
# container-IP addressing sidesteps needing mDNS to work at all.
set -euo pipefail

ADDON_DIR="$(dirname "$0")/../pipewire_audio_router"
IMAGE="${IMAGE:-pipewire_audio_router:dev}"
CONTAINER_NAME="pw-addon-phase2-test"
NETWORK_NAME="pw-addon-phase2-net"
DATA_DIR="$(mktemp -d)"
HOST_PORT="${HOST_PORT:-18090}"
BASE="http://localhost:$HOST_PORT"
WAV_HOST_PATH="${WAV_HOST_PATH:-/usr/share/sounds/speech-dispatcher/pipe.wav}"
CLIRAOP_PATH="${CLIRAOP_PATH:-$(dirname "$0")/../../music-assistant-server/music_assistant/providers/airplay/bin/cliraop-linux-x86_64}"

SOURCE_NAME="airplay-in-test-airplay"
OUTPUT_NAME="sendspin-out-kitchen"

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

fail() {
  echo "FAIL: $1"
  echo "--- container logs ---"
  docker logs "$CONTAINER_NAME" 2>&1 | tail -40 || true
  exit 1
}

echo "--- building add-on image ---"
docker build -t "$IMAGE" "$ADDON_DIR"

# No options.json seeding — the output and the AirPlay source are created at
# runtime via the API once the daemon is up (below).
docker network create "$NETWORK_NAME" >/dev/null 2>&1 || true
docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
# --cap-add SYS_NICE/IPC_LOCK mirror the add-on config.yaml privileges. Without
# IPC_LOCK, PipeWire's mem.mlock-all (50-mlock.conf) mlockall() fails and 1.6.2
# then aborts context creation, so PipeWire never comes up in the container.
docker run -d --cap-add SYS_NICE --cap-add IPC_LOCK --name "$CONTAINER_NAME" --network "$NETWORK_NAME" \
  -v "$DATA_DIR:/data" -p "$HOST_PORT:8099" "$IMAGE" >/dev/null

echo "--- waiting for bridge-daemon HTTP API ---"
READY=""
for _ in $(seq 1 30); do
  if curl -sf "$BASE/health" >/dev/null 2>&1; then
    READY=1
    break
  fi
  sleep 1
done
[ -n "$READY" ] || fail "bridge-daemon never became healthy"

echo "--- standing in for a sendspin device, then adding it ---"
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c "
pw-cli create-node adapter \"{ factory.name=support.null-audio-sink node.name=$OUTPUT_NAME media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }\" >/dev/null
" || fail "could not create the virtual sendspin sink"
# 200-or-nothing: a write's verdict is its status, there is no `ok` field in the
# body (bridge-daemon/src/api/error/mod.rs; phase1_e2e explains the switch).
ADOPT=$(curl -s -w '\n%{http_code}' -X POST "$BASE/api/outputs/$OUTPUT_NAME/adopt")
[ "$(tail -1 <<<"$ADOPT")" = "200" ] || fail "could not add output $OUTPUT_NAME: $ADOPT"

echo "--- creating the AirPlay source via the API ---"
CODE=$(curl -s -o /tmp/phase2_src.json -w '%{http_code}' -X POST "$BASE/api/sources" \
  -H 'Content-Type: application/json' -d '{"label":"Test AirPlay","kind":"airplay"}')
[ "$CODE" = "201" ] || fail "POST /api/sources returned $CODE: $(cat /tmp/phase2_src.json)"
# Port 5000 is the base the store allocates from for the first AirPlay source —
# and the port cliraop below connects to, so a change here has to be deliberate.
grep -q '"port":5000' /tmp/phase2_src.json || fail "expected the first AirPlay source on port 5000: $(cat /tmp/phase2_src.json)"
rm -f /tmp/phase2_src.json

echo "--- waiting for both endpoints in the matrix ---"
READY=""
for _ in $(seq 1 30); do
  MATRIX=$(curl -sf "$BASE/api/routing" 2>/dev/null || true)
  if echo "$MATRIX" | grep -q "\"node_name\":\"$SOURCE_NAME\"" && echo "$MATRIX" | grep -q "\"node_name\":\"$OUTPUT_NAME\""; then
    READY=1
    break
  fi
  sleep 1
done
[ -n "$READY" ] || fail "source and output never both appeared in /api/routing: ${MATRIX:-}"
echo "OK: sendspin sink and AirPlay source both present"

echo "--- routing the source to the output via POST /api/routing/link ---"
LINK=$(curl -s -w '\n%{http_code}' -X POST "$BASE/api/routing/link" -H 'Content-Type: application/json' \
  -d "{\"source\":\"$SOURCE_NAME\",\"output\":\"$OUTPUT_NAME\"}")
[ "$(tail -1 <<<"$LINK")" = "200" ] || fail "link request was refused: $LINK"
REAL_LINKS=$(docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" pw-link -l)
echo "$REAL_LINKS" | grep -q "$OUTPUT_NAME" || { echo "$REAL_LINKS"; fail "no real pw-link into $OUTPUT_NAME after linking"; }
echo "OK: real per-channel links in place before any audio is sent"

CONTAINER_IP=$(docker inspect "$CONTAINER_NAME" --format "{{(index .NetworkSettings.Networks \"$NETWORK_NAME\").IPAddress}}")
echo "--- container IP: $CONTAINER_IP ---"

# cliraop sends well ahead of real-time and exits within ~1-2s regardless
# of clip length (see spikes/shairport-sync-source.md) — loop the sample
# out to ~8s so the capture below has a comfortable window.
if command -v ffmpeg >/dev/null 2>&1; then
  TEMP_WAV="$(mktemp -u --suffix=.wav)"
  rm -f "$TEMP_WAV"
  ffmpeg -y -loglevel error -stream_loop 9 -i "$WAV_HOST_PATH" -c copy "$TEMP_WAV"
fi
PLAYBACK_WAV="${TEMP_WAV:-$WAV_HOST_PATH}"

echo "--- capturing from the sendspin sink while sending a real AirPlay stream ---"
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" \
  bash -c "timeout 8 pw-record --target $OUTPUT_NAME --rate 48000 --channels 2 --format s16 /tmp/captured.raw" &
RECORD_PID=$!
sleep 0.3

"$CLIRAOP_PATH" -et 0 "$CONTAINER_IP" "$PLAYBACK_WAV" > /tmp/phase2_cliraop.log 2>&1 &
CLIRAOP_PID=$!

wait "$CLIRAOP_PID" || { echo "FAIL: cliraop exited with an error"; cat /tmp/phase2_cliraop.log; exit 1; }
wait "$RECORD_PID" || true
docker cp "$CONTAINER_NAME:/tmp/captured.raw" /tmp/phase2_captured.raw

echo "--- signal analysis (peak/RMS — confirms real audio, not silence) ---"
STATS=$(ffmpeg -f s16le -ar 48000 -ac 2 -i /tmp/phase2_captured.raw -af astats -f null - 2>&1)
echo "$STATS" | grep -E "Peak level dB|RMS level dB" | head -2
# Silence reads as -inf/-100 dB, so a finite peak above the noise floor is the
# assertion the old version left to the reader's eyes.
PEAK=$(echo "$STATS" | grep -m1 "Peak level dB" | grep -oE '\-?[0-9]+\.[0-9]+' | head -1)
[ -n "$PEAK" ] || { echo "$STATS" | tail -20; fail "could not read a peak level from the capture"; }
awk -v p="$PEAK" 'BEGIN { exit !(p > -60) }' || fail "captured audio is silence (peak ${PEAK} dB)"
rm -f /tmp/phase2_captured.raw /tmp/phase2_cliraop.log

echo "PASS: AirPlay source -> bridge-daemon routing API -> sendspin sink, real signal (peak ${PEAK} dB) confirmed through the real add-on code"
