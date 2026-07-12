# Bridge daemon API reference

The bridge daemon (`pipewire_audio_router/bridge-daemon/`, Rust) exposes
a REST + WebSocket API on `0.0.0.0:8080` by default. This is what the
[Home Assistant integration](../custom_components/pipewire_audio_router/README.md)
and the [manual routing UI](../pipewire_audio_router/README.md#manual-routing-ui)
both talk to — there is no other way to control the router.

All request/response bodies are JSON unless noted.

## Health & inspection

### `GET /health`
Plain-text `"ok"` (200). Liveness check, no auth, no body.

### `GET /api/nodes`
Full raw snapshot of every node/port PipeWire currently reports — no
filtering, no HA-specific concepts.

```json
{
  "nodes": [ { "node_id": 42, "node_name": "raop-out-pioneer", "media_class": "Audio/Sink" } ],
  "ports": [ { "port_id": 7, "node_id": 42, "port_name": "playback_FL", "direction": "in" } ]
}
```

## Linking

### `POST /api/links`
Low-level: links exactly one port pair by their literal PipeWire names.
Caller is responsible for pairing FL/FR etc. themselves — this is what
`pw-link` itself does, just over HTTP.

```json
// Request
{ "from_port": "alsa_playback.shairport-sync:output_FL", "to_port": "raop-out-pioneer:playback_FL" }
// Response
{ "ok": true, "message": "linked ... -> ..." }
```

Implemented as a `pw-link` subprocess call (not `pipewire-rs`'s native
API — see [decisions.md](decisions.md#pw-link-subprocess-not-native-pipewire-rs-link-mutation)).
"Already linked" (`pw-link` stderr contains `"File exists"`) is treated
as success (200, `ok: true`) for idempotent retries against racy
short-lived source nodes. Other `pw-link` failures → 400 with the
trimmed stderr as `message`; failure to spawn `pw-link` at all → 500.

### `GET /api/routing` / `POST /api/routing/link` / `POST /api/routing/unlink` / `GET /api/routing/ws`
The higher-level pairing used by the manual routing UI — pairs matching
channel suffixes automatically instead of requiring literal port names.
See "Routing matrix" below.

## Media players

### `GET /api/media_players`
Returns every node the daemon recognizes as a configured output — node
names starting with `raop-out-` or `sendspin-out-` — with live state.
This is exactly what backs the HA integration's entities.

```json
[
  { "node_id": 42, "node_name": "raop-out-pioneer", "state": "playing", "volume": 0.62 },
  { "node_id": 51, "node_name": "sendspin-out-kitchen", "state": "idle", "volume": null }
]
```

`state` is `"playing"` if any link currently feeds the node, else
`"idle"`. `volume` comes from `wpctl get-volume`; `null` if that command
failed or its output couldn't be parsed.

### `GET /api/media_players/:node_id/volume`
```json
{ "volume": 0.62, "message": null }
```
`volume: null` + a `message` on failure (400 if `wpctl` ran but failed,
500 if it couldn't be spawned or its output was unparseable).

### `POST /api/media_players/:node_id/volume`
```json
// Request
{ "volume": 0.5 }
// Response
{ "volume": 0.5, "message": null }
```
0.0–1.0, matching `wpctl`'s own scale and HA's `volume_level`.

### `POST /api/media_players/:node_id/announce`
Ducks every source currently linked into the target, plays a clip, then
unconditionally restores original volumes (even on failure). Full design
rationale in [decisions.md](decisions.md#ttsannounce-ducking-url-based-v1-and-wyoming-based-v2-additive).

Exactly one of `url` or `wyoming` must be present:

```json
// v1: fetch + ffmpeg-decode a rendered clip
{ "url": "http://homeassistant.local:8123/api/tts_proxy/....mp3", "duck_volume": 0.25 }

// v2: direct Wyoming TTS synthesis, no ffmpeg involved
{
  "wyoming": { "host": "192.168.1.20", "port": 10200, "text": "Front door opened", "voice": null },
  "duck_volume": 0.25
}
```

`duck_volume` defaults to `0.25` if omitted. Response:

```json
{ "ok": true, "message": "announced on raop-out-pioneer, ducked 1 source(s)" }
```

Failure modes: both/neither of `url`/`wyoming` set → 400. Target
`node_id` not found → 404. `url` fetch failure → 502. `ffmpeg` decode
failure → 400. Wyoming synthesis failure → 502. Failure writing the
synthesized WAV → 500. `pw-cat` playback failure → 400.

This call blocks until playback (and restore) completes — for a
multi-second announcement, expect the HTTP response to take that long
too. The HA integration's `async_play_media` reflects this; it isn't a
bug if a voice response takes a few seconds to return from a service
call.

## Routing matrix (manual routing UI)

### `GET /api/routing`
```json
{
  "sources": [ { "node_id": 12, "display_name": "PipeWire Router" } ],
  "outputs": [ { "node_id": 42, "display_name": "Pioneer" }, { "node_id": 51, "display_name": "Kitchen" } ],
  "links": [ [12, 42] ]
}
```
- **Outputs** = the same `raop-out-`/`sendspin-out-` nodes
  `/api/media_players` recognizes (shared source of truth).
- **Sources** = any other node with at least one non-`monitor_*`
  output-direction port (excludes every sink's own `pw-record` monitor
  tap).
- Both lists sorted alphabetically by `display_name` (prefix stripped,
  `_`/`-` replaced with spaces).

### `POST /api/routing/link` / `POST /api/routing/unlink`
```json
{ "source_node_id": 12, "output_node_id": 42 }
```
Pairs every non-monitor output port on the source with the input port
on the output sharing the same channel suffix (`output_FL` ~ `send_FL`
~ `playback_FL` all match as `FL`), then runs `pw-link`/`pw-link -d` for
each matched pair. `link` returns `ok: false` if either node doesn't
exist or no channel pairs match; `unlink` always returns `ok: true`
(treats "already unlinked"/registry races as success).

### `GET /api/routing/ws`
WebSocket. Sends one JSON `RoutingMatrix` snapshot (same shape as
`GET /api/routing`) immediately on connect, then a fresh snapshot every
time the PipeWire registry actually changes (node/port/link add or
remove) — driven by a broadcast channel the registry-observer thread
pings synchronously, not polling. A slow client that misses a ping just
gets caught up by the next snapshot it does receive; there's no event
replay. The client never needs to send anything.

## `GET /`
Serves the static manual routing UI (`routing_ui.html`) — a single
self-contained page that opens the `/api/routing/ws` WebSocket and
renders the source × output matrix with clickable link/unlink cells and
per-output volume sliders (polling `/api/media_players` every 3s for
slider sync, since volume changes aren't currently a registry event).
