<img src="docs/branding/logo.png" alt="PipeWire Audio Router" width="420">

[![CI](https://github.com/davidgraeff/homeassistant-audio-routing/actions/workflows/ci.yml/badge.svg)](https://github.com/davidgraeff/homeassistant-audio-routing/actions/workflows/ci.yml)

**Whole-home audio for Home Assistant, without the stutter.** Play from your
phone, your PC or any Bluetooth device, send it to any speakers in the house
from one dashboard card, and let Home Assistant talk over the top of it —
ducked, not interrupted.

It replaces Music Assistant's Python audio engine with PipeWire's own realtime
graph, driven by a small Rust daemon: the same speakers, the same
`media_player` entities, but the audio path is native. That is the whole point
— on a Raspberry Pi 4 the Python engine stuttered audibly and took seconds to
start a stream, and moving the path onto PipeWire is what fixed both.

## Routing is one card

![The audio routing dashboard card: inputs on the left, zones on the right, live routes drawn as wires between them](docs/images/audio_routing_card.png)

Tap an input, then where it should play. Tap a wire to remove a route. Speakers
fed by the same inputs are synchronised into one group for you, so "play this
in the kitchen and the living room" is two taps and stays in sync.

## What you get

- **Play in from** an iPhone, Mac or PC over AirPlay · any Bluetooth device, via
  a small ESP32 or Raspberry Pi bridge box · a Linux PC, via a receiver agent ·
  the YouTube Music app's own Cast button, via a side add-on you deploy
  yourself.
- **Play out to** AirPlay-2 receivers, AV receivers and HomePods · ESPHome
  speakers such as Home Assistant Voice PE · any Linux PC running the agent.
- **Ordinary Home Assistant entities**: a `media_player` per output plus one per
  group, with volume, and routing you can drive from automations and scripts —
  not just from the card.
- **Announcements that duck**, per room: TTS plays over the music at full
  clarity, the music dips and comes back, and a PC's own audio (a browser, a
  game) dips with it.
- **Discovery you stay in charge of.** Every compatible device on the LAN is
  *offered*, including the neighbours' AirPlay speakers; nothing is routable,
  gets an entity, or is ever sent audio until you add it — each with a
  test-tone button so you can tell which speaker is which.

## Quick install

### 1. The add-on — required

This is the piece that moves the audio. Add the repository, then install
**PipeWire Audio Router** from the store and start it. There are no options to
fill in: outputs and sources are added at runtime in its own web UI.

[![Open your Home Assistant instance and add this add-on repository.](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fdavidgraeff%2Fhomeassistant-audio-routing)

### 2. The integration — recommended

This is what turns the add-on's outputs into `media_player` entities and brings
the routing card to your dashboard. Download it with HACS:

[![Open your Home Assistant instance and open this repository inside HACS.](https://my.home-assistant.io/badges/hacs_repository.svg)](https://my.home-assistant.io/redirect/hacs_repository/?owner=davidgraeff&repository=homeassistant-audio-routing&category=integration)

If HACS does not open the repository, add it by hand first — HACS → ⋮ →
**Custom repositories** → this repo's URL, category **Integration**. Without
HACS, copy [`custom_components/pipewire_audio_router/`](custom_components/pipewire_audio_router/)
into your config's `custom_components/`.

Then restart Home Assistant and go to Settings → Devices & Services → **Add
integration** → *PipeWire Audio Router*, and give it the add-on's host and port
(default `8099`). Setup calls the add-on to check it is reachable, so a typo
fails loudly instead of silently.

### 3. A Bluetooth bridge box — optional

Turns any phone, tablet or laptop that can pair over Bluetooth into a source.
Flash [`firmware/bt-bridge/`](firmware/bt-bridge/README.md) onto an
original-ESP32 board (not S2/S3/C3/C6 — that README explains why), or use
[`firmware/pi-bridge/`](firmware/pi-bridge/README.md) on a Raspberry Pi Zero 2 W
for higher-quality codecs.

### 4. YouTube Music's Cast button — optional, and not click-to-install

Puts the house in the Cast menu of the YouTube Music app. This one is
deliberately unpublished, because it leans on unofficial protocols, on `yt-dlp`
keeping pace with YouTube, and on **your own signed-in account**: you build and
push it from a workstation (`./scripts/deploy-dev.sh ytmusic`), then give it an
RTP source in the router and a cookie jar. Steps in
[`ytmusic_receiver/`](ytmusic_receiver/README.md), reasoning and every measured
gotcha in [docs/ytmusic-receiver.md](docs/ytmusic-receiver.md).

Each component's own README has the full detail; this page is the map.

## Status

**Functionally complete for daily use; not yet cut over from Music
Assistant.** Every phase through the manual routing UI is done and verified
end-to-end against real hardware and signal measurements, not just unit tests.
Still open: final latency/soak measurement on the real Pi 4, and the cutover
itself.

Last tested against **Home Assistant 2026.8.1** — CI and the local test runner
both install exactly that, from the single pin in
[`custom_components/pipewire_audio_router/tests/requirements.txt`](custom_components/pipewire_audio_router/tests/requirements.txt).
The integration declares a **minimum of 2026.7.0** (`hacs.json`). How that pin
is chosen and what past Home Assistant bumps broke:
[docs/addon_maintenance.md](docs/addon_maintenance.md).

## Documentation

- [**docs/system-architecture.md**](docs/system-architecture.md) — how the
  pieces fit together, with diagrams.
- [**docs/decisions.md**](docs/decisions.md) — why it's built this way: real
  investigations with concrete findings (RAOP quirks, PipeWire's actual
  volume/module-loading capabilities, ESP32 hardware constraints, and more),
  not preferences.
- [**docs/api-reference.md**](docs/api-reference.md) — the bridge daemon's
  REST/WebSocket API.
- [**docs/addon_maintenance.md**](docs/addon_maintenance.md) — keeping up with
  Home Assistant: which HA version the tests run against and how to bump it,
  what past bumps broke, and the commands for testing and deploying against
  the real instance.
- [**docs/ytmusic-receiver.md**](docs/ytmusic-receiver.md) — the YouTube Music
  Cast receiver: how it works and every measured gotcha behind it.
- [**docs/branding/**](docs/branding/README.md) — the icon and logo, and how to
  re-render them.
- [**tests/**](tests/) — the runnable scripts backing those write-ups; nothing
  here is "verified" without one. All of it — the Rust daemon
  (rustfmt/clippy/tests), the web UI (svelte-check/build), the HA integration
  (pytest), and the add-on end-to-end scripts — runs in CI on every push to
  `main` and every PR
  ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

## How it works

Phones, PCs and a Bluetooth bridge box stream in; a Rust daemon routes and
mixes them through a native PipeWire graph and out to AirPlay-2 receivers,
ESPHome speakers and PCs; Home Assistant sees ordinary `media_player` entities
at the far end.

![System architecture](docs/diagrams/system-architecture.svg)

## Components

Independently installable pieces, plus supporting dev tooling. Each has its own
README with install/config/usage details.

| Component | What | README |
|---|---|---|
| **Bridge daemon add-on** | Rust + PipeWire/WirePlumber, packaged as a real HA add-on (`repository.yaml` at repo root) | [`pipewire_audio_router/`](pipewire_audio_router/README.md) |
| **Home Assistant integration** | `media_player` entities backed by the add-on's REST API, plus the `custom:pipewire-router-card` dashboard card | [`custom_components/pipewire_audio_router/`](custom_components/pipewire_audio_router/README.md) |
| **Bluetooth bridge firmware** | ESP32 + ESPHome, turns any Bluetooth device into a whole-home-audio source | [`firmware/bt-bridge/`](firmware/bt-bridge/README.md) |
| **Raspberry Pi bridge** | the same job on a Pi Zero 2 W, for higher-quality Bluetooth codecs | [`firmware/pi-bridge/`](firmware/pi-bridge/README.md) |
| **pw-sink receiver agent** | makes a Linux PC an output the router can stream to, and duck | [`pipewire_audio_router/pwrouter-agent/`](pipewire_audio_router/pwrouter-agent/README.md) |
| **YouTube Music Cast receiver** | the YouTube Music app's Cast button, as a source — a local, deliberately unpublished add-on | [`ytmusic_receiver/`](ytmusic_receiver/README.md) |

## Repo layout

```
pipewire_audio_router/                the HA add-on: bridge daemon (Rust) + Dockerfile + config.yaml
custom_components/pipewire_audio_router/   HA integration (Python)
firmware/bt-bridge/                   ESPHome firmware for the Bluetooth bridge box
firmware/pi-bridge/                   Raspberry Pi Bluetooth bridge
firmware/pi-ytmusic/                  Raspberry Pi YouTube Music receiver (canonical receiver app)
ytmusic_receiver/                     the same receiver as a local HA add-on
container/                            throwaway bare-PipeWire dev sandbox — not the real add-on
docs/                                 architecture, decisions, API reference, branding
tests/                                verification scripts
scripts/                              dev tooling (e.g. arm64 cross-build, branding render)
```
