# Bridge-daemon module layout — refactoring plan

Turning `bridge-daemon/src/` from a **flat list of 60 modules / ~36.8k lines**
into a directory tree that states the daemon's layering, without changing any
behaviour. This is a *move* refactor: `git mv` plus `mod` re-declarations, with
one deliberate code extraction (`state.rs`) and one file split (`api.rs`).

Read [`architecture.md`](architecture.md) first — the target tree is that
document's structure made visible on disk (§2 the graph, §3 sources, §4/§5
outputs, §6 clocks, §9 control plane). Design rationale that outlives this plan
should graduate into [`decisions.md`](decisions.md).

---

## 1. Why now, and what the evidence is

Nothing here is aesthetic. Four measurable problems, all from the current tree:

**1. Filenames carry no layering signal.** One subsystem, three spellings:
`pw_sink.rs`, `pwsink_server.rs`, `pw_sink_liveness.rs`, `pwsink_agent.rs`,
`pw_target_discovery.rs`, `pw_target_liveness.rs`, `pw_sink_spike.rs`,
`applemidi_sender.rs`. Nothing in the names says these eight are the same
backend, or that `applemidi_sender` is its transport.

**2. Four god-files.** Code lines excluding inline tests:

| file | code | tests | crate deps |
|---|---:|---:|---:|
| `api.rs` | 3659 | 224 | 45 |
| `align/measure.rs` | 3108 | 941 | 8 |
| `sync_group.rs` | 1847 | 367 | 21 |
| `align/levels.rs` | 1520 | 507 | 2 |

`api.rs` depends on 45 of the other 59 modules — it is the crate's hub, and its
`AppState` is why.

**3. The clusters already exist, undeclared.** `align_*` (6 files, 10.6k
lines), `ap2_*` (8), `sendspin_*` (6), `pw*sink*` (8), `*_store` (5). The
prefixes are a directory tree that someone typed instead of created.

**4. One module is mis-filed, and it hides a design fact.**
`sendspin_capture.rs` has **zero** crate dependencies and seven dependents,
including `ap2_server`, `pwsink_server`, `overlay_mixer` and `sync_group`. It is
the generic "capture PCM off a graph node" primitive that every output backend
builds on — its name says it belongs to one backend. Re-homing it to
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
    thread.rs             the graph thread, registry snapshot, PwCommand (22 users)
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
`store/`, `align/`, `outputs/`, `spike/`), `mod.rs` is a declaration list and a
`//!` header explaining the boundary — including, for `store/`, the
locks-only-dependency rule above.

---

## 3. Full rename table

60 files. `★` marks the two moves that change meaning rather than location;
everything else is pure relocation.

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

**Wave 0 — prerequisite.** Land the CI fmt/clippy gate green *first*. A 60-file
rename stacked on 163 pending `cargo fmt` findings produces a diff no one can
review, and `git log --follow` stops working through a rename+reformat combo.
No exceptions: this plan is worthless if its diffs can't be read.

**Wave 1** — `util/` + `audio/` + `pw/`. Leaves, no crate deps between them
except `pw/thread` → `util/locks`, `pw/profiler`.

**Wave 2** — `store/`. Depends only on `util/`.

**Wave 3** — `sources/`.

**Wave 4** — `outputs/`, in three sub-commits (`sendspin/`, `ap2/`, `pwsink/`)
plus one for the two shared files. The `ap2` cycle moves as a unit.

**Wave 5** — `routing/`, `announce/`, `align/`, `spike/`.

**Wave 6** — `state.rs` extraction, the `api/` split, and the cycle break (§5,
§6). Last, because everything else points at it.

**Wave 7** — the comment/doc reference sweep (§7), and deletion of the
transitional aliases (§4.1).

### 4.1 The zero-touch move trick

Do **not** rewrite `use` paths during waves 1–5. `git mv` the files, declare the
new modules, then add a transitional alias block at the top of `main.rs`:

```rust
// TRANSITIONAL (module-layout-plan.md §4.1) — keeps pre-move `crate::foo::…`
// paths resolving so each wave is a pure rename. Removed in wave 7.
pub(crate) use outputs::sendspin::server as sendspin_server;
pub(crate) use pw::capture as sendspin_capture;
// …one line per moved module
```

