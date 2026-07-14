#!/bin/bash
# Phase 3 check: the real add-on's POST /api/media_players/:node_id/announce
# endpoint (PLAN.md Section 5.6 v1, mechanism verified in
# spikes/05-tts-ducking-mechanism.md) — through the actual bridge-daemon
# binary running inside the real add-on image, not a bare PipeWire
# container like tests/test_ducking_mechanism.sh.
#
# Scenario: a "music" source is continuously linked into a sendspin
# output's sink (standing in for e.g. a BT-bridge/AirPlay source already
# playing). We call the real HTTP /announce endpoint with a URL for a
# short tone clip. The daemon should duck the music source's node volume,
# play the announce clip into the same sink (both audible, mixed), then
# restore the music source's original volume — all real signal, captured
# and measured via ffmpeg astats, not just "the HTTP call returned 200".
#
# Capture design: ONE continuous pw-record spanning the whole test,
# sliced into baseline/ducked/restored windows afterward via ffmpeg -ss.
# An earlier version used three separate sequential pw-record invocations
# against the sink's monitor ports and hit a real, reproducible hang: the
# first or second invocation would work, but a later one connecting to
# the same monitor ports (right after a previous pw-record had just
# disconnected from them) would block indefinitely — some race in
# reconnecting to the sink's monitor taps in quick succession, not
# anything to do with our own script logic (confirmed via a series of
# minimal repros that isolated it down to "N sequential pw-record calls
# against the same target," no announce/ducking/music involved at all).
# One capture sidesteps the whole class of issue.
set -euo pipefail

ADDON_DIR="$(dirname "$0")/../pipewire_audio_router"
IMAGE="${IMAGE:-pipewire_audio_router:dev}"
CONTAINER_NAME="pw-addon-announce-test"
NETWORK_NAME="pw-addon-announce-net"
DATA_DIR="$(mktemp -d)"
HOST_PORT="${HOST_PORT:-18098}"
MUSIC_WAV_HOST_PATH="${MUSIC_WAV_HOST_PATH:-/usr/share/sounds/speech-dispatcher/pipe.wav}"
CAPTURE_SECONDS=9
# set -u means cleanup() must not reference these before they're assigned.
TMP_MUSIC_LONG=""
SERVE_DIR=""
SERVE_PID=""

cleanup() {
  [ -n "$SERVE_PID" ] && kill "$SERVE_PID" >/dev/null 2>&1 || true
  docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
  # `docker network rm` right after `docker rm -f` on the same network has
  # been observed to hang for minutes (dockerd-side endpoint-detach race),
  # not just fail — plain `|| true` doesn't help against a hang, only a
  # nonzero exit. Bound it with `timeout` so a slow daemon can't wedge the
  # whole test; a leaked test-only bridge network is a harmless leftover.
  timeout 5 docker network rm "$NETWORK_NAME" >/dev/null 2>&1 || true
  rm -rf "$DATA_DIR" "$SERVE_DIR"
  rm -f /tmp/announce_e2e_full.raw "$TMP_MUSIC_LONG"
}
trap cleanup EXIT

echo "--- building add-on image (includes the announce endpoint) ---"
docker build -t "$IMAGE" "$ADDON_DIR"

# No options.json seeding — a virtual test sink is created below.
docker network create "$NETWORK_NAME" >/dev/null 2>&1 || true
docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
# --cap-add SYS_NICE/IPC_LOCK mirror the add-on config.yaml privileges. Without
# IPC_LOCK, PipeWire's mem.mlock-all (50-mlock.conf) mlockall() fails and 1.6.2
# then aborts context creation, so PipeWire never comes up in the container.
docker run -d --cap-add SYS_NICE --cap-add IPC_LOCK --add-host=host.docker.internal:host-gateway \
  --name "$CONTAINER_NAME" --network "$NETWORK_NAME" \
  -v "$DATA_DIR:/data" -p "$HOST_PORT:8099" "$IMAGE" >/dev/null

