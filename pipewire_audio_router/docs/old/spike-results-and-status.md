# Two-tier grouping (MG/AG) — spike results & implementation status

**Companion to** `architecture-limitations-and-options.md` (the L#/O# labels and
the MG/AG model come from there). This doc records what was actually built and
verified in the implementation session, what the spikes concluded, and what
remains — split into *done*, *staged (needs hardware/decision)*, and the exact
next steps.

Guiding constraint for the session: do the software-provable, low-risk work and
verify it; do **not** deploy a change to the live audio path that can't be
verified without physical listening, and don't ship the breaking HA entity
migration before the group-creation UI exists. Those are called out below.

---

## Status at a glance

| Piece | Spike | State | Verified by |
|---|---|---|---|
| `SharedTimeline` extraction + per-device stamping | **S2** | **Done** | `cargo test` (unit + `tests/group_sync.rs`) |
| Two independent senders stamp identical `ts` | **S1/S2** | **Proven in software** | `separate_groups_sharing_a_timeline_stamp_identically` |
| Clock-domain sharing is safe | open-q | **Resolved** | `raw_clock.rs` reads `CLOCK_MONOTONIC_RAW`, stateless |
| `expose_outputs_as_media_players` setting | — | **Done** | daemon unit tests + `svelte-check` |
| AG announcement scheduler (queue/barge/TTL) | **S8** | **Done (logic)** | `announce_arbiter.rs` — 10 unit tests |
| Per-device senders in reconciler (flag) | O-B | **Done on hardware** | `per_device_sendspin_senders`; group restarts into mode |
| Per-device duck/overlay mixing | O-A/O-B | **Done on hardware** | `overlay_mixer.rs` (7 tests); duck+tone isolated per speaker, both directions |
| Scheduler-driven per-device announce (`/api/announce`) | AG | **Done on hardware** | real clip ducks+plays+auto-completes; groupmate untouched; queue verified |
| MG/AG data model + CRUD API + announce-by-group | O-D | **Done (API-validated)** | `groups_store.rs` (5 tests); exclusivity enforced; `/api/groups/*`; `/api/announce {announcement_group}` |
| Per-device PipeWire node end-to-end | **S3** | **Done on hardware** | deployed; node + capture + device-connect confirmed; audible |
| Per-device audio quality (dropouts) | **S3** | **Gap found (null-sink path)** | ~1 dropout/60s vs the anchor path — see below |
| Multi-device shared-timeline driver | **S1** | **Done on hardware** | `start_server_per_device`; deployed |
| Acoustic coincidence of 2+ per-device senders | **S1** | **CONFIRMED** | operator: 2 Voice PEs on one shared timeline play in-sync |
| Named MG store vs routing matrix | **S8/O-D** | **Staged (decision)** | routing-migration open question |
| HA entity model: per MG/AG (+ per-output) | — | **Staged (breaking UX)** | needs group-creation UI first |

Hardware available and confirmed this session: 3 Voice PE sendspin devices
(`home_assistant_voice_093ca8/096287/0966f3`) + 2 RAOP receivers
(`raop-out-dusche`, `raop-out-pioneer_vsx_934_f11b89`); daemon reachable at
`http://homeassistant.local:8099`.

---

## Done & verified

### S2 — `SharedTimeline` (the O-B gate)

The make-or-break question for O-B was: *if we split playback into one
independently-addressable sender per device, do they still stay
sample-accurately coincident?* The doc's answer is "only if they share one
timeline." That timeline now exists as a first-class type.

- **New:** `vendor/sendspin/src/server/timeline.rs` — `SharedTimeline` owns the
  clock domain, the send-ahead lead, and the anchored `next_ts`/`residue`
  advancement (the exact logic previously inline in `Group::push_audio`), exposed
  as `stamp(pcm_len) -> ts`, plus `set_config`/`config`/`clear_config`/`reset`.
- **Refactor:** `Group` now sits on an `Arc<SharedTimeline>` instead of owning the
  timeline fields. Behaviour is preserved (`Group::new` still creates its own
  timeline; all public methods unchanged). Added:
  - `Group::with_timeline(Arc<SharedTimeline>)` — several groups share one timeline.
  - `Group::timeline()` — hand the timeline to a parallel sender.
  - `Group::push_encoded(ts, pcm)` — fan a pre-stamped frame without advancing the
    timeline (the shared-timeline path: stamp once, deliver that `ts` to each
    group/sender).
