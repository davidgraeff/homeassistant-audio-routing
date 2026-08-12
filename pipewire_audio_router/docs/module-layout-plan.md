# Bridge-daemon module layout — refactoring plan

Turning `bridge-daemon/src/` from a flat module list into a directory tree that
states the daemon's layering, without changing any behaviour. This is a *move*
refactor: `git mv` plus `mod` re-declarations, with one deliberate code
extraction (`state.rs`) and one file split (`api.rs`).

Read [`architecture.md`](architecture.md) first — the target tree is that
document's structure made visible on disk (§2 the graph, §3 sources, §4/§5
outputs, §6 clocks, §9 control plane). Design rationale that outlives this plan
should graduate into [`decisions.md`](decisions.md).

---

## 0. Status (2026-08-12)

**Landed:** `align/` — 7 files moved plus a new `mod.rs`, in `fc627e4`
("align: move the alignment cluster into `align/`"). §3 rows 26 and 51–56.

**Remaining:** 53 files still flat at the crate root, ~26.0k lines. `align/`
holds the other ~19.8k across 8 files, so the crate is now **61 files /
~45.8k lines** — the flat part is a minority of the code but a large majority of
the files.

The align wave went first, out of order (§4 lists it as wave 5), and it was not a
leaf when it did: **eight files outside `align/` reach into it** — `api.rs`,
`sync_group.rs`, `announce.rs`, `overlay_mixer.rs` and the four output senders
(`ap2_server`, `sendspin_server`, `pwsink_server`, `pwsink_agent`), the last group
via `relay_delay` (28 references, the widest fan-in in the cluster).

It worked anyway, and that is the lesson: **a move is textual, not structural**.
Rust does not care about module cycles inside a crate, so dependency direction
does not constrain the order at all. What constrains it is how many call sites a
wave has to edit and how likely those files are to have concurrent edits in
flight (§4.1). Treat §4's wave numbers as advisory; the only ordering that
genuinely holds is `api/` last, because it is the only wave that also *changes*
code (§6).

One consequence for wave 4: those four output senders import from `align/`, which
is the single upward arrow in the target tree. It is deliberate (§10.3 —
`relay_delay` exists only for alignment even though the hook lives in the relays),
so wave 4 should not "fix" it by dragging `relay_delay.rs` into `outputs/`.

Three things the first wave taught, folded in below: the baseline convention
replaces the wave-0 gate (§4), the transitional-alias trick is optional rather
than mandatory (§4.1), and every `mod.rs` gets a `//!` boundary header (§2).

---

## 1. Why now, and what the evidence is

Nothing here is aesthetic. Four measurable problems, all from the current tree:

**1. Filenames carry no layering signal.** One subsystem, three spellings:
`pw_sink.rs`, `pwsink_server.rs`, `pw_sink_liveness.rs`, `pwsink_agent.rs`,
`pw_target_discovery.rs`, `pw_target_liveness.rs`, `pw_sink_spike.rs`,
`applemidi_sender.rs`. Nothing in the names says these eight are the same
backend, or that `applemidi_sender` is its transport.

**2. Five god-files.** Code lines excluding inline tests, re-measured
2026-08-12:

| file | code | tests | crate deps |
|---|---:|---:|---:|
| `align/measure.rs` | 7028 | 3062 | 11 |
| `api.rs` | 3857 | 224 | 46 |
| `sync_group.rs` | 1847 | 367 | 21 |
| `align/calibrate.rs` | 1702 | 1138 | 12 |
| `align/levels.rs` | 1577 | 576 | 2 |

`api.rs` reaches 46 of the other 60 modules — it is the crate's hub, and its
`AppState` is why. `align/measure.rs` is now **the largest file in the crate by
a factor of two**, and it grew ~3.9k lines between this plan being written and
the align wave landing (W12 chaining, the equivalence experiment). Moving it
into `align/` did nothing about that; §10 is the design that does.

**3. The clusters already exist, undeclared.** `ap2_*` (8 files), `sendspin_*`
(6), `pw*sink*` (8), `*_store` (5) — and `align_*` (6), which is the one that
has since become a directory. The prefixes are a tree that someone typed instead
of created.

**4. One module is mis-filed, and it hides a design fact.**
`sendspin_capture.rs` has **zero** crate dependencies and six dependents:
`ap2_server`, `pwsink_server`, `overlay_mixer`, `sendspin_server`,
`sendspin_codec` — and now `align/measure`. It is the generic "capture PCM off a
graph node" primitive that every output backend builds on, and that alignment
measures through; its name says it belongs to one backend. Re-homing it to
`pw/capture.rs` is the single highest-information move in this plan.

