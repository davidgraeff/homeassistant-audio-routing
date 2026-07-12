# Roadmap / status

Short version: **Phases 0–4 are done, Phase 5 is functionally done with
one integration item still owed, and Phase 6 (cutover from Music
Assistant) hasn't started.** Every "done" claim below has a real
end-to-end test behind it, not just code that compiles — see the
`tests/` script named alongside each item.

## Phase 0 — Spikes

All the foundational risk questions from before any real building
started. Every spike passed; see [decisions.md](decisions.md) for the
findings and `spikes/*.md` for the full evidence trail per spike.

## Phase 1 — Minimal add-on skeleton — done

`pipewire_audio_router/`, `tests/test_addon_phase1_e2e.sh`. Real HA
add-on skeleton: options schema, multi-stage Dockerfile, `run.sh`
startup sequence, the daemon's `generate-config`/`serve` subcommands.
Verified end-to-end: `/data/options.json` → generated
`pipewire.conf.d` → PipeWire actually loads the `raop-sink` module → the
daemon's registry listener discovers it → the REST API reports it
correctly. Cross-builds for `linux/arm64`.

## Phase 2 — Single source → single sink, end to end — done

`tests/test_addon_phase2_e2e.sh`. Real AirPlay source
(`shairport-sync`) plus real sendspin output (`sendspin-adapter.py`),
linked via a real `POST /api/links` call, verified with independent
signal analysis (peak/RMS via `ffmpeg astats`), not just byte counts.

## Phase 3 — Multi-output mixing + HA entities — done

- **Multi-output fan-out**: one source linked into two outputs
  simultaneously (`tests/test_addon_phase3_multi_output.sh`) — the
  actual "Brave→Dusche+Pioneer" scenario that motivated this project.
  Required no new mixing logic; PipeWire's graph already supports one
  output port linking to many inputs.
- **`media_player` entities**: `custom_components/pipewire_audio_router/`,
  one entity per output the daemon reports, backed by
  `GET /api/media_players`. Verified with 7 real tests against actual HA
  internals (`pytest-homeassistant-custom-component`) — only the
  network layer is mocked.
- **TTS/announce ducking**: `POST /api/media_players/:id/announce`,
  verified end-to-end against the real add-on binary
  (`tests/test_addon_announce_ducking_e2e.sh`): baseline RMS -16.7dB,
  ducked+announce -22.3dB, restored -17.3dB (matches baseline, clean
  restore confirmed).

## Phase 3.5 — Streaming TTS via Wyoming — done, additive

`pipewire_audio_router/bridge-daemon/src/wyoming.rs`,
`tests/test_addon_announce_wyoming_e2e.sh`. Caller picks `url` (v1,
unchanged) or `wyoming` (v2) per call via HA's standard `play_media`
`extra` dict — no daemon-wide mode switch. Verified end-to-end with a
protocol-conformant mock Wyoming server (real synthesized tone, proving
wire-level parsing, not TTS quality). **Not done**: standing up a real
local Piper instance against this now-verified client — should need no
further bridge-daemon code, just a `host`/`port` pointed at Piper's own
Wyoming server.

## Phase 4 — Manual routing web UI — done

`pipewire_audio_router/bridge-daemon/src/routing.rs`,
`bridge-daemon/static/routing_ui.html`,
`tests/test_addon_routing_ui_e2e.sh`. Source × output matrix,
live-updated via WebSocket, driven by real PipeWire registry events
(not polling). Verified end-to-end: real `pw-link -l` output cross-
checked against the API's own claims, WebSocket confirmed pushing a
fresh snapshot within milliseconds of a real link change.

## Phase 5 — Bluetooth bridge box — functionally done, one item owed

`firmware/bt-bridge/`. Real hardware, real pairing, real AVRCP metadata,
all HA entities confirmed live, RTP audio confirmed reaching a real
`rtp-source` node with genuine signal (not silence) — see
[decisions.md](decisions.md#bluetooth-bridge-box-hardware-and-firmware-constraints)
for the three real bugs found and fixed along the way.

**Still open:** this was verified against a native PipeWire session on
a dev machine
(`~/.config/pipewire/pipewire.conf.d/60-bt-bridge-rtp-source.conf`), not
yet inside the actual add-on container. Turn that config into a proper
checked-in template under `container/` (mirroring RAOP's
`10-raop-static.conf`) and re-verify once done.

Bonus beyond original scope: RTP host/port are HA-configurable at
runtime (`text`/`number` entities, `restore_value: true`) — no reflash
needed to repoint the box at a different PipeWire host.

## Phase 6 — Cutover — not started

Side-by-side run with Music Assistant disabled; validate all rooms, all
automations, voice responses; only then uninstall the MA add-on. This
is the one remaining phase with no code behind it yet — everything it
depends on (Phases 1–5) is done.

## Acceptance criteria for declaring the project successful

- Stream start latency (tap play → audible sound) under ~300ms on both
  sendspin and RAOP outputs, vs. the Music Assistant baseline —
  **not yet measured** on real hardware end-to-end (the individual
  mechanisms are proven fast in isolation, e.g. `pipewire-rs`'s 0.07ms
  link creation, but a full tap-to-audible measurement hasn't been run).
- No audible stutter/dropouts during a 30-minute continuous playback
  test on at least 2 simultaneous outputs — not yet run.
- All rooms controllable from HA (volume, play/pause, routing) and from
  voice-assistant TTS — **met** for volume/routing/TTS; play/pause was a
  deliberate non-goal (no clean mapping onto a passive routing sink with
  no queue of its own).
- CPU/memory headroom on the Pi 4 confirmed via `htop`/`docker stats`
  during simultaneous 3-output playback — not yet run; all testing so
  far has been on dev hardware (x86_64) plus arm64 cross-build smoke
  tests, not the real target Pi 4.

These four are the actual gate for Phase 6. Everything else in this
roadmap is infrastructure to make these measurable, not the goal itself.
