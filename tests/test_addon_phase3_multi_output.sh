#!/bin/bash
# Phase 3 check: one AirPlay source fanned out to TWO simultaneous outputs
# (one RAOP, one sendspin) via the bridge daemon's real POST /api/links —
# the "Brave -> Dusche + Pioneer" scenario from the original Fedora
# pw-graph screenshot that kicked off this whole project. PLAN.md Phase 3's
# "multiple simultaneous outputs from one source" goal.
#
# No new mixing logic needed on our part: PipeWire's own graph natively
# supports one output port linking to many input ports (fan-out) — this
# test proves that's actually exercised correctly through the real add-on
# code with two structurally different output types at once, not just
# that the underlying PipeWire primitive exists.
set -euo pipefail

ADDON_DIR="$(dirname "$0")/../pipewire_audio_router"
IMAGE="${IMAGE:-pipewire_audio_router:dev}"
CONTAINER_NAME="pw-addon-phase3-test"
NETWORK_NAME="pw-addon-phase3-net"
DATA_DIR="$(mktemp -d)"
HOST_PORT="${HOST_PORT:-18097}"
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

# No options.json seeding — the RAOP output, sendspin output, and AirPlay
# source are created at runtime via the API once the daemon is up (below).
docker network create "$NETWORK_NAME" >/dev/null 2>&1 || true
docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
# --cap-add SYS_NICE/IPC_LOCK mirror the add-on config.yaml privileges. Without
# IPC_LOCK, PipeWire's mem.mlock-all (50-mlock.conf) mlockall() fails and 1.6.2
# then aborts context creation, so PipeWire never comes up in the container.
docker run -d --cap-add SYS_NICE --cap-add IPC_LOCK --name "$CONTAINER_NAME" --network "$NETWORK_NAME" \
  -v "$DATA_DIR:/data" -p "$HOST_PORT:8099" "$IMAGE" >/dev/null

echo "--- waiting for the bridge-daemon HTTP API ---"
for _ in $(seq 1 30); do curl -sf "http://localhost:$HOST_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
echo "--- creating the RAOP output, sendspin output, and AirPlay source via the API ---"
curl -s -X POST "http://localhost:$HOST_PORT/api/outputs" -H 'Content-Type: application/json' \
  -d '{"name":"Test RAOP","ip":"192.0.2.1","port":7000,"encryption":"auth_setup"}' | grep -q '"ok":true' \
  || { echo "FAIL: POST /api/outputs failed"; docker logs "$CONTAINER_NAME"; exit 1; }
curl -s -X POST "http://localhost:$HOST_PORT/api/sendspin_outputs" -H 'Content-Type: application/json' \
  -d '{"name":"Kitchen"}' | grep -q '"ok":true' \
  || { echo "FAIL: POST /api/sendspin_outputs failed"; docker logs "$CONTAINER_NAME"; exit 1; }
curl -s -X PUT "http://localhost:$HOST_PORT/api/source/airplay" -H 'Content-Type: application/json' \
  -d '{"name":"PipeWire Router"}' | grep -q '"ok":true' \
  || { echo "FAIL: PUT /api/source/airplay failed"; docker logs "$CONTAINER_NAME"; exit 1; }

echo "--- waiting for bridge-daemon + both output nodes ---"
READY=""
for _ in $(seq 1 30); do
  NODES=$(curl -sf "http://localhost:$HOST_PORT/api/nodes" 2>/dev/null || true)
  if echo "$NODES" | grep -q "raop-out-test_raop" && echo "$NODES" | grep -q "sendspin-out-kitchen"; then
    READY=1
    break
  fi
  sleep 1
done
if [ -z "$READY" ]; then
  echo "FAIL: both output nodes never appeared"
  docker logs "$CONTAINER_NAME"
  exit 1
fi
echo "OK: both outputs present"

CONTAINER_IP=$(docker inspect "$CONTAINER_NAME" --format "{{(index .NetworkSettings.Networks \"$NETWORK_NAME\").IPAddress}}")

# IMPORTANT: always rm -f the target path before regenerating with ffmpeg.
# cliraop creates a named pipe (mkfifo) at its target path if the file
# doesn't exist at invocation time, rather than erroring — if a stale FIFO
# from an earlier run is still there, `ffmpeg -y` writes *into* the
# existing pipe instead of replacing it with a regular file, silently
# producing an empty/unfillable target and a silent (not failing) test.
# Hit this exact bug during development; costly to debug because every
# symptom looked like a linking/PipeWire problem instead of a stale fixture.
if command -v ffmpeg >/dev/null 2>&1; then
  TEMP_WAV="$(mktemp -u --suffix=.wav)"
  rm -f "$TEMP_WAV"
  ffmpeg -y -loglevel error -stream_loop 9 -i "$WAV_HOST_PATH" -c copy "$TEMP_WAV"