- **Exported** `SharedTimeline` from `server/mod.rs`.
- **Also fixed** the vendored `tests/group_sync.rs` (it was stale: it `.await`-ed
  the now-sync `push_audio` and called `.collect()` on the now-`Vec` `member_ids`).
  The vendored `tests/`/`examples/` never compiled in the daemon build (only the
  lib does), so this had gone unnoticed. `examples/play_wav.rs` is still stale
  (missing the `fullname` field) — pre-existing, out of scope here.

**The proof (S1's premise, in software).** `tests/group_sync.rs::
separate_groups_sharing_a_timeline_stamp_identically` stands up **two separate
`Group`s** (each one member — i.e. two independent per-device senders) sharing one
`Arc<SharedTimeline>`, stamps the timeline **once**, and delivers that `ts` to
each via `push_encoded`. It asserts both members' frames carry the **identical
timestamp**, while also carrying **different payloads** — the per-device
overlay/duck primitive. This is the same correctness argument as today's
single-`Group` path (one `ts` → all members), now with per-device addressing.

Why this de-risks S1: driving all of a group's per-device senders from **one** PCM
source (the group anchor's monitor) and stamping **once** per chunk makes the
bytes and the timestamp byte-identical across senders by construction — there is
no remaining mechanism for them to diverge except each device's own clock-sync,
which is exactly what keeps today's single group coincident. The remaining S1 work
is confirmatory (see Staged).

**Open question resolved — clock sharing.** `sync/raw_clock.rs::DefaultClock`
reads `CLOCK_MONOTONIC_RAW` directly and holds no per-instance state on Linux (its
`epoch` field is unused there), so two `DefaultClock` instances already share one
process-wide timebase. Passing the *same* `Arc<dyn Clock>` (as `SharedTimeline`
does) is the portable guarantee. Either way, clock sharing across senders is safe;
this was flagged as the S2/open-questions risk and is now closed.

### `expose_outputs_as_media_players` setting (the addon-UI toggle)

End-to-end, additive, inert until the HA integration consumes it (safe to ship):

- `bridge-daemon/src/settings_store.rs` — new `expose_outputs_as_media_players:
  bool` (`serde(default)` = false) + getter/setter, mirroring `sendspin_delay_live`;
  round-trip covered by the store's unit tests.
- `bridge-daemon/src/api.rs` — field added to `SettingsInfo`, `SetSettingsRequest`,
  `settings_info()`, and the `set_settings` PATCH handler (`GET`/`PUT /api/settings`).
- `frontend/src/lib/types.ts` — field on `AppSettings` (`AppSettingsUpdate` is
  `Partial`, so `api.ts` needed no change).
- `frontend/src/components/SettingsTab.svelte` — a "Home Assistant entities" card
  with the toggle, following the `toggleDelayLive` pattern. `svelte-check`: 0 errors.

### S8 — AG priority-preempt arbiter (logic)

`bridge-daemon/src/announce_arbiter.rs` — the pure per-output decision logic for
**overlay + duck with priority preempt, drop-on-preempt**, with a group-level
coordinator (`AnnounceScheduler`). No PipeWire/sendspin dependency; **10 unit
tests**. Emits `Action`s (`DuckMusic`/`UnduckMusic`/`Start`/`StopAnnouncement` per
output) for the audio path to carry out; not yet wired (marked `#![allow(dead_code)]`).

**Concurrency model (decided — see the architecture doc's AG section):** announcements
are atomic clips; per-announcement `{ priority, on_busy: Queue|Reject, barge_in,
ttl_ms }`. Busy → **queue by default** (priority then arrival), `Reject` opt-in returns
a busy admission; `barge_in` preempts and **re-queues the preempted clip whole** (never
partial); `ttl_ms` drops stale ones (`0` = play-now-or-drop). Disjoint targets play
concurrently; a multi-output announcement waits for all its targets. Every admission
returns `Playing | Queued{pos} | Rejected(reason)`. The earlier preempt+drop prototype
was replaced by this queue model.

---

## Staged — needs hardware, a decision, or a large audio-path build

### S1 — acoustic coincidence on real hardware *(needs ears / mic)*
The timestamp invariant is proven in software (S2). What software can't decide is
whether two Voice PE devices *sound* coincident once the per-device-sender path is
actually driving them. This needs either physical listening (the `calibrate.rs`
click, as the doc's S1 describes) or a measurement mic. **Action for a human:** once
S3 lands on-device, run the click into a 2-device group and confirm coincidence vs.
today's single-`Group` path. Nothing else here can substitute for that.

### S3 — one per-device PipeWire node end-to-end — **DONE (validated on hardware)**
Built as an inert-by-default spike (`per_device_spike.rs`, `POST`/`DELETE
/api/spike/per-device`) that gives one device its own `support.null-audio-sink` +
capture + single-member sendspin sender, links a source into it, and frees the
device from production routing (reversibly) so the single-member server wins the
dial. Deployed to the live add-on (v0.2.0.0047) and run against Voice PE
`…0966f3`, fed from `airplay-in`:

- **Structural (logs/`pw-link`):** the `perdev-…0966f3` node appears in the graph
  (a real, routable node), its own capture attaches (`capture connected to node
  94`), the source links into it, and the device connects to the single-member
  sender (`sendspin device connected: …0966f3`). ✅
- **Audible (operator):** audio plays on that device, isolated — no other speaker
  disturbed. ✅ **This confirms the O-B per-device-node mechanism end-to-end.**
- **Quality gap found:** ~1 dropout / 60 s on the per-device path, vs. effectively
  none on the shared-anchor path to a sibling device (`…093ca8`). `pw-top` shows
  the production anchor `sync-grp-…` driving at a steady **QUANT 1024**, while the
  per-device `null-audio-sink` shows **QUANT 0** — it isn't establishing itself as
  a steady-clock driver, and no xruns are recorded on its capture. Hypothesis: the
  per-device null-sink isn't clocked as stably as the anchor, so PCM reaches the
  sender in slightly irregular bursts → occasional device-side underrun.

**Design implication for productionization.** This is exactly why the doc's O-B
sketch says to drive a *sync group's* per-device senders from **one** PCM source
(the group anchor's monitor), not from a separate null-sink per device: the anchor
is the steady driver. So:
- **Music-group members** → keep them fed from the shared anchor monitor (steady
  QUANT), and split only the *sender* per device (share the `SharedTimeline`, use
  `Group::with_timeline` + `push_encoded`). This should avoid the dropouts.
- **Independent per-device routing** (a device on its own source) genuinely needs a
  per-device sink; that path must fix the null-sink clocking (force a driver/quantum
  on the per-device sink, or interpose a steady-clock node) before it's dropout-free.

Primitives in place: `SharedTimeline`, `Group::with_timeline`, `push_encoded`, and
the validated per-device sink/capture/sender wiring in `per_device_spike.rs`.

### S1 — multi-device sync on a shared timeline — **CONFIRMED on hardware**
`sendspin_server::start_server_per_device` + `per_device_spike::start_multi` +
`POST/DELETE /api/spike/multi-device`: **one** anchor sink + **one** capture (the
single steady PCM source) + **one** `SharedTimeline`, driving **one single-member
`Group` per device**, with the capture loop stamping **once** per chunk and
`push_encoded`-ing that identical `ts` to every device. Deployed (v0.2.0.0151) and
run on Voice PE `…0966f3` + `…096287` fed from `airplay-in`.

- **Sync (the make-or-break):** operator confirmed the **two devices play
  in-sync** — no echo/phasing. Independent per-device senders on a shared timeline
  stay coincident on real hardware. **This closes O-B's central risk** ("The hard
  problem"): together with S2 (identical timestamps in software) the shared-timeline
  approach is validated end-to-end.
- **Dropouts were present but common-mode and traced to the source, not the path.**
  Both spike devices dropped *together*, and so did the production-path device
  `…093ca8` playing the same `airplay-in`. `pw-top` showed **all** xruns on
  `airplay-in` (ERR ~452, WAIT ~2.3ms) and **ERR 0** on every downstream node
  (anchor, per-device sink, both captures). Root cause: the AirPlay *receive*
  stream (a Linux PipeWire sender, flaky since a receiver rename) underrunning — it
  propagates identically to every consumer. Not the per-device split, not the
  shared timeline. (Caveat: a *device-side* WS/network drop wouldn't register as a
  PipeWire xrun, so it isn't 100% excluded — but the evidence squarely implicates
  the source.)
- **Contrast with S3:** S3's ~1/60s dropout was on a *per-device null-sink* (QUANT 0,
  not a steady driver). This S1 path feeds from one steady anchor capture (QUANT
  1024-class) and showed **no** downstream xruns — confirming the "feed a sync
  group from one anchor monitor, split only the sender" shape is the right one.

**Fragility found (for productionization):** the spike links its source into the
anchor with a *non-persisted* `ensure_link_by_name`. Renaming the AirPlay receiver
recreated the `airplay-in` node (new id) and the reconciler did **not** restore
that link (it's not a stored route), silencing the anchor until the spike was
re-run. → Per-device/anchor routing links must be **persisted and reconciled** like
normal routes so a source-node recreate re-links them automatically.

Remaining before productionizing per-device nodes: fix the standalone per-device
null-sink clocking (for the independent-routing case, S3), persist per-device
links, then wire per-device senders into `sync_group.rs` behind a flag (music-group
members fed from the shared anchor monitor per S1).

### Named MG store vs. the existing routing matrix *(decision first)*
Building a `MusicGroup { id, name, members }` store was deliberately **not** done
yet: how explicit MGs relate to the existing source×output routing matrix
(`routing.rs`/`routing_store.rs`) and how persisted `routing.json` migrates is an
open question in the architecture doc. Implementing an MG store disconnected from
routing risks building the wrong thing. **Decision needed:** does an MG *replace* a
routing row (group = the unit you route sources to), or *layer on top* of the
current matrix? The AG store is cleaner (announcements are additive and don't touch
routing) but its delivery still depends on the S3 audio path.

### HA integration entity model *(breaking UX; needs group UI first)*
Switching the custom component from "one media_player per output" to "one per MG +
per AG (+ per output when the toggle is on)" is a breaking change: with no MGs/AGs
defined yet, it would delete every current per-output player. It must land **after**
there's a way to define MGs/AGs (store + API + UI) and with a migration path.
Concrete shape already specified in `architecture-limitations-and-options.md`
("HA integration surface & the per-output toggle"): new daemon endpoint enumerating
named MGs/AGs; integration reads `/api/settings` for the toggle (it doesn't today)
and re-reconciles on change; `unique_id` namespaced `_mg_`/`_ag_`/`_out_`;
`cleanup_entities` updated to the new scheme. The `expose_outputs_as_media_players`
setting it consumes is already in place (above).

### Other spikes (all hardware-gated), unchanged from the plan
S5 (scale: N per-device senders — CPU/socket/join-leave), S6 (O-E RAOP per-output
volume node + added latency), S7 (O-F warm anchor switch latency). Each needs the
S3 path and on-device measurement.

---

## Decisions — all resolved (2026-07-25)
1. **Announcement concurrency:** RESOLVED → atomic clips; **queue by default**
   (reject opt-in), **`barge_in`** per-announcement (default queue-jump; preempted
   clip re-queued whole), **TTL** per-announcement with a default. Implemented +
   tested in `announce_arbiter.rs` (`AnnounceScheduler`).
2. **MG vs routing matrix:** RESOLVED → an MG replaces a routing row, but the routing
   target is modeled **polymorphically** (target id → output | MG | future) so the
   backend keeps other options open. Outputs in an MG aren't individually assignable;
   whether a per-output entity (toggle on) may take an independent input is deferred
   to hands-on evaluation. `routing.json` migration still to design during
   productionization.
3. **AG/MG media_player semantics:** CONFIRMED (AG = announce-only; MG volume =
   master; layered per-output volume).

## Open items for productionization
- `routing.json` migration to the polymorphic target-id model.
- Whether per-output entities may take independent inputs (evaluate after MG UI).

## Suggested next step
S1 + S2 + S3 are done: the O-B shared-timeline approach is **validated end-to-end**
(software: identical timestamps; hardware: 2 devices in-sync; dropouts proven
source-side, not the path). The risk that gated O-B is retired.

Productionization:
1. ~~Persist per-device/anchor routing links~~ — **N/A for the flag path.** The
   spike's broken link was an artifact of its *ad-hoc* `ensure_link_by_name`; the
   reconciler path reuses the group's already-persisted-and-reconciled anchor
   links, so a source-node recreate re-links automatically. (Still relevant later
   for *independent* per-device sinks with their own routes.)
2. **Wire per-device senders into `sync_group.rs` behind a flag** — **DONE
   (2026-07-25).** Runtime flag `per_device_sendspin_senders` (settings + `/api/settings`
   + Settings-tab "Experimental" toggle, default off). `sync_group::reconcile` branches
   `start_server` → `start_server_per_device` and restarts a group when the mode
   flips. Deployed (v0.2.0.1213) and validated on the live reconciler: toggling
   restarted the production group `sync-grp-…` "(per-device senders)", the device
   reconnected, and flipping back returned it to the shared Group — cleanly, no
   crash. (Audio/sync in this mode = the S1 result.) Left OFF (default) in production.
3. **Per-device duck/overlay — MIXING DONE + validated on hardware (2026-07-25).**
   `overlay_mixer.rs` (`OverlayMixer`, 7 unit tests) provides per-output
   `duck(music)+overlay` frames; the per-device capture loop
   (`start_server_per_device`) consults it (via a shared client→node map) so a
   device with an active overlay gets ducked music + the announcement slice while
   its groupmates get plain music, all on the one shared timestamp. Spike endpoint
   `POST /api/spike/overlay` injects a test tone. **Hardware test (v0.2.0.1233,
   two Voice PEs on the multi-device spike):** injected a tone on one device — it
   ducked + played the tone and recovered, the groupmate kept full music; repeated
   with the roles swapped — same result. Per-speaker announcement/duck with
   groupmate isolation **confirmed both directions.**
   - **3b DONE + validated on hardware (2026-07-25, v0.2.0.1304).** `announce.rs`
     (`AnnounceCoordinator`, process-global) ties `AnnounceScheduler` ↔
     `OverlayMixer`: `POST /api/announce {targets, test|url|wyoming, priority,
     on_busy, barge_in, ttl_ms, duck}` decodes the clip to 48k/S16/stereo
     (`resample.rs` — 4 tests; `decode::decode_file_to_pcm_48k_stereo`,
     `wav::read_pcm16`), `scheduler.begin` → `OverlayMixer::start` per target; a
     150 ms poll loop (main.rs) drains `OverlayMixer::take_finished` →
     `scheduler.complete` (next queued / un-duck) and ticks TTLs. Hardware: the
     built-in test clip announced to one Voice PE ducked + played + auto-completed
     with the groupmate untouched (both directions), and two back-to-back
     announces to one device **queued** and played in sequence. Ducking is implicit
     in the mix for sendspin; RAOP per-output duck is O-E (future).
     - Remaining for a full product feature: address targets by **announcement
       group** (not raw node names) once the MG/AG data model exists; a per-device
       announce path for RAOP (O-E); HA integration surface.
4. **MG/AG data model + API — DONE (2026-07-25, v0.2.0.1353).** `groups_store.rs`
   (persisted `/data/groups.json`; MG with enforced exclusivity, AG with
   priority/duck; 5 tests) + CRUD `/api/groups/music|announcement` + `/api/announce`
   accepting `announcement_group` (resolves targets/priority/duck). API-validated
   end-to-end (create, exclusivity reject, announce-by-group, delete). Store is
   empty in production (no groups defined yet).
5. Fix the standalone per-device **null-sink clocking** (QUANT 0 → steady driver)
   for the independent-routing case (device on its own source, S3 path). *Needs
   iterative on-hardware tuning.*

### Done (2026-07-25, user authorized breaking HA + live routing + new UI)
- **Group-creation UI** — a "Groups" tab (`GroupsTab.svelte`) over `/api/groups/*`:
  create/delete music + announcement groups, member picker with the exclusivity
  rule enforced, per-AG test-announce. `svelte-check` clean; deployed (v0.2.0.1414).
- **MG ↔ routing** — `POST/DELETE /api/groups/music/:id/route`: routing a source to
  a music group expands to per-member links, reusing the existing routing store +
  reconciler (no matrix restructure; the polymorphic-target intent, without a
  routing.json format change). API-validated (route → both members linked → clean
  unroute).
- **HA entity migration** — the custom component now creates one `media_player`
  **per music group** (source-select routes the whole group via the route endpoint;
  master volume applied to all members) and **per announcement group** (`play_media`/
  TTS → `/api/announce {announcement_group}`), plus **per output only when
  `expose_outputs_as_media_players` is on**. Namespaced unique_ids
  (`_mg_`/`_ag_`/`_out_`); coordinator fetches groups + settings each poll;
  `cleanup_entities` purges the old per-output ids. Deployed (integration rsync + HA
  core restart); verified: creating a MG + AG produced `media_player.kitchen_zone`
  (`…_mg_…`) and `media_player.everywhere` (`…_ag_…`), integration loaded clean, no
  orphans.

### Remaining
- **RAOP per-output duck (O-E):** the announce overlay path is sendspin-only; RAOP
  targets need a per-output volume node for per-device duck. *(Not started.)*
- **Standalone per-device null-sink clocking** (independent per-device routing, S3
  QUANT-0 dropout): needs iterative on-hardware tuning. *(Not started.)*
- MG media_player group-volume is "mean of members / set all"; a true group master
  scalar is a refinement.
