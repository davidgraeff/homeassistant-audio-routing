# Multiple, dynamically-added inputs (AirPlay + RTP) — implementation plan

## Goal

Today the daemon has **exactly one** AirPlay-receive source and **exactly one**
RTP-receive source, both fixed-name and configured through singular endpoints.
Move to a model where:

- there are **zero** inputs on a fresh install,
- the user can **add / remove** inputs at runtime,
- of **either type** (AirPlay-receive or RTP-receive),
- **more than one per type** (e.g. "Kitchen AirPlay" + "Bathroom AirPlay",
  two RTP bridges), each independently routable,
- and the set is **persisted** across restarts.

The payoff is real: multiple named AirPlay endpoints become independent input
"zones" a phone can pick from, each routed to a different output set; multiple
RTP inputs let several Bluetooth bridges (or other RTP senders) coexist.

## Key insight: the audio graph is already source-agnostic

Almost none of the work is in the audio path. Everything downstream of a source
already keys on the node's **name**, not on "the one source":

- **Routing** classifies a source as *any node with a non-monitor output port*
  (`routing.rs` `build_matrix`), so an Nth source node shows up in the matrix
  automatically — no per-source special-casing.
- **Metering** taps whatever present sources exist (`metering.rs`
  `reconcile_sources`) — already a set.
- **Sync anchors / grouping** key on source-*sets* (`sync_group.rs`
  `source_set_of`) — multiple sources already handled.
- **Routing intent** persists links by stable node name (`routing_store.rs`).

The only fixed-name coupling in the graph layer is **cosmetic**: a display-name
override for `airplay-in` and a per-node latency-estimate lookup
(`routing.rs` `node_latency_ms`, matching `AIRPLAY_NODE_NAME` /
`RTP_SOURCE_NODE_NAME`). Both generalize trivially.

⇒ The work is entirely in the **management layer** (config store, lifecycle,
API, UI) that currently hard-codes "one of each." This is a bounded refactor,
not an architectural change.

## Current singleton assumptions (what has to change)

| Layer | Today | Target |
|---|---|---|
| `sources_store.rs` | `airplay_source_name: Option<String>` + `rtp_source: Option<RtpSourceConfig>` | keyed collections `BTreeMap<SourceId, SourceConfig>` |
| Node names | consts `AIRPLAY_NODE_NAME="airplay-in"`, `RTP_SOURCE_NODE_NAME="bt-bridge-rtp"` | per-instance `airplay-in-<slug>` / `rtp-in-<slug>` |
| AirPlay lifecycle | `SharedAirplay = Arc<Mutex<Option<AirplayHandle>>>`, one `RaopServer` on fixed `AIRPLAY_PORT=5000` | `BTreeMap<SourceId, AirplayHandle>`, **port allocation** per instance |
| RTP lifecycle | one module, loaded by fixed node name (`PwCommand::Load` already keyed) | iterate collection; distinct listen port per instance |
| `rtp_membership.rs` (IGMP watchdog) | assumes the single `RTP_SOURCE_NODE_NAME` | one keepalive+watch per multicast RTP instance |
| AirPlay clients / ban / takeover | one global registry (`airplay_clients.rs`, `/api/source/airplay/clients/*`) | per-receiver (design decision — see Open questions) |
| API | `/api/source/airplay`, `/api/source/rtp` (singular PUT/DELETE) | collection CRUD `/api/sources` |
| UI | `SourcesTab.svelte`: two fixed panels | list + "add source (type)" + per-instance cards |
| `routing.rs` cosmetics | fixed-name display + latency lookup | keyed by source id / config |

## Data model

A single tagged collection keyed by a stable id (slug), persisted in
`sources.json`:

Stored shape (`SourceEntry` with `#[serde(flatten)]` config, internally tagged
by `kind` — this is normative; the HTTP `SourceView` response is the *nested*
form with `airplay`/`rtp` sub-objects, see the Interfaces section):

```jsonc
{
  "sources": [
    { "id": "kitchen-airplay", "label": "Kitchen AirPlay", "kind": "airplay",
      "latency_msec": 100, "auth_setup": false, "prevent_takeover": true, "port": 5000 },
    { "id": "bt-bridge", "label": "Bluetooth Bridge", "kind": "rtp",
      "port": 46000, "latency_msec": 200, "source_addr": "239.255.42.42",
      "ignore_ssrc": true, "rate": 48000 }
  ]
}
```

- **`id`** — stable slug, also the basis for the PipeWire node name
  (`airplay-in-<id>` / `rtp-in-<id>`) and the routing key. Immutable once
  created; generated from the label on add (collision-suffixed).
