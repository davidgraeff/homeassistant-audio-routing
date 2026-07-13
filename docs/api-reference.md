# Bridge daemon API reference

The bridge daemon (`pipewire_audio_router/bridge-daemon/`, Rust) exposes
a REST + WebSocket API on `0.0.0.0:8099` by default. This is what the
[Home Assistant integration](../custom_components/pipewire_audio_router/README.md)
and the [web UI](../pipewire_audio_router/README.md#web-ui)
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

## Outputs (RAOP, hot-reloadable)

RAOP (AirPlay) outputs are managed live here — adding or removing one
loads/unloads a `libpipewire-module-raop-sink` module in the daemon's
own PipeWire context, with no restart and no disturbance to audio on the
other outputs (see
[decisions.md](decisions.md#loading-pipewire-modules-at-runtime)). The
set is persisted to a daemon-owned store (`/data/raop-outputs.json`) that
starts empty on a fresh install — there is no `options.json` seeding; this
API (and mDNS auto-discovery) is the only way outputs get created. The
AirPlay-receive source, RTP source, and sendspin outputs are configured
the same way (runtime API, persisted, no seeding) — see
[Sources & sendspin outputs](#sources--sendspin-outputs).

### `GET /api/outputs`
Every configured RAOP output, plus whether its module is loaded right now
(its node is present in the live registry).

```json
[
  { "name": "Pioneer VSX-934", "ip": "192.168.178.35", "port": 7000,
    "encryption": "auth_setup", "node_name": "raop-out-pioneer_vsx_934", "loaded": true }
]
```

### `POST /api/outputs`
Add an output and load it live. Body is one output config — the same
shape as an `outputs` entry in the add-on config; `port` defaults to
`7000` and `encryption` to `auth_setup` if omitted.

```json
// Request
{ "name": "Pioneer VSX-934", "ip": "192.168.178.35", "port": 7000, "encryption": "auth_setup" }
// Response (201)
{ "ok": true, "message": "added output 'raop-out-pioneer_vsx_934'" }
```

The module is loaded first; only on success is the output persisted, so
a failed load leaves no stale store entry. Failure modes: an output with
the same node name (the slugified display name) already exists → 409;
libpipewire refuses the load → 502; the PipeWire thread is unreachable,
or the load succeeded but couldn't be persisted → 500.

### `DELETE /api/outputs/:node_name`
Remove an output by its `node_name` (e.g. `raop-out-pioneer_vsx_934`) and
unload its module live.

```json
{ "ok": true, "message": "removed output 'raop-out-pioneer_vsx_934'" }
```

Unknown `node_name` → 404. The unload is idempotent, so once the output
existed it's gone afterward regardless of registry-timing races.

## Sources & sendspin outputs

Config for the non-RAOP sources/outputs, persisted in a daemon-owned store
(`/data/sources.json`) that starts empty on a fresh install (no
`options.json` seeding) and is managed live here with no restart
(`sources_store.rs`). Three different mechanisms under the hood — reflected
in whether each reports `running` or `loaded`:

- the **AirPlay-receive source** (`shairport-sync`) is an external process
  the daemon supervises (`supervisor.rs`) — no in-daemon equivalent exists;
- the **RTP source** is a native `libpipewire-module-rtp-source` (see below);
- **sendspin outputs** are embedded native servers running inside the daemon
  itself (`sendspin_server.rs`, on the `sendspin` crate) — not subprocesses.

### `GET /api/source/airplay`
```json
{ "name": "PipeWire Router", "running": true }
```
`name` is `null` when the source is disabled; `running` is whether the
`shairport-sync` process is up right now.

### `PUT /api/source/airplay`
```json
// Request  (empty string disables the source)
{ "name": "Living Room" }
// Response
{ "ok": true, "message": "AirPlay source set to 'Living Room'" }
```
Persists the advertised AirPlay name, then (re)starts `shairport-sync`
with it (an empty/whitespace name stops the process and disables it). The
name is saved *before* the process is reconciled, so if the process fails
to start the setting still persists and the response is `ok: false` with
the reason (502).

### `DELETE /api/source/airplay`
Disables the source — stops the process and clears the stored name.
```json
{ "ok": true, "message": "AirPlay source disabled" }
```

### RTP source (Bluetooth bridge) — a module, not a process
The RTP source that receives the [Bluetooth bridge firmware](../firmware/bt-bridge/README.md)'s
audio stream is a native `libpipewire-module-rtp-source`, loaded into the
daemon's own context at runtime like a RAOP sink — **not** a supervised
subprocess. So its liveness is `loaded` (its `bt-bridge-rtp` node is in the
live registry), the same signal `/api/outputs` reports, rather than
`running`. Everything but the listen port is fixed to match the firmware's
wire format (native-endian `S16LE`, 44100 Hz stereo, `sess.ignore-ssrc`);
see `bridge-daemon/src/rtp_source.rs`.

### `GET /api/source/rtp`
```json
{ "enabled": true, "port": 46000, "loaded": true }
```
`enabled` is whether it's on in the store; `port` is the stored UDP port (or
the `46000` default when disabled); `loaded` is whether the `bt-bridge-rtp`
node is present in the live PipeWire graph right now.

### `PUT /api/source/rtp`
```json
// Request  (port optional; defaults to 46000)
{ "port": 46000 }
// Response
{ "ok": true, "message": "RTP source enabled on port 46000" }
```
Enables the source (or changes its port): persists the port, then reloads the
module on it (unload-then-load, so a re-enable or port change is a clean
reload). The port is saved *before* the module is reconciled — if the load
fails the setting still persists and the response is `ok: false` with the
reason (502).

### `DELETE /api/source/rtp`
Disables the source — unloads the module (its node disappears live) and clears
the stored config.
```json
{ "ok": true, "message": "RTP source disabled" }
```

### `GET /api/sendspin_outputs`
```json
[ { "name": "Kitchen", "port": 8927, "node_name": "sendspin-out-kitchen", "running": true } ]
```
`running` is whether that output's embedded sendspin server is active
(sink node created + capture + server role up), all in-process.

### `POST /api/sendspin_outputs`
```json
// Request
{ "name": "Kitchen" }
// Response (201)
{ "ok": true, "message": "added sendspin output 'Kitchen' on port 8927" }
```
Adds an output, allocates the lowest free port at/above the base
(`8927`), persists it, and starts its embedded sendspin server —
creating the sink node, capturing from it, and running the server role,
all in-process (`sendspin_server.rs`). A duplicate (same slugified name)
→ 409; the server failing to start → 502 (still persisted).

### `DELETE /api/sendspin_outputs/:node_name`
Removes the output (e.g. `sendspin-out-kitchen`) and stops its embedded
server, tearing down its sink node and capture.
```json
{ "ok": true, "message": "removed sendspin output 'sendspin-out-kitchen'" }
```
Unknown `node_name` → 404.

## Linking

### `POST /api/links`
Low-level: links exactly one port pair by their literal PipeWire names.
Caller is responsible for pairing FL/FR etc. themselves.

```json
// Request
{ "from_port": "alsa_playback.shairport-sync:output_FL", "to_port": "raop-out-pioneer:playback_FL" }
// Response
{ "ok": true, "message": "linked ... -> ..." }
```

Created natively via `Core::create_object` on the PipeWire thread
(`pw_thread.rs`) — the port names are resolved to object ids against the
live registry, then a create command is handed to that thread. Idempotent:
a link already present between the same ports is reported as success
(`ok: true`). Failure modes: either port name not found in the registry
→ 400; the PipeWire thread unreachable/dropped the request → 500.

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
`"idle"`. `volume` is read natively from the node's SPA `Props` param
(`channelVolumes`, `volume.rs`); `null` if the node exposes no volume
control.

### `GET /api/media_players/:node_id/volume`
```json
{ "volume": 0.62, "message": null }
```
`volume: null` with a `message` when the node has no volume control (200);
`volume: null` + `message` with 500 if the read failed outright.

### `POST /api/media_players/:node_id/volume`
```json
// Request
{ "volume": 0.5 }
// Response
{ "volume": 0.5, "message": null }
```
0.0–1.0, on the same cubic scale as `wpctl`/HA's `volume_level`: the value
`V` is written to the node's `channelVolumes` as `V³` (linear gain), so it
reads back identically to what `wpctl` would show.

### `POST /api/media_players/:node_id/announce`
Ducks every source currently linked into the target, plays a clip, then
unconditionally restores original volumes (even on failure). Full design
rationale in [decisions.md](decisions.md#ttsannounce-ducking-url-based-v1-and-wyoming-based-v2-additive).

Exactly one of `url` or `wyoming` must be present:

```json
// v1: fetch + decode a rendered clip (symphonia — pure Rust, any
// format: mp3, wav, aac, ogg, flac; no ffmpeg/system dependency)
{ "url": "http://homeassistant.local:8123/api/tts_proxy/....mp3", "duck_volume": 0.25 }

// v2: direct Wyoming TTS synthesis, no decode step needed at all
{
  "wyoming": { "host": "192.168.1.20", "port": 10200, "text": "Front door opened", "voice": null },
  "duck_volume": 0.25
}
```

`duck_volume` defaults to `0.25` if omitted. Response:

```json
{ "ok": true, "message": "announced on raop-out-pioneer, ducked 1 source(s)" }
```

Playback is native — a `pw::stream` targeting the sink node (`player.rs`),
not a `pw-cat` subprocess; volume ducking/restore is the native Props path
(`volume.rs`). Failure modes: both/neither of `url`/`wyoming` set → 400.
Target `node_id` not found → 404. `url` fetch failure → 502. Decode
failure → 400. Wyoming synthesis failure → 502. Failure writing the
synthesized WAV → 500. Native playback (or reading the clip back) failure
→ 400.

This call blocks until playback (and restore) completes — for a
multi-second announcement, expect the HTTP response to take that long
too. The HA integration's `async_play_media` reflects this; it isn't a
bug if a voice response takes a few seconds to return from a service
call.

## Routing matrix (manual routing UI)

### `GET /api/routing`
```json
{
  "sources": [ { "node_id": 12, "node_name": "shairport-sync", "display_name": "PipeWire Router" } ],
  "outputs": [
    { "node_id": 42, "node_name": "raop-out-pioneer", "display_name": "Pioneer" },
    { "node_id": 51, "node_name": "sendspin-out-kitchen", "display_name": "Kitchen" }
  ],
  "links": [ [12, 42] ]
}
```
- Each node carries both its ephemeral `node_id` and its stable
  `node_name` (unchanged across a module reload); links are keyed by id.
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
~ `playback_FL` all match as `FL`), then creates/destroys those links
natively via the PipeWire thread (`Core::create_object` /
`Registry::destroy_global`). `link` returns `ok: false` if either node
doesn't exist or no channel pairs match. `unlink` removes every link
between the two nodes by id and returns `ok: true` even when there were
none (the desired "not linked" end state holds regardless of registry
races).

### `GET /api/routing/ws`
WebSocket. Sends one JSON `RoutingMatrix` snapshot (same shape as
`GET /api/routing`) immediately on connect, then a fresh snapshot every
time the PipeWire registry actually changes (node/port/link add or
remove) — driven by a broadcast channel the registry-observer thread
pings synchronously, not polling. A slow client that misses a ping just
gets caught up by the next snapshot it does receive; there's no event
replay. The client never needs to send anything.

## `GET /` (and other non-API paths)
Serves the built web UI — a Vite + Svelte single-page app (source in
`pipewire_audio_router/frontend/`, served as static files from
`--static-dir`), styled to match Home Assistant with light/dark themes and
also surfaced in the HA sidebar via ingress. It's a full admin console: the
source × output routing matrix (live over `/api/routing/ws`, clickable
link/unlink cells, per-output volume sliders) plus RAOP-output, AirPlay/RTP-
source, and sendspin management and an announce test. The volume sliders
poll `/api/media_players` every few seconds for sync, since volume changes
aren't a registry event.
