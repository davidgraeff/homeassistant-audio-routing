#!/bin/bash
# Verifies shairport-sync works as an AirPlay-receive source feeding real
# audio into PipeWire (PLAN.md Section 5.2), fully automated:
#
#   cliraop (real RAOP/AirPlay-1 sender) -> shairport-sync -> ALSA "default"
#   (already routed to PipeWire in this image, see spikes/shairport-sync-source.md)
#   -> PipeWire playback stream -> linked into a test sink -> pw-record ->
#   real-signal verification (ffmpeg astats, not just byte counts).
#
# Requires --network host (shairport-sync/avahi need real interface access,
# consistent with every other real-network spike in this project) and a
# built cliraop binary. Reuses the one bundled with Music Assistant's
# airplay provider rather than vendoring a duplicate copy here — override
# CLIRAOP_PATH if that repo isn't checked out at the expected location.
set -euo pipefail

IMAGE="${IMAGE:-pw-audio-router:dev}"
SPIKE_IMAGE="pw-shairport-spike:dev"
CONTAINER_NAME="pw-shairport-spike-test"
WAV_HOST_PATH="${WAV_HOST_PATH:-/usr/share/sounds/speech-dispatcher/pipe.wav}"
CLIRAOP_PATH="${CLIRAOP_PATH:-$(dirname "$0")/../../music-assistant-server/music_assistant/providers/airplay/bin/cliraop-linux-x86_64}"

if [ ! -x "$CLIRAOP_PATH" ]; then
  echo "FAIL: cliraop binary not found/executable at $CLIRAOP_PATH — set CLIRAOP_PATH"
  exit 1
fi

cleanup() {
  docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "--- building shairport-sync spike image ---"
docker build -t "$SPIKE_IMAGE" --build-arg BASE_IMAGE="$IMAGE" -f "$(dirname "$0")/shairport_sync_spike.Dockerfile" "$(dirname "$0")"

docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER_NAME" --network host --entrypoint bash "$SPIKE_IMAGE" -c '
set -e
mkdir -p /run/dbus
dbus-daemon --system --fork
export XDG_RUNTIME_DIR=/run/pipewire
mkdir -p "$XDG_RUNTIME_DIR"
eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS
pipewire > /tmp/pw.log 2>&1 &
sleep 1
wireplumber > /tmp/wp.log 2>&1 &
sleep 2
avahi-daemon --daemonize --no-drop-root
sleep 1
shairport-sync -v -a "SpikeTest" > /tmp/sps.log 2>&1 &
sleep 2
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=shairport-test-sink media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }" >/dev/null
sleep infinity
' >/dev/null

echo "--- waiting for shairport-sync + test sink to be ready ---"
sleep 4
if ! docker exec "$CONTAINER_NAME" grep -q "started at" /tmp/sps.log 2>/dev/null; then
  echo "FAIL: shairport-sync did not start cleanly"
  docker exec "$CONTAINER_NAME" cat /tmp/sps.log
  exit 1
fi
echo "OK: shairport-sync running, mDNS/D-Bus services started"

# cliraop sends well ahead of real-time (it feeds the receiver's buffer,
# then exits — it does not block for the full nominal playback duration),
# so the resulting PipeWire node only exists for a couple of seconds
# regardless of clip length. Loop the short sample out to ~8s anyway so
# there's a comfortable window to link into it before cliraop finishes.
PLAYBACK_WAV="$WAV_HOST_PATH"
TEMP_WAV=""
if command -v ffmpeg >/dev/null 2>&1; then
  TEMP_WAV="$(mktemp --suffix=.wav)"
  ffmpeg -y -loglevel error -stream_loop 9 -i "$WAV_HOST_PATH" -c copy "$TEMP_WAV"
  PLAYBACK_WAV="$TEMP_WAV"
fi

echo "--- capturing from test sink while sending a real AirPlay stream via cliraop ---"
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" \
  bash -c 'timeout 6 pw-record --target shairport-test-sink --rate 44100 --channels 2 --format s16 /tmp/captured.raw' &
RECORD_PID=$!
sleep 0.5

# -et 0 tells cliraop "not an MFi/AirPort-Express device" — without it,
# cliraop unconditionally probes POST /auth-setup first and segfaults on
# the 500 response shairport-sync gives back (it doesn't advertise POST
# in its OPTIONS response at all; confirmed via a raw RTSP probe). This is
# a cliraop client bug, not a shairport-sync problem.
"$CLIRAOP_PATH" -et 0 127.0.0.1 "$PLAYBACK_WAV" > /tmp/cliraop_spike.log 2>&1 &
CLIRAOP_PID=$!

# cliraop's own process wraps up within ~1-2s of connecting (it feeds the
# receiver's buffer well ahead of real-time, then exits), so the resulting
# "alsa_playback.shairport-sync" PipeWire node only exists for a short,
# none-too-predictable window. A single fixed sleep-then-link raced this
# and failed intermittently in practice — retry tightly instead of
# guessing a delay.
echo "--- linking the shairport-sync PipeWire stream into the test sink (tight retry, node is short-lived) ---"
LINKED=""
for _ in $(seq 1 40); do
  if docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c '
    pw-link "alsa_playback.shairport-sync:output_FL" shairport-test-sink:playback_FL 2>/dev/null &&
    pw-link "alsa_playback.shairport-sync:output_FR" shairport-test-sink:playback_FR 2>/dev/null
  '; then
    LINKED=1
    break
  fi
  sleep 0.05
done
if [ -z "$LINKED" ]; then
  echo "FAIL: could not link the shairport-sync stream — check port names via pw-link -o"
  [ -n "$TEMP_WAV" ] && rm -f "$TEMP_WAV"
  exit 1
fi

wait "$CLIRAOP_PID" || { echo "FAIL: cliraop exited with an error"; cat /tmp/cliraop_spike.log; [ -n "$TEMP_WAV" ] && rm -f "$TEMP_WAV"; exit 1; }
[ -n "$TEMP_WAV" ] && rm -f "$TEMP_WAV"
wait "$RECORD_PID" || true
docker cp "$CONTAINER_NAME:/tmp/captured.raw" /tmp/shairport_spike_captured.raw

echo "--- signal analysis (peak/RMS — confirms real audio, not silence) ---"
ffmpeg -f s16le -ar 44100 -ac 2 -i /tmp/shairport_spike_captured.raw -af astats -f null - 2>&1 \
  | grep -E "Peak level dB|RMS level dB" | head -2
rm -f /tmp/shairport_spike_captured.raw

echo "PASS: cliraop -> shairport-sync -> ALSA-via-PipeWire -> real captured signal, full chain verified"
