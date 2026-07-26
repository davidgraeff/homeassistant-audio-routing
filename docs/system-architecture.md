# Architecture

This project replaces Music Assistant's Python audio engine with a native
PipeWire graph for whole-home audio routing, controlled by a small Rust
daemon and exposed to Home Assistant as ordinary `media_player` entities.
The reasoning behind *why* it's built this way — the investigations,
dead ends, and hardware-specific findings — lives in
[decisions.md](decisions.md). This document describes the system as it
stands.

## System overview

![System architecture](diagrams/system-architecture.svg)

Three independently-installable pieces:

| Component | What it is | Where |
|---|---|---|
| **Bridge daemon add-on** | Rust binary + PipeWire/WirePlumber, packaged as a Home Assistant add-on | [`pipewire_audio_router/`](../pipewire_audio_router/README.md) |
| **Home Assistant integration** | Python `custom_components/` integration exposing `media_player` entities backed by the add-on's REST API | [`custom_components/pipewire_audio_router/`](../custom_components/pipewire_audio_router/README.md) |
| **Bluetooth bridge** | A Bluetooth-speaker-like A2DP sink that feeds audio into the router via RTP. Two interchangeable implementations: ESPHome firmware on an ESP32, or a pure-config (BlueZ + PipeWire, no firmware) setup on a Raspberry Pi | [`firmware/bt-bridge/`](../firmware/bt-bridge/README.md) · [`firmware/pi-bridge/`](../firmware/pi-bridge/README.md) |

None of these three depend on each other at build time — the add-on runs
standalone (with its own manual routing web UI), the HA integration just
needs *some* reachable instance of the add-on's API, and the BT bridge
(ESP32 firmware or Pi config) just needs an RTP-source endpoint (normally
the add-on, but any PipeWire `rtp-source` would do).

## Audio flow

At the system level the flow is: **sources** feed PCM into a PipeWire
graph inside the add-on container, the **graph** routes/mixes any source
to any set of **outputs**, the **bridge daemon** controls all of that and
exposes it over REST/WebSocket, and **Home Assistant** drives it through
the `custom_components` integration. The internals of each stage — the
anchor + per-device-sender model, the two output backends, the AP2 PTP
clock, the real-time thread ladder — are the bridge daemon's own concern
and are documented in detail in
[`../pipewire_audio_router/docs/architecture.md`](../pipewire_audio_router/docs/architecture.md).

1. **Sources.** Phones/PCs stream in as an **AirPlay receiver** (a native,
   in-process RAOP receiver, `airplay-in`); the **Bluetooth bridge box**
   (ESP32/Pi A2DP sink) RTP-encodes audio into the container's
   `rtp-source` node — see
   [bt-bridge-firmware.svg](diagrams/bt-bridge-firmware.svg).
2. **The PipeWire graph** is a plain many-to-many routing/mixing graph —
   any source can be linked to any set of sinks at once (the "one AirPlay
   source → AV receiver **and** Voice PE speaker simultaneously" scenario
   that motivated this project).
3. **Outputs** are two backends, both fed from one steady per-group anchor
   with only the *sender* split per device: **AirPlay-2** (in-process Rust
   AP2 sender + PTP grandmaster, for AV receivers like Yamaha/Pioneer —
   replacing the older RAOP path) and **Sendspin** (auto-discovered
   ESPHome speakers like HA Voice PE, streamed over WebSocket).
4. **The bridge daemon** (Rust) is the only thing that knows about HA,
   config files, or "what a room is." It observes the live PipeWire
   registry and exposes state + native mutation over a REST + WebSocket API
   (full reference: [api-reference.md](api-reference.md)); the AirPlay
   receiver, Sendspin servers, AP2 senders and RTP source all run
   in-process.
5. **Home Assistant** talks to that API through the `custom_components`
   integration, which creates one `media_player` entity per routing-matrix
   **output**. Volume, `play_media`/announce (real
   `MediaPlayerEntityFeature.MEDIA_ANNOUNCE`, ducking not replacing),
   source selection and state all round-trip through the daemon's REST API;
   a `cleanup_entities` service purges stale leftovers. The daemon's own
   `GET /` serves a manual source × output routing matrix as an
   alternative to the HA entities.

## Bluetooth bridge box

![BT bridge firmware architecture](diagrams/bt-bridge-firmware.svg)