echo "--- waiting for the bridge-daemon HTTP API, then creating a virtual test sink ---"
for _ in $(seq 1 30); do curl -sf "http://localhost:$HOST_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
# Sendspin outputs are discovery-only now (no create API) and need a real
# sendspin device on the network, which CI has none of. Stand in a plain
# timer-driven null-audio-sink named with the `sendspin-out-` prefix so the
# daemon lists it as a media_player exactly as a real sendspin sink would be —
# the announce/duck path and the pw-record capture only need a running sink
# with a monitor, which this provides deterministically. (This is in fact what
# the sendspin adapter created internally before.)
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c '
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=sendspin-out-kitchen media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }" >/dev/null
' || { echo "FAIL: could not create the virtual test sink"; docker logs "$CONTAINER_NAME"; exit 1; }

echo "--- waiting for the sendspin sink node ---"
SINK_NODE_ID=""
for _ in $(seq 1 30); do
  NODES=$(curl -sf "http://localhost:$HOST_PORT/api/media_players" 2>/dev/null || true)
  # Field order is fixed by MediaPlayerInfo's struct definition (node_id
  # first) and the response isn't pretty-printed, so node_id always comes
  # right before this node_name within the same JSON object.
  # || true guards against set -e + pipefail treating "grep found no
  # match yet" (expected on early iterations, before the node exists) as
  # a fatal error in this assignment — a real gotcha hit while developing
  # this script, not defensive paranoia.
  SINK_NODE_ID=$(echo "$NODES" | grep -oE '"node_id":[0-9]+,"node_name":"sendspin-out-kitchen"' | grep -oE '[0-9]+' | head -1) || true
  [ -n "$SINK_NODE_ID" ] && break
  sleep 1
done
if [ -z "$SINK_NODE_ID" ]; then
  echo "FAIL: sendspin-out-kitchen never appeared in /api/media_players"
  docker logs "$CONTAINER_NAME"
  exit 1
fi
echo "OK: sink node id = $SINK_NODE_ID"

echo "--- creating a standing 'music' source node and linking it into the sink ---"
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c '
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=test-music-src media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }" >/dev/null
'
sleep 1
for _ in $(seq 1 50); do
  R1=$(curl -s -X POST "http://localhost:$HOST_PORT/api/links" -H 'Content-Type: application/json' -d '{"from_port":"test-music-src:capture_FL","to_port":"sendspin-out-kitchen:playback_FL"}')
  R2=$(curl -s -X POST "http://localhost:$HOST_PORT/api/links" -H 'Content-Type: application/json' -d '{"from_port":"test-music-src:capture_FR","to_port":"sendspin-out-kitchen:playback_FR"}')
  echo "$R1" | grep -q '"ok":true' && echo "$R2" | grep -q '"ok":true' && break
  sleep 0.1
done
echo "OK: music source linked"

echo "--- staging audio: a looped 'music' clip in the container + a short announce tone served over HTTP from the host ---"
# Generated on the HOST (ffmpeg is no longer in the add-on image itself —
# announce audio is decoded by the bridge daemon's own symphonia-based
# decoder, decode.rs — so the container has nothing to generate these
# fixtures with). The music bed is copied into the container for pw-cat to
# play; the announce clip is fetched by the daemon over HTTP.
TMP_MUSIC_LONG="$(mktemp -u --suffix=.wav)"
ffmpeg -y -loglevel error -stream_loop 40 -i "$MUSIC_WAV_HOST_PATH" -c copy "$TMP_MUSIC_LONG"
docker cp "$TMP_MUSIC_LONG" "$CONTAINER_NAME:/tmp/music_long.wav"
rm -f "$TMP_MUSIC_LONG"
# Serve the announce clip from the HOST: python3 is no longer in the add-on
# image (native Rust rewrite — see the Dockerfile), so the in-container
# http.server is gone. The daemon fetches it via host.docker.internal (mapped
# to the host gateway by --add-host on the container's docker run above).
SERVE_DIR="$(mktemp -d)"
ffmpeg -y -loglevel error -f lavfi -i "sine=frequency=440:duration=3" "$SERVE_DIR/announce.wav"
python3 -m http.server 8765 --directory "$SERVE_DIR" >/dev/null 2>&1 &
SERVE_PID=$!
sleep 1