### The one real cycle

The dependency graph is otherwise a clean DAG. The exceptions are thin and
worth naming, because they are what makes `api/` the last wave:

- `routing.rs` → `api.rs` uses exactly three items: `AppState`, `OutputInfo`,
  `outputs_listings`.
- `pwsink_agent.rs` → `api.rs` uses exactly one: `AppState`.
- `ap2_server` ↔ `ap2_health` ↔ `ap2_liveness` is a genuine three-way cycle,
  but entirely inside `outputs/ap2/`, so the tree contains it.

Rust permits module cycles inside a crate, so **none of these block the move**.
They only matter for §6.

---

## 2. Target tree

```
src/
  main.rs                 boot + wiring only (624 lines, already clean)
  state.rs                AppState + Shared* aliases, extracted from api.rs   ★new
  supervisor.rs           discovery_supervisor.rs — owns the single ServiceDaemon

  pw/                     PipeWire plumbing; no product concepts
    thread.rs             the graph thread, registry snapshot, PwCommand (21 users)
    capture.rs            long-lived PCM capture off a node        ★re-homed
    player.rs             short-lived clip playback to a node
    profiler.rs           Profiler FFI → per-node xrun counts
    metering.rs           on-demand peak taps

  audio/                  format/DSP helpers; zero crate deps
    resample.rs  wav.rs  decode.rs

  util/
    locks.rs              LockRecover (29 users)
    node_names.rs         config.rs — the *-dev-/​*-out- node-name prefixes
    host_assessment.rs    "is this host strong enough for RT audio?"

  store/                  file-backed JSON, no live handles
    routing.rs  outputs.rs  groups.rs  settings.rs
    migration.rs          raop_migration.rs — the one-shot raop→ap2 rewrite

  sources/                audio into the graph (§3)
    mod.rs                sources_store.rs — store *and* reconciler
    airplay.rs  airplay_clients.rs  rtp.rs  now_playing.rs
    bt_bridge.rs          bt_bridge_discovery.rs — a sender, not an output

  outputs/                audio out of the graph (§4, §5)
    overlay_mixer.rs      per-output duck+overlay frame
    sendspin/  server.rs codec.rs discovery.rs liveness.rs volume.rs
    ap2/       server.rs discovery.rs liveness.rs health.rs probe.rs
               volume.rs ptp.rs
    pwsink/    server.rs agent.rs applemidi.rs module_args.rs
               discovery.rs sender_liveness.rs target_liveness.rs

  routing/
    mod.rs                routing.rs — matrix reconcile + /api/routing handlers
    sync_group.rs  sync_settings.rs

  announce/
    mod.rs                announce.rs
    arbiter.rs            announce_arbiter.rs — pure scheduling decisions

  align/
    measure.rs estimator.rs levels.rs group.rs mic.rs
    calibrate.rs          the manual (non-mic) alignment path
    relay_delay.rs        per-output provisional delay — see §10.3

  api/                    §9 control plane; see §5 for the split
    mod.rs                the route table, and nothing else
    …one file per resource

  spike/                  dev-only, reachable via /api/spike/*
    per_device.rs  ap2.rs  pwsink.rs
```

### Two deliberate exceptions to the obvious grouping

`sources_store.rs` becomes `sources/mod.rs`, **not** `store/sources.rs`: it
depends on `airplay_source` and `rtp_source` because it reconciles live
handles. It is a supervisor that happens to persist.

`sync_settings.rs` stays in `routing/`, **not** `store/`: it pushes settings
into `ap2_server`, `applemidi_sender` and `sendspin_codec` on write. `store/` is
reserved for modules whose only dependency is `util/locks` — that invariant is
what makes the directory meaningful.

### Directory module style

Use `foo/mod.rs`, not `foo.rs` + `foo/`. Both work in edition 2021; `mod.rs`
keeps a subsystem in exactly one directory, which is the point of the exercise.
Where a directory's parent module has real content (`routing/`, `sources/`,
`announce/`), that content *is* the `mod.rs`. Where it does not (`pw/`, `util/`,
`store/`, `align/`, `outputs/`, `spike/`), `mod.rs` is a declaration list.

Either way it carries a `//!` header that states the boundary and names each
child's role — `align/mod.rs` (landed) is the pattern to copy: 13 lines that say
what alignment *is not* ("never carrying audio"), walk the pipeline
`calibrate → mic → estimator → levels → measure` in order, and justify the one
member whose placement is arguable (`relay_delay`, §10.3). That header is the
payoff of the whole refactor — the flat list could not hold it anywhere. A
declaration list without one is a directory that explains nothing, which is
where we started.

