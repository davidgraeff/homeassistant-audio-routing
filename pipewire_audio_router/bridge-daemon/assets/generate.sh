#!/usr/bin/env bash
# Regenerate the committed diagnostic clip test-announcement.mp3 using Gemini
# TTS (https://ai.google.dev/gemini-api/docs/speech-generation).
#
# The API returns base64-encoded *raw* PCM (24 kHz, 16-bit, mono, no container),
# so we decode it and wrap+encode to MP3 with ffmpeg. Run from anywhere; the
# output always lands next to this script.
#
# Requires: curl, jq, ffmpeg, and the API key in the environment (NEVER commit
# it — pass it at call time):
#
#     GEMINI_API_KEY=... ./generate.sh
#
set -euo pipefail

: "${GEMINI_API_KEY:?set GEMINI_API_KEY in the environment (do not hardcode it)}"

TEXT="${TEXT:-This is a test announcement to verify that the announcement functionality is working correctly.}"
VOICE="${VOICE:-Kore}"
MODEL="${MODEL:-gemini-3.1-flash-tts-preview}"

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$DIR/test-announcement.mp3"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Requesting TTS from $MODEL (voice: $VOICE)…"
curl -sS -X POST "https://generativelanguage.googleapis.com/v1beta/interactions" \
  -H "x-goog-api-key: $GEMINI_API_KEY" \
  -H "Content-Type: application/json" \
  -d "$(jq -n --arg model "$MODEL" --arg input "$TEXT" --arg voice "$VOICE" '{
        model: $model,
        input: $input,
        response_format: { type: "audio" },
        generation_config: { speech_config: [ { voice: $voice } ] }
      }')" \
  -o "$TMP/resp.json"

# Pull the audio part out of the /interactions response. It lives as an
# `audio` content item inside a model_output step, carrying the base64 PCM plus
# its own format (mime_type audio/l16 = signed-16-bit LE PCM, channels, rate):
#   { "steps": [ { "content": [ { "type": "audio", "data": "…",
#                                 "channels": 1, "sample_rate": 24000 } ] } ] }
AUDIO="$(jq -c 'first(.steps[]?.content[]? | select(.type == "audio"))' "$TMP/resp.json")"

if [ -z "$AUDIO" ] || [ "$AUDIO" = "null" ]; then
  echo "No audio in response — the API returned:" >&2
  cat "$TMP/resp.json" >&2
  exit 1
fi

RATE="$(jq -r '.sample_rate // 24000' <<<"$AUDIO")"
CH="$(jq -r '.channels // 1' <<<"$AUDIO")"
jq -r '.data' <<<"$AUDIO" | base64 -d > "$TMP/audio.pcm"

# The clip is raw signed-16-bit little-endian PCM (audio/l16) at the rate and
# channel count the response reported. Wrap it in that format on input,
# normalize loudness, and encode to a small MP3 (stereo 44.1 kHz, matching the
# rest of the router's audio).
echo "Decoded audio/l16: ${RATE} Hz, ${CH} ch → encoding MP3…"
ffmpeg -y -loglevel error \
  -f s16le -ar "$RATE" -ac "$CH" -i "$TMP/audio.pcm" \
  -ac 2 -ar 44100 -af "loudnorm=I=-16:TP=-1.5:LRA=11" \
  -codec:a libmp3lame -q:a 5 "$OUT"

echo "Wrote $OUT"
ffprobe -loglevel error -show_entries format=duration,bit_rate:stream=channels,sample_rate \
  -of default=noprint_wrappers=1 "$OUT" || true