A `pub(crate) use` in the crate root creates a crate-root binding, so every
existing `crate::sendspin_server::start_server_per_device(…)` keeps resolving
unchanged. The consequence is that waves 1–5 are diffs `git` reports as **pure
renames** — reviewable by reading the tree, not the hunks — and that a wave can
be reverted without touching call sites.

Wave 7 then removes the aliases one subsystem at a time. Each removal is a
compile-error-driven `use`-path update with zero judgement calls, and the
compiler proves completeness.

---

## 5. Splitting `api.rs`

3659 code lines, 45 crate deps, 224 test lines — and 18 `// ---- section ----`
markers that are already the file boundaries. This is the cheapest of the four
god-files by a wide margin, which is why it is in scope here and the others are
not (§8).

| target | current lines | content |
|---|---|---|
| `state.rs` | 45–140 | `AppState` + the `Shared*` aliases ★ |
| `api/mod.rs` | 141–318 | `router()` — the route table only |
| `api/static_files.rs` | 366–419 | `StaticAssets`, content types, SPA fallback |
| `api/nodes.rs` | 319–475 | `/health`, `/api/nodes`, manual `/api/links` |
| `api/outputs.rs` | 476–1171 | the outputs listing + adopt/ignore/rename/latency/codec |
| `api/clients.rs` | 1172–1379 | per-receiver client registry, ban/priority/disconnect |
| `api/now_playing.rs` | 1380–1506 | now-playing get/put/report/artwork |
| `api/sources.rs` | 1507–1786 | RTP target + multi-source CRUD |
| `api/volume.rs` | 1787–1926 | sendspin + AP2 per-device volume/mute |
| `api/agents.rs` | 1927–1979 | receiver-agent pairing + WS |
| `api/sync.rs` | 1980–2167 | group lead, per-device delay, `lead_floor` |
| `api/settings.rs` | 2168–2251 | `/api/settings`, `/api/status` |
| `api/spike.rs` | 2252–2537 | every `/api/spike/*` handler |
| `api/announce.rs` | 2538–2698 | `ag_announce` + PCM acquisition |
| `api/duck.rs` | 2699–2856 | duck holds |
| `api/groups.rs` | 2857–3061 | music + announcement groups |
| `api/align.rs` | 3062–3213 | manual alignment (`align/calibrate.rs`) |
| `api/measure.rs` | 3214–3659 | mic-assisted measurement |

The 224 inline test lines split by subject: the route-table shape tests stay in
`api/mod.rs`, the rest follow their handlers.

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

Module filenames are load-bearing prose in this codebase:

- **333** `*.rs` references in source comments (`// see sendspin_server.rs`),
- **185** in `docs/*.md` — `calibrate.rs` ×11, `sync_group.rs` ×10, `api.rs`
  ×10, `pw_thread.rs` ×7, `pwsink_server.rs` ×7, and on down.

Handle it as a scripted `sed` pass driven by the §3 table, in its own commit,
after the moves are done. Two rules keep it mechanical: leaf basenames stay
recognizable (`sendspin_server.rs` → `outputs/sendspin/server.rs`, never
`ss_srv.rs`), and references become path-qualified (`outputs/sendspin/server.rs`,
not bare `server.rs`) — otherwise a comment saying "see `server.rs`" is now
ambiguous across four backends.

Check afterwards: `grep -roh '[a-z0-9_]*\.rs' src docs` should surface no name
that no longer exists on disk. `docs/architecture.md`, `docs/decisions.md` and
`docs/mic-alignment-plan.md` carry the densest references and deserve a read,
not just a sed.

---

## 8. Non-goals

**The other three god-files.** `align/measure.rs` (3108 code + 941 test lines),
`sync_group.rs` (1847), `align/levels.rs` (1520) need real decomposition
thought, and they must not ride along in a rename commit. They land in
`align/` and `routing/` at full size; splitting them is separate, later work
against a tree where their neighbours are already visible.

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

Per wave:

- `cargo build` and `cargo test` green (baseline: 104 tests passing).
- `cargo fmt --check` clean — and *no* reformatting inside moved files.
- `git show --stat -M` reports the wave's files as renames, not
  delete+add. If it doesn't, content was edited during a move: split the commit.