Declare children `pub(crate) mod`, as `align/mod.rs` does. Nothing in this crate
is a library surface, and `pub` on a child of a `pub(crate)` parent invites the
"is this exported?" question every time.

`store/`'s header additionally carries the locks-only-dependency rule above —
it is the one directory whose membership is defined by a constraint rather than
by subject.

---

## 3. Full rename table

60 files, of which 7 have landed. `★` marks the two moves that change meaning
rather than location; everything else is pure relocation. The `mod.rs` files each
directory needs are new files, not rows here — `align/mod.rs` was written from
scratch (§2, "Directory module style").

| # | current | target |
|---:|---|---|
| 1 | `main.rs` | `main.rs` *(unchanged)* |
| 2 | `discovery_supervisor.rs` | `supervisor.rs` |
| 3 | `pw_thread.rs` | `pw/thread.rs` |
| 4 | `sendspin_capture.rs` | `pw/capture.rs` ★ |
| 5 | `player.rs` | `pw/player.rs` |
| 6 | `profiler.rs` | `pw/profiler.rs` |
| 7 | `metering.rs` | `pw/metering.rs` |
| 8 | `resample.rs` | `audio/resample.rs` |
| 9 | `wav.rs` | `audio/wav.rs` |
| 10 | `decode.rs` | `audio/decode.rs` |
| 11 | `locks.rs` | `util/locks.rs` |
| 12 | `config.rs` | `util/node_names.rs` ★ |
| 13 | `host_assessment.rs` | `util/host_assessment.rs` |
| 14 | `routing_store.rs` | `store/routing.rs` |
| 15 | `outputs_store.rs` | `store/outputs.rs` |
| 16 | `groups_store.rs` | `store/groups.rs` |
| 17 | `settings_store.rs` | `store/settings.rs` |
| 18 | `raop_migration.rs` | `store/migration.rs` |
| 19 | `sources_store.rs` | `sources/mod.rs` |
| 20 | `airplay_source.rs` | `sources/airplay.rs` |
| 21 | `airplay_clients.rs` | `sources/airplay_clients.rs` |
| 22 | `rtp_source.rs` | `sources/rtp.rs` |
| 23 | `now_playing.rs` | `sources/now_playing.rs` |
| 24 | `bt_bridge_discovery.rs` | `sources/bt_bridge.rs` |
| 25 | `overlay_mixer.rs` | `outputs/overlay_mixer.rs` |
| 26 ✔ | `relay_delay.rs` | `align/relay_delay.rs` — §10.3, not `outputs/` |
| 27 | `sendspin_server.rs` | `outputs/sendspin/server.rs` |
| 28 | `sendspin_codec.rs` | `outputs/sendspin/codec.rs` |
| 29 | `sendspin_discovery.rs` | `outputs/sendspin/discovery.rs` |
| 30 | `sendspin_liveness.rs` | `outputs/sendspin/liveness.rs` |
| 31 | `sendspin_volume.rs` | `outputs/sendspin/volume.rs` |
| 32 | `ap2_server.rs` | `outputs/ap2/server.rs` |
| 33 | `ap2_discovery.rs` | `outputs/ap2/discovery.rs` |
| 34 | `ap2_liveness.rs` | `outputs/ap2/liveness.rs` |
| 35 | `ap2_health.rs` | `outputs/ap2/health.rs` |
| 36 | `ap2_probe.rs` | `outputs/ap2/probe.rs` |
| 37 | `ap2_volume.rs` | `outputs/ap2/volume.rs` |
| 38 | `ap2_ptp.rs` | `outputs/ap2/ptp.rs` |
| 39 | `pwsink_server.rs` | `outputs/pwsink/server.rs` |
| 40 | `pwsink_agent.rs` | `outputs/pwsink/agent.rs` |
| 41 | `applemidi_sender.rs` | `outputs/pwsink/applemidi.rs` |
| 42 | `pw_sink.rs` | `outputs/pwsink/module_args.rs` |
| 43 | `pw_target_discovery.rs` | `outputs/pwsink/discovery.rs` |
| 44 | `pw_sink_liveness.rs` | `outputs/pwsink/sender_liveness.rs` |
| 45 | `pw_target_liveness.rs` | `outputs/pwsink/target_liveness.rs` |
| 46 | `routing.rs` | `routing/mod.rs` |
| 47 | `sync_group.rs` | `routing/sync_group.rs` |
| 48 | `sync_settings.rs` | `routing/sync_settings.rs` |
| 49 | `announce.rs` | `announce/mod.rs` |
| 50 | `announce_arbiter.rs` | `announce/arbiter.rs` |
| 51 ✔ | `align_measure.rs` | `align/measure.rs` |
| 52 ✔ | `align_estimator.rs` | `align/estimator.rs` |
| 53 ✔ | `align_levels.rs` | `align/levels.rs` |
| 54 ✔ | `align_group.rs` | `align/group.rs` |
| 55 ✔ | `align_mic.rs` | `align/mic.rs` |
| 56 ✔ | `calibrate.rs` | `align/calibrate.rs` |
| 57 | `per_device_spike.rs` | `spike/per_device.rs` |
| 58 | `ap2_spike.rs` | `spike/ap2.rs` |
| 59 | `pw_sink_spike.rs` | `spike/pwsink.rs` |
| 60 | `api.rs` | `api/` + `state.rs` — see §5 |

