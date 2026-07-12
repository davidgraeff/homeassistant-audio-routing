# Spike 3 result: sendspin via aiosendspin + PipeWire capture — PASSED

Per PLAN.md Section 7 spike #3 and Section 5.4b. Two scripts, each
proving one half of the design; both pass cleanly.

## Half 1 — the aiosendspin protocol mechanism (no PipeWire involved yet)

`tests/spike03_sendspin_pushstream.py`: runs a real `aiosendspin`
`SendspinServer` (the same library MA depends on,
`aiosendspin[server]==6.1.1`, matching MA's pinned version) *and* a real
`aiosendspin` client — the library's own client-side implementation, the
reference implementation of the wire protocol — in one process, connected
over a genuine WebSocket (`ws://127.0.0.1:8927/sendspin`). Feeds 2
seconds of synthetic sine-wave PCM into the server-side `PushStream` via
`group.start_stream()` → `prepare_audio()` → `commit_audio()`, and
verifies the client's `add_audio_chunk_listener` receives every byte.

Result: **80/80 chunks, 384000/384000 bytes**, zero loss.

One correction needed to get a clean run: my first attempt set
`buffer_capacity=1000` (bytes) on the fake client's `player_support`,
which is far smaller than a single 100ms chunk (4800 bytes at 48kHz
stereo 16-bit) — the server logged "Chunk size exceeds reported buffer
capacity... blocking until buffer drains" for every chunk and dropped a
few late ones. Not a library bug — `buffer_capacity` is genuinely in
bytes, and a real device needs to advertise something realistic (fixed
to `SAMPLE_RATE * CHANNELS * bytes_per_sample * 2`, i.e. ~2 seconds of
headroom). Also added `supported_commands=[VOLUME, MUTE]` to silence
"client sent volume/muted field without declaring..." warnings — cosmetic
config issues in the test harness, not the protocol.

## Half 2 — the full PipeWire integration (the actual Section 5.4b design)

`tests/spike03_pipewire_capture_to_sendspin.py`: proves the complete
path the real sendspin sink adapter needs, using real PipeWire, not
synthetic data:

```
pw-cat (plays a real WAV) --> sendspin-test-sink (PipeWire Audio/Sink)
                                     |
                                     | pw-record --target sendspin-test-sink
                                     v
                        this script's asyncio loop
                                     |
                                     v
               aiosendspin PushStream.prepare_audio()/commit_audio()
                                     |
                                     v (real WebSocket)
                    aiosendspin client (stand-in for a real ESP32)
```

Confirmed empirically before writing the integration: `pw-record
--target <sink-name>` captures exactly what's playing into that sink via
its monitor ports by default — no extra flags needed, byte counts lined
up precisely with capture duration in a manual check.

Result:
```
captured from PipeWire: 1163288 bytes
pushed to aiosendspin:  60 chunks (1152000 bytes)
received by fake ESP32: 240 chunks (1152000 bytes)
PASS
```
Pushed and received bytes match exactly (1,152,000 = 1,152,000). The
~11KB gap between captured and pushed is just the last partial 100ms
chunk still sitting in the read buffer when the test's deadline hit —
expected, not a loss.

## What this confirms about Section 5.4b's design

- No custom sendspin protocol code is needed — `aiosendspin` (already a
  MA dependency) is a complete, working, standalone server
  implementation. Real ESP32 devices already speak this exact wire
  protocol against MA today; nothing about the protocol changes here.
- The only genuinely new piece — capturing PCM from a PipeWire sink and
  feeding it into `PushStream` instead of MA's own ffmpeg/queue pipeline
  — works exactly as designed, byte-for-byte, using the simplest possible
  mechanism (`pw-record` as a subprocess, no custom PipeWire bindings
  needed for capture).
- `client.group` auto-creates a `SendspinGroup` per connected client;
  `group.start_stream()` returns the `PushStream` — no manual group
  wiring required, simpler than initially expected from reading
  `push_stream.py`'s large, MA-feature-rich internals (multi-codec
  transcoding, catch-up, historical rewind) — none of which this project
  needs for v1; the public surface (`prepare_audio`/`commit_audio`) is
  small.

## Not yet tested: a real ESP32 device

Both scripts use `aiosendspin`'s own client library as the stand-in for
a real ESP32, not actual ESPHome firmware. This is a reasonable stopping
point for this spike specifically because the *protocol* isn't new or in
question — real ESP32 sendspin devices already connect to this exact
library's server side in production via MA today. What was actually
unproven (and is now proven) is the PipeWire-capture feed mechanism,
which is invisible to the client either way — same bytes over the same
WebSocket regardless of where the server sourced them from. A real-ESP32
round-trip would still be a reasonable final confidence check whenever
convenient, but isn't blocking further work the way spike 2's hardware
testing was (that surfaced three real, otherwise-invisible bugs; this
protocol has none of RAOP's version/encryption/port landmines since it's
the same library already working in your house today).

## Files added

- `tests/spike03_sendspin_pushstream.py` — protocol-only proof.
- `tests/spike03_pipewire_capture_to_sendspin.py` — full PipeWire
  integration proof.
- `tests/spike03.Dockerfile` — throwaway image layering Python +
  `aiosendspin[server]` on top of the real `pw-audio-router:dev` image.
  Not part of the production Dockerfile — bridge-daemon packaging
  (same container vs. sidecar) is a later decision (PLAN.md Section 5.5).