- **`label`** — user-facing display name (drives the routing-matrix
  `display_name`, replacing the fixed `airplay-in` override).
- **`kind`** + a per-kind config sub-object (reuse today's `RtpSourceConfig`
  fields verbatim; the AirPlay config gathers the four scattered `airplay_*`
  fields into one struct).
- The AirPlay **service name** advertised over mDNS = `label` (what the phone
  sees), independent of `id`.

### Node naming

`rtp_source.rs` / `airplay_source.rs` take the node name as a **parameter**
instead of a const. Keep the old consts as the default id for the migration
only. Routing already tolerates arbitrary names, so nothing else changes.

## Constraints that are physics/protocol, not code

- **AirPlay RTSP port.** Each `RaopServer` binds one RTSP port
  (`AIRPLAY_PORT=5000`). N receivers need **distinct ports** — add a small port
  allocator (base 5000, walk upward, skip in-use), stored per source so it's
  stable across restarts. Each also needs a unique mDNS name (the label) and
  hwaddr (already `derive_hwaddr(name)` — unique names give unique hwaddrs).
  Verify the vendored `RaopServer` supports N concurrent instances in one
  process (builder is already per-instance; expected fine — confirm in a spike).
- **RTP listen port.** Each RTP instance needs a distinct UDP port (or a
  distinct multicast group) — already per-source config, so no new constraint,
  just validation that two enabled RTP sources don't collide on port.

## Interfaces (the contract)

These are frozen so phases can be built in parallel against them. Field names
are normative (they cross the HTTP + serde boundary).

### Rust — source model (`sources_store.rs`)

```rust
/// Stable id = slug, also the base of the PipeWire node name and the routing
/// key. Immutable once created.
pub type SourceId = String;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind { Airplay, Rtp }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AirplaySourceConfig {
    #[serde(default = "default_airplay_latency_msec")] pub latency_msec: u32,
    #[serde(default)]                                  pub auth_setup: bool,
    #[serde(default = "default_true")]                 pub prevent_takeover: bool,
    /// RTSP port. Allocated on add (base 5000, first free), persisted so it is
    /// stable across restarts. 0 = "allocate on next load".
    #[serde(default)]                                  pub port: u16,
}

// RtpSourceConfig is UNCHANGED (port, latency_msec, source_addr, ignore_ssrc, rate).

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConfig { Airplay(AirplaySourceConfig), Rtp(RtpSourceConfig) }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceEntry {
    pub id: SourceId,
    pub label: String,
    #[serde(flatten)] pub config: SourceConfig,   // {"kind":"airplay"|"rtp", ...}
}
```

Persisted shape (`sources.json`): `{ "sources": [SourceEntry, ...] }`.
"Present in the list" = enabled (no separate flag), matching today's `Option`
semantics.

### Rust — node naming (`sources_store.rs`, pure fn)

```rust
pub const LEGACY_AIRPLAY_ID: &str = "airplay-in";
pub const LEGACY_RTP_ID: &str     = "bt-bridge-rtp";

/// AirPlay → "airplay-in-<id>", RTP → "rtp-in-<id>", EXCEPT the two legacy ids
/// map to the bare legacy names so existing routing.json links keep resolving.
pub fn source_node_name(kind: SourceKind, id: &str) -> String;
```

### Rust — `SourcesStore` API (`sources_store.rs`)

```rust
impl SourcesStore {
    pub fn load(path: &Path) -> anyhow::Result<Self>;   // migrates legacy single-field shape on load
    pub fn list(&self) -> Vec<SourceEntry>;             // sorted by label
    pub fn get(&self, id: &str) -> Option<SourceEntry>;
    /// Slugify `label` → unique id (collision-suffixed); allocate an AirPlay
    /// port if unset; validate RTP port uniqueness; persist; return the entry.
    pub fn add(&mut self, label: String, config: SourceConfig) -> anyhow::Result<SourceEntry>;
    /// `id` and `kind` are immutable; `label` and same-kind config are updatable.
    pub fn update(&mut self, id: &str, label: Option<String>, config: Option<SourceConfig>) -> anyhow::Result<SourceEntry>;
    pub fn remove(&mut self, id: &str) -> anyhow::Result<bool>;

    // --- BACK-COMPAT SHIMS (keep the crate compiling until Phases 2-4 land) ---
    // Implemented over the collection using the legacy ids, so existing call
    // sites in main.rs/api.rs/routing.rs keep working unchanged. Removed in
    // Phase 6 cleanup.
    pub fn airplay_source_name(&self) -> Option<&str>;   // label of LEGACY_AIRPLAY_ID entry
    pub fn airplay_latency_msec(&self) -> u32;
    pub fn airplay_auth_setup(&self) -> bool;
    pub fn airplay_prevent_takeover(&self) -> bool;
    pub fn rtp_source(&self) -> Option<RtpSourceConfig>; // config of LEGACY_RTP_ID entry
    pub fn set_airplay_source_name(&mut self, name: Option<String>) -> anyhow::Result<()>;
    pub fn set_airplay_latency_msec(&mut self, msec: u32) -> anyhow::Result<()>;
    pub fn set_airplay_auth_setup(&mut self, enabled: bool) -> anyhow::Result<()>;
    pub fn set_airplay_prevent_takeover(&mut self, enabled: bool) -> anyhow::Result<()>;
    pub fn set_rtp_source(&mut self, cfg: Option<RtpSourceConfig>) -> anyhow::Result<()>;
}
```

