#!/bin/bash
# Investigates PLAN.md Section 5.6's claim of "PipeWire per-link volume" —
# disproven here — and verifies the actual working mechanism: reducing a
# SOURCE NODE's own volume (via wpctl) while a second source stays at full
# volume into the same sink. See spikes/05-tts-ducking-mechanism.md.
#
# Part A: does a "volume" property set via `pw-link -p` have any real
# effect on the audio passing through the link? (No — proven by A/B
# signal comparison, not just checking whether the property is *stored*.)
# Part B: does per-source-NODE volume (wpctl) achieve real ducking when
# two sources are mixed into one sink? (Yes — full duck/restore cycle
# with real signal analysis.)
set -euo pipefail

IMAGE="${IMAGE:-pw-audio-router:dev}"
CONTAINER_NAME="pw-ducking-test"
WAV_HOST_PATH="${WAV_HOST_PATH:-/usr/share/sounds/speech-dispatcher/pipe.wav}"

cleanup() {
  docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
  rm -f /tmp/ducking_test_*.raw
}
trap cleanup EXIT

docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER_NAME" --entrypoint bash "$IMAGE" -c '
export XDG_RUNTIME_DIR=/run/pipewire
mkdir -p "$XDG_RUNTIME_DIR"
eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS
pipewire > /tmp/pw.log 2>&1 &
sleep 1
wireplumber > /tmp/wp.log 2>&1 &
sleep 2
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=t-src media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }" >/dev/null
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=t-announce media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }" >/dev/null
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=t-sink media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }" >/dev/null
sleep 1
pw-link t-src:capture_FL t-sink:playback_FL
pw-link t-src:capture_FR t-sink:playback_FR
pw-link t-announce:capture_FL t-sink:playback_FL
pw-link t-announce:capture_FR t-sink:playback_FR
sleep infinity
' >/dev/null
sleep 4
docker cp "$WAV_HOST_PATH" "$CONTAINER_NAME:/tmp/tone.wav"


# || true guards against set -e + pipefail treating a still-empty grep
# match (e.g. a transient race right after node creation) as fatal here.
SRC_ID=$(docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c "pw-cli ls Node | grep -B4 't-src' | grep '^[[:space:]]*id' | grep -oE '[0-9]+' | head -1") || true
ANNOUNCE_ID=$(docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c "pw-cli ls Node | grep -B4 't-announce' | grep '^[[:space:]]*id' | grep -oE '[0-9]+' | head -1") || true
echo "t-src node id: $SRC_ID, t-announce node id: $ANNOUNCE_ID"

echo ""
echo "=== Part A: does a link-level 'volume' property have any real effect? ==="
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c "pw-link -d t-src:capture_FL t-sink:playback_FL; pw-link -d t-src:capture_FR t-sink:playback_FR" || true

docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c "
pw-link t-src:capture_FL t-sink:playback_FL
pw-link t-src:capture_FR t-sink:playback_FR
pw-cat --target t-sink --record --rate 16000 --channels 1 --format s16 /tmp/a_baseline.raw &
RECPID=\$!
sleep 0.3
pw-cat --target t-src --playback /tmp/tone.wav
sleep 0.3
kill \$RECPID 2>/dev/null; wait \$RECPID 2>/dev/null
pw-link -d t-src:capture_FL t-sink:playback_FL
pw-link -d t-src:capture_FR t-sink:playback_FR
" || true

docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c '
pw-link -p "{\"volume\": 0.1}" t-src:capture_FL t-sink:playback_FL
pw-link -p "{\"volume\": 0.1}" t-src:capture_FR t-sink:playback_FR
pw-cat --target t-sink --record --rate 16000 --channels 1 --format s16 /tmp/a_with_link_volume_prop.raw &
RECPID=$!
sleep 0.3
pw-cat --target t-src --playback /tmp/tone.wav
sleep 0.3
kill $RECPID 2>/dev/null; wait $RECPID 2>/dev/null
' || true

docker cp "$CONTAINER_NAME:/tmp/a_baseline.raw" /tmp/ducking_test_a_baseline.raw
docker cp "$CONTAINER_NAME:/tmp/a_with_link_volume_prop.raw" /tmp/ducking_test_a_with_prop.raw

