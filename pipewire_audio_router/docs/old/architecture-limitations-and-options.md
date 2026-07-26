# Sync-group architecture: limitations & options

**Status:** design notes plus early implementation. The design (limits, options,
the MG/AG model) is captured here; what has actually been built and verified —
and what remains, gated on hardware or a decision — lives in
`spike-results-and-status.md`. As of the first implementation session: **S2 (the
`SharedTimeline` O-B gate) is done and proven**, the per-output-media-player
setting is shipped, and the **S8 announcement arbiter logic** is implemented and
tested; S1 acoustic sign-off, the S3 per-device audio path, the MG store, and the
HA entity migration are staged there. Read the "Current architecture" map first;
the limitations (L1…) reference it, and the options (O-A…) reference the
limitations.

Guiding preference (from discussion): **prefer generic, potentially upstreamable
mechanisms over special-cases.** Where two options solve the same limitation,
the one that generalizes (and that we could contribute back to sendspin-rs /
PipeWire) wins even if it's more work.

---

## Current architecture (the map)

Grouping is **emergent**, not declared: the set of outputs routed from the *same
source-set* becomes one group, backed by one `support.null-audio-sink` — the
**sync anchor** — which is the group's single clock/timeline.

- `bridge-daemon/src/sync_group.rs` — `GroupReconciler`. Keyed by source-set
  (`source_key`). Per group: creates the anchor (`SYNC_GRP_PREFIX` = `sync-grp-`),
  links the group's sources **into** the anchor, dials sendspin members with one
  filtered server, and monitor-links each RAOP member from the anchor. Also
  `force_server_restart` (drops a group's sendspin server so devices reconnect)
  and `snapshot`/`SharedGroups` (read by the align API).
- `bridge-daemon/src/sendspin_server.rs` — `start_server(server_name,
  display_name, port, sink_node_id, device_filter, send_ahead_us, control,
  devices)`. Attaches to the anchor sink, captures it, runs **one** embedded
  sendspin server pushing to **one** `Group` that dials exactly the member
  devices. Does *not* own the sink (the anchor's lifetime is independent).
- `bridge-daemon/src/sendspin_capture.rs` — a `pw::stream` (Input) on the
  anchor's monitor → `mpsc` PCM (48 kHz / S16 / 2ch) into the server.
- `bridge-daemon/vendor/sendspin/src/server/group.rs` — `Group`: the sync core.
  One shared timeline (`DEFAULT_SEND_AHEAD_US` = 250 000 µs lead; `next_ts_us`
  advances by each chunk's exact duration). `push_audio` stamps **one** timestamp
  and fans the **identical** encoded frame to **every** member's `ServerSender`.
  Per-member `send_player_command` (volume / mute / `SetStaticDelay`).
- `bridge-daemon/src/routing.rs` — `raop_uses_anchor` (the single predicate for
  "is this RAOP output anchored vs. direct"), `ensure_link_by_name`,
  `ensure_monitor_link_by_name`, `destroy_links_between`, `reconcile`.
- `bridge-daemon/src/discovery.rs` / `discovery_supervisor.rs` — RAOP mDNS
  discovery; `SharedDiscovered` (resolved ip/port/encryption per node name).
- `bridge-daemon/src/api.rs` — `announce` (ducks the nodes currently linked into
  the target, then `player::play_wav_to_target(node_id, wav)`); `list_media_players`
  and `list_outputs` classify outputs by node-name prefix.
- `bridge-daemon/src/sync_settings.rs` — group lead (`send_ahead`), per-sendspin
  static delay, per-RAOP `raop.latency.ms`.
- `bridge-daemon/src/calibrate.rs` — by-ear alignment: loops a test click into a
  group's anchor, mutes all but a reference + target, user drags the target's
  offset.

**Two device classes, asymmetric:**

| | RAOP / AirPlay | sendspin (ESPHome / Voice PE) |
|---|---|---|
| PipeWire node | **real** (`raop-out-*`, `libpipewire-module-raop-sink`) | **none** — virtual (`sendspin-dev-*`) |
| Audio path | fed from the anchor **monitor** (own graph node) | reached **only** via the group's one timestamped stream |
| Volume | node Props (`volume.rs`) | in-band player command (`sendspin_volume.rs`) |
| Multiroom sync | follows the anchor clock | one `Group`, identical timestamps to all members |
| Node-based APIs (announce, metering, matrix `node_id`) | work | **don't apply** (no node id) |

---

## Limitations

### L1 — No per-device audio path for sendspin devices
A sendspin device is reachable **only** through its group's single shared
stream. There is no way to send *different* audio (or a *different mix*) to one
member. Root cause: sendspin devices have no PipeWire node; the one channel to
them is `Group::push_audio`, which sends identical bytes to all members.

Consequences:
- **Notifications can't target one sendspin device.** `announce` addresses a
  `node_id`; sendspin devices aren't in `list_media_players` (it filters
  `raop-out-*` / `sendspin-out-*`, not `sendspin-dev-*`), so there's no target.
- Per-device overlay of any transient audio is impossible.

### L2 — Ducking is group-wide, never per-device
`announce` ducks the nodes feeding the target. In a group the target's feed is
the **shared anchor**, so ducking dips the music for **every** member, not just
the announced one. This is inherent to "duck the shared feed" and affects RAOP
too (all RAOP members read one monitor). You cannot duck one member's copy of
the music.

### L3 — Grouping is implicit (source-set), not explicit/named
Groups can't be defined beforehand, named, or given membership independent of
routing. A device can't belong to several groups (e.g. "announcements" +
"music"). Routing *two* sources to one output **mixes** them rather than
switching — there's no notion of exclusivity or priority. (See the separate
discussion notes; this doc treats explicit groups as O-D.)

