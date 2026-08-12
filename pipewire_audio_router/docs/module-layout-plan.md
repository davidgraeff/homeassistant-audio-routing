# Bridge-daemon module layout — the refactor, and what it taught

`bridge-daemon/src/` was a flat list of 60 modules whose filenames carried no
layering signal. It is now a directory tree that states the daemon's layering,
with no behaviour change anywhere. This file records **what the tree is**, **why
it is shaped that way**, and **the handful of things the move taught** — the kind
of thing the next large rename in this repo should read first.

The layering itself is described in [`architecture.md`](architecture.md)
(§1.1 the tree, §2 the graph, §3 sources, §4/§5 outputs, §6 clocks, §9 control
plane). Rationale that outlives this file lives in
[`decisions.md`](decisions.md#module-layout-directories-that-state-the-layering).

---

## 1. Outcome

**Complete, 2026-08-12.** `bridge-daemon/src/` is **12 directories and three root
files** (`main.rs`, `state.rs`, `supervisor.rs`). It landed in seven waves, each
one commit, each compiling and testing green on its own.

| directory | what |
|---|---|
| `pw/` | PipeWire plumbing, no product concepts: the graph thread, capture, clip playback, the Profiler FFI, peak taps |
| `audio/` | format/DSP helpers with zero crate deps: resample, wav, decode |
| `util/` | `LockRecover`, the node-name prefixes, the host RT-capability assessment |
| `store/` | file-backed JSON with no live handles — routing, outputs, groups, settings, the one-shot raop→ap2 migration |
| `sources/` | audio into the graph: the AirPlay receiver + its client registry, RTP, now-playing, BT-bridge discovery. `mod.rs` *is* the source collection and its reconciler |
| `outputs/` | three backends (`sendspin/`, `ap2/`, `pwsink/`) plus the shared overlay mixer and the listing model |
| `routing/` | the matrix and its handlers (`mod.rs`), `sync_group.rs`, `sync_settings.rs` |
| `announce/` | announcement delivery (`mod.rs`) and the pure scheduling decisions (`arbiter.rs`) |
| `align/` | the manual and mic-assisted alignment cluster |
| `api/` | the route table (`mod.rs`) plus one module per resource |
| `spike/` | dev-only experiments reachable via `/api/spike/*` |

Verified identically after every wave, and stated in each commit message:
`cargo test` **450 passed / 0 failed / 3 ignored**, `cargo clippy --all-targets`
at the same **6** pre-existing warning sites (2 `derivable_impls`,
4 `chunks_exact`), `cargo fmt --check` clean, `cargo check --tests` clean, and
`git show --stat -M` reporting each wave's files as renames rather than
delete+add. The only code that moved between modules is what §4 called for.

### Two moves that changed meaning, not just location

- **`sendspin_capture.rs` → `pw/capture.rs`.** It has zero crate dependencies
  and six dependents (all three output backends, the overlay mixer, sendspin's
  codec, and `align/measure`). It is the generic "capture PCM off a graph node"
  primitive every backend builds on; its name claimed it belonged to one of them.
  This was the single highest-information move in the refactor.
- **`config.rs` → `util/node_names.rs`.** It holds no configuration — it holds
  the `*-dev-` / `*-out-` node-name prefix constants, and the daemon has no
  `options.json` seeding at all.

Three further renames exist only to stop names lying: `pw_sink.rs` →
`outputs/pwsink/module_args.rs` (its entire content is the two SPA-JSON `args`
strings), and `pw_sink_liveness` / `pw_target_liveness` → `sender_liveness` /
`target_liveness` (one watches our AppleMIDI sender, the other watches the remote
host; the old pair was indistinguishable by name). Everything else was pure
relocation into the prefix clusters that already existed undeclared — `ap2_*`,
`sendspin_*`, `pw*sink*`, `*_store`, `align_*`. `git log --follow <path>` reaches
every file's pre-move history, which is where the full rename table now lives.

### Deliberate exceptions to the obvious grouping

- **`sources_store.rs` became `sources/mod.rs`, not `store/sources.rs`.** It
  depends on the AirPlay and RTP receivers because it reconciles live handles: a
  supervisor that happens to persist.
- **`sync_settings.rs` stays in `routing/`, not `store/`.** It pushes settings
  into the AP2 server, the AppleMIDI sender and the sendspin codec on write.
  `store/` is reserved for modules whose only dependency is `util/locks` — that
  invariant is what makes the directory mean something, and `store/mod.rs`'s
  header carries it.
- **`relay_delay.rs` lives in `align/`, not `outputs/`.** It exists only for
  alignment (provisional delays and the calibration mute) even though it is
  *called* from the three output relays. The hook is in `outputs/`; the reason is
  not. This is the single upward arrow in the tree and it is intentional — do not
  "fix" it by dragging the file into `outputs/`.

### Directory module style

`foo/mod.rs`, not `foo.rs` + `foo/`: it keeps a subsystem in exactly one
directory, which is the point of the exercise. Where the parent module has real
content (`routing/`, `sources/`, `announce/`) that content *is* the `mod.rs`;
elsewhere `mod.rs` is a declaration list. Children are `pub(crate) mod` —
nothing here is a library surface, and `pub` on a child of a `pub(crate)` parent
invites the "is this exported?" question every time.

Either way each `mod.rs` carries a `//!` header that states the boundary and
names each child's role. `align/mod.rs` is the pattern to copy: it says what
alignment *is not* ("never carrying audio"), walks the pipeline
`calibrate → mic → estimator → levels → measure` in order, and justifies the one
member whose placement is arguable. **That header is the payoff of the whole
refactor** — the flat list had nowhere to put it. A declaration list without one
is a directory that explains nothing, which is where this started.

---

## 2. What the evidence was

Recorded because it is the shape of argument that justified the churn, and
because two of the four problems are still measurable today.

1. **Filenames carried no layering signal.** One subsystem, three spellings:
   `pw_sink.rs`, `pwsink_server.rs`, `pw_sink_liveness.rs`, `pwsink_agent.rs`,
   `pw_target_discovery.rs`, `pw_target_liveness.rs`, `pw_sink_spike.rs`,
   `applemidi_sender.rs`. Nothing in the names said those eight were one backend,
   or that `applemidi_sender` was its transport.
2. **Five god-files**, `api.rs` at 3857 code lines reaching 46 of the other 60
   modules — the crate's hub, and its `AppState` was why.
3. **The clusters already existed, undeclared** (the prefixes above): a tree
   someone had typed instead of created.
4. **One module was mis-filed and it hid a design fact** — `sendspin_capture.rs`,
   above.

The dependency graph was otherwise a clean DAG. The exceptions were thin:
`routing.rs` → `api.rs` used exactly three items, `pwsink_agent.rs` → `api.rs`
exactly one, and `ap2_server ↔ ap2_health ↔ ap2_liveness` is a genuine three-way
cycle contained entirely inside `outputs/ap2/`. Rust permits module cycles inside
a crate, so none of them blocked the move; they only mattered for §4.

---

## 3. The four lessons

**A move is textual, not structural.** The `align/` wave went first, out of the
planned order, and it was not a leaf when it did: eight files outside `align/`
reached into it, `relay_delay` alone with 28 references across six files. It
worked anyway. Rust does not care about module cycles inside a crate, so
dependency direction does not constrain wave order at all. What constrains it is
**how many call sites a wave has to edit** and **how likely those files are to
have concurrent edits in flight**. The one ordering that genuinely held was
`api/` last, because it is the only wave that also *changes* code (§4).

**"Baseline unchanged" beats "clean".** The original instruction was to land the
CI fmt/clippy gate green before any wave. That gate was still red when the first
wave landed, the move went ahead, and what made it reviewable was *recording the
four numbers and proving them unchanged* — not having no findings to begin with.
A wave that silences a pre-existing finding is as suspect as one that adds one.
Two consequences worth reusing: state the numbers in the commit message so the
next wave can diff against them without re-deriving, and fix pre-existing `fmt`
findings **in a separate commit before** the wave that moves those files, or
"baseline unchanged" turns into an argument about whether the diff was a reformat
or a move. A move must also not change the *test count*: a drop means a
`#[cfg(test)]` module lost its `mod` declaration in the shuffle, which compiles
silently (§5.3).

**Transitional aliases are optional, and were never needed.** The trick — `git
mv`, declare the new modules, then `pub(crate) use outputs::sendspin::server as
sendspin_server;` in the crate root so pre-move paths keep resolving — makes a
wave a diff `git` reports as a *pure rename*, reviewable by reading the tree
instead of the hunks, and revertible without touching call sites. No wave used
it: each rewrote its call sites in the same commit and stayed reviewable (align:
24 files, +248/−225, all seven renames detected, one at zero changed lines). The
deciding number is call-site fan-in, not subsystem size: **rewrite directly under
~10 dependent files; alias above that, or whenever another session is editing the
same files** — a rename commit touching 29 files is a merge-conflict generator
where the alias version touches one. What is *not* optional is updating the
comment references inside the moved files in the move commit; deferring them
means the tree spends the whole refactor describing itself wrongly, and the later
sweep then has to distinguish "stale because not yet moved" from "stale because
missed".

**Module filenames are load-bearing prose here, so the reference sweep is real
work.** At the start there were 350 `*.rs` references in source comments, 188 in
`pipewire_audio_router/docs/*.md` and 26 in the repo-root `docs/*.md` — the last
group is the one nearly missed, because `docs/api-reference.md` lives outside
this directory. Rules that made it mechanical: leaf basenames stay recognisable
(`sendspin_server.rs` → `outputs/sendspin/server.rs`, never `ss_srv.rs`), and
every reference becomes **path-qualified**, because a comment saying "see
`server.rs`" is ambiguous across four backends. Three exclusions:
`pipewire_audio_router/docs/old/` is a historical archive that describes the old
tree correctly; a module's own name inside its own file is not stale; and four
references deliberately keep an old name because their whole job is to explain a
rename (`state.rs` on `api.rs`, `pw/mod.rs` on `sendspin_capture.rs`,
`util/mod.rs` on `config.rs`, `outputs/pwsink/mod.rs` on the two liveness files).

Beware the measurement, too — `grep -c 'calibrate\.rs'` counts the *correct*
`align/calibrate.rs`, and reading that as 22 stale references is how you invent
work that does not exist. Drive the check from the **old** names rather than from
"does this basename exist under `src/`" (the existence test flagged 25 files in
other trees — the sendspin submodule, `vendor/`, `pw-control`, the Pi firmware —
as false positives):

```sh
OLD='align_measure|align_estimator|align_levels|align_group|align_mic|calibrate|relay_delay'
grep -rnE "(^|[^/[:alnum:]_])($OLD)\.rs" bridge-daemon/src docs ../docs \
  | grep -v 'docs/old/'
```

The leading `[^/]` is what makes it useful: it matches bare `calibrate.rs` and
skips the already-correct `align/calibrate.rs`. `architecture.md`, `decisions.md`
and the alignment write-up carry the densest references and deserve a read rather
than a `sed` — they explain *mechanisms* by filename, so a mechanical rewrite can
leave a sentence that parses and no longer means anything. One file type the
sweep missed and a later pass caught: the **`.drawio` diagrams**, whose node
labels name modules too and which no text grep over `*.md`/`*.rs` sees.

---

## 4. Splitting `api.rs`, and breaking the one cycle

`api.rs` was 3857 code lines with 46 module deps — and 18 `// ---- section ----`
markers that were already the file boundaries, which is why it was the one
god-file in scope. It is now `state.rs` plus 17 modules under `api/`: `mod.rs`
(the route table and nothing else), then `nodes`, `static_files`, `outputs`,
`clients`, `now_playing`, `sources`, `volume`, `agents`, `sync`, `settings`,
`spike`, `announce`, `duck`, `groups`, `align`, `measure`. Inline tests followed
their handlers; the route-table shape tests went to `api/tests.rs` (§5).

Two ordering facts worth keeping: **extract `AppState` first** and let it compile
crate-wide, then carve handlers out — reversing it means every carved file needs
an `AppState` import that moves again a commit later. And derive the boundaries
from the `// ---- ` markers at cut time rather than from line numbers, which go
stale within a day on a file under active development.

The same commit broke the api ↔ routing cycle: `AppState` to `state.rs` (which
alone resolved `pwsink_agent.rs` → `api.rs` and two thirds of `routing.rs` →
`api.rs`), and the output-listing model to `outputs/listing.rs`, which
`routing/mod.rs` may depend on downward. **`api/` is now a leaf** — nothing in
the crate depends on it. Worth doing even though Rust tolerates the cycle,
because it makes "handlers depend on subsystems, never the reverse" enforceable
by eye, and because it is the precondition for a future crate split if build
times ever justify one: `util/`, `audio/`, `pw/`, `store/` and `align/` are
already near-leaf, and `api/` was the only thing in the way. The `outputs/ap2/`
three-way cycle was left alone — untangling it is behaviour work, not layout
work.

### What was deliberately left out

**Behaviour, signatures, visibility tightening.** No `pub` → `pub(crate)`
sweeps, no dead-code removal, no clippy fixes inside moved files: each makes a
rename diff unreviewable, and each is cheap afterwards.

**Feature-gating `spike/`.** Grouping the three modules makes
`#[cfg(feature = "spike")]` easy later and the `/api/spike/*` routes are the only
callers. Worth doing; not part of a move.

**The vendored trees.** `vendor/` and the sendspin submodule are untouched, and
the `[workspace] exclude` + crate-boundary lint convention stay exactly as they
are.

**The remaining god-files** — see §6.

---

## 5. Where tests live

Tests are 10 338 of the crate's 46 157 lines. Most of them should stay exactly
where they are — a small `#[cfg(test)] mod tests` next to a tricky function is the
best documentation that function will ever have. The rule is about the handful that
had grown past that:

**Extract when a test module passes ~300 lines, or when it pushes its file past
~1500.** That was seven files; the other 80 keep their inline tests.

| file | before | after | tests |
|---|---:|---:|---|
| `align/measure/mod.rs` | 10092 | 7032 | `measure/tests/` — 9 files by subject |
| `align/calibrate.rs` | 2841 | 1704 | `calibrate/tests.rs` |
| `routing/sync_group.rs` | 2216 | 1851 | `sync_group/tests.rs` |
| `align/levels.rs` | 2153 | 1579 | `levels/tests.rs` |
| `align/estimator.rs` | 1501 | 953 | `estimator/tests.rs` |
| `align/relay_delay.rs` | 1388 | 894 | `relay_delay/tests.rs` |
| `align/group.rs` | 1125 | 810 | `group/tests.rs` |
| `api/mod.rs` | 485 | 263 | `api/tests.rs` — by the rule below |

`api/mod.rs` came in under a second rule worth keeping: **tests outweighing the
code in a file whose job is something else.** Its 221 test lines were all
route-table shape assertions, and removing them leaves a file that is only the
route table.

### 5.1 Mechanics

A file may have a sibling directory of the same name, so nothing needs renaming to
`mod.rs` and no `#[path]` attribute is involved:

```rust
// align/calibrate.rs
#[cfg(test)]
mod tests;                    // -> align/calibrate/tests.rs
```

**One `tests.rs` needs no visibility changes at all.** It is a *direct* child of
the module under test, so `use super::*;` means what it meant inline and every
private item stays reachable. Prefer this shape whenever one file will do.

**A subject split costs `pub(super)`.** `align/measure/tests/{gate,solve,…}.rs`
are grandchildren of `measure`, so they reach the code under test through
`use super::super::*;` — still fine, private items included — but the shared
fixtures now live in a *sibling* module (`tests/harness.rs`), and sharing them
means `pub(super)` on each one (88 items and fields for measure's harness). Only
worth it when the tests are big enough to want an index, which in practice meant
one file out of 87.

Put **every** support item in `harness.rs`, including ones only one subject uses.
The alternative is deciding, per fixture, whether a second subject might want it
later — 50 judgement calls with no way to be right.

### 5.2 What does not work

A crate-root `tests/` directory. `bridge-daemon` is a bin-only crate: integration
tests cannot link it, and they would see only the public API while nearly
everything is `pub(crate)`. Extraction has to stay in-crate.

### 5.3 The check that matters

`cargo test` must report the **same count** before and after — 450 passed / 0
failed / 3 ignored. A dropped `mod tests;` declaration does not fail the build or
the suite; it silently stops running those tests, and the only signal is the total.

---

## 6. Still outstanding: decomposing `align/measure/mod.rs`

`align/measure/mod.rs` is the one file where size is still a real problem: **7032
lines**, the largest in the crate by a factor of nearly four, and it grew ~3.9k
lines *during* this refactor (multi-position chaining plus the relay-vs-device
equivalence experiment). The rest of the cluster is defensible at its size:
`routing/sync_group.rs` 1851, `align/calibrate.rs` 1704, `align/levels.rs` 1579.
The growth rate is the argument — a file that adds 4k lines in a week will not
get easier to split later.

**The test split (§5) was the first step, and it doubles as a dry run of the code
split.** The subjects the tests naturally fell into — `chain`, `equivalence`,
`gate`, `knobs`, `run`, `signal`, `solve`, `walk` — are exactly the submodule
seams below, which is the strongest evidence available that they are the right
ones. It also forced the harness question (`FakeMic`, `FakeSession`,
`FakeWriter`, the paused-clock end-to-end runs) to be answered *before* any code
moves; otherwise a code split duplicates it.

The file has **one** reason to be large: it holds the run's *reported shape*, its
*seams*, its *pure arithmetic* and its *driver* in one place, and those four have
different reasons to change — which is the whole test for a split. Extract in
this order, decreasing risk: each of the first four is pure, densely tested, and
has almost no inbound coupling, so the compiler proves the move.

1. **`knobs.rs`** — the feasible-interval model (`KnobPolarity`, `Knob`,
   `knob_of`, `MemberInterval`, `choose_target`). Pure arithmetic, no I/O, no
   state, and the highest-value extraction: it is where the sign bug lived, and it
   should be readable without the run around it.
2. **`gate.rs`** — self-contained, one entry point (`observe`).
3. **`signal.rs`** — already independent of the run (it deliberately takes no
   session).
4. **`feeder.rs`** — the mic-window → estimator pump; small and mechanical.
5. **`equivalence.rs`** (~1300 lines) — the relay-vs-device experiment, with its
   own manager, two endpoints and WebSocket. It is *not* part of a measurement
   run. **Decide its fate before investing in splitting it**: once the question it
   answers is recorded in `decisions.md`, it is a candidate for deletion or for
   the same treatment as `spike/`, which takes ~19% off the file with no
   decomposition work at all.
6. **`status.rs`** — the reported DTOs (`Phase`, `MeasureStatus`,
   `MemberProgress`, `Proposal`, `Checks`, `Verification`, `WalkProgress`,
   `Refusal`, `Warning`). Mechanical but large; do it alone, because a serde DTO
   move that silently renames a field changes the API.
7. **`solve.rs`**, then **`chain.rs`**, then **`deps.rs`** (the seams:
   `SessionControl`, `MicFeed`, `DelayWriter`, `MeasureDeps`, `LiveMic`), then
   `run.rs` / `walk.rs` / `push.rs`.

`chain.rs` sits late on purpose: it is the newest code in the file, so it is
where a follow-up commit is most likely to be in flight, and a rename collision
there is more expensive than in code that has settled. Keep its pure step
arithmetic ahead of its async driver within the file — the arithmetic is what has
tests, the driver is what has `await` points. And keep `Inner` in `mod.rs`,
passing `&Arc<Mutex<Inner>>` down as today: making it public across submodules to
avoid one parameter would undo the point of the split.

The other three, for completeness. **`align/calibrate.rs`** would split into
`mod.rs` (manager + session + teardown), `audibility.rs` (the per-output silence
and level channel resolution — a self-contained decision table) and `click.rs`
(the tone constants, which nothing else needs), with teardown staying with the
session on purpose: it is the one funnel every path reaches, and splitting it is
how a restore step gets skipped. It is also the cluster's most coupled file — it
is where alignment reaches into the output backends — so it is the one `align/`
file a future `outputs/` reshuffle would have to touch. **`align/levels.rs`** is
already a pure library with no I/O; `crosstalk.rs` is the one genuinely separable
piece, low priority. **`align/relay_delay.rs`** stays where it is (§1).

Same rules as §4 apply throughout: no behaviour change, no visibility sweep, no
clippy fixes, and in particular **no renaming of serialized fields** —
`MeasureStatus` and `AlignState` are consumed by the frontend, and a DTO move is
exactly where a field name gets "tidied". `npm run check` will not catch it; only
reading the diff will.
