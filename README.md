# PipeWire Whole-Home Audio Router

A PipeWire-based replacement for Music Assistant's audio engine: phones/PCs
and a dedicated Bluetooth bridge box stream in, a Rust daemon routes/mixes
them through a native PipeWire graph, and Home Assistant gets ordinary
`media_player` entities — including real ducked TTS announcements — out
the other end. Built to fix Music Assistant's audible stutter and
multi-second stream startup on a Raspberry Pi 4 by moving the audio path
off a Python engine and onto PipeWire's own realtime graph.

![System architecture](docs/diagrams/system-architecture.svg)

## Status

**Functionally complete for daily use; not yet cut over from Music
Assistant.** Every phase through the manual routing UI is done and
verified end-to-end against real hardware/signal measurements, not just
unit tests — see [docs/roadmap.md](docs/roadmap.md) for the itemized
status and what's still open (mainly: final latency/soak measurements on
the real Pi 4, and the actual MA cutover).

## Components

This repo is three independently-installable pieces plus supporting dev
tooling. Each has its own README with install/config/usage details —
this file is just the map.

| Component | What | README |
|---|---|---|
| **Bridge daemon add-on** | Rust + PipeWire/WirePlumber, packaged as a real HA add-on (`repository.yaml` at repo root) | [`pipewire_audio_router/`](pipewire_audio_router/README.md) |
| **Home Assistant integration** | `media_player` entities backed by the add-on's REST API | [`custom_components/pipewire_audio_router/`](custom_components/pipewire_audio_router/README.md) |
| **Bluetooth bridge firmware** | ESP32 + ESPHome, turns any Bluetooth device into a whole-home-audio source | [`firmware/bt-bridge/`](firmware/bt-bridge/README.md) |

## Quick install overview

1. Add this repo as an HA add-on repository (Settings → Add-ons →
   Add-on Store → ⋮ → Repositories), then install **PipeWire Audio
   Router** and configure your RAOP/sendspin outputs.
2. Copy `custom_components/pipewire_audio_router/` into your HA config's
   `custom_components/`, restart HA, and add the integration (host/port
   of the add-on).
3. *(Optional)* Flash `firmware/bt-bridge/` onto an original-ESP32 board
   (not S2/S3/C3/C6 — see that README for why) to add a Bluetooth-speaker
   source to the router.

Full details, including the add-on's config schema and the integration's
setup flow, are in each component's own README linked above.

## Documentation

- [**docs/architecture.md**](docs/architecture.md) — how the pieces fit
  together, with diagrams.
- [**docs/decisions.md**](docs/decisions.md) — why it's built this way:
  real investigations with concrete findings (RAOP quirks, PipeWire's
  actual volume/module-loading capabilities, ESP32 hardware constraints,
  and more), not preferences.
- [**docs/api-reference.md**](docs/api-reference.md) — the bridge
  daemon's REST/WebSocket API.
- [**docs/roadmap.md**](docs/roadmap.md) — phase-by-phase status and
  acceptance criteria.
- [**spikes/**](spikes/) — the empirical write-up behind every claim in
  `decisions.md`: what was tried, what broke, what fixed it.
- [**tests/**](tests/) — the runnable scripts backing those write-ups;
  nothing here is "verified" without one.

## Repo layout

```
pipewire_audio_router/                the HA add-on: bridge daemon (Rust) + Dockerfile + config.yaml
custom_components/pipewire_audio_router/   HA integration (Python)
firmware/bt-bridge/                   ESPHome firmware for the Bluetooth bridge box
container/                            throwaway bare-PipeWire dev sandbox — not the real add-on
docs/                                 architecture, decisions, API reference, roadmap
spikes/                               per-experiment write-ups
tests/                                verification scripts
scripts/                              dev tooling (e.g. arm64 cross-build)
PLAN.md                               historical planning document, superseded by docs/
```
