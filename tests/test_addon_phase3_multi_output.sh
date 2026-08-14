#!/bin/bash
# Phase 3 check: one AirPlay source fanned out to TWO simultaneous outputs via
# the bridge daemon's real routing API — the "Brave -> Dusche + Pioneer" scenario
# from the original Fedora pw-graph screenshot that kicked off this whole
# project. PLAN.md Phase 3's "multiple simultaneous outputs from one source" goal.
#
# No new mixing logic needed on our part: PipeWire's own graph natively
# supports one output port linking to many input ports (fan-out) — this test
# proves that's actually exercised correctly through the real add-on code, with
# real audio arriving at both sinks at once, not just that the underlying
# PipeWire primitive exists.
#
# **Narrowed on purpose.** It used to fan out to two structurally *different*
# output types (one RAOP, one sendspin). The RAOP output path was removed in
# Phase 6 (raop_migration.rs, "drop-raop 2026-07"), and the two remaining dialed
# backends (AirPlay-2, pw-sink) have no PipeWire node to capture from and need a
# real receiver on the network, which CI has none of. So both stand-ins here are
# `sendspin-out-` null sinks: the fan-out itself is still proven end to end, the
# heterogeneous-backend half is not, and that gap needs hardware (see
# docs/ for the per-device spikes that cover it there).
set -euo pipefail

ADDON_DIR="$(dirname "$0")/../pipewire_audio_router"
IMAGE="${IMAGE:-pipewire_audio_router:dev}"
CONTAINER_NAME="pw-addon-phase3-test"
NETWORK_NAME="pw-addon-phase3-net"
DATA_DIR="$(mktemp -d)"
HOST_PORT="${HOST_PORT:-18097}"
BASE="http://localhost:$HOST_PORT"
WAV_HOST_PATH="${WAV_HOST_PATH:-/usr/share/sounds/speech-dispatcher/pipe.wav}"
CLIRAOP_PATH="${CLIRAOP_PATH:-$(dirname "$0")/../../music-assistant-server/music_assistant/providers/airplay/bin/cliraop-linux-x86_64}"

SOURCE_NAME="airplay-in-test-airplay"
OUT_A="sendspin-out-kitchen"
OUT_B="sendspin-out-bedroom"

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

# No options.json seeding — both outputs and the AirPlay source are created at
# runtime via the API once the daemon is up (below).
docker network create "$NETWORK_NAME" >/dev/null 2>&1 || true
docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
# --cap-add SYS_NICE/IPC_LOCK mirror the add-on config.yaml privileges. Without
# IPC_LOCK, PipeWire's mem.mlock-all (50-mlock.conf) mlockall() fails and 1.6.2
# then aborts context creation, so PipeWire never comes up in the container.
docker run -d --cap-add SYS_NICE --cap-add IPC_LOCK --name "$CONTAINER_NAME" --network "$NETWORK_NAME" \
  -v "$DATA_DIR:/data" -p "$HOST_PORT:8099" "$IMAGE" >/dev/null

echo "--- waiting for the bridge-daemon HTTP API ---"
READY=""
for _ in $(seq 1 30); do
  if curl -sf "$BASE/health" >/dev/null 2>&1; then READY=1; break; fi
  sleep 1
done
[ -n "$READY" ] || fail "bridge-daemon never became healthy"

echo "--- standing in for two sendspin devices, then adding both ---"
for NAME in "$OUT_A" "$OUT_B"; do
  docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c "
pw-cli create-node adapter \"{ factory.name=support.null-audio-sink node.name=$NAME media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }\" >/dev/null
" || fail "could not create the virtual sink $NAME"
  # Discovery is only an offer since the adoption gate (outputs_store.rs) — an
  # unadded device is not routable, so linking below would be refused.
  # 200-or-nothing: a write's verdict is its status, there is no `ok` field in the
  # body (bridge-daemon/src/api/error/mod.rs; phase1_e2e explains the switch).
  ADOPT=$(curl -s -w '\n%{http_code}' -X POST "$BASE/api/outputs/$NAME/adopt")
  [ "$(tail -1 <<<"$ADOPT")" = "200" ] || fail "could not add output $NAME: $ADOPT"
done

echo "--- creating the AirPlay source via the API ---"
CODE=$(curl -s -o /tmp/phase3_src.json -w '%{http_code}' -X POST "$BASE/api/sources" \
  -H 'Content-Type: application/json' -d '{"label":"Test AirPlay","kind":"airplay"}')
[ "$CODE" = "201" ] || fail "POST /api/sources returned $CODE: $(cat /tmp/phase3_src.json)"
rm -f /tmp/phase3_src.json

