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
| `GET` | `/api/sendspin/volumes` | all sendspin volumes |
| `PUT` | `/api/sendspin/volume` | set one sendspin volume |
| `PUT` | `/api/sendspin/mute` | mute one sendspin device |
| `POST` | `/api/sendspin/clear` | `stream/clear` one device — discard its buffers and re-anchor |
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

## Outputs (discovered, then adopted)

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
| `pwsink` | remote PipeWire hosts | a `pwrouter-agent` on that host dials in (`pwsink_agent.rs`) — not a browse |

Discovery only **offers** a device, though: mDNS on a home network also finds the
neighbours' AirPlay speakers, so each discovered device carries a user verdict
(`outputs_store.rs`, `/data/outputs.json`) in its `state` field —

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
    "sendspin_send_ahead_ms": 250 },

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
| `"tone": true` | the calibration pattern: an alternating two-tone **8 ms click**, one per second in a 2 s loop (`calibrate.rs`, `CLICK_MS = 8.0`) | *aligning* two speakers — comparing arrival times |
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
[architecture.md §5.4](../pipewire_audio_router/docs/architecture.md#54-announcing-to-an-output-with-nothing-routed-into-it).

Every call logs one `USER ACTION: announce -> N target(s) [...]` line with the admission,
any on-demand sessions being opened, and anything skipped and why — so an "it didn't
play" report is answerable from the log.

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
The volume/state overlay for outputs backed by a real **PipeWire node**, filtered on
the `sendspin-out-` prefix.

> **On the current architecture this is normally an empty list, and that is correct.**
> Nothing creates `sendspin-out-*` nodes any more: RAOP outputs were dropped, and
> sendspin, AirPlay-2 and pw-sink outputs are all **virtual** — a per-device sender fed
> by the group relay, with no node in the graph. Verified live: the daemon's graph
> contains only the source, the sync anchor, the relay captures and the peak taps.
>
> The HA integration knows this and takes virtual outputs' state from the **routing
> matrix** instead — see `media_player.py`'s `_is_virtual`: *"A virtual output (sendspin
> or AirPlay-2) has no PipeWire node: it never appears in the polled media_players feed,
> so its state comes from routing rather than the feed."*
>
> So do **not** "fix" an empty response by pointing the filter at `sendspin-dev-`: that
> would give the integration a second, conflicting source of truth for devices it
> already tracks. An earlier revision of this document claimed the endpoint returns the
> live `sendspin-dev-*` nodes — that was wrong on both counts (wrong prefix, and
> sendspin devices deliberately never appear here).

```json
[
  { "node_id": 42, "node_name": "sendspin-out-kitchen", "state": "playing", "volume": 0.62 }
]
```

`state` is `"playing"` if any link currently feeds the node, else `"idle"`. `volume` is
read natively from the node's SPA `Props` param (`channelVolumes`, `volume.rs`); `null`
if the node exposes no volume control. If node-backed outputs ever return, this reports
them again with no change.

For the volume of a *virtual* output, use the backend's own control:
[`/api/sendspin/volume`](#put-apisendspinvolume) or
[`/api/ap2/volume`](#put-apiap2volume), with current values from
[`GET /api/outputs`](#get-apioutputs).

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
WebSocket. Carries **typed frames**, each a JSON object with a `type`:

| `type` | Payload | When it is sent |
|---|---|---|
| `matrix` | the `RoutingMatrix` fields at the top level (same shape as `GET /api/routing`, plus `type`) | on connect, on every registry change, and every 250 ms while watched so the input meters and volumes stay live |
| `outputs` | `{ outputs: OutputInfo[] }` — same as `GET /api/outputs` | on connect, then on the first 250 ms tick after a change moves that listing's payload |
| `discovered` | `{ outputs: OutputInfo[] }` — same as `GET /api/outputs/discovered` | ditto |
| `agents` | `{ agents: AgentInfo[] }` — same as `GET /api/agents`; receiver hosts, paired and pending. Diagnostic: the pairing UI reads the two output listings, where a host waiting to pair is a `discovered` `pwsink` output | ditto |

The matrix frame is *internally* tagged so its fields stay at the top
level: a client written against the older protocol, which parsed every
frame as a bare `RoutingMatrix`, still works and simply ignores the
listing frames.

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
synchronized-group badges, and a live input-level meter per source) plus
RAOP-output and AirPlay/RTP-source management and per-output diagnostic test
buttons (Play tone / Play announcement). Sendspin
devices are auto-discovered, so there's no manual sendspin management — just a
capabilities note. Volume sliders poll `/api/media_players` +
`/api/sendspin/volumes` every few seconds, since volume changes aren't a
registry event.