### L4 — Notification is overlay-and-duck only, never takeover
The only notification model is "mix a clip on top + duck." There is no
"notification *replaces* the music on this device, then it rejoins." Any takeover
would require moving a device between anchors — costly (L5).

### L5 — Membership change restarts the sendspin server (switch latency)
A group's sendspin dial-filter is fixed at `start_server`, so changing which
devices a group dials means **drop + recreate the server** (see the
`sendspin_node_names != prev_devices` path and `force_server_restart`). The
devices reconnect and re-buffer → a few hundred ms up to ~1 s gap. This makes
dynamic membership / preemption expensive and is also why a live per-device
delay change needs a reconnect (current ESPHome firmware reads `SetStaticDelay`
at stream start, not live).

### L6 — RAOP sets a high absolute latency floor for a mixed group
`raop.latency.ms` defaults to **1500 ms**; a synced group is only as snappy as
its slowest member, so a group containing a RAOP receiver inherits that. Bad for
announcements. Tunable down per-output, but bounded by the receiver.

### L7 — Sendspin devices aren't first-class routable nodes
Everything sendspin is special-cased (volume, delay, discovery, grouping)
because there's no node. The matrix, metering, announce, and the HA integration
all have a "sendspin exception." This is the root asymmetry under L1/L2.

### L8 — No cross-group sync
Different anchors (different source-sets) are not synced to each other. Usually
fine, but it means two "groups" that happen to overlap in space can't be phase-
locked. (Low priority; documented for completeness.)

### L9 — No late-join catch-up
A device added to a running `Group` mid-stream gets `stream/start` + audio from
that point on (no buffer replay). Relevant to preemption/rejoin: a device
returning from a takeover re-syncs going forward with a brief gap. (Minor.)

---

## Options (solution building blocks)

Each option notes what it fixes, the sync implication, effort, and
upstream/genericity.

### O-A — Per-device mix in sendspin-rs *(rejected as primary)*
Extend `Group::push_audio` so a member with an active overlay gets a
custom-encoded frame (`duck(music) + overlay`) at the **same** timestamp;
everyone else gets the shared frame. A daemon path feeds decoded announce PCM
(resampled to 48k/S16/2) to the server tagged for that device.

- **Fixes:** L1, L2, L4 (overlay form) for sendspin — cheaply and with sync
  preserved for free (rides the existing shared timeline; no restart).