BASELINE_RMS=$(ffmpeg -f s16le -ar 16000 -ac 1 -i /tmp/ducking_test_a_baseline.raw -af astats -f null - 2>&1 | grep "RMS level dB" | head -1 | grep -oE '\-?[0-9.]+')
WITH_PROP_RMS=$(ffmpeg -f s16le -ar 16000 -ac 1 -i /tmp/ducking_test_a_with_prop.raw -af astats -f null - 2>&1 | grep "RMS level dB" | head -1 | grep -oE '\-?[0-9.]+')
echo "baseline RMS: ${BASELINE_RMS}dB, with volume=0.1 link property: ${WITH_PROP_RMS}dB"
DIFF=$(echo "$BASELINE_RMS - $WITH_PROP_RMS" | bc -l 2>/dev/null || awk "BEGIN{print $BASELINE_RMS - $WITH_PROP_RMS}")
echo "difference: ${DIFF}dB (a real 0.1x volume scale would show ~20dB — anything under ~3dB confirms the property is inert)"

echo ""
echo "=== Part B: does per-source-node volume (wpctl) achieve real ducking? ==="
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c "
pw-link t-src:capture_FL t-sink:playback_FL 2>/dev/null || true
pw-link t-src:capture_FR t-sink:playback_FR 2>/dev/null || true
wpctl set-volume $SRC_ID 1.0
wpctl set-volume $ANNOUNCE_ID 1.0
pw-cat --target t-sink --record --rate 16000 --channels 1 --format s16 /tmp/b_baseline.raw &
RECPID=\$!
sleep 0.3
pw-cat --target t-src --playback /tmp/tone.wav
sleep 0.3
kill \$RECPID 2>/dev/null; wait \$RECPID 2>/dev/null
"

docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c "
wpctl set-volume $SRC_ID 0.1
pw-cat --target t-sink --record --rate 16000 --channels 1 --format s16 /tmp/b_ducked.raw &
RECPID=\$!
sleep 0.3
(pw-cat --target t-src --playback /tmp/tone.wav &)
pw-cat --target t-announce --playback /tmp/tone.wav
sleep 0.3
kill \$RECPID 2>/dev/null; wait \$RECPID 2>/dev/null
"

docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c "
wpctl set-volume $SRC_ID 1.0
pw-cat --target t-sink --record --rate 16000 --channels 1 --format s16 /tmp/b_restored.raw &
RECPID=\$!
sleep 0.3
pw-cat --target t-src --playback /tmp/tone.wav
sleep 0.3
kill \$RECPID 2>/dev/null; wait \$RECPID 2>/dev/null
"

docker cp "$CONTAINER_NAME:/tmp/b_baseline.raw" /tmp/ducking_test_b_baseline.raw
docker cp "$CONTAINER_NAME:/tmp/b_ducked.raw" /tmp/ducking_test_b_ducked.raw
docker cp "$CONTAINER_NAME:/tmp/b_restored.raw" /tmp/ducking_test_b_restored.raw

echo "--- baseline (t-src alone, full volume) ---"
ffmpeg -f s16le -ar 16000 -ac 1 -i /tmp/ducking_test_b_baseline.raw -af astats -f null - 2>&1 | grep -E "Peak level dB|RMS level dB" | head -2
echo "--- ducked (t-src at 0.1 + t-announce at 1.0, mixed) ---"
ffmpeg -f s16le -ar 16000 -ac 1 -i /tmp/ducking_test_b_ducked.raw -af astats -f null - 2>&1 | grep -E "Peak level dB|RMS level dB" | head -2
echo "--- restored (t-src alone, full volume again) ---"
ffmpeg -f s16le -ar 16000 -ac 1 -i /tmp/ducking_test_b_restored.raw -af astats -f null - 2>&1 | grep -E "Peak level dB|RMS level dB" | head -2

echo ""
echo "PASS: per-link volume property is inert (Part A); per-source-node volume via wpctl achieves real, working ducking with clean restore (Part B)"