This is a separate ESP32 device (confirmed: original `ESP32-D0WDQ6` —
the only current-generation-adjacent chip with classic Bluetooth
hardware at all) running ESPHome plus a custom `external_components`
package (`a2dp_bridge`) that ESPHome has no built-in equivalent for.
It's a normal ESPHome device otherwise — WiFi, OTA, HA native API — none
of which conflicts with owning the classic-BT radio, since ESPHome only
touches Bluetooth when `esp32_ble`-family components are present in
YAML, and this device's YAML deliberately never includes any of them.

The firmware pairs as an ordinary Bluetooth speaker (A2DP sink + AVRCP
controller), decodes the negotiated SBC stream internally (handled by
the ESP-IDF Bluedroid stack), and re-encodes the resulting PCM as plain
RTP/UDP packets aimed at a configurable host:port — normally the
add-on's `rtp-source` node, reusing that already-proven receiving path
rather than inventing a new transport (`sendspin-cpp` was investigated
and ruled out for this direction — see decisions.md). The RTP target is
runtime-configurable from Home Assistant (`text`/`number` entities with
`restore_value: true`), not just a compile-time secret.

### Raspberry Pi alternative (no firmware)

The same role can be filled by a **Raspberry Pi** instead of the ESP32 —
[`firmware/pi-bridge/`](../firmware/pi-bridge/README.md). Here there is
**no custom firmware at all**: BlueZ + PipeWire do the A2DP sink and the
RTP send with pure configuration, applied idempotently by a single
[`setup_pi_bridge.py`](../firmware/pi-bridge/README.md) script. It pairs
as an ordinary Bluetooth speaker, then a `module-loopback` +
`module-rtp-sink` chain forwards the audio into the container's *same*
`bt-bridge-rtp` source node the ESP32 feeds — so the add-on accepts either
sender with no change. Because BlueZ negotiates **aptX / AAC / SBC** (the
ESP32 sink is effectively SBC-only), audio from modern phones arrives at
higher quality, at the cost of running a full Linux box. Verified
end-to-end on a Pi Zero 2 W (Raspberry Pi OS Trixie, streaming aptX from a
Samsung S23); the headless-WirePlumber-0.5 and shared-radio gotchas behind
the config are captured in
[decisions.md](decisions.md#raspberry-pi-bluetooth--rtp-bridge).

## Directory map

```
PLAN.md                              historical planning document — see docs/ instead
docs/                                 architecture, decisions, API reference, roadmap (this directory)
container/                            throwaway bare-PipeWire dev sandbox (spike 1) — not the real add-on
pipewire_audio_router/                the real HA add-on: bridge daemon (Rust) + Dockerfile + config.yaml
custom_components/pipewire_audio_router/   HA integration (Python) — media_player entities
firmware/bt-bridge/                   ESPHome firmware for the ESP32 Bluetooth bridge box
firmware/pi-bridge/                   Raspberry Pi (BlueZ + PipeWire, no firmware) Bluetooth bridge — setup_pi_bridge.py
spikes/                               write-ups: what was tried, what broke, what the fix was
tests/                                verification scripts backing every spike/phase claim
scripts/                              dev tooling (e.g. arm64 cross-build)
```

## Where to go next

- **Bridge daemon internals in detail** (anchor + per-device senders, the
  Sendspin and AirPlay-2 backends, the PTP clock, the real-time thread
  ladder, the full AirPlay-in → Voice PE + AP2 flow):
  [`../pipewire_audio_router/docs/architecture.md`](../pipewire_audio_router/docs/architecture.md)
- **Why it's built this way** — project-wide (no MQTT, ESP32 chip
  constraints, Pi bridge): [decisions.md](decisions.md); daemon-specific
  (Rust not Python, Ubuntu 26.04 not Debian, per-node not per-link volume,
  RAOP/AP2 output, container packaging):
  [`../pipewire_audio_router/docs/decisions.md`](../pipewire_audio_router/docs/decisions.md)
- **Bridge daemon REST/WebSocket API reference**:
  [api-reference.md](api-reference.md)
- **What's done vs. remaining** for the AirPlay-2 output:
  [`../pipewire_audio_router/docs/airplay2-roadmap.md`](../pipewire_audio_router/docs/airplay2-roadmap.md)
- **Empirical write-ups per experiment**: [`../spikes/`](../spikes/)