- **Sync:** perfect (identical timestamp, per-member bytes).
- **Effort:** ~100–200 lines in vendored `Group` + a daemon feed path.
- **Downside / why not primary:** it's a **special-case** ("server-side per-
  member mixing") we don't expect to land upstream, and it doesn't generalize to
  independent routing (L3/L7). Keep documented as a fast fallback if O-B stalls.

### O-B — A real PipeWire node per sendspin device *(preferred direction)*
Give every sendspin device its own node so it's a first-class output, symmetric
with RAOP. **In-process Rust** (per-device `null-audio-sink` + per-device capture
stream + per-device single-member sendspin sender), **not** a C module (that's
O-C). Routing a source to device A = link `source → sendspin-dev-A` sink; its
sender streams that.

- **Fixes:** L1, L2, L4, L7 generically, and unblocks L3 (per-device nodes make
  "device in multiple groups, arbitrated" natural). Notifications reuse the
  existing node path (`play_wav_to_target` + node-volume duck) per device; per-
  device duck becomes possible.
- **Sync (the catch):** N independent per-device senders each anchor their **own**
  timeline → they drift unless they **share one timeline**. Requires a shared-
  timeline coordinator (see "The hard problem" below). This turns "sync is
  automatic" into "sync is coordinated" — the main risk and the bulk of the work.
- **Effort:** significant. Per-device node/stream/sender plumbing in the daemon +
  a sendspin-rs refactor to separate *timeline* from *membership*.
- **Upstream:** the sendspin-rs change (a shareable timeline / phase-locked
  senders) is a **generic** capability, plausibly upstreamable — unlike O-A.

### O-C — A C `module-sendspin-sink` PipeWire module *(rejected)*
Same "node per device" outcome as O-B but as a loadable `.so`. Means
reimplementing the sendspin protocol in C (or FFI), a separate build, and
discarding sendspin-rs — for no benefit here, since the daemon already creates
nodes in its own context in Rust. Only worth it if a standalone, distro-shipped
module were a goal in itself.

### O-D — Explicit named groups + priority arbitration *(front-end model; pairs with O-B)*
Replace "group = source-set" with defined groups (id + member set + priority),
route sources → groups, and arbitrate **per contested device**: a device claimed
by several *active* groups joins the highest-priority one's anchor; uncontested
members of the other groups keep playing. "Active" comes from a source-activity
signal (the metering peak hub already exists) with hysteresis to avoid flapping.

- **Fixes:** L3, L4 (takeover form).
- **Depends on:** cheap per-device attach/detach — which is painful today (L5)
  and clean with O-B. Can be prototyped on the current model but really wants
  O-B + O-F.
- **Note:** keep per-device arbitration (not one global winner) so an
  announcement seizes only the speakers it shares with music.
- **Refined by** the two-tier MG/AG model below (this is O-D's concrete
  front-end). With *exclusive* music groups the per-device arbitration
  disappears for **music** — each output has exactly one anchor — and priority
  arbitration survives only for **announcements** (AG priority-preempt).

### O-E — Per-RAOP-output volume node (per-device RAOP duck)
Insert a small per-device volume/loopback node between the anchor monitor and a
`raop-sink` so that one RAOP member can be ducked without touching the others.

- **Fixes:** L2 for RAOP (sendspin gets it via O-A or O-B).
- **Cost:** one extra node + ~one quantum latency per RAOP output that needs it.
- Optional / on-demand (only insert when a per-device duck is requested).

### O-F — Warm anchor / fast preemption
Pre-create (or keep alive) the announcement path's anchor + sender so a takeover
doesn't pay the cold `start_server` cost (L5). Reduces switch latency for the
latency-sensitive announcement case.

- **Fixes:** the transition cost of L4/L5.
- **Pairs with:** O-D (preemption) and O-B (per-device attach).

---

## Explicit two-tier grouping model (MG / AG)

**Decision (this session):** O-D's explicit-groups front-end is refined into two
distinct, *named* group types with different guarantees — this is the target UX and
data model. O-B stays the strategic **mechanism** target, and announcements ship
**after** O-B (do the spikes first), not before.

