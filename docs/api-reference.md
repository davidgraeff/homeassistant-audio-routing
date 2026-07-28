# Bridge daemon API reference

The bridge daemon (`pipewire_audio_router/bridge-daemon/`, Rust) exposes
a REST + WebSocket API on `0.0.0.0:8099` by default. This is what the
[Home Assistant integration](../custom_components/pipewire_audio_router/README.md)
and the [web UI](../pipewire_audio_router/README.md#web-ui)
both talk to — there is no other way to control the router.

All request/response bodies are JSON unless noted.

## Endpoint index

The complete route table, as registered in `bridge-daemon/src/api.rs`. Sections below
document the common paths in detail; the rest are listed here with their purpose and
handler name, which is the authoritative place to check the exact body shape.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health` | plain-text liveness |
| `GET` | `/api/nodes` | raw PipeWire node/port snapshot |
| `GET` | `/api/status` | daemon status summary (`get_status`) |
| `GET`/`PUT` | `/api/settings` | daemon settings (`get_settings` / `set_settings`) |
| `GET`/`PUT` | `/api/sync/settings` | group lead + per-device delays (`get_sync_settings` / `set_sync_settings`) |
| `GET` | `/api/outputs` | every discovered output + live state |
| `PUT` | `/api/outputs/{node_name}/latency` | per-output latency |
| `PUT` | `/api/outputs/{node_name}/ap2-rate` | AirPlay-2 rate mode |
| `PUT` | `/api/outputs/{node_name}/sendspin-codec` | sendspin codec choice |
| `GET`/`POST` | `/api/sources` | list / create a source |
| `GET`/`PUT`/`DELETE` | `/api/sources/{id}` | read / update / remove a source |
| `GET` | `/api/sources/{id}/clients` | connected senders (AirPlay) |
| `POST` | `/api/sources/{id}/clients/ban` | ban / unban a sender |
| `POST` | `/api/sources/{id}/clients/disconnect` | kick a sender |
| `POST` | `/api/sources/{id}/clients/forget` | drop a remembered sender |
| `POST` | `/api/sources/{id}/clients/priority` | set a sender's priority |
| `PUT` | `/api/sources/{id}/policy` | anti-takeover policy |
| `GET` | `/api/sendspin/volumes` | all sendspin volumes |
| `PUT` | `/api/sendspin/volume` | set one sendspin volume |
| `PUT` | `/api/sendspin/mute` | mute one sendspin device |
| `GET` | `/api/sendspin/delays` | per-device sendspin delays |
| `PUT` | `/api/sendspin/delay` | set one sendspin delay |
| `PUT` | `/api/ap2/volume` | set an AirPlay-2 receiver's volume |
| `PUT` | `/api/ap2/mute` | mute an AirPlay-2 receiver |
| `POST` | `/api/links` | low-level port link |
| `GET` | `/api/routing` | routing matrix |
| `POST` | `/api/routing/link` / `/api/routing/unlink` | edit the matrix |
| `DELETE` | `/api/routing/entity/{node_name}` | forget an entity |
| `GET` | `/api/routing/ws` | matrix change WebSocket |
| `POST` | `/api/announce` | announce to explicit targets |
| `GET`/`POST` | `/api/groups/music` | list / create a Music group |
| `PUT`/`DELETE` | `/api/groups/music/{id}` | edit / delete a Music group |
| `POST`/`DELETE` | `/api/groups/music/{id}/route` | route / unroute a Music group |
| `GET`/`POST` | `/api/groups/announcement` | list / create an Announcement group |
| `PUT`/`DELETE` | `/api/groups/announcement/{id}` | edit / delete an Announcement group |
| `GET` | `/api/align/groups` | groups available for alignment |
| `GET`/`DELETE` | `/api/align` | alignment status / stop |
| `POST` | `/api/align/start` | begin the alignment wizard |
| `POST` | `/api/align/select` | pick the device being aligned |
| `POST` | `/api/align/volume` | set the alignment reference volume |
| `POST`/`DELETE` | `/api/spike/per-device` | dev-only spike harness |
| `POST`/`DELETE` | `/api/spike/multi-device` | dev-only spike harness |
| `POST`/`DELETE` | `/api/spike/overlay` | dev-only spike harness |
| `POST`/`DELETE` | `/api/spike/ap2` | dev-only spike harness |
| `POST`/`DELETE` | `/api/spike/pw-sink` | dev-only spike harness |

> **Two-tier groups (`/api/groups/*`) and the alignment wizard (`/api/align/*`) are not
> yet documented in detail here.** They are the newest subsystems; see
> `bridge-daemon/src/api.rs` for their bodies and
> [architecture.md](../pipewire_audio_router/docs/architecture.md) for the concepts.
> The `/api/spike/*` routes are development harnesses, not a supported interface.

## Health & inspection

### `GET /health`
Plain-text `"ok"` (200). Liveness check, no auth, no body.

### `GET /api/nodes`
Full raw snapshot of every node/port PipeWire currently reports — no
filtering, no HA-specific concepts.

```json
{
  "nodes": [ { "node_id": 42, "node_name": "ap2-dev-pioneer_vsx_934_f11b89", "media_class": "Audio/Sink" } ],
  "ports": [ { "port_id": 7, "node_id": 42, "port_name": "playback_FL", "direction": "in" } ]
}
```

## Outputs (discovered, not configured)

> **RAOP outputs are gone.** Earlier versions of this API let you `POST`/`DELETE`
> `libpipewire-module-raop-sink` outputs by hand. AirPlay output is now the native
> **AirPlay 2** sender, and there are no create/delete endpoints left: outputs are
> **auto-discovered over mDNS** and appear on their own. `raop_migration.rs` rewrites
> any surviving `raop-out-*` names in the routing and groups stores on startup.

Three output backends are discovered and reported here, all as virtual routing
outputs:

| `kind` | What it is | Discovery |
|---|---|---|
| `sendspin` | ESPHome sendspin speakers (e.g. HA Voice PE) | mDNS (`sendspin_discovery.rs`) |
| `airplay2` | native AirPlay-2 receivers | mDNS (`ap2_discovery.rs`) |
| `pwsink` | remote PipeWire hosts | mDNS |

Per-output *settings* are persisted; the outputs themselves are not created here.

### `GET /api/outputs`
Every discovered output with its live state. Common fields: `node_name`, `name`,
`kind`, `present` (in the live registry now), `configured` (has persisted settings),
`ip`, `port`, `encryption`, `latency_ms`. Each backend adds its own:

```json
[
  { "node_name": "sendspin-dev-home_assistant_voice_093ca8", "name": "home-assistant-voice-093ca8",
    "kind": "sendspin", "present": true, "configured": false,
    "ip": "192.168.178.52", "port": 8928, "encryption": "None", "latency_ms": null,
    "sendspin_codec": "opus", "sendspin_codec_active": "opus",
    "sendspin_codec_options": [ { "codec": "auto", "available": true } ],
    "sendspin_send_ahead_ms": 250 },

  { "node_name": "ap2-dev-pioneer_vsx_934_f11b89", "name": "Pioneer VSX-934 F11B89",
    "kind": "airplay2", "present": true, "configured": false,
    "ip": "192.168.178.35", "port": 7000, "encryption": "HomeKit", "latency_ms": 200,
    "ptp_locked": true, "ptp_lock_age_s": 0, "ptp_supported": true, "ptp_relevant": true,
    "ap2_features": { "raw": "0x445F8A00,0x1C340", "ptp": true,
                      "buffered_audio": true, "transient_pairing": true },
    "ap2_rate_mode": "auto", "ap2_rate": 44100, "ap2_volume": 0.44, "ap2_muted": false },

  { "node_name": "pwsink-dev-david_local", "name": "david-local", "kind": "pwsink",
    "present": true, "configured": false, "ip": "192.168.178.21", "port": null,
    "encryption": "None", "latency_ms": null, "pwsink_streaming": false }
]
```

`ptp_locked` is **runtime** state, not a capability — a receiver can advertise PTP and
sit unlocked without anything being wrong outside a multi-room group.

### `PUT /api/outputs/{node_name}/latency`
Per-output latency in ms. Persisted, applied to the running sender.

### `PUT /api/outputs/{node_name}/ap2-rate`
AirPlay-2 rate mode for one receiver — `auto` or a fixed rate (e.g. `fixed_44100`),
mirroring the `ap2_rate_mode` / `ap2_rate` fields above.

### `PUT /api/outputs/{node_name}/sendspin-codec`
Pick a sendspin device's codec from its `sendspin_codec_options` (`auto`, `opus`,
`flac`, `pcm`). `sendspin_codec` is the request; `sendspin_codec_active` is what the
device actually negotiated.

> **Per-output diagnostics.** The Outputs tab's **Play tone** / **Play
> announcement** buttons don't use a per-output endpoint; they post to the
> per-device `POST /api/announce` with `{ "targets": ["<node_name>"], "tone":
> true }` or `{ …, "test": true }` (built-in calibration tone / committed TTS
> clip `bridge-daemon/assets/test-announcement.mp3`). This is the
> backend-agnostic per-device-sender path (Sendspin + AirPlay 2 + pw-sink).
>
> A target only hears the clip while a per-device sender is streaming it, so
> before starting the clip the daemon makes sure one is: a Sendspin device
> always has its idle sender, and an **AirPlay-2 receiver or pw-sink target
> with nothing routed into it gets an on-demand session** — audible a few
> seconds later (AP2: pairing + its render delay; pw-sink: the target
> discovering our advert and initiating the handshake) and handed back after a
> 30 s lease. Targets that nothing can carry are **dropped from the
> announcement and named in `message`** (with all of them unavailable the call
> is rejected), so a "playing" answer means audio is really going somewhere.
> See
> [architecture.md §5.4](../pipewire_audio_router/docs/architecture.md#54-announcing-to-an-output-with-nothing-routed-into-it).

## Sources

Full CRUD over the router's inputs, persisted in a daemon-owned store
(`/data/sources.json`) that starts empty on a fresh install (no `options.json`
seeding) and is managed live here with no restart (`sources_store.rs`).

> **These endpoints replaced the singular `/api/source/rtp` and
> `/api/source/airplay`**, which no longer exist. The router now supports **several
> concurrent sources of each kind**, so every source has an `id` and its own config,
> and there is one uniform collection API instead of two hard-coded singletons.

Two kinds exist, both running **natively in-process** — no supervised subprocesses:

| `kind` | Implementation |
|---|---|
| `airplay` | native embedded AirPlay/RAOP **receiver** (vendored+patched pure-Rust `shairplay`, `airplay_source.rs`) — not a `shairport-sync` subprocess |
| `rtp` | native `libpipewire-module-rtp-source` (`rtp_source.rs`), e.g. the [Bluetooth bridge](../firmware/pi-bridge/README.md) |

Creating or updating a source loads/starts it immediately; deleting one unloads it.
Sendspin, AirPlay-2 and pw-sink **outputs** are auto-discovered, not configured — see
[Outputs](#outputs-discovered-not-configured).

### `GET /api/sources`
Every configured source, with the kind-specific config **nested** under `airplay` or
`rtp` (exactly one is non-null) plus the derived `node_name` and the live `present`
flag.

```json
{ "sources": [
  { "id": "bt-bridge-rtp", "label": "Bluetooth Bridge", "kind": "rtp",
    "present": true, "node_name": "bt-bridge-rtp", "airplay": null,
    "rtp": { "port": 46000, "latency_msec": 100, "source_addr": "239.255.42.42",
             "ignore_ssrc": true, "rate": 48000 } },
  { "id": "airplay-in", "label": "Music Now 2", "kind": "airplay",
    "present": true, "node_name": "airplay-in", "rtp": null,
    "airplay": { "latency_msec": 150, "auth_setup": false,
                 "prevent_takeover": true, "port": 5000 } }
] }
```

`present` means a node called `node_name` is in the live PipeWire registry right now —
it generalizes the old singular `loaded` (RTP) and `running` (AirPlay) flags.

### `POST /api/sources`
Create a source. `label` and `kind` are required; the matching config object carries
partial fields (each has a serde default) and may be **omitted entirely** to accept all
defaults.

```json
// Request
{ "label": "Bluetooth Bridge", "kind": "rtp",
  "rtp": { "port": 46000, "source_addr": "239.255.42.42" } }
// Response (201) — the created SourceView
{ "id": "bluetooth-bridge", "label": "Bluetooth Bridge", "kind": "rtp",
  "present": true, "node_name": "bluetooth-bridge", "airplay": null, "rtp": { "...": "..." } }
```

The store validates first (e.g. an RTP **port collision** with another source) → 400.
On success the source is loaded/started immediately, then routing and groups are
nudged. `id` is derived from `label` (slugified, with collision suffixing).

### `GET /api/sources/{id}`
One source, same `SourceView` shape as above. Unknown `id` → 404.

### `PUT /api/sources/{id}`
Update a source. Every field is optional: `label` renames it, and an `airplay`/`rtp`
object **replaces** that source's config. `kind` is immutable — sending the config
object of the wrong kind is rejected. Omitting both config objects is a label-only
update.

```json
// Request — switch this RTP source to unicast and a longer jitter buffer
{ "rtp": { "port": 46000, "latency_msec": 200, "source_addr": "0.0.0.0",
           "ignore_ssrc": true, "rate": 48000 } }
```

The config is persisted first, then the source is reconciled (for RTP that is an
unload-then-load, so a change is a clean reload). If the reload fails the setting still
persists and the response carries the reason.

### `DELETE /api/sources/{id}`
Remove a source: stops/unloads it (its node disappears live) and drops the stored
entry. Unknown `id` → 404.

### RTP source config (`rtp`)

| Field | Meaning |
|---|---|
| `port` | UDP listen port (default `46000`) |
| `latency_msec` | receiver jitter buffer; raise on a weak link to trade latency for fewer dropouts (default `200`) |
| `source_addr` | bind address — `0.0.0.0` for a plain unicast listener, or an IPv4 **multicast group** (e.g. `239.255.42.42`) so several receivers share one sender's stream |
| `ignore_ssrc` | `true` (default) accepts **any** sender on the port; `false` latches onto the first SSRC seen and rejects every other — the "only one client" corruption guard |
| `rate` | sample rate, **default `48000`** — must match what the sender transmits |

Everything else is fixed to match the bridge's wire format: native-endian **`S16LE`**
(*not* RFC 3551's big-endian `L16`), stereo. See `bridge-daemon/src/rtp_source.rs`.

> **Leave `ignore_ssrc` at `true` unless you specifically want single-sender locking.**
> With `true` the module never rejects a packet on SSRC grounds, so a sender that
> reboots with a fresh SSRC is picked up seamlessly. Setting it `false` *adds* the only
> SSRC rejection path there is. See
> [rtp-input-dropouts-plan.md §4](../pipewire_audio_router/docs/rtp-input-dropouts-plan.md)
> for the measurements behind this.

### AirPlay source config (`airplay`)

| Field | Meaning |
|---|---|
| `latency_msec` | producer jitter buffer (higher = fewer stutters, more latency) |
| `port` | RTSP listen port (e.g. `5000`) |
| `auth_setup` | perform the `auth-setup` handshake |
| `prevent_takeover` | refuse a second sender while one is streaming |

The receiver advertises unencrypted ALAC (`et=0`, `cn=1`).

## Source clients (AirPlay senders)

Which senders are connected to a source, and control over them. `key` identifies a
client as reported by `GET .../clients`.

### `GET /api/sources/{id}/clients`
The senders this source has seen, as a JSON array of client info.

### `POST /api/sources/{id}/clients/ban`
```json
{ "key": "<client key>", "banned": true }
```
Ban (or unban) a sender so it cannot connect.

### `POST /api/sources/{id}/clients/disconnect`
```json
{ "key": "<client key>" }
```
Kick a currently-connected sender.

### `POST /api/sources/{id}/clients/forget`
```json
{ "key": "<client key>" }
```
Drop a remembered sender from the list.

### `POST /api/sources/{id}/clients/priority`
```json
{ "key": "<client key>", "priority": 10 }
```
Set a sender's priority, used when several compete for the source.

### `PUT /api/sources/{id}/policy`
```json
{ "prevent_takeover": true }
```
Toggle one AirPlay source's anti-takeover policy: persisted into that source's config
and applied to the running receiver live, with no restart.

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

### `PUT /api/sendspin/mute`
Mute or unmute one sendspin device, same addressing as the volume call
(`set_sendspin_mute`).

### `GET /api/sendspin/delays` / `PUT /api/sendspin/delay`
Per-device playback delay in ms, used to time-align speakers within a group (a device
with no entry has no extra delay). `GET` returns the sparse map keyed by device node
name; `PUT` sets one entry. A delay edit is applied without restarting the group's
server — see [`/api/sync/settings`](#endpoint-index) for the group-wide lead.

## AirPlay-2 receiver volume

AirPlay-2 receivers carry volume in-band over RTSP like sendspin devices do, so they
get their own pair of endpoints rather than a PipeWire node volume. Current values are
reported as `ap2_volume` / `ap2_muted` by [`GET /api/outputs`](#get-apioutputs).

### `PUT /api/ap2/volume`
Set one receiver's volume (`set_ap2_volume`).

### `PUT /api/ap2/mute`
Mute or unmute one receiver (`set_ap2_mute`).

> The daemon deliberately **does not impose** a volume on connect — an earlier version
> force-sent maximum volume when a session opened, which made a receiver's real level
> (e.g. −67 dB on a Pioneer) disagree with the UI slider after a restart.

## Linking

### `POST /api/links`
Low-level: links exactly one port pair by their literal PipeWire names.
Caller is responsible for pairing FL/FR etc. themselves.

```json
// Request
{ "from_port": "airplay-in:output_FL", "to_port": "ap2-dev-pioneer_vsx_934_f11b89:playback_FL" }
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
Returns the live **sendspin** nodes (`sendspin-dev-*`) with their state +
node-level volume — the volume/state overlay the HA integration layers on top of the
routing matrix.

```json
[
  { "node_id": 42, "node_name": "sendspin-dev-home_assistant_voice_093ca8",
    "state": "playing", "volume": 0.62 }
]
```

`state` is `"playing"` if any link currently feeds the node, else `"idle"`. `volume` is
read natively from the node's SPA `Props` param (`channelVolumes`, `volume.rs`); `null`
if the node exposes no volume control.

> **This list is sendspin-only.** `list_media_players` filters the registry on the
> `sendspin-dev-` prefix, so AirPlay-2 and pw-sink outputs are **not** here — their
> volume is in-band, via [`/api/ap2/volume`](#put-apiap2volume) and reported by
> [`GET /api/outputs`](#get-apioutputs). The HA integration creates one `media_player`
> per routing-matrix **output** across all backends, not from this list directly.

### `GET /api/media_players/{node_id}/volume`
```json
{ "volume": 0.62, "message": null }
```
`volume: null` with a `message` when the node has no volume control (200);
`volume: null` + `message` with 500 if the read failed outright.

### `POST /api/media_players/{node_id}/volume`
```json
// Request
{ "volume": 0.5 }
// Response
{ "volume": 0.5, "message": null }
```
0.0–1.0, on the same cubic scale as `wpctl`/HA's `volume_level`: the value
`V` is written to the node's `channelVolumes` as `V³` (linear gain), so it
reads back identically to what `wpctl` would show.

### `POST /api/media_players/{node_id}/announce`
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
{ "ok": true, "message": "announced on ap2-dev-pioneer_vsx_934_f11b89, ducked 1 source(s)" }
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
    { "node_name": "ap2-dev-pioneer_vsx_934_f11b89", "display_name": "Pioneer", "present": true, "configured": false, "node_id": 42, "peak": 0.0 },
    { "node_name": "sendspin-dev-voice_kitchen", "display_name": "Voice Kitchen", "present": true, "configured": false, "node_id": null, "peak": 0.0 }
  ],
  "links": [ { "source": "airplay-in", "output": "ap2-dev-pioneer_vsx_934_f11b89" } ]
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
{ "source": "airplay-in", "output": "ap2-dev-pioneer_vsx_934_f11b89" }
```
By stable **name**. `link` records the intent and, if both endpoints are
present, pairs every non-monitor output port on the source with the matching
channel-suffix input on the output (`output_FL` ~ `send_FL` ~ `playback_FL`
all match as `FL`) and creates those links natively; the intent is reapplied
automatically if an endpoint (re)appears later. `unlink` removes the intent
and any live links and returns `ok: true` even if there were none.

### `DELETE /api/routing/entity/{node_name}`
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