`✔` marks rows already landed. The left column stays at the pre-move spelling on
purpose — it is the record of what moved, and §7's stale-reference grep should
skip this table.

Naming decisions worth defending: `pw_sink.rs` → `module_args.rs` because its
entire content is the two SPA-JSON `args` strings; `pw_sink_liveness` →
`sender_liveness` and `pw_target_liveness` → `target_liveness` because the
current pair is indistinguishable by name (one watches our AppleMIDI sender, the
other watches the remote host); `config.rs` → `node_names.rs` because it holds
no configuration — it holds the node-name prefix constants, and the daemon has
no `options.json` seeding at all.

---

## 4. Migration waves

Each wave compiles and passes `cargo test` on its own, and each is one commit.

**Wave 0 — the baseline, not a gate.** The original instruction here was "land
the CI fmt/clippy gate green first, no exceptions". The align wave overtook it:
the gate was still not green, the move went ahead anyway, and what made it
reviewable was **recording the baseline and proving it unchanged** rather than
having no findings to begin with. That is the convention now (§9), and the
measured baseline as of `fc627e4` is:

- `cargo test` — **450 passed / 0 failed**
- `cargo clippy --all-targets` — 6 pre-existing warnings (2 `derivable_impls`,
  4 `chunks_exact`)
- `cargo fmt --check` — 3 pre-existing findings, all in `applemidi_sender.rs`
  and `pwsink_server.rs`
- frontend `npm run check` — 160 files, 0 errors, 0 warnings

One consequence to act on: **those two fmt findings sit in files that wave 4
moves** (`outputs/pwsink/applemidi.rs`, `outputs/pwsink/server.rs`). Fix them in
a separate commit *before* wave 4, or the "baseline unchanged" check for that
wave turns into an argument about whether the diff was a reformat or a move.

**Wave 1** — `util/` + `audio/` + `pw/`. Leaves, no crate deps between them
except `pw/thread` → `util/locks`, `pw/profiler`.

**Wave 2** — `store/`. Depends only on `util/`.

**Wave 3** — `sources/`.

**Wave 4** — `outputs/`, in three sub-commits (`sendspin/`, `ap2/`, `pwsink/`)
plus one for the two shared files. The `ap2` cycle moves as a unit. Preceded by
the fmt fix above.

**Wave 5** — `routing/`, `announce/`, `spike/`. ✔ `align/` landed early
(`fc627e4`); see §0 for why the ordering did not matter.

**Wave 6** — `state.rs` extraction, the `api/` split, and the cycle break (§5,
§6). Last, because everything else points at it.

**Wave 7** — the comment/doc reference sweep (§7), and deletion of any
transitional aliases a wave chose to use (§4.1).

### 4.1 The zero-touch move trick — optional, and align skipped it

**What actually happened.** The align wave rewrote every call site in the same
commit as the move, with no aliases. It touched 12 source files outside `align/`
and was still reviewable: 24 files, +248/−225, and `git` detected all 7 renames
(`estimator.rs` at 0 changed lines, the rest carrying only path text). The
alias-plus-cleanup version would have been more total commits for less clarity.

**When to use aliases anyway.** The deciding number is call-site fan-in, not
subsystem size. Align's widest was `relay_delay` at 28 references across 6 files
— comfortably hand-editable. The waves ahead are worse: `util/locks` (29
dependent files), `pw/thread` (21), `util/node_names` (12), `pw/capture` (6 files
but they are the output backends and `align/measure`, the code most likely to
have edits in flight). Rule of thumb: **rewrite directly under ~10 dependent
files; alias above that, or whenever another session is editing the same files**
(a rename commit touching 29 files is a merge-conflict generator; the alias
version touches one).

Where aliases are used: `git mv` the files, declare the new modules, then add a
transitional alias block at the top of `main.rs`:

```rust
// TRANSITIONAL (module-layout-plan.md §4.1) — keeps pre-move `crate::foo::…`
// paths resolving so each wave is a pure rename. Removed in wave 7.
pub(crate) use outputs::sendspin::server as sendspin_server;
pub(crate) use pw::capture as sendspin_capture;
// …one line per moved module
```

A `pub(crate) use` in the crate root creates a crate-root binding, so every
existing `crate::sendspin_server::start_server_per_device(…)` keeps resolving
unchanged. The consequence is that such a wave is a diff `git` reports as a
**pure rename** — reviewable by reading the tree, not the hunks — and that it can
be reverted without touching call sites.

Wave 7 then removes those aliases one subsystem at a time. Each removal is a
compile-error-driven `use`-path update with zero judgement calls, and the
compiler proves completeness.

Either way, one thing is not optional: the comment references inside the moved
files (`// see relay_delay.rs`) get updated **in the move commit**, as align did.
Deferring them to wave 7 means the tree spends the whole refactor describing
itself wrongly, and the sweep then has to distinguish "stale because not yet
moved" from "stale because missed".

---

## 5. Splitting `api.rs`

3857 code lines, 46 module deps, 224 test lines — and 18 `// ---- section ----`
markers that are already the file boundaries. It is the cheapest of the five
god-files by a wide margin, which is why it is in scope here and the others are
not (§8, §10).

Line numbers below are **as of 2026-08-12** and will be stale again by wave 6 —
the file grew +197 lines between this plan being written and the align wave
landing, all of it in the mic-alignment section. Derive the boundaries from the
`// ---- ` markers at cut time; the table's value is the *set of files* and what
belongs in each.

| target | lines | content |
|---|---|---|
| `state.rs` | 45–140 | `AppState` + the `Shared*` aliases ★ |
| `api/mod.rs` | 141–336 | `router()` — the route table only |
| `api/nodes.rs` | 337–493 | `/health`, `/api/nodes`, manual `/api/links` |
| `api/static_files.rs` | (within 337–493) | `StaticAssets`, content types, SPA fallback |
| `api/outputs.rs` | 494–1189 | the outputs listing + adopt/ignore/rename/latency/codec |
| `api/clients.rs` | 1190–1397 | per-receiver client registry, ban/priority/disconnect |
| `api/now_playing.rs` | 1398–1524 | now-playing get/put/report/artwork |
| `api/sources.rs` | 1525–1804 | RTP target + multi-source CRUD |
| `api/volume.rs` | 1805–1944 | sendspin + AP2 per-device volume/mute |
| `api/agents.rs` | 1945–1997 | receiver-agent pairing + WS |
| `api/sync.rs` | 1998–2185 | group lead, per-device delay, `lead_floor` |
| `api/settings.rs` | 2186–2269 | `/api/settings`, `/api/status` |
| `api/spike.rs` | 2270–2555 | every `/api/spike/*` handler |
| `api/announce.rs` | 2556–2707 | `ag_announce` + PCM acquisition |
| `api/duck.rs` | 2708–2874 | duck holds |
| `api/groups.rs` | 2875–3079 | music + announcement groups |
| `api/align.rs` | 3080–3231 | manual alignment (`align/calibrate.rs`) |
| `api/measure.rs` | 3232–3856 | mic-assisted measurement + equivalence |

The 224 inline test lines split by subject: the route-table shape tests stay in
`api/mod.rs`, the rest follow their handlers.

`api/measure.rs` at 625 lines is now the second-largest slice and still growing
with the alignment feature — it is the piece to cut first if wave 6 has to be
done in halves.

Order inside wave 6: extract `state.rs` first and let it compile crate-wide,
*then* carve handlers out. Reversing that order means every carved file needs an
`AppState` import that will move again a commit later.

---

## 6. Breaking the api ↔ routing cycle

Three items, one commit, done as part of wave 6:

1. `AppState` → `state.rs`. This alone resolves `pwsink_agent.rs` → `api.rs`
   and two thirds of `routing.rs` → `api.rs`.
2. `OutputInfo` + `outputs_listings` → `api/outputs.rs`, which `routing/mod.rs`
   may depend on downward.
3. Result: `api/` becomes a **leaf**. Nothing in the crate depends on it.

That is worth doing even though Rust tolerates the cycle, for two reasons. It
makes the "handlers depend on subsystems, never the reverse" rule enforceable by
eye. And it is the precondition for a future crate split, if daemon build times
ever justify one — `util/`, `audio/`, `pw/`, `store/` and `align/` are already
near-leaf, and `api/` is the only thing standing in the way.

Not in scope: the `ap2_server` ↔ `ap2_health` ↔ `ap2_liveness` cycle. It is
contained by `outputs/ap2/` and untangling it is behaviour work, not layout
work.

---

