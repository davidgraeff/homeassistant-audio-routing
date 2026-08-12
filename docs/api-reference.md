# Bridge daemon API reference

The bridge daemon (`pipewire_audio_router/bridge-daemon/`, Rust) exposes
a REST + WebSocket API on `0.0.0.0:8099` by default. This is what the
[Home Assistant integration](../custom_components/pipewire_audio_router/README.md)
and the [web UI](../pipewire_audio_router/README.md#web-ui)
both talk to — there is no other way to control the router.

All request/response bodies are JSON unless noted.

## Endpoint index

The complete route table, as registered in `bridge-daemon/src/api/mod.rs`. Sections below
document the common paths in detail; the rest are listed here with their purpose and
handler name, which is the authoritative place to check the exact body shape.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health` | plain-text liveness |
| `GET` | `/api/nodes` | raw PipeWire node/port snapshot |
| `GET` | `/api/status` | daemon status summary (`get_status`) |
| `GET`/`PUT` | `/api/settings` | daemon settings (`get_settings` / `set_settings`) |
| `GET`/`PUT` | `/api/sync/settings` | group lead + Opus send-ahead floor (`get_sync_settings` / `set_sync_settings`) |
| `GET` | `/api/outputs` | your outputs (adopted) + live state |
| `GET` | `/api/outputs/discovered` | devices discovery is offering |
| `POST` | `/api/outputs/{node_name}/adopt` | add a discovered device (for `pwsink`, this pairs it) |
| `POST` | `/api/outputs/{node_name}/ignore` | dismiss a discovered device |
| `POST` | `/api/outputs/{node_name}/unpair` | `pwsink` only: revoke the pairing *and* remove the output |
| `DELETE` | `/api/outputs/{node_name}` | remove an output (back to discovered) |
| `PUT` | `/api/outputs/{node_name}/name` | rename an output |
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
| `GET` | `/api/now_playing` | what every source is playing |
| `GET`/`PUT`/`DELETE` | `/api/now_playing/{node_name}` | read / update / clear one source's metadata |
| `GET` | `/api/now_playing/{node_name}/artwork` | embedded cover-art bytes |
| `POST` | `/api/now_playing/report` | self-identifying report from a remote producer |
| `GET` | `/api/sendspin/volumes` | all sendspin volumes |
| `PUT` | `/api/sendspin/volume` | set one sendspin volume |
| `PUT` | `/api/sendspin/mute` | mute one sendspin device |
| `POST` | `/api/sendspin/clear` | `stream/clear` one device — discard its buffers and re-anchor |
| `GET` | `/api/sendspin/delays` | per-device sendspin delays |
| `PUT` | `/api/sendspin/delay` | set one sendspin delay |
| `PUT` | `/api/ap2/volume` | set an AirPlay-2 receiver's volume |
| `PUT` | `/api/ap2/mute` | mute an AirPlay-2 receiver |
| `POST` | `/api/ap2/resync` | rebuild one AirPlay-2 receiver's session (lost PTP lock) |
| `POST` | `/api/links` | low-level port link |
| `GET` | `/api/routing` | routing matrix |
| `POST` | `/api/routing/link` / `/api/routing/unlink` | edit the matrix |
| `DELETE` | `/api/routing/entity/{node_name}` | forget an entity |
| `GET` | `/api/routing/ws` | matrix change WebSocket |
| `POST` | `/api/announce` | announce to explicit targets |
| `GET`/`POST` | `/api/duck` | list duck holds / start one (voice ducking) |
| `POST`/`DELETE` | `/api/duck/{hold_id}` | renew / release a duck hold |
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
> `bridge-daemon/src/api/` (one module per resource) for their bodies and
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

## Outputs (discovered, then adopted)

> **RAOP outputs are gone.** Earlier versions of this API let you `POST`/`DELETE`
> `libpipewire-module-raop-sink` outputs by hand. AirPlay output is now the native
> **AirPlay 2** sender, and there are no create/delete endpoints left: outputs are
> **auto-discovered over mDNS** and appear on their own. `store/migration.rs` rewrites
> any surviving `raop-out-*` names in the routing and groups stores on startup.

Three output backends are discovered and reported here, all as virtual routing
outputs:

| `kind` | What it is | Discovery |
|---|---|---|
| `sendspin` | ESPHome sendspin speakers (e.g. HA Voice PE) | mDNS (`outputs/sendspin/discovery.rs`) |
| `airplay2` | native AirPlay-2 receivers | mDNS (`outputs/ap2/discovery.rs`) |
| `pwsink` | remote PipeWire hosts | a `pwrouter-agent` on that host dials in (`outputs/pwsink/agent.rs`) — not a browse |

Discovery only **offers** a device, though: mDNS on a home network also finds the
neighbours' AirPlay speakers, so each discovered device carries a user verdict
(`store/outputs.rs`, `/data/outputs.json`) in its `state` field —

| `state` | Meaning |
|---|---|
| `adopted` | one of your outputs: routable, groupable, an HA `media_player` |
| `discovered` | found on the network, awaiting a decision — **inert** |
| `ignored` | dismissed; hidden behind the Outputs page's "show ignored" |

Only an adopted output is in the routing matrix (and therefore in Home Assistant),
and only adopted intent forms a sync group — nothing is ever streamed to a merely
discovered device. Routing intent is *filtered* by adoption, never deleted, so
adding a device back restores the routing it had. The one exception is
[`POST /api/announce`](#post-apiannounce), which will open an on-demand session for a
discovered device: playing a test tone is how you identify it before adding it.

Per-output *settings* are persisted; the outputs themselves are not created here.

### `GET /api/outputs`
Your outputs — `state: "adopted"` only — with their live state. Common fields:
`node_name`, `name`, `renamed` (`name` is the user's, not the device's — see
`PUT …/name`), `kind`, `present` (in the live registry now), `configured` (has
persisted settings), `state`, `ip`, `port`, `encryption`, `latency_ms`. Each backend
adds its own:

```json
[
  { "node_name": "sendspin-dev-home_assistant_voice_093ca8", "name": "home-assistant-voice-093ca8",
    "kind": "sendspin", "present": true, "configured": false, "state": "adopted",
    "ip": "192.168.178.52", "port": 8928, "encryption": "None", "latency_ms": null,
    "sendspin_codec": "opus", "sendspin_codec_active": "opus",
    "sendspin_codec_options": [ { "codec": "auto", "available": true } ],
    "sendspin_send_ahead_ms": 250,
    "sendspin_out_of_sync": false, "sendspin_sync_errors": 0 },

  { "node_name": "ap2-dev-pioneer_vsx_934_f11b89", "name": "Pioneer VSX-934 F11B89",
    "kind": "airplay2", "present": true, "configured": false, "state": "adopted",
    "ip": "192.168.178.35", "port": 7000, "encryption": "HomeKit", "latency_ms": 200,
    "ptp_locked": true, "ptp_lock_age_s": 0, "ptp_supported": true, "ptp_relevant": true,
    "ap2_features": { "raw": "0x445F8A00,0x1C340", "ptp": true,
                      "buffered_audio": true, "transient_pairing": true },
    "ap2_rate_mode": "auto", "ap2_rate": 44100, "ap2_volume": 0.44, "ap2_muted": false },

  { "node_name": "pwsink-dev-david_local", "name": "david-local", "kind": "pwsink",
    "present": true, "configured": false, "state": "adopted",
    "ip": "192.168.178.21", "port": null,
    "encryption": "None", "latency_ms": null,
    "pwsink_paired": true, "pwsink_streaming": false }
]
```

`ptp_locked` is **runtime** state, not a capability — a receiver can advertise PTP and
sit unlocked without anything being wrong outside a multi-room group.

`sendspin_out_of_sync` / `sendspin_sync_errors` are the *speaker's own* verdict on whether
it is rendering what it was sent: `client/state: "error"`, which `sendspin-cpp` ≥ 0.7.0
sends on an unexpected loss of sync (a buffer underrun), with `synchronized` on recovery.
This is the only receiver-side signal in the system — everything the daemon can observe by
itself (blocks sent, exact timestamps, bytes on the wire, clock-sync exchanges) reads
perfect during exactly that fault. The flag is *now*; the count is the history, and it is
the count that says whether the group lead is too short for that speaker's WiFi. Both stay
`false`/`0` on firmware that never reports the state (ESPHome ≤ 2026.7.x pins 0.6.1, which
has the value but never sets it), so absence is not proof of health.

### `GET /api/outputs/discovered`
The same shape, for everything found but **not** adopted (`state` is `discovered` or
`ignored` — both, in one listing, so a UI can filter client-side). Carries the full
connection details and codec picker, because identifying a device is exactly what you
need before deciding about it.

### `POST /api/outputs/{node_name}/adopt`
Add a discovered device. It becomes routable, groupable and (with the
`expose_outputs_as_media_players` setting on) an HA `media_player`; any routing it had
starts applying again on the next reconcile. Clears an `ignored` verdict. Idempotent.

For a `pwsink` host this is also the **pairing** step: the daemon mints that host's
token first, so "pair" and "add" are one call. It fails (leaving the host unadopted)
if no agent is currently asking to pair as that node name. A host whose agent has
dialled in but isn't paired is listed under `/api/outputs/discovered` with
`pwsink_paired: false` and a `pwsink_pair_code` — the code that host's own agent
logged, to be compared before pairing.

### `POST /api/outputs/{node_name}/ignore`
Dismiss a discovered device. The stronger form of remove: it also clears the device's
routing intent and its music/announcement-group membership. Idempotent.

### `POST /api/outputs/{node_name}/unpair`
`pwsink` only. Revokes the host's pairing *and* does everything `DELETE` does
(routing intent, group membership, adoption). Idempotent, and it does not require a
pairing to exist: an output can outlive one (a lost `agents.json`), and this is that
card's only removal button.

The host's agent is not stopped by this and does not give up: it drops the token it
can no longer use and keeps dialling in, so the host reappears in
`/api/outputs/discovered` as pairable — the same thing an un-adopted speaker that is
still on the network does. Use `/ignore` to keep it out of the way.

### `DELETE /api/outputs/{node_name}`
Remove an output — back to `discovered`. Clears its routing intent, group membership
and HA entity. A device that's still on the network reappears in
`/api/outputs/discovered`; an offline one disappears until it shows up again.
Idempotent.

### `PUT /api/outputs/{node_name}/name`
`{"name": "Shower"}` renames an output; `{"name": null}` drops the override so it goes
back to the name discovery reports. Trimmed, and at least 3 characters — a shorter one
is a `400`, because this name becomes an HA entity name and a routing-graph label,
where a slip is hard to notice. Stored against the stable `node_name`, so it survives
the device dropping off the network, and it is what every listing shows from then on:
`/api/outputs`, the routing matrix (and therefore the graph, the group editors and Home
Assistant). Independent of the adoption verdict — removing or un-ignoring a device
keeps the name you gave it. Nothing restarts.

### `PUT /api/outputs/{node_name}/latency`
Per-output playout delay in ms (`{"latency_ms": 40}`; `null` clears the override and
returns the output to the add-on default). Persisted per node name and applied to the
running sender. Two kinds have this knob:

- **AirPlay 2** — the render delay (default 0, up to 2000 ms). Applied live to the
  running stream, no reconnect.
- **pw-sink** — the receiving host's jitter buffer (`sess.latency.msec`, default 100 ms
  = the PipeWire module's own). Clamped to a multiple of the 5 ms packet time, 15–2000 ms,
  and pushed to that host's
  agent, which reloads its receiver — a sub-second gap in that one target's audio. A
  disconnected host is not an error: the value applies when it reconnects.

Sendspin has no entry here; its equivalent is the static delay
(`PUT /api/sendspin/delay`) over the group lead (`PUT /api/sync/settings`).
`latency_ms` in a listing is the stored override (`null` = none) and
`latency_effective_ms` is what the output is actually running.

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
> [architecture.md §5.5](../pipewire_audio_router/docs/architecture.md#55-announcing-to-an-output-with-nothing-routed-into-it).

## Sources

Full CRUD over the router's inputs, persisted in a daemon-owned store
(`/data/sources.json`) that starts empty on a fresh install (no `options.json`
seeding) and is managed live here with no restart (`sources/mod.rs`).

> **These endpoints replaced the singular `/api/source/rtp` and
> `/api/source/airplay`**, which no longer exist. The router now supports **several
> concurrent sources of each kind**, so every source has an `id` and its own config,
> and there is one uniform collection API instead of two hard-coded singletons.

Two kinds exist, both running **natively in-process** — no supervised subprocesses:

| `kind` | Implementation |
|---|---|
| `airplay` | native embedded AirPlay/RAOP **receiver** (vendored+patched pure-Rust `shairplay`, `sources/airplay.rs`) — not a `shairport-sync` subprocess |
| `rtp` | native `libpipewire-module-rtp-source` (`sources/rtp.rs`), e.g. the [Bluetooth bridge](../firmware/pi-bridge/README.md) |

Creating or updating a source loads/starts it immediately; deleting one unloads it.
Sendspin, AirPlay-2 and pw-sink **outputs** are auto-discovered, not configured — see
[Outputs](#outputs-discovered-then-adopted).

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
(*not* RFC 3551's big-endian `L16`), stereo. See `bridge-daemon/src/sources/rtp.rs`.

> **Leave `ignore_ssrc` at `true` unless you specifically want single-sender locking.**
> With `true` the module never rejects a packet on SSRC grounds, so a sender that
> reboots with a fresh SSRC is picked up seamlessly. Setting it `false` *adds* the only
> SSRC rejection path there is. See
> [rtp-input-dropouts.md §4](../pipewire_audio_router/docs/rtp-input-dropouts.md)
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

## Now playing (per-source metadata)

What each source is currently playing — title, artist, album, position and cover art —
from whichever producer can say. See
[source-metadata-plan.md](../pipewire_audio_router/docs/old/source-metadata-plan.md) for the model and
`bridge-daemon/src/sources/now_playing.rs` for the implementation.

**Keyed by source *node name*, not source id.** That is the key the routing matrix, the
persisted routing intent and the Home Assistant integration already share, and it is what
the WebSocket frame below is keyed by — so a consumer never has to hold both keys.

Live updates arrive as a **`now_playing` frame on `/api/routing/ws`** (see *Routing
matrix*), pushed only when something changes. Two consumers read it: the Home Assistant
integration (on each output's and each music group's `media_player`) and this add-on's own
routing graph, which shows a second row on every source card. These REST routes are the cold-path
companion: a consumer that just connected, a debugging `curl`, or a producer reporting in.

Entries are in-memory only, expire after 90 s without an update, and are dropped when a
source is deleted — so nothing here ever describes a source that no longer exists.

### `GET /api/now_playing`
```json
{
  "sources": {
    "airplay-in": {
      "state": "playing",
      "title": "Song", "artist": "Artist", "album": "Album",
      "duration_ms": 213000,
      "position_ms": 41000,
      "position_updated_at": 1786000000000,
      "artwork": { "kind": "embedded", "rev": 3, "mime": "image/jpeg", "len": 74210,
                   "path": "/api/now_playing/airplay-in/artwork?rev=3" }
    }
  }
}
```
Every source with something to say. Absent fields are omitted, not null. `state` is
`playing`, `paused` or `stopped`.

`position_updated_at` is Unix milliseconds: the position is only meaningful *together
with* when it was true, and consumers extrapolate from it rather than expecting a fast
update cadence (position is published at most every 5 s). It leads the sound by the
ingest jitter buffer plus the output's playout latency; that drift is accepted.

`artwork` is either `{"kind": "url", "url": "…"}` (fetch it yourself) or `{"kind":
"embedded", …}` with a ready-made `path` to append to this daemon's base URL. `rev`
changes with the picture, so the path is a safe hard-cache key.

### `GET /api/now_playing/{node_name}`
One source's entry, or `404` when nothing is known.

### `GET /api/now_playing/{node_name}/artwork`
The current cover-art bytes, with their real `Content-Type` (sniffed from the data, not
trusted from the sender) and an `ETag`. The `?rev=` in the published path is a
cache-buster, not a selector — the current image is always what is returned.

### `PUT /api/now_playing/{node_name}`
```json
{ "state": "playing", "title": "Song", "artist": "Artist", "album": "Album",
  "duration_ms": 213000, "position_ms": 41000, "artwork_url": "https://…/cover.jpg" }
```
Merge metadata into a source. **Every field is optional and an omitted field is left
alone** — that is what lets a producer send title/artist and progress separately without
clobbering itself. A *changed* `title` is treated as a new track, so the previous track's
album, artist, duration and cover art are dropped rather than lingering under the new
name. Blank strings count as absent. `404` if no source has that node name; `400` if the
body carries no fields at all.

An `artwork_url` never replaces embedded bytes for the current track, so a producer that
can supply both does not downgrade itself.

### `DELETE /api/now_playing/{node_name}`
The session ended. Idempotent. Prefer this to letting the TTL collect the entry: it is
what makes a media card in Home Assistant collapse instead of freezing on the last track.

### `POST /api/now_playing/report`
```json
{ "rtp_port": 46000, "title": "Song", "artist": "Artist", "state": "playing" }
```
The entry point for a **remote** producer — the Pi bridge's reporter. It knows the UDP
port its `module-rtp-sink` transmits to, and does not need to learn the source ids this
daemon assigned, so `rtp_port` is resolved against the source store (the same key
`bt_bridge_discovery` matches an advert to a source by). Metadata fields are flattened
alongside it and merge exactly as `PUT` does.

A body with **no metadata fields** means "nothing is playing any more" and clears the
entry — so a reporter needs no second endpoint for that. `404` when no RTP source is
configured on that port, which is not worth retrying: the bridge may simply have been set
up before its source was added here.

## Sendspin devices

Sendspin speakers (ESPHome, e.g. HA Voice PE) are **auto-discovered** over
mDNS (`outputs/sendspin/discovery.rs`) — there is no per-output config to create.
Each discovered device shows up as a virtual routing output
(`sendspin-dev-<slug>`); devices routed from the same source set are formed
into one synchronized group automatically (`sendspin_group.rs`). Online/offline
is decided by the live connection plus a TCP liveness probe, not raw mDNS
(`outputs/sendspin/liveness.rs`). The only per-device control is volume, carried
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

### `POST /api/sendspin/clear`
Ask one device to discard buffered-but-unplayed audio and re-anchor, **without ending
its stream** — the protocol's `stream/clear`.

```json
// Request
{ "node_name": "sendspin-dev-voice_pe_kitchen" }
// Response
{ "ok": true, "message": "cleared 'Kitchen' — it will re-anchor on the next audio" }
```

This is the recovery action for a device that is demonstrably being *sent* audio and
renders none — the 2026-08-03 failure where three of four devices went silent while the
daemon, the graph and the clock sync were all healthy (see
[sendspin-open-items.md](../pipewire_audio_router/docs/sendspin-open-items.md)). Before it
existed the only lever was restarting the add-on, which interrupted every other output
and destroyed the evidence.

It is one frame, and deliberately **per device**: it does *not* reset the group's shared
timeline, so the other members of the group keep playing undisturbed. That is why it does
not use the library's `Group::clear_stream` helper, which also re-anchors the timeline.

Cheaper than the alternatives — a per-device *reconnect* (nudging its static delay) costs
a full re-dial and a fresh clock filter for that device; a group restart costs that for
everyone.

`ok` is `false` with an explanatory message when the device has no live connection: there
is nothing to clear, and its next stream starts fresh anyway. Exposed in the web UI as
**Resync** on each connected sendspin output.

### `GET /api/sendspin/delays` / `PUT /api/sendspin/delay`
Per-device playback delay in ms, used to time-align speakers within a group (a device
with no entry has no extra delay). `GET` returns the sparse map keyed by device node
name; `PUT` sets one entry. A delay edit is applied without restarting the group's
server — see [`/api/sync/settings`](#getput-apisyncsettings) for the group-wide lead.

### `GET`/`PUT` `/api/sync/settings`
The two group-wide sendspin timing knobs.

```json
{ "group_lead_ms": 180, "opus_floor_ms": 40 }
```

* **`group_lead_ms`** — the head start every group gets, over what its members ask for.
  Default **180 ms**, bisected on hardware (2026-08-13): the smallest lead four ESP32
  speakers played cleanly at over 2.4 GHz WiFi. It is not 0 because the ESPHome firmware
  pinned here reports no `min_buffer_ms`, so without it a group falls back to the Opus
  block floor and stutters. A site measurement, not a constant — 802.11 guarantees no
  latency at all.
* **`opus_floor_ms`** — the head start an **Opus** stream gets whether or not a device
  asked for one: time for the network hop, the speaker's decode and its scheduling.
  Default 40 ms (two Opus blocks). PCM and FLAC impose nothing, and a device that
  reports its own `min_buffer_ms` overrides it. Optional in the `PUT`, which leaves it
  unchanged. Clamped at the Opus block size (20 ms at 48 kHz) — nothing is sent before a
  whole block exists.

A group's send-ahead is `max(group_lead_ms, largest member requirement)`, where a
member's requirement is its reported `min_buffer_ms` (else its codec's floor) plus its
own static delay. `GET` therefore also reports what that resolves to, read-only:
`group_lead_floor_ms`, `group_lead_effective_ms`, `group_lead_floor_sources` (which
devices set the floor and why), and `opus_floor_min_ms`.

#### The lead in force vs. the lead computed

`GET` also reports **`group_lead_running_ms`** (and `group_lead_running`, the same per
group with its anchor name): what the running sendspin servers are *actually* streaming
at. This is not the same number as `group_lead_effective_ms`, and the difference is not a
bug in either:

* a group's send-ahead is fixed when its shared timeline is constructed, and
* it is a **high-water mark** — a raise restarts the group's server, a drop deliberately
  does not, because honouring every incidental drop would reconnect every speaker (tens of
  seconds of silence each on real ESPHome firmware) to buy back a few tens of ms.

So a group can sit indefinitely on a larger figure than the settings imply — measured on
2026-08-12: `group_lead_effective_ms: 130` while the relay logged `lead 899..921 ms`, left
over from one static-delay experiment. Reporting only the computed value made that
invisible, and made tuning *downward* look like it had worked when nothing had changed.

A `PUT` now also **re-arms** the lead: each group whose requirement differs from what its
server is running restarts, so a *lower* value takes effect without restarting the add-on
(the previous workaround was to raise one speaker's static delay and put it back, which
forced a group restart as a side effect). The response says how many groups that costs.

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

### `POST /api/ap2/resync`
Release one receiver's AirPlay session and build a fresh one — re-arming its PTP peer on
the way — while its groupmates keep streaming. The AP2 counterpart of
[`POST /api/sendspin/clear`](#post-apisendspinclear), for the same symptom: an output that
is reachable and being sent audio yet renders nothing.

```json
// Request
{ "node_name": "ap2-dev-pioneer_vsx_934_f11b89" }
// Response
{ "ok": true, "message": "rebuilding 'Pioneer''s session — it should be back in a few seconds" }
```

`ok: false` means the receiver has no live sender, so there is no session to rebuild (the
reconciler is what gives it one) — reported rather than treated as success.

On this hardware the fault it recovers is a **lost PTP clock lock**: our PT=87 anchors are
timestamps in the grandmaster's timeline, so a receiver whose slaved clock has drifted off
it plays nothing at all. A Pioneer VSX-934 does this repeatedly, and until this existed
the only fixes were restarting the add-on or power-cycling the AVR — both of which work
only because both build a new session.

The daemon also does this **by itself**: the AP2 liveness task watches each receiver's
gPTP lock age and rebuilds the session of one that had a lock, is still being streamed to,
and has gone quiet for 30 s (at most one attempt every two minutes, and never for a
receiver that has *never* locked — a Yamaha WX-021 never sends a `Delay_Req` and plays
perfectly). This endpoint is for the cases it cannot see, and for not waiting.

## Announcements

### `POST /api/announce`
Play a clip on explicit targets, ducking whatever is already playing on them. This is
the backend-agnostic per-device path (sendspin + AirPlay 2 + pw-sink) and the one the
Outputs tab's **Play tone** / **Play announcement** buttons use.

```json
// Request — targets plus one audio source
{ "targets": ["sendspin-dev-kitchen"], "tone": true }
{ "targets": ["ap2-dev-dusche"], "test": true }
{ "announcement_group": "doorbell" }
// Response
{ "ok": true, "admission": "playing", "position": null, "reason": null,
  "message": "announce to 1 target(s): playing" }
```

Targets come from `targets`, or from a named group via `announcement_group` (an explicit
`targets`/`duck` in the request still wins). `duck` defaults to the daemon setting;
`on_busy` is `"queue"` (default) or `"reject"`; `barge_in` and `ttl_ms` are honoured by
the arbiter. `admission` is `playing`, `queued` (with `position`) or `rejected`.

Two audio sources are built in, and they are **not** interchangeable as a functional
test:

| | What it is | Use it for |
|---|---|---|
| `"tone": true` | the calibration pattern: an alternating two-tone **8 ms click**, one per second in a 2 s loop (`align/calibrate/mod.rs`, `CLICK_MS = 8.0`) | *aligning* two speakers — comparing arrival times |
| `"test": true` | the committed speech clip `bridge-daemon/assets/test-announcement.mp3` | *"is this speaker working?"* |

The tone is deliberately a tick, not a tone — at 20 ms blocks roughly one block in fifty
carries audio. It is easy to mistake for "nothing played", so reach for `test` when the
question is whether a device makes sound at all.

**A target only hears the clip while a per-device sender is streaming it**, so before
starting the clip the daemon makes sure one is: a sendspin device always has its idle
sender, and an **AirPlay-2 receiver or pw-sink target with nothing routed into it gets an
on-demand session** — audible a few seconds later (AP2: pairing + its render delay;
pw-sink: the target discovering our advert and handshaking) and handed back after a 30 s
lease. Targets that nothing can carry are **dropped and named in `message`** (with all of
them unavailable the call is rejected), so a "playing" answer means audio is really going
somewhere. See
[architecture.md §5.5](../pipewire_audio_router/docs/architecture.md#55-announcing-to-an-output-with-nothing-routed-into-it).

Every call logs one `USER ACTION: announce -> N target(s) [...]` line with the admission,
any on-demand sessions being opened, and anything skipped and why — so an "it didn't
play" report is answerable from the log.

## Duck holds (voice ducking)

A **duck hold** attenuates an output's music with **no clip of its own** — an
open-ended lease rather than an announcement. It exists for voice assistants that
speak through their *own* speaker (an HA Voice PE): the router has nothing to
play, only music to get out of the way. Ducking happens as a gain in the
per-device mix (`outputs/overlay_mixer.rs`), so a device's own volume never moves, there
is nothing to restore, and one member of a synchronized group can duck while its
groupmates keep playing.

Deliberately **not** the announce path. That one is built for atomic clips
(whole-or-nothing, queue, barge-in, TTL) and it *occupies* its targets — a
doorbell would queue behind someone's voice turn instead of playing over the
already-ducked music. Holds and announcement overlays compose instead: the mix
takes the stronger (lower) of the two gains, and the clip itself is never
attenuated.

The daemon knows nothing about rooms. Home Assistant resolves "which speakers are
in the room the satellite is in" from its own area registry and posts output
names (see the integration's `voice_duck.py`).

### `POST /api/duck`
```json
// Request — targets, or an announcement group's targets
{ "targets": ["sendspin-dev-kitchen"], "level": 0.25, "ttl_ms": 30000 }
// Response
{ "ok": true, "hold_id": 4, "ducked": ["sendspin-dev-kitchen"], "level": 0.25,
  "message": "ducking 1 target(s) to 0.25" }
```

`level` is a **gain** (0.25 = quarter volume), defaulting to the daemon's
`default_duck` setting; `ttl_ms` defaults to 30 000. `targets` may be replaced by
`announcement_group` to reuse a named group's target list. Unknown group → 400;
no targets at all → 400.

Holds **compose**: two callers ducking the same output don't fight (strongest
gain wins) and each releases only its own. A hold on an output with nothing
playing is inaudible and harmless — that is deliberate, so a caller needn't know
which speakers are live, and music that *starts* mid-hold comes up already
ducked.

### `POST /api/duck/{hold_id}`
```json
// Request (ttl_ms optional)
{ "ttl_ms": 30000 }
```
Extends the lease from now. **404** once the hold is gone (expired, or the daemon
restarted) — the caller should then start a new one rather than believe it is
still ducking.

### `DELETE /api/duck/{hold_id}`
Releases it now — the normal end of a voice turn. Releasing a hold that is
already gone is **200**, not an error: the caller's intent ("not ducking") holds
either way.

### `GET /api/duck`
```json
[ { "output": "sendspin-dev-kitchen", "hold_id": 4, "level": 0.25 } ]
```
Live holds, sorted by output — the answer to "why is this speaker quiet?". The
Outputs tab polls this every 2 s and shows a `ducked NN%` badge per output, so
the answer is on screen while it is happening (holds last one voice turn).

**A pw-sink host also ducks its own audio.** For an agent-backed `pwsink-dev-*`
output the duck is mirrored to that host's agent, which attenuates the streams it
is playing *itself* (music not in ours). The agent is told an absolute depth, so
the daemon re-asserts the aggregate of every hold *and* any announcement overlay
(`OverlayMixer::effective_duck`) on each change — an announcement ending never
un-ducks a host whose room still has an assistant talking.

**The lease is the safety net.** Nothing else un-ducks a hold whose owner died:
the announce tick (150 ms) expires overdue leases and logs one line per hold, so
a Home Assistant restart or a dropped network mid-turn costs at most one TTL of
quiet music instead of silence until someone notices. Every call logs one
`USER ACTION: duck -> N target(s) [...]` / `USER ACTION: unduck -> hold N` line.

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
(`pw/thread.rs`) — the port names are resolved to object ids against the
live registry, then a create command is handed to that thread. Idempotent:
a link already present between the same ports is reported as success
(`ok: true`). Failure modes: either port name not found in the registry
→ 400; the PipeWire thread unreachable/dropped the request → 500.

### `GET /api/routing` / `POST /api/routing/link` / `POST /api/routing/unlink` / `GET /api/routing/ws`
The higher-level pairing used by the manual routing UI — pairs matching
channel suffixes automatically instead of requiring literal port names.
See "Routing matrix" below.

## Media players

Outputs are all virtual now (a per-device sender fed by the group relay, no PipeWire
node of their own), so there is no node-backed volume/state overlay any more: the
`GET /api/media_players` listing, the per-node `…/volume` get/set endpoints and the
node-based ducked announce (`POST /api/media_players/{node_id}/announce`) were all
removed along with the node-volume code behind them (`volume.rs`).

Announcements go to [`POST /api/announce`](#post-apiannounce), which ducks and overlays
per device in the relay instead of moving node volumes.

Take an output's state from the **routing matrix** ([`GET /api/routing`](#get-apirouting)),
and its volume from the backend's own control:
[`/api/sendspin/volume`](#put-apisendspinvolume) or
[`/api/ap2/volume`](#put-apiap2volume), with current values from
[`GET /api/outputs`](#get-apioutputs).

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
  is open — see `pw/metering.rs`).
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
WebSocket. Carries **typed frames**, each a JSON object with a `type`:

| `type` | Payload | When it is sent |
|---|---|---|
| `matrix` | the `RoutingMatrix` fields at the top level (same shape as `GET /api/routing`, plus `type`) | on connect, then on every registry change **whose payload actually differs** |
| `meters` | `{ nodes: { <node_name>: { peak?, xruns? } } }` — the live figures only | on connect, then on a 250 ms tick while watched, deduped |
| `outputs` | `{ outputs: OutputInfo[] }` — same as `GET /api/outputs` | on connect, then on the first 250 ms tick after a change moves that listing's payload |
| `discovered` | `{ outputs: OutputInfo[] }` — same as `GET /api/outputs/discovered` | ditto |
| `agents` | `{ agents: AgentInfo[] }` — same as `GET /api/agents`; receiver hosts, paired and pending. Diagnostic: the pairing UI reads the two output listings, where a host waiting to pair is a `discovered` `pwsink` output | ditto |
| `now_playing` | `{ sources: { <node_name>: NowPlaying } }` — same as `GET /api/now_playing` | ditto |

**Every frame on this socket is deduped**, the matrix included: it is sent only when
its serialized payload differs from the last one sent on that socket.

**The matrix frame has no timer behind it** — it is pushed on a change notification and
on nothing else. Daemon-side, that makes "anything that changes a routing node's fields
or the link set must notify `changes`" an invariant rather than a nicety: a path that
forgets leaves every client showing the old value indefinitely. That visible staleness
is deliberate, and preferred to a periodic re-check that would hide the omission.

**`meters` is the only frame on a timer, and it is the reason the matrix is not.**
The matrix used to be re-pushed every 250 ms so that peaks and xrun counts stayed
live. Measured on a live instance: 2 210 bytes per frame of which the peaks were
36 — 1.6 % — 73 % static configuration, **49 of 49 consecutive frames byte-identical
at idle**, 9.0 KiB/s per client. The daemon's own cost was negligible (~0.2 % of a
core per client); the cost was that every client re-read its whole view four times a
second to learn nothing. So the two were split.

In `meters`, **a field absent means zero**: a node with no signal and no xruns is
left out of `nodes` entirely, and a silent system therefore sends
`{"type":"meters","nodes":{}}` once and then nothing until something moves. A client
must read absence as zero (that is how a level decaying to silence is expressed) and
must not treat `RoutingNode.peak`/`.xruns` from a matrix frame as live — those are
just the sample taken when the matrix was last built. `GET /api/routing` still
carries both, so a cold read is complete.

`nodes` only ever mentions nodes the last `matrix` frame showed. The profiler
reports every active node in the graph, most of which the matrix does not display.

The matrix frame is *internally* tagged so its fields stay at the top
level: a client written against the older protocol, which parsed every
frame as a bare `RoutingMatrix`, still works and simply ignores the
listing frames.

`now_playing` is deliberately its own frame rather than a field on `matrix`:
the matrix is a large payload a client re-reads in full (the web UI recomputes
its graph layout, the HA integration re-renders every entity), while a track
changes once a song, and an artwork revision has nothing to do with routing.
Keeping the descriptive payload off it is the design
(see [source-metadata-plan.md](../pipewire_audio_router/docs/old/source-metadata-plan.md) §3.2), so do not move
it back onto a routing node.

**A client must switch on `type` and ignore frames it does not know.** Frame
types get added; a client that parses every frame as a matrix will mis-read the
listings (an `OutputInfo` has `name`, not `display_name`) or, for a payload with
neither `sources` nor `outputs`, silently build an empty matrix.

The listing frames exist so a UI does not have to poll those endpoints.
They are only sent when the built payload differs from the last one sent
on that socket — comparing the payload rather than tracking which events
affect which listing, so a new field cannot be forgotten and a burst of
unrelated changes stays quiet. The REST endpoints remain the way to get a
listing on first paint; the socket keeps it fresh afterwards.

A change does not rebuild the listings immediately: it marks them dirty
and the next 250 ms tick does the work, so a reconcile burst costs one
rebuild instead of one per notification (measured against 20 rapid
mutations: 19 listing frames before coalescing, 1 after). The price is up
to 250 ms of latency on a background change — a client that just made a
mutation re-reads the endpoint itself and never waits for this path.

Change frames are driven by a broadcast channel the registry-observer
thread pings synchronously, not by polling. A slow client that misses a
ping is caught up by the next frame it does receive; there is no event
replay. The client never needs to send anything.

## `GET /` (and other non-API paths)
Serves the built web UI — a Vite + Svelte single-page app (source in
`pipewire_audio_router/frontend/`, served as static files from
`--static-dir`), styled to match Home Assistant with light/dark themes and
also surfaced in the HA sidebar via ingress. It's a full admin console: the
routing matrix (outputs as rows, sources as columns, live over
`/api/routing/ws`, clickable link/unlink cells, per-output volume sliders
including sendspin devices, offline endpoints grayed with a forget button,
synchronized-group badges, a `ducked NN%` badge while an output's music is
attenuated for a voice turn, and a live input-level meter per source) plus
RAOP-output and AirPlay/RTP-source management and per-output diagnostic test
buttons (Play tone / Play announcement). Sendspin
devices are auto-discovered, so there's no manual sendspin management — just a
capabilities note. Volume sliders poll `/api/sendspin/volumes` (and read
`/api/outputs` for AirPlay-2 levels) every few seconds, since volume changes
aren't a registry event.