- `git log --follow <new path>` reaches the file's pre-move history.

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

§1's table is out of date — the cluster grew while the feature was built
(measured 2026-08-11):

| file | total | code | test |
|---|---:|---:|---:|
| `align/measure.rs` | 5830 | **4075** | 1755 |
| `align/levels.rs` | 2028 | 1521 | 507 |
| `align/calibrate.rs` | 2086 | 1284 | 802 |
| `align/relay_delay.rs` | 1364 | 868 | 496 |
| `align/estimator.rs` | 1501 | 951 | 550 |
| `align/group.rs` | 1204 | 862 | 342 |
| `align/mic.rs` | 699 | 520 | 179 |

~14.7k lines total, and `align/measure.rs` alone is now larger than `api.rs` was
when §1 was written. It is the priority; the rest are defensible at their size.

### 10.2 `align/measure.rs` → `align/measure/`

The file has **one** reason to be large: it holds the run's *reported shape*, its
*seams*, its *pure arithmetic* and its *driver* in one place. Those four have
different reasons to change, which is the whole test for a split.

```
align/measure/
  mod.rs          MeasureManager + Inner + the public entry points
  status.rs       the reported DTOs: Phase, MeasureStatus, MemberProgress,
                  Proposal, ProposedDelay, Checks, Verification, WalkProgress,
                  GateProgress, Refusal(+Kind), Warning(+Kind)
  deps.rs         the seams: SessionControl, MicFeed, DelayWriter, MeasureDeps,
                  LiveMic, SendAheadContext
  gate.rs         Gate, GateConfig, GateSample, GateReason, GateStep
  feeder.rs       Feeder — the mic-window → estimator pump
  knobs.rs        KnobPolarity, Knob, knob_of, MemberInterval, choose_target
  solve.rs        solve, fit_drift, transitivity, residual
  run.rs          run_measure / run_apply / measure_member / measure_passes
  walk.rs         near-field arrival + close orchestration
  signal.rs       signal_check — the pre-flight verdict
  equivalence.rs  the relay-vs-device measurement (W21)
  push.rs         measure_ws + the change notifier
```

**Extract in this order**, because it is the order of decreasing risk — each of
the first four is pure, densely tested, and has almost no inbound coupling, so
the compiler proves the move:

1. `knobs.rs` — the §2.4.2 interval model. Pure arithmetic, no I/O, no state. The
   single highest-value extraction: it is where the sign bug lived, and it should
   be readable without the run around it.
2. `gate.rs` — self-contained, ~10 tests, one entry point (`observe`).
3. `signal.rs` — already independent of the run (it deliberately takes no session).
4. `feeder.rs` — small and mechanical.
5. `status.rs` — mechanical but large; do it alone, because a serde DTO move that
   silently changes a field name changes the API.
6. `solve.rs`, then `deps.rs`, then `run.rs`/`walk.rs`/`equivalence.rs`/`push.rs`.

Two cautions specific to this file:

- **The tests are 1755 lines and share a harness** (`FakeMic`, `FakeSession`,
  `FakeWriter`, `UnionFixture`-style builders, the paused-clock end-to-end runs).
  That harness has to land somewhere shared — `align/measure/testkit.rs` behind
  `#[cfg(test)]` — or the split will duplicate it. Decide that *before* moving the
  first test.
- **`Inner` is the shared mutable state and everything mutates it.** Keep it in
  `mod.rs` and pass `&Arc<Mutex<Inner>>` down, as today; making it public across
  submodules to avoid one parameter would undo the point of the split.

### 10.3 The other three

- **`align/calibrate.rs` → `align/calibrate/`**: `mod.rs` (AlignManager + Session +
  teardown), `audibility.rs` (the per-output `SilenceChannel`/level-channel
  resolution and its application — a self-contained decision table), `click.rs`
  (`click_wav` and the tone constants, which nothing else needs). Teardown stays
  with the session on purpose: it is one funnel every path reaches, and splitting
  it is how a restore step gets skipped.
- **`align/levels.rs`**: already a pure library with no I/O; 1521 lines is not
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
