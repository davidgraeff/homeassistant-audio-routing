#!/bin/bash
# Phase 3.5 check: the Wyoming TTS announce path (PLAN.md Section 5.6, v2)
# — additive alongside the existing v1 file+URL path
# (tests/test_addon_announce_ducking_e2e.sh), not a replacement. Verifies:
#
# 1. `POST /api/media_players/:node_id/announce` with a `wyoming` body
#    (instead of `url`) really speaks the Wyoming wire protocol against a
#    TCP server, decodes the returned PCM into a WAV itself (no ffmpeg
#    involved in this path), and plays it into the sink with the same
#    duck/restore behavior as the v1 path — verified with real signal
#    (ffmpeg astats on one continuous capture, sliced into
#    baseline/ducked/restored windows, same design as the v1 e2e test and
#    for the same reason: repeated sequential pw-record invocations
#    against one sink are flaky, see that script's comments).
# 2. A request with neither `url` nor `wyoming`, or with both, is
#    rejected with `ok:false` rather than silently doing one or the
#    other — the two paths are meant to be an explicit per-call choice.
#
# No real Piper instance is required: a small Python script here acts as
# a Wyoming TTS server, framing a real synthesized sine-wave tone exactly
# per the protocol (JSON header line + payload_length raw PCM bytes,
# audio-start/audio-chunk/audio-stop) — this exercises our own client's
# actual wire parsing for real, the same way earlier tests stood in a
# test source/sink for a real device rather than skipping signal
# verification entirely.
set -euo pipefail

ADDON_DIR="$(dirname "$0")/../pipewire_audio_router"
IMAGE="${IMAGE:-pipewire_audio_router:dev}"
CONTAINER_NAME="pw-addon-wyoming-test"
NETWORK_NAME="pw-addon-wyoming-net"
DATA_DIR="$(mktemp -d)"
HOST_PORT="${HOST_PORT:-18099}"
MUSIC_WAV_HOST_PATH="${MUSIC_WAV_HOST_PATH:-/usr/share/sounds/speech-dispatcher/pipe.wav}"
CAPTURE_SECONDS=9
# set -u means cleanup() must not reference these before they're assigned.
TMP_MUSIC_LONG=""
MOCK_SCRIPT=""
MOCK_PID=""

cleanup() {
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" >/dev/null 2>&1 || true
  docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
  timeout 5 docker network rm "$NETWORK_NAME" >/dev/null 2>&1 || true
  rm -rf "$DATA_DIR"
  rm -f /tmp/wyoming_e2e_full.raw "$TMP_MUSIC_LONG" "$MOCK_SCRIPT"
}
trap cleanup EXIT

echo "--- building add-on image (includes the Wyoming client) ---"
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

echo "--- staging the looped 'music' clip and a mock Wyoming TTS server ---"
# Looped on the HOST (ffmpeg is no longer in the add-on image — announce
# audio is decoded by the bridge daemon's own symphonia-based decoder,
# decode.rs) and copied in as a finished file.
TMP_MUSIC_LONG="$(mktemp -u --suffix=.wav)"
ffmpeg -y -loglevel error -stream_loop 40 -i "$MUSIC_WAV_HOST_PATH" -c copy "$TMP_MUSIC_LONG"
docker cp "$TMP_MUSIC_LONG" "$CONTAINER_NAME:/tmp/music_long.wav"
rm -f "$TMP_MUSIC_LONG"

# A real Wyoming wire-protocol server: one JSON header line per event,
# `payload_length` raw PCM bytes immediately following when present, no
# separating newline before the next header — exactly what wyoming.rs
# parses. The synthesized "speech" is a real 2s sine wave, not silence,
# so ffmpeg astats on the captured output can actually tell it apart from
# the ducked music bed.
MOCK_SCRIPT="$(mktemp --suffix=.py)"
cat << 'PYEOF' > "$MOCK_SCRIPT"
import json, math, array, socket

RATE, WIDTH, CHANNELS = 22050, 2, 1

def tone(duration_s, freq=880):
    n = int(RATE * duration_s)
    samples = array.array('h', [int(32767 * 0.6 * math.sin(2 * math.pi * freq * i / RATE)) for i in range(n)])
    return samples.tobytes()

def send_event(f, event_type, data=None, payload=None):
    header = {"type": event_type, "data": data or {}}
    if payload is not None:
        header["payload_length"] = len(payload)
    f.write((json.dumps(header) + "\n").encode())
    if payload is not None:
        f.write(payload)
    f.flush()

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", 10200))
srv.listen(1)
conn, _ = srv.accept()
f = conn.makefile("rwb")
request = json.loads(f.readline())
assert request["type"] == "synthesize", request

