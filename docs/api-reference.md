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

> **Per-output diagnostics.** The Outputs tab's **Play tone** / **Play
> announcement** buttons don't use a per-output endpoint; they post to the
> per-device `POST /api/announce` with `{ "targets": ["<node_name>"], "tone":
> true }` or `{ …, "test": true }` (built-in calibration tone / committed TTS
> clip `bridge-daemon/assets/test-announcement.mp3`). This is the
> backend-agnostic per-device-sender path (Sendspin now, AirPlay 2 later), so
> it only works for targets running as per-device senders — not RAOP sink
> nodes, which that path replaces.

## Sources

Config for the non-RAOP sources, persisted in a daemon-owned store
(`/data/sources.json`) that starts empty on a fresh install (no
`options.json` seeding) and is managed live here with no restart
(`sources_store.rs`). Both sources run **natively in-process** — there are
no supervised subprocesses:

- the **AirPlay-receive source** is a native, embedded RAOP receiver (a
  vendored+patched pure-Rust `shairplay` crate, `airplay_source.rs`) — not
  a `shairport-sync` subprocess;
- the **RTP source** is a native `libpipewire-module-rtp-source` (see below).

(Sendspin devices are auto-discovered, not configured here — see
[Sendspin devices](#sendspin-devices).)

### `GET /api/source/airplay`
```json
{ "name": "PipeWire Router", "running": true, "latency_msec": 150 }
```
`name` is `null` when the source is disabled; `running` is whether the
embedded receiver is up right now; `latency_msec` is the producer jitter
buffer (higher = fewer stutters, more latency).

### `PUT /api/source/airplay`
```json
// Request  (empty string disables the source; latency_msec optional)
{ "name": "Living Room", "latency_msec": 150 }
// Response
{ "ok": true, "message": "AirPlay source set to 'Living Room'" }
```
Persists the advertised AirPlay name + jitter buffer, then (re)starts the
embedded receiver with it (an empty/whitespace name stops it and disables
the source). Saved *before* the receiver is reconciled, so if it fails to
start the setting still persists and the response is `ok: false` with the
reason (502). The receiver advertises unencrypted ALAC (`et=0`, `cn=1`);
see [decisions.md](decisions.md#native-airplay-receive-source-vendored-shairplay-not-shairport-sync)
for why that combination is what PipeWire senders actually drive.

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
{ "enabled": true, "port": 46000, "latency_msec": 200, "source_addr": "0.0.0.0", "ignore_ssrc": true, "loaded": true }
```
`enabled` is whether it's on in the store; `port` is the stored UDP port (or
the `46000` default when disabled); `latency_msec` is the receiver jitter
buffer (raise on a weak link to trade latency for fewer dropouts);
`source_addr` is the bind address — `0.0.0.0` for a normal unicast target, or
a multicast group so several receivers share one bridge stream; `ignore_ssrc`
is `sess.ignore-ssrc` (`true` accepts any sender on the port; `false` latches
onto the first SSRC and rejects the rest — the "only one client" corruption
guard, safe now the firmware sends a stable MAC-derived SSRC); `loaded` is
whether the `bt-bridge-rtp` node is present in the live graph right now.

### `PUT /api/source/rtp`
```json
// Request  (the daemon replaces the whole config; omitted fields default)
{ "port": 46000, "latency_msec": 200, "source_addr": "239.255.42.42", "ignore_ssrc": true }
// Response
{ "ok": true, "message": "RTP source enabled on 239.255.42.42:46000 (200 ms jitter buffer, any sender)" }
```
Enables the source (or changes it): persists the config, then reloads the
module (unload-then-load, so a change is a clean reload). Saved *before* the
module is reconciled — if the load fails the setting still persists and the
response is `ok: false` with the reason (502). Set `source_addr` to a
multicast group (and point the firmware's RTP host at the same group) to fan
one bridge stream out to several PipeWire hosts. Set `ignore_ssrc` to `false`
to reject every sender but the first one seen (`true`, the default, accepts
any). The add-on's web UI folds these two knobs into one "Source" radio —
*Accept all senders* (`0.0.0.0`, `ignore_ssrc` true), *Only one client*
(`0.0.0.0`, `ignore_ssrc` false), *Multicast group* (the group, `ignore_ssrc`
true).

### `DELETE /api/source/rtp`
Disables the source — unloads the module (its node disappears live) and clears
the stored config.
```json
{ "ok": true, "message": "RTP source disabled" }
```

## Sendspin devices

Sendspin speakers (ESPHome, e.g. HA Voice PE) are **auto-discovered** over
mDNS (`sendspin_discovery.rs`) — there is no per-output config to create.
Each discovered device shows up as a virtual routing output
(`sendspin-dev-<slug>`); devices routed from the same source set are formed
into one synchronized group automatically (`sendspin_group.rs`). Online/offline
is decided by the live connection plus a TCP liveness probe, not raw mDNS
(`sendspin_liveness.rs`). The only per-device control is volume, carried
in-band over the protocol (there is no PipeWire node volume for these virtual
outputs).

### `GET /api/sendspin/volumes`
Desired per-device volume (0–100), keyed by device node name. Sparse — a
device with no entry is at full scale.
```json
{ "sendspin-dev-home_assistant_voice_093ca8": 60 }
```

### `PUT /api/sendspin/volume`
```json
// Request
{ "node_name": "sendspin-dev-home_assistant_voice_093ca8", "volume": 60 }
// Response
{ "ok": true, "message": "set 'sendspin-dev-home_assistant_voice_093ca8' to 60%" }
```
Sends the volume to the device in-band and stores it (re-applied on the
device's next reconnect). If the device isn't connected the value is still
stored (`"saved … (device not connected)"`).

## Linking

### `POST /api/links`
Low-level: links exactly one port pair by their literal PipeWire names.
Caller is responsible for pairing FL/FR etc. themselves.

```json
// Request
{ "from_port": "airplay-in:output_FL", "to_port": "raop-out-pioneer:playback_FL" }
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
Returns the **live RAOP output** nodes (`raop-out-*`) with their state +
node-level volume. It's the RAOP volume/state overlay the HA integration
layers on top of the routing matrix.

```json
[
  { "node_id": 42, "node_name": "raop-out-pioneer", "state": "playing", "volume": 0.62 }
]
```

`state` is `"playing"` if any link currently feeds the node, else
`"idle"`. `volume` is read natively from the node's SPA `Props` param
(`channelVolumes`, `volume.rs`); `null` if the node exposes no volume
control. Sendspin devices are **not** here — they're virtual (no PipeWire
node); the integration sources them from the routing matrix and gets their
volume from [`/api/sendspin/volumes`](#sendspin-devices). The HA integration
creates one `media_player` per routing-matrix **output** (RAOP + sendspin),
not from this list directly.

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
The matrix is keyed by **stable `node_name`**, not the ephemeral id — so
routing intent survives module reloads and device churn, and links to a
currently-offline endpoint are kept and reapplied when it returns.

```json
{
  "sources": [
    { "node_name": "airplay-in", "display_name": "Music Now", "present": true, "configured": true, "node_id": 12, "peak": 0.31 },
    { "node_name": "bt-bridge-rtp", "display_name": "BT Bridge (RTP)", "present": true, "configured": true, "node_id": 44, "peak": 0.0 }
  ],
  "outputs": [
    { "node_name": "raop-out-pioneer", "display_name": "Pioneer", "present": true, "configured": false, "node_id": 42, "peak": 0.0 },
    { "node_name": "sendspin-dev-voice_kitchen", "display_name": "Voice Kitchen", "present": true, "configured": false, "node_id": null, "peak": 0.0 }
  ],
  "links": [ { "source": "airplay-in", "output": "raop-out-pioneer" } ]
}
```
- Each node carries its stable `node_name`, an ephemeral `node_id`
  (`null` when offline, or always for virtual sendspin devices), `present`
  (in the live graph now — `false` = configured/known but offline, shown
  grayed), `configured` (manually added vs auto-discovered), and `peak`
  (a live input-level meter for sources, populated only while the matrix WS
  is open — see `metering.rs`).
- **Outputs** = live RAOP sinks + discovered sendspin devices + any
  offline endpoint with saved routing intent. **Sources** = the AirPlay
  receiver, the RTP source, and any other non-monitor output-direction node.
- `links` are `{source, output}` **name** pairs — the persisted intent.

### `POST /api/routing/link` / `POST /api/routing/unlink`
```json
{ "source": "airplay-in", "output": "raop-out-pioneer" }
```
By stable **name**. `link` records the intent and, if both endpoints are
present, pairs every non-monitor output port on the source with the matching
channel-suffix input on the output (`output_FL` ~ `send_FL` ~ `playback_FL`
all match as `FL`) and creates those links natively; the intent is reapplied
automatically if an endpoint (re)appears later. `unlink` removes the intent
and any live links and returns `ok: true` even if there were none.

### `DELETE /api/routing/entity/:node_name`
Forget an offline endpoint entirely — drops its saved routing intent so it
stops appearing (grayed) in the matrix. A real device that later reappears
comes back unrouted.

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
routing matrix (outputs as rows, sources as columns, live over
`/api/routing/ws`, clickable link/unlink cells, per-output volume sliders
including sendspin devices, offline endpoints grayed with a forget button,
synchronized-group badges, and a live input-level meter per source) plus
RAOP-output and AirPlay/RTP-source management and per-output diagnostic test
buttons (Play tone / Play announcement). Sendspin
devices are auto-discovered, so there's no manual sendspin management — just a
capabilities note. Volume sliders poll `/api/media_players` +
`/api/sendspin/volumes` every few seconds, since volume changes aren't a
registry event.