### The two types

**Music groups (MG) — sync-hard, membership-static, exclusive.**
- A named set of outputs playing the *same* stream in tight multiroom sync.
- **Exclusive:** an output belongs to at most one MG; MGs partition the outputs.
- Maps directly onto today's emergent anchor group (`sync_group.rs`): one MG = one
  `support.null-audio-sink` anchor + one shared timeline. It just adds a **name** and
  an **explicit, exclusive membership set** to what routing already forms implicitly.
- Because membership is exclusive, **O-D's per-contested-device arbitration
  disappears for music** — each output has exactly one music anchor. No priority, no
  activity signal, no hysteresis in the music tier.
- Membership is user-driven and infrequent, so the sendspin **L5** restart cost on
  regrouping is acceptable here (paid when a human regroups zones, not per stream).

**Announcement groups (AG) — sync-none, membership-overlapping, priority-preempt.**
- A named set of target outputs an announcement plays to, **independent of MG
  membership** (an AG may span several MGs and idle outputs).
- **No inter-output sync requirement** — each target gets the announcement on its own
  path; no cross-output timeline coordination.
- **Per-output duck only:** an announcement ducks *its target outputs'* music, never
  the whole MG. Announcing to Kitchen (in MG1 with Bathroom) dips Kitchen alone;
  Bathroom keeps full music. This is exactly **L2** (per-device duck) — which is why
  AG is the concrete driver for per-output audio paths.