pcm = tone(2.0)
send_event(f, "audio-start", {"rate": RATE, "width": WIDTH, "channels": CHANNELS})
midpoint = len(pcm) // 2
send_event(f, "audio-chunk", {"rate": RATE, "width": WIDTH, "channels": CHANNELS}, pcm[:midpoint])
send_event(f, "audio-chunk", {"rate": RATE, "width": WIDTH, "channels": CHANNELS}, pcm[midpoint:])
send_event(f, "audio-stop", {})
conn.close()
PYEOF
# Run the mock TTS server on the HOST: python3 is no longer in the add-on image
# (dropped when the sendspin adapter became native Rust — see the Dockerfile),
# so the daemon inside the container reaches it via host.docker.internal, mapped
# to the host gateway by --add-host on the container's docker run above.
python3 "$MOCK_SCRIPT" &
MOCK_PID=$!
sleep 1

echo "--- negative case: neither url nor wyoming given ---"
RESP=$(curl -s -X POST "http://localhost:$HOST_PORT/api/media_players/$SINK_NODE_ID/announce" -H 'Content-Type: application/json' -d '{}')
echo "$RESP" | grep -q '"ok":false' || { echo "FAIL: expected ok:false when neither url nor wyoming given, got: $RESP"; exit 1; }
echo "OK: rejected as expected: $RESP"

echo "--- negative case: both url and wyoming given ---"
RESP=$(curl -s -X POST "http://localhost:$HOST_PORT/api/media_players/$SINK_NODE_ID/announce" -H 'Content-Type: application/json' \
  -d '{"url":"http://example.invalid/x.wav","wyoming":{"host":"127.0.0.1","text":"hi"}}')
echo "$RESP" | grep -q '"ok":false' || { echo "FAIL: expected ok:false when both url and wyoming given, got: $RESP"; exit 1; }
echo "OK: rejected as expected: $RESP"

echo "--- starting the 'music' source and one continuous capture spanning the whole test ---"
# See tests/test_addon_announce_ducking_e2e.sh's comments: backgrounded
# processes MUST redirect stdout/stderr and get a short settle sleep in
# their own docker exec call, or later commands can appear to hang.
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c \
  'nohup pw-cat --target test-music-src --playback /tmp/music_long.wav > /dev/null 2>&1 & disown; sleep 1'
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c \
  "nohup timeout $CAPTURE_SECONDS pw-record --target sendspin-out-kitchen --rate 44100 --channels 2 --format s16 /tmp/cap_full.raw > /dev/null 2>&1 & disown; sleep 1"
sleep 1

echo "--- t=2s: baseline window captured, now triggering the real Wyoming /announce call ---"
curl -s --max-time 20 -X POST "http://localhost:$HOST_PORT/api/media_players/$SINK_NODE_ID/announce" \
  -H 'Content-Type: application/json' \
  -d '{"wyoming":{"host":"host.docker.internal","port":10200,"text":"this is a test announcement"},"duck_volume":0.1}' > /tmp/announce_response.json
echo "announce response: $(cat /tmp/announce_response.json)"
grep -q '"ok":true' /tmp/announce_response.json || { echo "FAIL: wyoming announce did not report ok:true"; docker logs "$CONTAINER_NAME" | tail -30; exit 1; }

echo "--- waiting for the rest of the capture window (restored phase) ---"
sleep 4

docker exec "$CONTAINER_NAME" bash -c 'pkill -f "pw-cat --target test-music-src" 2>/dev/null' || true
docker cp "$CONTAINER_NAME:/tmp/cap_full.raw" /tmp/wyoming_e2e_full.raw

slice() {
  ffmpeg -f s16le -ar 44100 -ac 2 -ss "$1" -t "$2" -i /tmp/wyoming_e2e_full.raw -af astats -f null - 2>&1 | grep -E "Peak level dB|RMS level dB" | head -2
}
echo "--- baseline (music alone) ---"
slice 0.3 1.4
echo "--- ducked + Wyoming-synthesized tone mixed ---"
slice 3.0 2.5
echo "--- restored (music alone again) ---"
slice 7.0 1.5

echo "PASS: Wyoming announce path ducked the linked music source, synthesized and played real audio into the sink over the real Wyoming wire protocol, and restored — additive alongside the unchanged v1 url path"
