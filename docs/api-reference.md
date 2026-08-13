# Bridge daemon API reference

The bridge daemon (`pipewire_audio_router/bridge-daemon/`, Rust) exposes
a REST + WebSocket API on `0.0.0.0:8099` by default. This is what the
[Home Assistant integration](../custom_components/pipewire_audio_router/README.md)
and the [web UI](../pipewire_audio_router/README.md#web-ui)
both talk to — there is no other way to control the router.

All request/response bodies are JSON unless noted.

## How a call answers

**The HTTP status carries success.** A 2xx means it happened; anything else carries a
typed reason:

```json
// 200 — a write that happened. `message` is the sentence to show the user.
{ "message": "set 'Kitchen' to 42%" }
// 409 — it did not. `kind` is machine-readable; branch on it, not on the prose.
{ "kind": "conflict", "message": "'Kitchen' has no live connection, so there is nothing to clear" }
```

| `kind` | Status | Means |
|---|---|---|
| `not_found` | 404 | no such output, source, hold or group |
| `bad_request` | 400 | a value out of range, a kind with no such knob, a body that does not make sense |
| `conflict` | 409 | the request is fine, the target is not in a state where it applies — no live connection to clear, no session to rebuild |
| `unavailable` | 503 | a far end this daemon depends on is not answering: a PipeWire host with no agent, the PipeWire thread gone |
| `internal` | 500 | this daemon broke — a store it could not persist |

Reads answer with their resource and no envelope. A write that has something to return
(a duck hold, an announcement's admission) returns *that*, on a 200.

The alignment subsystem answers refusals in the same envelope with its own richer
vocabulary — `kind` values like `mic_lost` or `estimator`, plus the member to blame and the
estimator's own verdict — because each names a state the user can act on.

> **There is no `ok` field.** There used to be, alongside the status, and they disagreed:
> `POST /api/sendspin/clear` on a disconnected device answered `200 {ok:false}`,
> `PUT /api/pwsink/volume` with no agent `503 {ok:false}`, a bad name `400 {ok:false}`. So
> both consumers checked the status *and* the body on every call, and the rule was
> unwritten. `message` was also the only carrier of *why*, so reacting differently to "no
> agent connected" than to "unknown output" meant matching sentences — which is what `kind`
> is for.

## Endpoint index

The complete route table, as registered in `bridge-daemon/src/api/mod.rs`. Sections below
document the common paths in detail; the rest are listed here with their purpose and
handler name, which is the authoritative place to check the exact body shape.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health` | plain-text liveness |
| `GET` | `/api/events` | **the** push socket — every live feed, topics subscribed by message |
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
| `PUT` | `/api/outputs/{node_name}/volume` | one output's level, `0.0`–`1.0`, whatever its kind |
| `PUT` | `/api/outputs/{node_name}/mute` | mute one output |
| `PUT` | `/api/outputs/{node_name}/delay` | its timing knob in ms (polarity per kind, reported by `GET /api/outputs`) |
| `POST` | `/api/outputs/{node_name}/resync` | ask it to recover (sendspin `stream/clear`, AP2 fresh session) |
| `PUT` | `/api/outputs/{node_name}/ap2-rate` | AirPlay-2 rate mode |
| `PUT` | `/api/outputs/{node_name}/sendspin-codec` | sendspin codec choice |
| `GET`/`POST` | `/api/sources` | list / create a source |
| `GET`/`PUT`/`DELETE` | `/api/sources/{id}` | read / update / remove a source |
| `GET` | `/api/sources/{id}/clients` | connected senders (AirPlay) |
| `DELETE` | `/api/sources/{id}/clients/{key}` | forget a remembered sender |
| `PUT` | `/api/sources/{id}/clients/{key}/ban` | ban / unban a sender |
| `PUT` | `/api/sources/{id}/clients/{key}/priority` | set a sender's priority |
| `POST` | `/api/sources/{id}/clients/{key}/disconnect` | kick a sender |
| `PUT` | `/api/sources/{id}/policy` | anti-takeover policy |
| `GET` | `/api/now_playing` | what every source is playing |
| `GET`/`PUT`/`DELETE` | `/api/now_playing/{node_name}` | read / update / clear one source's metadata |
| `GET` | `/api/now_playing/{node_name}/artwork` | embedded cover-art bytes |
| `POST` | `/api/now_playing/report` | self-identifying report from a remote producer |
| `GET` | `/api/sendspin/volumes` | all sendspin volumes |
| `GET` | `/api/sendspin/delays` | per-device sendspin delays |
| `GET` | `/api/agents` | paired and pending receiver hosts (diagnostics) |
| `GET` | `/api/agent/ws` | the socket a `pwrouter-agent` dials in on — **agents only**, not a status feed |
| `POST` | `/api/links` | low-level port link |
| `GET` | `/api/routing` | routing matrix |
| `POST` | `/api/routing/link` / `/api/routing/unlink` | edit the matrix |
| `DELETE` | `/api/routing/entity/{node_name}` | forget an entity |
| `POST` | `/api/announce` | announce to explicit targets |
| `GET`/`POST` | `/api/duck` | list duck holds / start one (voice ducking) |
| `POST`/`DELETE` | `/api/duck/{hold_id}` | renew / release a duck hold |
| `GET`/`POST` | `/api/groups/music` | list / create a Music group |
| `PUT`/`DELETE` | `/api/groups/music/{id}` | edit / delete a Music group |
| `POST`/`DELETE` | `/api/groups/music/{id}/route` | route / unroute a Music group |
| `GET`/`POST` | `/api/groups/announcement` | list / create an Announcement group |
| `PUT`/`DELETE` | `/api/groups/announcement/{id}` | edit / delete an Announcement group |
| `GET` | `/api/align/groups` | groups available for alignment |
| `GET`/`DELETE` | `/api/align` | session status / stop (restores levels, mutes and routing) |
| `POST` | `/api/align/start` | hold these speakers exclusively — the run's **whole scope**, not one position's |
| `POST` | `/api/align/still-here` | postpone the idle teardown by one whole allowance |
| `POST` | `/api/align/select` | the by-ear reference/target pair |
| `POST` | `/api/align/audible` | which held members are audible (one to measure, N for a level round) |
| `POST` | `/api/align/volume` | playback level of the audible members (0–100) |
| `POST` | `/api/align/members/{node_name}/channel` | measure one member through one channel of its stereo pair (`both`/`left`/`right`) |
| `GET` | `/api/align/mic` | microphone-ingest status: frames, gaps, peak, recent clipping |
| `GET` | `/api/align/mic/ws` | binary microphone ingest — one socket at a time |
| `GET` | `/api/align/mic/signal` | the pre-flight verdict: is the level good enough to measure? |
| `POST` | `/api/align/measure/start` | begin a measured run (`{mode, chain}`) |
| `GET`/`DELETE` | `/api/align/measure` | run status / abandon (delays untouched) |
| `POST` | `/api/align/measure/arrival/{node_name}` | near field: "I am at this speaker now" |
| `POST` | `/api/align/measure/close` | near field: the closure reading that separates drift from real offsets |
| `POST` | `/api/align/measure/position` | multi-position: measure one listening spot through its overlaps |
| `POST` | `/api/align/measure/finish` | multi-position: renormalise the chain globally and propose one write |
| `POST` | `/api/align/measure/apply` | write the solved delays — explicit, never automatic |
| `POST` | `/api/align/measure/revert` | restore the start-of-session delay snapshot |
| `GET` | `/api/align/measure/log` | the run transcripts (JSONL per run, bounded) |
| `GET` | `/api/align/measure/split` | stored band-split calibrations |
| `POST`/`DELETE` | `/api/align/measure/split/{node_name}` | measure one output's band split at close range / clear it |
| `GET`/`POST`/`DELETE` | `/api/align/equivalence` | the relay-vs-device delay experiment (the daemon picks the member) |
| `POST` | `/api/align/equivalence/{node_name}` | …run it on one named member instead |
| `POST`/`DELETE` | `/api/spike/per-device` | dev-only spike harness |
| `POST`/`DELETE` | `/api/spike/multi-device` | dev-only spike harness |
| `POST`/`DELETE` | `/api/spike/overlay` | dev-only spike harness |
| `POST`/`DELETE` | `/api/spike/ap2` | dev-only spike harness |
| `POST`/`DELETE` | `/api/spike/pw-sink` | dev-only spike harness |

> **Two-tier groups (`/api/groups/*`) and speaker alignment (`/api/align/*`) are listed
> above but have no detail sections here.** They are the newest subsystems, and the
> alignment API in particular is one long protocol rather than a set of independent
> endpoints — a session holds speakers, a run walks the state machine of
> `docs/mic-alignment-plan.md` §8, and the order matters more than any single body. That
> plan's §11 is the authoritative description; `bridge-daemon/src/api/` (one module per
> resource) has the exact shapes. Two rules from it that a caller cannot guess:
> `POST /api/align/start` takes the run's **whole** scope and each position is chosen with
> `/api/align/audible` (re-starting per position would cost two reconnect waves), and only
> `/api/align/still-here` postpones the idle teardown — an open socket, a frame on it and a
> status poll all deliberately count for nothing.
>
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

### `PUT /api/outputs/{node_name}/volume` / `mute`
One output's level, `0.0`–`1.0`, and its mute — **the same call for every kind**. The
daemon converts and dispatches: 0–100 in-band over the sendspin protocol, an RTSP
`SET_PARAMETER` to an AirPlay-2 receiver, the host's own cubic lever through its agent for
a PipeWire host.

```json
// PUT /api/outputs/ap2-dev-dusche/volume
{ "volume": 0.42 }
// PUT /api/outputs/sendspin-dev-kitchen/mute
{ "muted": true }
```

* an unknown node name is a **404**, and a kind with no such knob a **400** naming the
  kind. There used to be one endpoint per kind, and a name sent to the wrong one was
  *stored as an intent for a device that will never connect* and answered `200 {ok:true}`
  — so a click looked accepted and the next pushed frame put the old value back;
* sendspin and AP2 **store** the value and re-apply it when the device reconnects
  (`"saved … (device not connected)"`);
* a PipeWire host does **not**: it owns its level and reports it back, so a host with no
  live agent is a **503**, never a saved intent.

The current values are in `GET /api/outputs` and on the `outputs`/`matrix` topics.

### `PUT /api/outputs/{node_name}/delay`
The output's timing knob in ms (`{"delay_ms": 40}`; `null` or omitted puts it back on its
default). One path, and the **polarity and cost differ by kind** — which is why the
response says what happened and `GET /api/outputs` reports the polarity, rather than the
URL naming a mechanism:

- **sendspin** — a static **advance**: the device subtracts it from every timestamp, so a
  larger value plays *earlier*. Persisted, and it costs that one speaker a reconnect
  (tens of seconds of silence), because current firmware reads it at stream start. Its
  groupmates keep streaming.
- **AirPlay 2** — the render delay (default 0, up to 2000 ms), applied live to the running
  stream.
- **pw-sink** — the receiving host's jitter buffer (`sess.latency.msec`, default 100 ms =
  the PipeWire module's own). Clamped to a multiple of the 5 ms packet time, 15–2000 ms,
  and pushed to that host's agent, which reloads its receiver — a sub-second gap in that
  one target's audio. A disconnected host is not an error: the value applies when it
  reconnects.

`latency_ms` in a listing is the stored override (`null` = none) and
`latency_effective_ms` is what the output is actually running. The group-wide lead is a
separate knob: `PUT /api/sync/settings`.

### `POST /api/outputs/{node_name}/resync`
Ask one output to recover — for one that is reachable and being sent audio yet plays
nothing. One intent, and the daemon picks the mechanism its kind has:

- **sendspin** — `stream/clear`: discard buffered-but-unplayed audio and re-anchor
  *without* ending the stream. One frame, and deliberately per device: it does not reset
  the group's shared timeline, so the groupmates keep playing.
- **AirPlay 2** — release the session and build a fresh one, re-arming its PTP peer, while
  its groupmates keep streaming.
- **pw-sink** — a **400**: a host has no such lever of its own (its receiver reloads when
  its playout delay changes).

```json
// Response
{ "message": "cleared 'Kitchen' — it will re-anchor on the next audio" }
// 409 when there was nothing to act on:
{ "kind": "conflict", "message": "'Kitchen' has no live connection, so there is nothing to clear" }
```

The sendspin case is the recovery action for the 2026-08-03 failure where three of four
devices went silent while the daemon, the graph and the clock sync were all healthy (see
[sendspin-open-items.md](../pipewire_audio_router/docs/sendspin-open-items.md)); before it
existed the only lever was restarting the add-on, which interrupted every other output and
destroyed the evidence. The AP2 case is a lost PTP clock lock, which the daemon's liveness
watchdog also handles by itself — this endpoint is for the cases it cannot see, and for
not waiting.

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

### `DELETE /api/sources/{id}/clients/{key}`
Forget a remembered sender: drops its stored name/ban/priority. It reappears with
defaults the next time it connects.

### `PUT /api/sources/{id}/clients/{key}/ban`
`{"banned": true}` — refuse this sender. A live session is dropped immediately.

### `PUT /api/sources/{id}/clients/{key}/priority`
`{"priority": 5}` — how this sender is ranked against others competing for the same
source (see the source's anti-takeover policy below).

### `POST /api/sources/{id}/clients/{key}/disconnect`
Kick the sender's current session without banning it.

> **The key is in the path** — it used to be in the body of four `POST`s. `key` is a
> sender identity from `GET …/clients`; percent-encode it.

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

Live updates arrive as a **`now_playing` frame on `/api/events`** (see *Routing
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

### `GET /api/sendspin/delays`
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

## AirPlay-2 receivers

Volume, mute, render delay and resync are the ordinary per-output endpoints
([`volume`](#put-apioutputsnode_namevolume--mute), [`delay`](#put-apioutputsnode_namedelay),
[`resync`](#post-apioutputsnode_nameresync)); an AP2 receiver carries all of them in-band
over RTSP rather than as a PipeWire node volume. Current values are reported as
`ap2_volume` / `ap2_muted` by [`GET /api/outputs`](#get-apioutputs).

Two AP2-specific things are worth knowing about those shared endpoints:

> The daemon deliberately **does not impose** a volume on connect — an earlier version
> force-sent maximum volume when a session opened, which made a receiver's real level
> (e.g. −67 dB on a Pioneer) disagree with the UI slider after a restart.

> `resync` on an AP2 receiver releases its session and builds a fresh one, re-arming its
> PTP peer, while its groupmates keep streaming. On this hardware the fault it recovers is
> a **lost PTP clock lock**: our PT=87 anchors are timestamps in the grandmaster's
> timeline, so a receiver whose slaved clock has drifted off it plays nothing at all. A
> Pioneer VSX-934 does this repeatedly, and until this existed the only fixes were
> restarting the add-on or power-cycling the AVR — both of which work only because both
> build a new session. The daemon also does it **by itself**: the AP2 liveness task
> rebuilds the session of a receiver that had a lock, is still being streamed to, and has
> gone quiet for 30 s (at most one attempt every two minutes, and never for a receiver
> that has *never* locked — a Yamaha WX-021 never sends a `Delay_Req` and plays
> perfectly). The endpoint is for the cases it cannot see, and for not waiting.

### `PUT /api/outputs/{node_name}/ap2-rate`
The one knob that *is* AP2-only: the wire sample-rate mode, `auto` (negotiate 48 kHz, fall
back to 44.1 kHz) or `fixed_44100`. Restarts that receiver's group at the new rate.

## PipeWire receiver hosts (agents)

A `pwsink-dev-*` output is a **remote machine running `pwrouter-agent`**
(`outputs/pwsink/agent.rs`, [receiver-agent.md](../pipewire_audio_router/docs/receiver-agent.md)).
The pairing *decisions* are ordinary output operations — [`adopt`](#post-apioutputsnode_nameadopt)
pairs, [`unpair`](#post-apioutputsnode_nameunpair) revokes, `ignore` hides — because a host
asking to pair **is** a discovered output and a second vocabulary for it would buy nothing.
What is left here is the listing and the host's own master volume.

### `GET /api/agent/ws`
The socket each agent dials in on, authenticated with the bearer token minted when the
host was adopted. **Not a UI feed** — the browser has no reason to open it, and a second
connection for one identity replaces the first.

### `GET /api/agents`
Paired and pending hosts (`AgentInfo`), for diagnostics. The pairing UI does not need it:
a host waiting to pair appears as a `discovered` `pwsink` output with a `pwsink_pair_code`.

### The host's own volume and mute
[`PUT /api/outputs/{node_name}/volume`](#put-apioutputsnode_namevolume--mute) and `mute`,
like every other kind. What is specific to a host is the **failure mode**: there is no
stored intent, because the host owns the value and reports it back, so a host with no live
agent answers `503` (`"no agent connected for '<name>'"`) rather than saving it for later
— receiver-agent §9.4.

Its **playout delay** is [`PUT /api/outputs/{node_name}/delay`](#put-apioutputsnode_namedelay);
`resync` does not apply (a host has no such lever, and says so).

Two things read that level rather than setting it: `GET /api/outputs` reports it per host,
and speaker alignment borrows it for the duration of a run and puts the host's own value
back at teardown (`LevelChannel::OutOfBand`, plan §7).

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
{ "admission": "playing", "position": null, "reason": null,
  "message": "announce to 1 target(s): playing" }
```

Targets come from `targets`, or from a named group via `announcement_group` (an explicit
`targets`/`duck` in the request still wins). `duck` defaults to the daemon setting;
`on_busy` is `"queue"` (default) or `"reject"`; `barge_in` and `ttl_ms` are honoured by
the arbiter. `admission` is `playing`, `queued` (with `position`) or `rejected` — **on a
200 either way**: a rejection is the arbiter's decision, not a failed request, so it is a
value rather than a status. Only a malformed call (no audio source, an unknown group, every
target unusable) is a `400`.

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
{ "hold_id": 4, "ducked": ["sendspin-dev-kitchen"], "level": 0.25,
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
{ "message": "linked ... -> ..." }
```

Created natively via `Core::create_object` on the PipeWire thread
(`pw/thread.rs`) — the port names are resolved to object ids against the
live registry, then a create command is handed to that thread. Idempotent:
a link already present between the same ports is reported as success
(`ok: true`). Failure modes: either port name not found in the registry
→ 400; the PipeWire thread unreachable/dropped the request → 500.

### `GET /api/routing` / `POST /api/routing/link` / `POST /api/routing/unlink`
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

Take an output's state from the **routing matrix** ([`GET /api/routing`](#get-apirouting))
and set its level with the per-output
[`PUT /api/outputs/{node_name}/volume`](#put-apioutputsnode_namevolume--mute) — one call
for every kind — with current values from [`GET /api/outputs`](#get-apioutputs).

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

### `GET /api/events` — the one push socket

Every live feed the daemon has, on **one connection**, with topics chosen by message.

```json
// client → server
{ "op": "subscribe",   "topics": ["matrix", "now_playing"] }
{ "op": "unsubscribe", "topics": ["meters"] }
// server → client, in reply
{ "type": "subscribed", "topics": ["matrix", "now_playing"], "unknown": [] }
```

| Topic | Frame | What it carries |
|---|---|---|
| `matrix` | `{type:"matrix", sources, outputs, links}` | the routing matrix — **flat**, the fields sit beside `type` |
| `outputs` | `{type:"outputs", outputs}` | `GET /api/outputs`, pushed |
| `discovered` | `{type:"discovered", outputs}` | `GET /api/outputs/discovered`, pushed |
| `agents` | `{type:"agents", agents}` | `GET /api/agents`, pushed |
| `now_playing` | `{type:"now_playing", sources}` | per-source metadata, keyed by source node name |
| `meters` | `{type:"meters", nodes}` | peaks + xrun counts, 250 ms tick, keyed by node name |
| `align` | `{type:"align", state}` | the alignment session, including the frame that says it ended |
| `measure` | `{type:"measure", status}` | the measurement run |
| `equivalence` | `{type:"equivalence", status}` | the relay-vs-device experiment |

`"all"` (or `"*"`) as a topic name means every topic — for a diagnostic client.

Three rules a consumer can rely on:

* **subscribing sends that topic's current state at once**, so no separate initial fetch
  is needed. `meters` is the exception: it has nothing to say until its next tick;
* **frames are deduplicated per topic** — the daemon's change notifier fires for *any*
  change, so without this every topic would wake for every one. A quiet house sends
  nothing at all;
* **a node absent from a `meters` frame has nothing to report, i.e. zero.** That is how a
  level decaying to silence is expressed, so merge the frame wholesale rather than into
  what you had.

> **Why one socket.** A browser gives a page **six** connections per host over HTTP/1.1.
> There used to be four status sockets — routing, the alignment session, the measurement
> run, the equivalence experiment — and the alignment wizard alone held three of them
> while the routing graph held a fourth, so the REST calls those same pages make queued
> behind idle sockets that would not close until the user navigated away.
>
> Subscription is per topic rather than per URL for the same reason in reverse: a page
> that leaves **unsubscribes**, the daemon stops that work, and the connection stays for
> the next page. `meters` is the sharp end of that — subscribing to it is what arms
> per-source peak metering and the PipeWire profiler, and the last unsubscribe disarms
> them.

Not on this socket, deliberately: `GET /api/align/mic/ws` (binary microphone ingest,
client → server, one at a time, its own handshake) and `GET /api/agent/ws` (the
receiver-agent protocol with its own bearer auth). Neither is a status feed.

## `GET /` (and other non-API paths)
Serves the built web UI — a Vite + Svelte single-page app (source in
`pipewire_audio_router/frontend/`, served as static files from
`--static-dir`), styled to match Home Assistant with light/dark themes and
also surfaced in the HA sidebar via ingress. It's a full admin console: the
routing matrix (outputs as rows, sources as columns, live over
the `matrix` topic on `/api/events`, clickable link/unlink cells, per-output volume sliders
including sendspin devices, offline endpoints grayed with a forget button,
synchronized-group badges, a `ducked NN%` badge while an output's music is
attenuated for a voice turn, and a live input-level meter per source) plus
RAOP-output and AirPlay/RTP-source management and per-output diagnostic test
buttons (Play tone / Play announcement). Sendspin
devices are auto-discovered, so there's no manual sendspin management — just a
capabilities note. Volume sliders poll `/api/sendspin/volumes` (and read
`/api/outputs` for AirPlay-2 levels) every few seconds, since volume changes
aren't a registry event.