- **Atomic clips, queue-by-default (decided; implemented in `announce_arbiter.rs`).**
  Announcements are whole clips the daemon holds in full (rendered/fetched WAV), so
  they always play from the start or not at all — **never "resume remaining parts."**
  Per-announcement contract: `{ priority, on_busy: Queue|Reject, barge_in: bool,
  ttl_ms }`.
  - **Busy → queue** by default: if the target outputs are busy, the clip is held and
    played when they free, ordered by **priority then arrival** (doorbell → timer →
    reminder). `on_busy = Reject` instead returns a *busy* admission so HA can
    reschedule (the router doesn't hold it).
  - **`barge_in`** (default off): a normal announcement queue-jumps (waits for the
    current clip to finish); a `barge_in` one preempts immediately and the preempted
    clip is **re-queued whole** (replayed from the start), or dropped if past its TTL.
  - **`ttl_ms`**: `None` = wait forever, `Some(0)` = play-now-or-drop, `Some(ms)` =
    drop (don't play late) if it can't start in time.
  - **Concurrency:** disjoint-target announcements play at once; a multi-output one
    waits until *all* its targets are free, then plays on all. Ducking is
    occupancy-refcounted per output. Every admission returns `Playing | Queued{pos} |
    Rejected(reason)` for HA visibility.
- Named + persistent, symmetric with MG, reusable from HA automations.

### Data model & invariants
- `MusicGroup { id, name, members: Set<Output> }`, invariant
  `∀ output: |{ MG : output ∈ MG }| ≤ 1` (validate on edit).
- `AnnouncementGroup { id, name, targets: Set<Output>, priority, duck_db }`, no
  exclusivity; overlaps MGs and other AGs freely.
- An AG is mechanically just a *reusable named target set + delivery policy*; the
  delivery mechanism is per-output regardless of the grouping.
- **Routing target is polymorphic (decided).** A routing-matrix row points sources at
  a **target id** that resolves to one of `{ individual output, music group, …future… }`.
  The default model is "an MG replaces a routing row" (you route sources to the group),
  but the matrix, links, and reconciler all speak *target id* so other groupings remain
  possible without reworking routing. Outputs that belong to an MG are not individually
  assignable (the MG is the unit); whether a per-output entity — when the
  `expose_outputs_as_media_players` toggle is on — may take an independent input is left
  open, to decide after using it.

### Exclusivity is policy, not physics
With O-B (per-device nodes) as the mechanism, MG exclusivity is a **rule enforced in
the group model**, not a constraint forced by the audio path. Per-device nodes *could*
support a speaker breaking out with its own independent stream later; the MG rules
simply forbid it today. Keeps the model simple now and the door open later without
repainting the path.

### AG delivery, two sub-cases per target output (post-O-B)
1. **Target currently in an active MG** → duck *its* copy only and overlay:
   - sendspin: its own per-device node (O-B) → node-volume duck + `play_wav_to_target`.
   - RAOP: **O-E** per-output volume/loopback node between the anchor monitor and the
     `raop-sink`. RAOP never becomes a sendspin node, so **O-E survives into the O-B
     world** — it is the one per-output-duck mechanism O-B does *not* subsume.

   The output's music stays synced in its MG throughout; only its copy is ducked.
2. **Target idle / not in an MG** → RAOP: `play_wav_to_target` on its node (works
   today). Idle sendspin: needs a sender dialing it — cold `start_server` (L5) or a
   warm announcement path (**O-F**).

### Why "after O-B" (sequencing decision)
Announcements are sync-free, so they do **not** need O-B's shared-timeline coordinator
— they *could* ship earlier via **O-A** (sendspin per-member overlay inside the MG
`Group`) + O-E. We chose to **ship AG after O-B anyway**: run the make-or-break spikes
(S1/S2) first, then per-device nodes, so sendspin announcements reuse the node path and
**no O-A is written** (it would be thrown away once per-device nodes exist). O-A stays
documented only as the emergency fallback if the O-B shared-timeline spike (S1)
disappoints. Trade-off accepted: announcements land later, after O-B, rather than as an
O-A/O-E interim.

### Caveats carried into this model
- **L6** still bites AG on RAOP targets: a receiver's `raop.latency.ms` floor
  (~1500 ms default) delays a doorbell-type announcement to an AirPlay speaker.
  sendspin/Voice PE is snappy. Tunable down, bounded by the receiver.
- **Unsynced overlay across adjacent rooms:** the same announcement on Kitchen +
  Bathroom can comb-filter if the rooms leak into each other. Accepted (AG is
  sync-none by design); noted as the one place "no sync" is perceptible.

### HA integration surface & the per-output toggle

**Target entity model.** The custom component creates one `media_player` **per MG and
per AG by default** — *not* per output. A toggle in the addon UI, **"expose all outputs
as individual media players"** (default **off**), *additionally* creates one
`media_player` **per output** when enabled.

This **inverts today's model**, where entities are created one-per-routing-matrix-output
(`media_player.py` `_output_node_names()` → `coordinator.routing.outputs`, driven by
`GET /api/routing`(`/ws`); `unique_id = f"{entry_id}_{node_name}"`; reconciled by
`_reconcile_entities`; `cleanup_entities` service). Under the two-tier model:

- **MG media_player** = the music control surface for the zone: transport + `source`
  select (which routed source the group plays) + group volume; its scope is the MG's
  outputs. This is what a user targets to "play music in the kitchen zone."
- **AG media_player** = an announcement target: `play_media` (TTS / clip) fans the
  announcement to the AG's targets with per-output duck + priority (see AG semantics
  above). *Not* a music source — no `source` select; volume = announcement level; state
  reflects announcing/idle.
- **Per-output media_player** (toggle on) = the existing single-output entity, for
  directly addressing one speaker (volume / announce) regardless of its MG.

**Where the toggle lives (grounded in current plumbing).** The addon has **no HA
options schema** — all runtime config is the daemon's `/api/settings`, persisted under
`/data`. So "addon UI" here = the daemon's **Svelte web UI**, and the toggle is a new
runtime setting mirroring `sendspin_delay_live`:
- `bridge-daemon/src/settings_store.rs` — add `expose_outputs_as_media_players: bool`
  to `Settings` (`#[serde(default)]`, persisted).
- `bridge-daemon/src/api.rs` — add it to `SettingsInfo` + `SetSettingsRequest` +
  `settings_info()` (`GET`/`PUT /api/settings`).
- `frontend/src/components/SettingsTab.svelte` — a checkbox card following the
  `toggleDelayLive` pattern; TS shapes in `frontend/src/lib/{types.ts,api.ts}`.

**Custom-component changes this implies.**
- A new daemon API to enumerate **named** MGs and AGs (none exists today — groups are
  emergent and unnamed; only `/api/align/groups` exposes ad-hoc source-sets). The
  integration builds its desired-entity set from that, not from `/api/routing` outputs.
- The integration must **read `/api/settings`** (it does not today) to know whether the
  per-output entities should exist, and re-reconcile when the toggle flips (live via the
  settings push, or the 5 s poll).
- `_reconcile_entities` desired-set becomes `MGs ∪ AGs ∪ (outputs if toggle)`. The
  `unique_id` scheme must namespace the three kinds (e.g. `{entry}_mg_{id}` /
  `{entry}_ag_{id}` / `{entry}_out_{node_name}`) so toggling per-output on/off can't
  collide with or orphan group entities; `cleanup_entities` follows the new scheme.

**Open design questions (defaults noted; confirm at build time):**
- **AG entity capabilities:** announce/TTS + volume only (default), or also usable as a
  music target routing to all members? Default: announcement-only — music is the MG's
  job.
- **Group volume model:** does an MG entity's volume act as a group master (scaling each
  member), with per-output entities still exposing the raw per-output volume underneath?
  Default: MG volume = master; per-output entities show/scale the underlying volume.