## 7. The reference sweep

Module filenames are load-bearing prose in this codebase. Current counts
(2026-08-12):

- **350** `*.rs` references in source comments (`// see sendspin_server.rs`),
- **188** in `pipewire_audio_router/docs/*.md` — densest in
  `mic-alignment-plan.md` (53), `architecture.md` (39), `decisions.md` (34),
  `pipewire-sink-roadmap.md` (26), `receiver-agent-plan.md` (24),
- **26** in the repo-root `docs/*.md` — `api-reference.md` was the one the align
  wave nearly missed, because it lives outside this directory.

Three exclusions, all learned the hard way:

- **`pipewire_audio_router/docs/old/`** (~300 refs) is a historical archive. It
  describes the tree as it was, correctly. Leave it alone.
- **This plan's §3 table** keeps the pre-move spelling in its left column by
  design, and contributes ~316 refs of its own. Exclude it from any count or it
  swamps the signal.
- **A module's own name inside its own file** is not stale.

Handle the rest as a scripted `sed` pass driven by the §3 table, in its own
commit. Two rules keep it mechanical: leaf basenames stay recognizable
(`sendspin_server.rs` → `outputs/sendspin/server.rs`, never `ss_srv.rs`), and
references become **path-qualified** (`outputs/sendspin/server.rs`, not bare
`server.rs`) — otherwise a comment saying "see `server.rs`" is ambiguous across
four backends.

The align wave came out clean on this: **zero stale references** to any of its
seven old names remain in `src/` or in the live docs, and every surviving mention
is path-qualified (`align/calibrate.rs`, never bare `calibrate.rs`) — including
in `wav.rs`, `player.rs`, `sync_group.rs` and the three output relays, which had
no reason to know the file had moved. That is the payoff of doing the references
in the move commit (§4.1) rather than deferring them here. Later waves should
expect the same standard.

Beware the measurement, though: `grep -c 'calibrate\.rs'` counts
`align/calibrate.rs` too, and reading that as 22 stale references is how you
invent work that does not exist. Use the anchored form below.

The check has to be driven by the §3 table's **old names**, not by "does this
basename exist under `src/`" — the docs legitimately reference files in four
other trees (the sendspin submodule, `vendor/`, `pw-control`, the Pi bridge
firmware), and the existence test flags 25 of them as false positives. Run this
from `pipewire_audio_router/`, with one alternative per landed rename:

```sh
OLD='align_measure|align_estimator|align_levels|align_group|align_mic|calibrate|relay_delay'
grep -rnE "(^|[^/[:alnum:]_])($OLD)\.rs" bridge-daemon/src docs ../docs \
  | grep -v 'docs/old/\|module-layout-plan.md'
```

The leading `[^/]` is what makes it useful: it matches bare `calibrate.rs` and
skips the already-correct `align/calibrate.rs`. Extend `OLD` as each wave lands.

`architecture.md`, `decisions.md` and `mic-alignment-plan.md` carry the densest
references and deserve a read, not just a sed — they explain *mechanisms* by
filename, so a mechanical rewrite can leave a sentence that parses but no longer
makes sense.

---

## 8. Non-goals

**The remaining god-files.** `align/measure.rs` (7028 code + 3062 test lines),
`sync_group.rs` (1847), `align/calibrate.rs` (1702) and `align/levels.rs` (1577)
need real decomposition thought, and they must not ride along in a rename commit.
The align four landed in `align/` at full size, exactly as planned; `sync_group.rs`
lands in `routing/` the same way. Splitting them is separate, later work against a
tree where their neighbours are already visible — and for the align cluster that
tree now exists, so §10 is no longer speculative.

**Behaviour, signatures, visibility tightening.** No `pub` → `pub(crate)`
sweeps, no dead-code removal, no clippy fixes inside moved files. Every one of
those makes a rename diff unreviewable, and each is cheap to do afterwards.

**Feature-gating `spike/`.** Grouping the three spike modules makes
`#[cfg(feature = "spike")]` easy later, and the `/api/spike/*` routes are the
only callers. Worth doing — not here.

**The vendored trees.** `vendor/` and the sendspin submodule are untouched;
`[workspace] exclude` and the crate-boundary lint convention stay exactly as
they are.

---

## 9. Verification checklist

The rule is **baseline unchanged**, not "clean" — the crate does not start clean
(§4 wave 0), and a wave that silences a pre-existing finding is as suspect as one
that adds one. State the four numbers in the commit message, as `fc627e4` does,
so the next wave can diff against them without re-deriving.

Per wave:

- `cargo test` — **450 passed / 0 failed** and unchanged. A move must not change
  the test count: a drop means a `#[cfg(test)]` module lost its `mod` declaration
  in the shuffle, which compiles silently.