### Rust — per-type lifecycle (Phases 2 & 4)

```rust
// airplay_source.rs — take the node name, service name, and RTSP port as params
// (drop the AIRPLAY_NODE_NAME / AIRPLAY_PORT consts as the source of truth):
pub async fn start(
    node_name: String, service_name: String, port: u16,
    latency_msec: u32, auth_setup: bool,
    clients: SharedAirplayClients, prevent_takeover: SharedPreventTakeover,
) -> anyhow::Result<AirplayHandle>;

// rtp_source.rs — args already parameterized; add the node name:
pub fn rtp_source_module_args(node_name: &str, port: u16, latency_msec: u32,
                              source_addr: &str, ignore_ssrc: bool, rate: u32) -> String;
```

Two **independent** reconcilers (NOT a shared struct — keeps file ownership
disjoint), each called from `main.rs` on the source-set changing:

- `airplay lifecycle` (Phase 4): owns `BTreeMap<SourceId, AirplayHandle>`; starts
  missing, stops removed, restarts changed. Lives in `airplay_source.rs`.
- `rtp lifecycle` (Phase 2): loads/unloads one module per RTP entry keyed by
  node name (pw_thread already keys `Load`/`Unload` by node name). Lives in
  `rtp_source.rs`. Generalizes `rtp_membership.rs` to one keepalive+watch per
  multicast RTP entry.

### HTTP API — collection CRUD (Phase 3, `api.rs`)

```
GET    /api/sources          -> { "sources": [SourceView, ...] }
POST   /api/sources          {label, kind, airplay?|rtp?}  -> 201 SourceView
GET    /api/sources/{id}     -> SourceView
PUT    /api/sources/{id}     {label?, airplay?|rtp?}        -> SourceView
DELETE /api/sources/{id}     -> {ok, message}
```

```jsonc
// SourceView (response)
{ "id": "kitchen-airplay", "label": "Kitchen AirPlay", "kind": "airplay",
  "present": true, "node_name": "airplay-in-kitchen-airplay",
  "airplay": { "latency_msec": 100, "auth_setup": false,
               "prevent_takeover": true, "port": 5000 },
  "rtp": null }
```

`present` = a node with `node_name` is in the live registry (generalizes today's
`rtp_source_loaded` / "airplay running"). The legacy singular routes
(`/api/source/airplay`, `/api/source/rtp`) stay as shims until the UI cuts over;
per-receiver AirPlay client routes become `/api/sources/{id}/clients/*` in Phase 4.

### TypeScript + UI (Phase 5, `frontend/src/lib/types.ts`, `lib/api.ts`, `components/SourcesTab.svelte`)

```ts
export type SourceKind = 'airplay' | 'rtp';
export interface AirplaySourceCfg { latency_msec: number; auth_setup: boolean; prevent_takeover: boolean; port: number }
export interface RtpSourceCfg { port: number; latency_msec: number; source_addr: string; ignore_ssrc: boolean; rate: number }
export interface SourceView {
  id: string; label: string; kind: SourceKind; present: boolean; node_name: string;
  airplay?: AirplaySourceCfg | null; rtp?: RtpSourceCfg | null;
}
// api.ts: listSources(), addSource(body), updateSource(id, body), deleteSource(id)
```

`SourcesTab.svelte`: a source list + "Add source" (choose AirPlay / RTP) + a
per-instance card with edit/remove. Reuse the existing per-field controls.

## Phased implementation plan

Each phase is independently shippable and leaves the system working.