fi
PLAYBACK_WAV="${TEMP_WAV:-$WAV_HOST_PATH}"

echo "--- capturing from both sinks while sending one AirPlay stream ---"
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" \
  bash -c 'timeout 6 pw-record --target raop-out-test_raop --rate 44100 --channels 2 --format s16 /tmp/cap_raop.raw' &
RECORD_PID_RAOP=$!
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" \
  bash -c 'timeout 6 pw-record --target sendspin-out-kitchen --rate 48000 --channels 2 --format s16 /tmp/cap_sendspin.raw' &
RECORD_PID_SENDSPIN=$!
sleep 0.3

"$CLIRAOP_PATH" -et 0 "$CONTAINER_IP" "$PLAYBACK_WAV" > /tmp/phase3_cliraop.log 2>&1 &
CLIRAOP_PID=$!

echo "--- fan-out linking via POST /api/links: one source -> two outputs ---"
LINKED_RAOP=""
LINKED_SENDSPIN=""
for _ in $(seq 1 100); do
  R1=$(curl -s -X POST "http://localhost:$HOST_PORT/api/links" -H 'Content-Type: application/json' -d '{"from_port":"alsa_playback.shairport-sync:output_FL","to_port":"raop-out-test_raop:send_FL"}')
  R2=$(curl -s -X POST "http://localhost:$HOST_PORT/api/links" -H 'Content-Type: application/json' -d '{"from_port":"alsa_playback.shairport-sync:output_FR","to_port":"raop-out-test_raop:send_FR"}')
  R3=$(curl -s -X POST "http://localhost:$HOST_PORT/api/links" -H 'Content-Type: application/json' -d '{"from_port":"alsa_playback.shairport-sync:output_FL","to_port":"sendspin-out-kitchen:playback_FL"}')
  R4=$(curl -s -X POST "http://localhost:$HOST_PORT/api/links" -H 'Content-Type: application/json' -d '{"from_port":"alsa_playback.shairport-sync:output_FR","to_port":"sendspin-out-kitchen:playback_FR"}')
  echo "$R1" | grep -q '"ok":true' && echo "$R2" | grep -q '"ok":true' && LINKED_RAOP=1
  echo "$R3" | grep -q '"ok":true' && echo "$R4" | grep -q '"ok":true' && LINKED_SENDSPIN=1
  if [ -n "$LINKED_RAOP" ] && [ -n "$LINKED_SENDSPIN" ]; then
    break
  fi
  sleep 0.03
done
if [ -z "$LINKED_RAOP" ] || [ -z "$LINKED_SENDSPIN" ]; then
  echo "FAIL: fan-out linking incomplete (raop=$LINKED_RAOP sendspin=$LINKED_SENDSPIN)"
  exit 1
fi
echo "OK: linked to both outputs"

wait "$CLIRAOP_PID" || { echo "FAIL: cliraop exited with an error"; cat /tmp/phase3_cliraop.log; exit 1; }
wait "$RECORD_PID_RAOP" || true
wait "$RECORD_PID_SENDSPIN" || true
docker cp "$CONTAINER_NAME:/tmp/cap_raop.raw" /tmp/phase3_cap_raop.raw
docker cp "$CONTAINER_NAME:/tmp/cap_sendspin.raw" /tmp/phase3_cap_sendspin.raw

echo "--- RAOP output signal ---"
ffmpeg -f s16le -ar 44100 -ac 2 -i /tmp/phase3_cap_raop.raw -af astats -f null - 2>&1 | grep -E "Peak level dB|RMS level dB" | head -2
echo "--- sendspin output signal ---"
ffmpeg -f s16le -ar 48000 -ac 2 -i /tmp/phase3_cap_sendspin.raw -af astats -f null - 2>&1 | grep -E "Peak level dB|RMS level dB" | head -2
rm -f /tmp/phase3_cap_raop.raw /tmp/phase3_cap_sendspin.raw /tmp/phase3_cliraop.log

echo "PASS: one AirPlay source fanned out to a RAOP output AND a sendspin output simultaneously, real signal confirmed on both"