- **Overlap UX:** with both MG and per-output entities present, two entities can control
  the same speaker's volume (layered as master × per-output). Acceptable, but the HA UI
  should make the relationship legible.

---

## The hard problem: a shared timeline for per-device senders (O-B)

Today sync is trivially correct because one `Group` sends **one** timestamp to
all members. Split senders per device and each would call `clock.now_micros()`
at its own first push → anchor at a different instant → devices offset.

What must be preserved (from `group.rs`):
- One **clock domain** shared by all senders (the same `DefaultClock` instance,
  or an equivalent monotonic wall clock all members' clock-sync converges to).
- One **timeline**: first push anchors at `now + send_ahead_us`; every chunk
  advances `next_ts_us` by its exact duration (with the sub-µs `residue` carry).
- Every member's chunk-N carries the **same** `ts`.

Refactor sketch (vendored sendspin-rs):
- Extract a `SharedTimeline { clock, send_ahead_us, next_ts_us, residue }` with a
  `stamp(pcm_len, format) -> ts` method (the current `push_audio` timestamp
  logic, minus the fan-out).
- A per-device sender (a single-member `Group`, or a thinner `MemberSender`)
  takes an `Arc<Mutex<SharedTimeline>>` and stamps from it, so device A's chunk-N
  and device B's chunk-N get identical `ts` regardless of callback ordering.
- The daemon drives all per-device senders for a sync group from **one** PCM
  source (the group anchor's monitor) so they see identical samples; the shared
  timeline guarantees identical timestamps. Result: same sync guarantee as today,
  but each device is independently addressable.

This is the piece to **spike first** (S2/S1) — if per-device senders on a shared
timeline don't hold sync on real hardware, O-B's premise fails and O-A becomes
the pragmatic choice.

---

## Recommended direction

1. **Spike the shared timeline first (S1/S2)** — the gate for everything below. If
   per-device senders can't hold sync on a shared timeline, O-B's premise fails and
   the fallback path (O-A) takes over.
2. **O-B** (per-device nodes, in-process Rust) as the strategic **mechanism** target
   — it lifts L1/L2/L4/L7 generically and unblocks L3, and its sendspin-rs change is
   the upstreamable one.
3. **MG** (explicit exclusive named music groups) — the O-D *music* tier. Buildable
   on the current anchor model (naming + exclusivity + membership); exclusivity drops
   the music arbitration entirely. Custom component exposes one media_player per MG.
4. **AG** (named priority-preempt announcements) — **after O-B**: per-device node
   duck for sendspin + **O-E** for RAOP; per-output priority stack (drop-on-preempt).
   One media_player per AG; a daemon-settings toggle optionally also exposes one
   media_player per output (default off).
5. **O-F** warm anchor to keep idle-output announcements / preemption snappy.
6. Keep **O-A** in the back pocket as the low-risk fallback if step 1's shared-
   timeline spike disappoints — it solves the notification/duck pain without per-device
   nodes. We do **not** write O-A on the happy path (AG-after-O-B), since per-device
   nodes make it throwaway.

Sequence so each step de-risks the next: spike the shared timeline → per-device nodes
for one device → sync check with 2+ devices → explicit exclusive music groups (MG) →
node-based announcement/duck + O-E (AG) → warm anchor.

---

## Spikes (concrete, for a fresh session)

Run on the add-on host (see `docs/rtp-source-to-raop-routing.md` for the ssh /
`XDG_RUNTIME_DIR` / `pw-top` / `pw-dump` notes; `curl`/`python3` absent in the
container). Real hardware needed: ≥2 sendspin devices (Voice PE / ESPHome) and
≥1 RAOP receiver.

- **S1 — Sync of per-device senders (the make-or-break).** Stand up two single-
  member sendspin `Group`s driven from one PCM source, sharing one clock +
  timeline (hand-wire the shared `next_ts` if needed). Play the calibrate click
  and confirm the two devices stay coincident as well as today's single-Group
  path. *Gate for O-B.*
- **S2 — sendspin-rs `SharedTimeline` refactor.** Extract timeline from `Group`
  (see sketch above); prove two `MemberSender`s stamp identical `ts`. Keep it
  shaped for an upstream PR.
- **S3 — One per-device node end-to-end.** Per-device `null-audio-sink` +
  capture + single-member sender for one device; route a source to it; confirm
  audio + it appears as a routable node.
- **S4 — Node-based notification + per-device duck.** With S3's node, use
  `play_wav_to_target` on that device's node and duck **only** that device;
  confirm the group's other members are unaffected and the device stays synced.
- **S5 — Scale check.** N per-device senders (N = 5–8): CPU, websocket/stream
  count, join/leave latency. Compare to one-server-per-group.
- **S6 — RAOP per-device volume node (O-E).** Insert a volume/loopback between
  the anchor monitor and one `raop-sink`; measure added latency; duck one RAOP
  member alone.
- **S7 — Warm anchor (O-F).** Measure cold `start_server` switch latency vs. a
  pre-warmed announcement anchor.
- **S8 — Two-tier groups (MG/AG) prototype.** Data model per "Explicit two-tier
  grouping model" above: `MusicGroup` (id, name, exclusive members) and
  `AnnouncementGroup` (id, name, targets, priority, duck_db). MG `compute_desired`
  keyed by group id (no music arbiter — exclusivity guarantees one anchor per output);
  AG per-output priority stack with drop-on-preempt. MG can start on the current model;
  AG delivery lands after O-B (node-path duck + O-E for RAOP).

---

## Open questions

- **Clock sharing:** is `DefaultClock` a process-wide monotonic wall clock safe
  to share across senders, or does each session assume its own? (Drives S2.)
- **Scale:** realistic device counts — does per-device (1 capture stream + 1 ws
  session per device) stay cheap vs. per-group? (S5.)
- **ESPHome firmware:** `SetStaticDelay` is read at stream start only — per-device
  nodes don't change that; live delay still needs a reconnect. Any firmware path
  to live delay? (Affects calibrate UX regardless of O-B.)
- **Mixed-group offset:** with sendspin on per-device senders and RAOP on the
  anchor monitor, the sendspin `send_ahead` vs. RAOP `raop.latency.ms` alignment
  is still the knob — confirm the shared timeline doesn't change the offset math.
- **Routing model migration:** O-D changes the UX from a source×output matrix to
  groups + routing. How do existing persisted routes (`routing.json`) migrate?
- **Upstreamability of O-B's sendspin-rs change:** design the `SharedTimeline` /
  phase-locked-senders API as a clean, general addition (not a hook for our
  daemon specifically) so it's PR-able.

---

## Related docs
- `docs/spike-results-and-status.md` — what's built & verified vs. staged, spike
  outcomes (S2 done/proven), resolved open questions, and decisions still needed.
- `docs/rtp-source-to-raop-routing.md` — why non-driver sources need the anchor,
  latency budget, and the host diagnostic recipes.