### Phase 0 — spike ✅ DONE (PASS)
Confirmed two `RaopServer` instances run concurrently in one process on distinct
ports (`:5000`/`:5001`) with distinct names, **both advertising over mDNS** (a
phone sees two endpoints). Reproducible via
`cargo run --example multi_raop_spike` (`bridge-daemon/examples/multi_raop_spike.rs`).
⇒ AirPlay-multi is viable; no 0..1 cap needed. The only production nuance is to
build each instance with the shared LAN-restricted mDNS daemon
(`discovery_supervisor::shared_advertise_daemon()`), exactly as `airplay_source::start`
already does, rather than shairplay's own all-interface daemon the spike used.

### Phase 1 — store → keyed collection + migration (½–1 day)
- Rewrite `sources_store.rs` to the tagged-collection model above.
- **Migration:** on load, if the old single-field shape is present
  (`airplay_source_name` / `rtp_source`), convert to one collection entry each
  (`id = "airplay-in"` / `"bt-bridge-rtp"` to preserve existing routing keys!),
  then persist the new shape. Mirrors `raop_migration.rs`. Keep it idempotent
  and deletable-after-one-boot.
- Unit tests: load-old→new, round-trip, empty install.
- No behavior change yet (still one of each) — safe to ship alone.

### Phase 2 — RTP multi-instance (½–1 day)
- `rtp_source_module_args` + load take a node name + id.
- `spawn_stored_sources` iterates the RTP collection; each loads under its own
  node name and port.
- Generalize `rtp_membership.rs`: one keepalive socket + watch per multicast RTP
  instance (map of `SourceId -> Keepalive`, reconciled against the collection).
  The pure `group_users` parser is unchanged.
- Port-collision validation on add/edit.

### Phase 3 — collection CRUD API (½ day)
- `GET /api/sources` (list, with live `present`/`loaded` status),
  `POST /api/sources` (add → returns id), `PUT /api/sources/{id}`,
  `DELETE /api/sources/{id}`.
- Keep the old singular routes as thin shims during transition (optional), or
  cut over the UI in the same PR.

### Phase 4 — AirPlay multi-instance (1–2 days)
- `SharedAirplay` → `Arc<Mutex<BTreeMap<SourceId, AirplayHandle>>>`.
- Port allocator (stored per source).
- `airplay_source::start` takes node name + label + port.
- Decide the **clients/ban/takeover** model (Open questions) and implement it
  per-receiver.

### Phase 5 — UI (1 day)
- `SourcesTab.svelte`: replace the two fixed panels with a source list +
  "Add source" (choose AirPlay / RTP) + per-instance edit/remove cards.
- Reuse the existing per-field controls; the AirPlay-clients panel becomes
  per-receiver (or global — per the decision).

### Phase 6 — cleanup (PARTIAL — 2026-07-28)
- ✅ **Done:** removed the fixed-name special cases in `routing.rs` — both
  `node_latency_ms` and the source display name now derive from the source store
  (`source_latencies` / `source_labels` maps keyed by node name), so **every**
  source instance gets a latency badge + label, not just the two legacy names.
  Startup log in `main.rs` generalized to a per-kind count.
- ⛔ **Deferred (blocked):** dropping the `SourcesStore` back-compat shims and the
  legacy singular routes. The **HA integration still depends on `/api/source/rtp`**
  (`custom_components/.../api.py` GET/PUT/DELETE — 11 refs — backing the Bluetooth
  RTP `switch`/`number` entities), and the frontend `api.ts` still exports the
  legacy AirPlay methods. Removing the routes/shims first requires **migrating the
  HA integration** (and the frontend) onto `/api/sources`, which changes HA entity
  semantics (the single-RTP-source model → a chosen source id) and needs its test
  suite + on-device re-validation. Until then the shims stay (now used only by the
  legacy `api.rs` handlers) and the legacy routes stay live.
- Drop the migration shim in `SourcesStore::load` after one deployed boot
  (already booted once on v0.2.0.0036 — safe to remove next pass).

## Migration & back-compat

- **Routing intent survives** because we keep the migrated ids equal to today's
  node names (`airplay-in`, `bt-bridge-rtp`) — existing `routing.json` links
  keep resolving. New sources get fresh ids.
- Old single-field `sources.json` auto-upgrades on first load (Phase 1).
- HA integration: the routing matrix / media_player surface is unchanged (still
  driven by observed nodes + intent).

## Resolved decisions (2026-07-27)

1. **AirPlay clients registry is per-receiver.** The connection list, ban list,
   priorities, and anti-takeover (`airplay_clients.rs`) become **keyed by source
   id**: `airplay_clients.json` holds one registry per AirPlay source, and the
   routes move to `/api/sources/{id}/clients/*` (list/forget/ban/priority/
   disconnect) + `/api/sources/{id}/policy`. Each `AirplayHandle` owns its own
   `SharedAirplayClients` + `SharedPreventTakeover`. (Phase 4.)