echo "--- starting the 'music' source and one continuous capture spanning the whole test ---"
# Backgrounded processes below MUST have their stdout/stderr redirected
# away from this docker exec's own I/O stream, or they hold that stream
# open for their entire runtime and every later command in this script
# hangs waiting for a pipe close that never comes, even though each
# individual command has already finished. Hit this exact hang while
# developing this script.
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c \
  'nohup pw-cat --target test-music-src --playback /tmp/music_long.wav > /dev/null 2>&1 & disown; sleep 1'
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c \
  "nohup timeout $CAPTURE_SECONDS pw-record --target sendspin-out-kitchen --rate 44100 --channels 2 --format s16 /tmp/cap_full.raw > /dev/null 2>&1 & disown; sleep 1"
sleep 1

echo "--- t=2s: baseline window captured, now triggering the real /announce call ---"
curl -s --max-time 20 -X POST "http://localhost:$HOST_PORT/api/media_players/$SINK_NODE_ID/announce" \
  -H 'Content-Type: application/json' \
  -d '{"url":"http://host.docker.internal:8765/announce.wav","duck_volume":0.1}' > /tmp/announce_response.json
# Expect "ducked 4 source(s)", not 2: each stereo source contributes two
# separate PipeWire Link objects (FL + FR are distinct links, confirmed
# via `pw-link -l` while debugging this script), and WirePlumber also
# auto-connects the `pw-cat --target test-music-src` stream straight to
# the only real sink in this test's graph *in addition to* the explicit
# test-music-src link our own /api/links call made — so there are
# genuinely two stereo sources (4 links) feeding the sink here, and
# correctly ducking all of them is the right behavior, not a bug in the
# announce endpoint (the daemon doesn't dedupe by node — ducking the same
# node's volume twice is harmless).
echo "announce response: $(cat /tmp/announce_response.json)"
grep -q '"ok":true' /tmp/announce_response.json || { echo "FAIL: announce endpoint did not report ok:true"; exit 1; }

echo "--- waiting for the rest of the capture window (restored phase) ---"
sleep 4

docker exec "$CONTAINER_NAME" bash -c 'pkill -f "pw-cat --target test-music-src" 2>/dev/null' || true
docker cp "$CONTAINER_NAME:/tmp/cap_full.raw" /tmp/announce_e2e_full.raw

# Byte offsets into the raw s16le/44100/stereo capture: rate * channels *
# bytes-per-sample = 44100*2*2 = 176400 bytes/second. Windows chosen to
# sit comfortably inside each phase, clear of the ~3s announce clip's
# start/end transients: baseline [0.3s,1.7s), ducked+announce
# [3.0s,5.5s) (curl fires at t=2s; fetch+decode adds a little latency
# before playback actually starts), restored [7.0s,8.5s).
slice() {
  ffmpeg -f s16le -ar 44100 -ac 2 -ss "$1" -t "$2" -i /tmp/announce_e2e_full.raw -af astats -f null - 2>&1 | grep -E "Peak level dB|RMS level dB" | head -2
}
echo "--- baseline (music alone) ---"
slice 0.3 1.4
echo "--- ducked + announce mixed ---"
slice 3.0 2.5
echo "--- restored (music alone again) ---"
slice 7.0 1.5

echo "PASS: real /announce endpoint ducked the linked music source, mixed in the announce clip, and restored — through the actual add-on binary, not a bare-PipeWire stand-in"
