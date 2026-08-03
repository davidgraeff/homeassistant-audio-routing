# PipeWire Audio Router (Home Assistant integration)

A Home Assistant custom integration that turns each output configured on
the [bridge daemon add-on](../../pipewire_audio_router/README.md) into a
regular `media_player` entity — volume, TTS/announce with real ducking,
live state, and **source routing controllable from automations** — plus
`switch`/`number` entities to turn the Bluetooth-bridge RTP source on and
set its port, all backed by the add-on's REST API.

This integration is useless without the add-on already running
somewhere reachable — install and configure that first.

## Installing

Not HACS-packaged (no `hacs.json`) — install manually:

1. Copy (or symlink) this directory into your Home Assistant config's
   `custom_components/pipewire_audio_router/`.
2. Restart Home Assistant.
3. Settings → Devices & Services → **Add Integration** → search
   "PipeWire Audio Router".
4. Enter the bridge daemon add-on's host and port (default `8099`).
   Setup actually calls the add-on (`GET /health`) to verify
   it's reachable before completing — if that fails you'll see "Could
   not reach the bridge daemon at that host/port" rather than a silent
   success.

There's no options flow for changing host/port later — remove and
re-add the integration if the add-on moves.

## Entities

### Bluetooth bridge RTP source (`switch` + `number`)

Two entities let you turn on and configure the RTP source that receives the
[ESP32 Bluetooth bridge firmware](../../firmware/bt-bridge/README.md) —
without opening the add-on's web UI:

- **Switch** *"Bluetooth bridge RTP source"* — on loads the RTP source
  into the add-on (via `/api/sources`), off removes it. Once on, the
  source appears in every output's `source_list` automatically (it's just
  another routable source — pick it with `select_source`, below).
- **Number** *"Bluetooth bridge RTP port"* — the UDP port to listen on;
  must match the port the firmware sends to (default `46000`). Changing it
  while the source is on re-points it live; while off it's just remembered
  for the next enable. (The add-on's daemon only remembers a port while the
  source is enabled, so the integration persists your chosen port across
  restarts.)

Both are config entities (they show under the device's *Configuration*
section) and go **unavailable** if the add-on is unreachable or too old to
expose the `/api/sources` collection.

### Voice-assistant ducking (`switch` + `number` + `select`)

While a voice assistant in a room is talking, the router's speakers **in that
room** play quietly. No automation, no blueprint:

| Entity | What it does |
|---|---|
| `switch.voice_assistant_ducking` | On/off. **Off by default** — if you still run a volume-ducking blueprint, turn that off first or you'll duck twice. |
| `number.voice_assistant_duck_level` | How quiet, as a **gain**: `0.25` = quarter volume, `1` = no ducking. (Not a divisor.) |
| `select.voice_assistant_duck_scope` | `area` (default) ducks only the satellite's own room, even mid-song inside a multi-room group. `music_group` widens it to the whole synchronized group — for open-plan rooms where the same track next door drowns the response. |

Every `assist_satellite` is covered automatically, including ones added later.
The room comes from Home Assistant's areas: the satellite's area (its entity's
override if set, else its device's), matched against the area each router output
adopted from the real speaker's device. A satellite in no area ducks nothing.

Why this beats ducking with `volume_set`: the add-on applies a gain inside its
mix, so **your speakers' volume never moves** (no slider jumping, nothing to
restore, no race if you change the volume mid-sentence), the duck lands within
milliseconds, overlapping turns in different rooms are independent, and a single
speaker of a synced group can duck while the others keep playing. The duck is
held on a lease the integration renews; if Home Assistant restarts or the network
drops mid-turn, the add-on un-ducks by itself within ~30 s.

The satellite's *own* speaker is ducked too, on purpose: the add-on may be
streaming music to it while the device speaks locally.

### Outputs (`media_player`)

One `media_player` entity per output in the add-on's routing matrix — the
auto-discovered virtual AirPlay-2 (`ap2-dev-*`) and sendspin (`sendspin-dev-*`)
devices — named by stripping the prefix and title-casing the rest (e.g.
`sendspin-dev-kitchen` → **Kitchen**). Playing/idle state and volume are
polled every 5 seconds; the routing (`source`) is **pushed live** over the
add-on's `/api/routing/ws` WebSocket, so re-wiring shows up immediately.

- **State**: `playing` if the add-on reports any source linked into
  that output, else `idle`.