2. **AirPlay RTSP port is auto-assigned + persisted.** On add, `SourcesStore`
   picks the first free port from base 5000 among existing AirPlay entries and
   stores it in `AirplaySourceConfig.port`; it stays stable across restarts.
   Hidden in the UI unless the user expands "advanced". (Phase 1 allocates;
   Phase 4 binds it.)
3. **Label is editable; id is immutable.** `update(id, Some(label), …)` changes
   only the display/mDNS name; the `id` — and therefore the PipeWire `node_name`
   and routing key — never changes, so routing intent survives a rename. (Phase 1
   `update`, already in the contract.)

## Effort estimate

~3–5 focused days, low architectural risk (the graph is already general):
Phase 0 ½d, Phase 1 ½–1d, Phase 2 ½–1d, Phase 3 ½d, Phase 4 1–2d, Phase 5 1d.

## Status (2026-07-28)

**Phases 0–5 DONE and integrated onto the working tree; whole crate green
(`cargo test` = 104 passed / 0 failed), frontend `npm run check` clean.** Built
by parallel worktree agents against the frozen interfaces above, then merged.
Runtime CRUD is wired: `/api/sources` add/update/delete calls `reconcile_sources`
(loads/unloads RTP modules + starts/stops AirPlay receivers live). **Deployed +
live-verified on v0.2.0.0036** (migration clean, both legacy sources
`present:true`). **Phase 6 cleanup is COMPLETE (2026-07-28, in the working tree,
not yet redeployed):** the `routing.rs` fixed-name special cases, the
`SourcesStore` back-compat shims (+ private helpers + shim tests), and ALL the
legacy singular routes/handlers/structs are removed; the HA integration
(`api.py`) and the frontend now speak only `/api/sources`. `cargo test` 107/0,
integration `.py` syntax OK, frontend `npm run check` 0 errors. Remaining: drop
the `SourcesStore::load` migration shim (booted once already), **redeploy BOTH
the add-on AND the integration together** (the daemon no longer serves
`/api/source/rtp`), and an on-device smoke test (two AirPlay + one RTP, route,
verify; check the BT switch/number entities + the AirPlay clients UI, since the
integration couldn't be unit-tested off-device).

## Parallelization & handoff

This is a **single Rust crate**, so agents editing shared files (`main.rs`,
`api.rs`) can't independently verify compilation, and the phases form a
dependency chain (Phase 1's store interface underpins 2/3/4). The frozen
interfaces above make disjoint-file parallelism possible; the key enabler is
that **Phase 1 keeps back-compat shims**, so it touches only `sources_store.rs`
and the crate keeps compiling — nothing else changes until 2/3/4 opt in.

**Wave 1 (parallel now):**
- **Phase 0** — spike. ✅ done (this doc).
- **Phase 1** — `SourcesStore` collection + model + migration + shims + tests.
  Owns `sources_store.rs` only. Verifies with `cargo test`.
- **Phase 5** — frontend Sources tab, built to the HTTP contract above. Owns
  `frontend/src/{components/SourcesTab.svelte, lib/api.ts, lib/types.ts}`.
  Verifies with `npm run check`. Independent of all Rust work.

These three touch disjoint files and run concurrently without conflict.

**Wave 2 (after Phase 1 lands — build on the new store API):**
- **Phase 2** — RTP multi-instance (`rtp_source.rs`, `rtp_membership.rs`).
- **Phase 3** — collection CRUD API (`api.rs`).
- **Phase 4** — AirPlay multi-instance (`airplay_source.rs`, `airplay_clients.rs`).

Phases 2/3/4 all also touch `main.rs`/`api.rs` wiring, so run each in its own
git worktree off the Phase-1-merged base and integrate, or sequence 2 → 3 → 4.
Phase 6 cleanup (drop shims + fixed-name special cases) is last.

Recommended order to get value early: **1 → 2 → 3 → (UI wiring) → 4**, i.e. ship
RTP-multi first (the actively-used Bluetooth path, no port-allocator or
clients-model unknowns), then AirPlay-multi.

## Coordinate with pending in-tree changes

Uncommitted/undeployed work already sits in the tree and should land in the same
deploy cycle rather than fighting this refactor:
- `rtp_membership.rs` — the IGMP-membership watchdog (Phase 2 will generalize
  it to per-instance).
- `profiler.rs` — per-node xrun badges for the routing UI.
- `RtpSourceConfig.rate` defaulting to 48000 (kills the Pi↔host double-resample).

These touch the same files (`main.rs`, `rtp_source.rs`, `routing.rs`), so fold
the multi-source work in on top of them.
