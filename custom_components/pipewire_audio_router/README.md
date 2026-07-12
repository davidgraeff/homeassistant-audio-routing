# PipeWire Audio Router (Home Assistant integration)

A Home Assistant custom integration that turns each output configured on
the [bridge daemon add-on](../../pipewire_audio_router/README.md) into a
regular `media_player` entity — volume, TTS/announce with real ducking,
and live state, all backed by the add-on's REST API.

This integration is useless without the add-on already running
somewhere reachable — install and configure that first.

## Installing

Not HACS-packaged (no `hacs.json`) — install manually:

1. Copy (or symlink) this directory into your Home Assistant config's
   `custom_components/pipewire_audio_router/`.
2. Restart Home Assistant.
3. Settings → Devices & Services → **Add Integration** → search
   "PipeWire Audio Router".
4. Enter the bridge daemon add-on's host and port (default `8080`).
   Setup actually calls the add-on (`GET /api/media_players`) to verify
   it's reachable before completing — if that fails you'll see "Could
   not reach the bridge daemon at that host/port" rather than a silent
   success.

There's no options flow for changing host/port later — remove and
re-add the integration if the add-on moves.

## Entities

One `media_player` entity per output the add-on reports via
`GET /api/media_players`, named by stripping the `raop-out-`/
`sendspin-out-` prefix and title-casing the rest (e.g.
`sendspin-out-kitchen` → **Kitchen**). Polled every 5 seconds.

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
__init__.py       setup/teardown, DataUpdateCoordinator (5s poll), platform forwarding
config_flow.py     single-step host/port form, validates by calling the add-on
media_player.py    the MediaPlayerEntity subclass described above
api.py             thin async HTTP client for the add-on's REST API
const.py           domain, default port, poll interval
manifest.json      integration metadata (config_flow: true, no extra deps)
```