echo "--- waiting for the source and both outputs in the matrix ---"
READY=""
for _ in $(seq 1 30); do
  MATRIX=$(curl -sf "$BASE/api/routing" 2>/dev/null || true)
  if echo "$MATRIX" | grep -q "\"node_name\":\"$SOURCE_NAME\"" \
    && echo "$MATRIX" | grep -q "\"node_name\":\"$OUT_A\"" \
    && echo "$MATRIX" | grep -q "\"node_name\":\"$OUT_B\""; then
    READY=1
    break
  fi
  sleep 1
done
[ -n "$READY" ] || fail "source and both outputs never appeared in /api/routing: ${MATRIX:-}"
echo "OK: all three endpoints present"

echo "--- fan-out: one source linked to BOTH outputs ---"
for OUT in "$OUT_A" "$OUT_B"; do
  LINK=$(curl -s -w '\n%{http_code}' -X POST "$BASE/api/routing/link" -H 'Content-Type: application/json' \
    -d "{\"source\":\"$SOURCE_NAME\",\"output\":\"$OUT\"}")
  [ "$(tail -1 <<<"$LINK")" = "200" ] || fail "link to $OUT was refused: $LINK"
done
# The point of the test: the *same* source port feeding two sinks at once. Assert
# it in the real graph, not just in the API's answer.
REAL_LINKS=$(docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" pw-link -l)
echo "$REAL_LINKS" | grep -q "$OUT_A" || { echo "$REAL_LINKS"; fail "no real pw-link into $OUT_A"; }
echo "$REAL_LINKS" | grep -q "$OUT_B" || { echo "$REAL_LINKS"; fail "no real pw-link into $OUT_B"; }
MATRIX=$(curl -s "$BASE/api/routing")
echo "$MATRIX" | grep -q "\"source\":\"$SOURCE_NAME\",\"output\":\"$OUT_A\"" || fail "matrix missing the link to $OUT_A: $MATRIX"
echo "$MATRIX" | grep -q "\"source\":\"$SOURCE_NAME\",\"output\":\"$OUT_B\"" || fail "matrix missing the link to $OUT_B: $MATRIX"
echo "OK: linked to both outputs"

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
  bash -c "timeout 8 pw-record --target $OUT_A --rate 48000 --channels 2 --format s16 /tmp/cap_a.raw" &
RECORD_PID_A=$!
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" \
  bash -c "timeout 8 pw-record --target $OUT_B --rate 48000 --channels 2 --format s16 /tmp/cap_b.raw" &
RECORD_PID_B=$!
sleep 0.3

"$CLIRAOP_PATH" -et 0 "$CONTAINER_IP" "$PLAYBACK_WAV" > /tmp/phase3_cliraop.log 2>&1 &
CLIRAOP_PID=$!

wait "$CLIRAOP_PID" || { echo "FAIL: cliraop exited with an error"; cat /tmp/phase3_cliraop.log; exit 1; }
wait "$RECORD_PID_A" || true
wait "$RECORD_PID_B" || true
docker cp "$CONTAINER_NAME:/tmp/cap_a.raw" /tmp/phase3_cap_a.raw
docker cp "$CONTAINER_NAME:/tmp/cap_b.raw" /tmp/phase3_cap_b.raw

# Real audio on BOTH, or the fan-out only looked right in the graph. Silence
# reads as -inf/-100 dB, so a finite peak above the noise floor is the check.
for PAIR in "$OUT_A:/tmp/phase3_cap_a.raw" "$OUT_B:/tmp/phase3_cap_b.raw"; do
  NAME="${PAIR%%:*}"; RAW="${PAIR##*:}"
  echo "--- $NAME signal ---"
  STATS=$(ffmpeg -f s16le -ar 48000 -ac 2 -i "$RAW" -af astats -f null - 2>&1)
  echo "$STATS" | grep -E "Peak level dB|RMS level dB" | head -2
  PEAK=$(echo "$STATS" | grep -m1 "Peak level dB" | grep -oE '\-?[0-9]+\.[0-9]+' | head -1)
  [ -n "$PEAK" ] || { echo "$STATS" | tail -20; fail "could not read a peak level from the $NAME capture"; }
  awk -v p="$PEAK" 'BEGIN { exit !(p > -60) }' || fail "$NAME captured silence (peak ${PEAK} dB) — the fan-out did not deliver"
done
rm -f /tmp/phase3_cap_a.raw /tmp/phase3_cap_b.raw /tmp/phase3_cliraop.log

echo "PASS: one AirPlay source fanned out to two outputs simultaneously, real signal confirmed on both"