- `cargo clippy --all-targets` — still 6 warnings (2 `derivable_impls`,
  4 `chunks_exact`), no new ones.
- `cargo fmt --check` — still exactly the 3 known findings, and *no* reformatting
  inside moved files. (Wave 4 clears these first; see §4.)
- `cargo check --tests` clean.
- `git show --stat -M` reports the wave's files as renames, not delete+add. With
  path-text edits riding along this still works — align's seven files were all
  detected, one at zero changed lines — but if a file shows as delete+add, the
  edits went too far: split the commit.
- `git log --follow <new path>` reaches the file's pre-move history.
- `npm run check` in `frontend/` — 160 files, 0 errors, 0 warnings. Only relevant
  for waves that touch a serialized DTO, which no pure move should.

Once, at the end:

- The `scripts/build-daemon.sh` container build succeeds (rootless podman — do
  not use ad-hoc `docker run -v`).
- No stale `*.rs` references in `src/` or `docs/` (§7).
- No transitional aliases left in `main.rs`.
- One deploy to the instance and a smoke test of the routing matrix, an
  announcement, and one alignment run — the three paths that cross the most
  module boundaries.

---

## 10. Decomposing the alignment cluster (the §8 non-goal, designed)

§8 defers the god-file splits to "separate, later work against a tree where their
neighbours are already visible". This section is that design for the
calibration/alignment cluster, so the move can happen without having to invent it
under time pressure. It is still **separate from the move commit**.

### 10.1 The numbers moved

Re-measured after the move landed (2026-08-12) — the cluster kept growing while
the feature was built, so every earlier count here was stale within a day:

| file | total | code | test |
|---|---:|---:|---:|
| `align/measure.rs` | 10090 | **7028** | 3062 |
| `align/calibrate.rs` | 2840 | 1702 | 1138 |
| `align/levels.rs` | 2153 | 1577 | 576 |
| `align/estimator.rs` | 1501 | 951 | 550 |
| `align/relay_delay.rs` | 1388 | 892 | 496 |
| `align/group.rs` | 1125 | 523 | 602 |
| `align/mic.rs` | 699 | 520 | 179 |
| `align/mod.rs` | 21 | 21 | 0 |

**19.8k lines — 43% of the daemon**, in eight files, for one feature. And
`align/measure.rs` alone is 10k lines: nearly twice `api.rs`, and it doubled
between the first draft of this section and the move commit (W12 chaining plus
the relay-vs-device equivalence experiment). It is the priority; the rest are
defensible at their size, `mod.rs` is the model, and the growth rate is the real
argument — a file that adds 4k lines in a week will not get easier to split
later.

### 10.2 `align/measure.rs` → `align/measure/`

The file has **one** reason to be large: it holds the run's *reported shape*, its
*seams*, its *pure arithmetic* and its *driver* in one place. Those four have
different reasons to change, which is the whole test for a split.

```
align/measure/                                                     approx. lines
  mod.rs          MeasureManager + Inner + the public entry points        ~810
  status.rs       the reported DTOs: Phase, MeasureStatus, MemberProgress,
                  Proposal, ProposedDelay, Checks, Verification,
                  WalkProgress, GateProgress, Refusal(+Kind),
                  Warning(+Kind)                                        ~1100
  deps.rs         the seams: SessionControl, MicFeed, DelayWriter,
                  MeasureDeps, LiveMic, SendAheadContext                 ~155
  gate.rs         Gate, GateConfig, GateSample, GateReason, GateStep      ~360
  feeder.rs       Feeder — the mic-window → estimator pump                ~250
  knobs.rs        KnobPolarity, Knob, knob_of, MemberInterval,
                  choose_target                                          ~250
  solve.rs        solve, fit_drift, transitivity, repeatability,
                  propose_knobs, KnobSolution, SolveInput                ~600
  chain.rs        W12 multi-position chaining: chain_step, solve_chain,
                  chain_error + Chain{Action,Overlap,Step,Error,Progress}
                  and the run_chain driver                               ~900
  run.rs          run_measure / measure_passes / run_apply / bind /
                  measure_member                                         ~600
  walk.rs         run_walk + walk_prompt — near-field arrival             ~310
  signal.rs       signal_check(_window) + SignalCheck — the pre-flight    ~250
  equivalence.rs  the relay-vs-device experiment (W21): plan_equivalence,
                  EquivalenceManager, its own routes and WS             ~1320
  push.rs         measure_ws + the change notifier                    (in mod.rs
                                                                    until split)
```

Two additions since this section was first drafted, both from reading the file as
it stands:

- **`chain.rs` did not exist in the original list.** W12 landed a whole second
  axis — chain a run across listening positions — as ~900 lines split between
  pure step arithmetic (`chain_step`, `solve_chain`, `chain_error`) and an async
  driver (`run_chain`, `chain_progress`, `chain_prompt`). Keep them in one file
  but in that order, pure first: the arithmetic is what has tests, and the driver
  is what has `await` points.
- **`equivalence.rs` is ~1320 lines and is not part of a measurement run.** It is
  the W21 experiment that answers "does a relay-side delay measure the same as a
  device-side one", with its own manager, its own two endpoints and its own WS.
  Once that question is answered and recorded in `decisions.md`, it is a
  candidate for deletion or for the same treatment as `spike/` — which would take
  ~19% off the file without any decomposition work at all. Decide that *before*
  investing in splitting it.

**Extract in this order**, because it is the order of decreasing risk — each of
the first four is pure, densely tested, and has almost no inbound coupling, so
the compiler proves the move:

1. `knobs.rs` — the §2.4.2 interval model. Pure arithmetic, no I/O, no state. The
   single highest-value extraction: it is where the sign bug lived, and it should
   be readable without the run around it.
2. `gate.rs` — self-contained, ~10 tests, one entry point (`observe`).
3. `signal.rs` — already independent of the run (it deliberately takes no session).
4. `feeder.rs` — small and mechanical.
5. `equivalence.rs` — decide its fate first (delete/gate/keep, above); if it
   stays, it lifts out whole, taking ~1320 lines with it. Cheapest large win.
6. `status.rs` — mechanical but large; do it alone, because a serde DTO move that
   silently changes a field name changes the API.
7. `solve.rs`, then `chain.rs`, then `deps.rs`, then
   `run.rs`/`walk.rs`/`push.rs`.

`chain.rs` sits late on purpose: W12 is the newest code in the file, so it is
where a follow-up commit is most likely to be in flight, and a rename collision
there is more expensive than in code that has settled.

Two cautions specific to this file:

- **The tests are 3062 lines and share a harness** (`FakeMic`, `FakeSession`,
  `FakeWriter`, `UnionFixture`-style builders, the paused-clock end-to-end runs).
  That harness has to land somewhere shared — `align/measure/testkit.rs` behind
  `#[cfg(test)]` — or the split will duplicate it. Decide that *before* moving the
  first test. Note the ratio: the tests are 30% of the file and outweigh every
  proposed submodule except `status.rs`, so "where does the harness live" is the
  design decision here, not an afterthought.
- **`Inner` is the shared mutable state and everything mutates it.** Keep it in
  `mod.rs` and pass `&Arc<Mutex<Inner>>` down, as today; making it public across
  submodules to avoid one parameter would undo the point of the split.

### 10.3 The other three

- **`align/calibrate.rs` → `align/calibrate/`** (now 1702 code + 1138 test lines,
  and the cluster's most coupled file at 12 module deps): `mod.rs` (AlignManager +
  Session + teardown), `audibility.rs` (the per-output `SilenceChannel`/
  level-channel resolution and its application — a self-contained decision table),
  `click.rs` (`click_wav` and the tone constants, which nothing else needs).
  Teardown stays with the session on purpose: it is one funnel every path reaches,
  and splitting it is how a restore step gets skipped.

  Its coupling is worth noting for §4: `calibrate.rs` is where alignment reaches
  into the output backends (`ap2_volume`, `sendspin_volume`, `pwsink_agent`,
  `pw_thread`, `routing_store`, `outputs_store`, `sync_group`, `player`, `wav`),
  so it is the one `align/` file **wave 4 will have to touch** when those move.
  Everything else in the cluster is insulated from that wave.
- **`align/levels.rs`**: already a pure library with no I/O; 1577 lines is not
  alarming. If split: `crosstalk.rs` (the matrix and its verdict) is the one
  genuinely separable piece. Low priority.
- **`align/relay_delay.rs`**: postdates §2's tree, which is why §3 row 26 first
  filed it under `outputs/`. It belongs in `align/` — it exists only for alignment
  (§1.1.1's provisional delays and W17's calibration mute), even though it is
  *called* from the three output relays. Do not file it under `outputs/`: the hook
  is there, the reason is not. §2 and §3 row 26 now say so.

### 10.4 What must not ride along

Same rule as §8. No behaviour change, no visibility sweep, no clippy fixes, and
in particular **no renaming of serialized fields** — `MeasureStatus` and
`AlignState` are consumed by the frontend, and a DTO move is exactly where a
field name gets "tidied". `npm run check` will not catch it; only reading the
diff will.
