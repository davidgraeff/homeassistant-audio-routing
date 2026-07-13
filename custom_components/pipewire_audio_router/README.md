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
   Setup actually calls the add-on (`GET /api/media_players`) to verify
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
  into the add-on (`PUT /api/source/rtp`), off unloads it. Once on, the
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
expose `/api/source/rtp`.

### Outputs (`media_player`)

One `media_player` entity per output the add-on reports via
`GET /api/media_players`, named by stripping the `raop-out-`/
`sendspin-out-` prefix and title-casing the rest (e.g.
`sendspin-out-kitchen` → **Kitchen**). Volume and playing/idle state are
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

### Using Wyoming TTS instead of a rendered URL

`tts.speak`/normal automations work unchanged (the URL path). To use
direct Wyoming synthesis instead of a rendered clip, call
`media_player.play_media` with the standard `extra` dict:

```yaml
action: media_player.play_media
target:
  entity_id: media_player.kitchen
data:
  media_content_id: ""          # ignored when extra.wyoming is set
  media_content_type: music
  extra:
    wyoming:
      host: 192.168.1.20
      port: 10200                # optional, defaults to 10200
      text: "Front door opened"
      voice: null                # optional
```

This is additive — setting `extra.wyoming` bypasses the URL path
entirely for that one call; every other automation is unaffected. See
[../../docs/api-reference.md](../../docs/api-reference.md#post-apimedia_playersnode_idannounce)
for what this actually sends to the add-on.

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
switch.py          Bluetooth-bridge RTP source enable/disable switch
number.py          Bluetooth-bridge RTP source listen-port number (restore-backed)
api.py             thin async HTTP client for the add-on's REST API (media players, routing matrix, RTP source)
const.py           domain, default port(s), poll interval, service/source names
services.yaml      link/unlink service descriptions for the automation editor
strings.json       UI strings (config flow + service descriptions)
manifest.json      integration metadata (config_flow: true, no extra deps)
```