- **Volume**: `media_player.volume_set` → the add-on's per-output
  `wpctl`-backed volume.
- **No play/pause**: deliberately not implemented — a passive routing
  sink has no queue of its own to pause. This is a design choice, not a
  missing feature (see [../../docs/decisions.md](../../docs/decisions.md)).
- **Announce / TTS**: declares `MediaPlayerEntityFeature.MEDIA_ANNOUNCE`
  (and `PLAY_MEDIA`, required alongside it — HA's `play_media` service
  rejects calls without it). `media_player.play_media` targeting one of
  these entities ducks whatever's currently playing on that output,
  plays the clip, and restores volume afterward.
- **Source (wiring)**: declares `MediaPlayerEntityFeature.SELECT_SOURCE`.
  `source_list` is the routable sources the add-on reports (plus `None`),
  `source` is whatever is currently linked into the output, and
  `media_player.select_source` re-wires it. Routing is **one source per
  output**: selecting a source unlinks whatever was feeding the output
  first; selecting `None` disconnects it. Automations key off source
  *names* (stable) — the add-on's ephemeral node ids are re-resolved on
  every call, so a routing automation keeps working across a module
  reload. The current wiring is kept in sync over the `/api/routing/ws`
  WebSocket (with automatic reconnect), so changes made elsewhere — the
  add-on's own web UI, another automation — reflect here without waiting
  for a poll.

## Routing from automations

The idiomatic path is `media_player.select_source`:

```yaml
action: media_player.select_source
target:
  entity_id: media_player.kitchen
data:
  source: shairport-sync        # a name from the output's source_list, or "None"
```

For anything beyond one-source-at-a-time (e.g. deliberately mixing two
sources into one output), two additive primitives are available — these
do **not** unlink existing sources the way `select_source` does:

```yaml
# Add a source without disturbing what's already linked
action: pipewire_audio_router.link
target:
  entity_id: media_player.kitchen
data:
  source: bt-bridge

# Remove one source, or omit `source` to disconnect all of them
action: pipewire_audio_router.unlink
target:
  entity_id: media_player.kitchen
data:
  source: bt-bridge             # optional
```

### Announcing TTS

Announcements take a **URL** (or a `media-source` id, which is resolved for
you), so Home Assistant's own TTS is the whole story:

```yaml
action: tts.speak
target:
  entity_id: tts.piper                # your TTS entity — Piper, Cloud, …
data:
  media_player_entity_id: media_player.downstairs_announcements
  message: "Front door opened"
```

The add-on fetches and decodes the rendered clip (symphonia — mp3, wav, aac,
ogg, flac), then mixes it over the ducked music per device.

> Earlier versions also accepted `extra.wyoming` to make the add-on synthesize
> against a Wyoming server (Piper) itself. That was removed: it duplicated a job
> Home Assistant already does better — its TTS entity handles voice selection
> and caching, whereas the add-on re-synthesized identical text on every call —
> and it meant pinning a Piper host/port inside automations. Use `tts.speak` as
> above.

## Testing

Real tests against actual HA internals (config flow machinery,
`DataUpdateCoordinator`, entity-platform forwarding, the real state
machine, real service-call dispatch) via
`pytest-homeassistant-custom-component` — only the network layer
(`PipewireRouterApiClient`) is mocked, HA itself is not:

```
pip install pytest-homeassistant-custom-component homeassistant
python3 -m pytest custom_components/pipewire_audio_router/tests/ -p pytest_homeassistant_custom_component
```

See `tests/README.md` for what each test actually covers.

## Files

```
__init__.py       setup/teardown, coordinator (5s poll: players + RTP source; routing pushed via /api/routing/ws), platform forwarding
config_flow.py     single-step host/port form, validates by calling the add-on
media_player.py    the MediaPlayerEntity subclass described above (incl. select_source + link/unlink services)
switch.py          RTP-source enable/disable + voice-ducking on/off switches
number.py          RTP-source port/latency + voice duck level numbers (restore-backed)
select.py          voice duck scope: area | music_group (restore-backed)
voice_duck.py      watches assist_satellite states, resolves area -> outputs, holds the daemon-side duck
api.py             thin async HTTP client for the add-on's REST API (media players, routing matrix, RTP source)
const.py           domain, default port(s), poll interval, service/source names, voice-duck defaults
services.yaml      link/unlink service descriptions for the automation editor
strings.json       UI strings (config flow + service descriptions)
manifest.json      integration metadata (config_flow: true, no extra deps)
```
