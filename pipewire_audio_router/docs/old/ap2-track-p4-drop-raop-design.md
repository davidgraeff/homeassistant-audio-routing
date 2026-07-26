# AP2 Track P4 — "drop RAOP" implementation-ready plan (Phase 6)

**Status: IMPLEMENTED 2026-07-26.** The RAOP output path is removed. Backend compiles
clean with 74 tests passing; frontend `npm run check` clean; the one-time
`raop_migration.rs` shim rewrites persisted `raop-out-*` → `ap2-dev-*` at startup
(idempotent). Two deviations from the plan below, both intentional:
- **`calibrate.rs` was in scope but the inventory missed it** — it carried
  `MemberKind::Raop` / node-id muting tied to a group's `raop_members`. Removed, and
  (per a mid-implementation request) **AP2 alignment was ADDED**: `GroupSnapshot` now
  carries `ap2_members`, `MemberKind::Airplay2` mutes via `ap2_control`, and the
  offset knob is the now-live render delay — so AP2-only groups are alignable (they
  weren't before, and would have regressed to sendspin-only).
- **`reconcile` in `routing.rs` was kept**, not deleted — with all outputs now virtual
  it's a documented cheap no-op (see the "flag, don't blind-delete" note honored).

Historical plan below (kept for the record).

---

**Status (original): DESIGN COMPLETE — implementation gated, not started.** This doc was a
design stub; it is now the implementation-ready plan (grep-verified inventory + the two
hard decisions resolved + ordering + rollback). **Do not code this in parallel with the AP2
audio-path work:** it edits the hot files (`routing.rs`, `sync_group.rs`, `api.rs`).
Land it only once Phase 5 is done (real speakers flipped to AP2 and confirmed on
hardware) — see **Gating** below.

Line anchors below are from the 2026-07-25 tree and are pointers, not contracts; re-grep
each symbol at implementation time (`is_raop_output`, `raop_uses_anchor`, `RAOP_NODE_PREFIX`,
`raop_latency`, `raop-out-`, `_raop._tcp`, `RAOP_MODULE_NAME`).

## Goal
RAOP was the architectural odd-one-out (a *real* PipeWire sink node loaded from a C
module, direct-linked or anchored case-by-case). Every other output is now a *virtual*
per-device sender fed from a group's null-sink anchor monitor. Once all target receivers
run over AP2 (Phase 5), delete the AirPlay-1/RAOP **output** path entirely — hard drop,
no fallback. This turns a pile of special-cases into a clean delete.

## Gating (this replaces "rollback" — there is no rollback)
Hard drop = no fallback path survives, so the safety is entirely in the **precondition**,
not in a revert. Do not start until ALL of:
1. **Phase 5 done** — every physical receiver in the deployment is routed through AP2 as
   the default and confirmed audible + in-sync on hardware (Yamaha WX-021 + Pioneer
   VSX-934 today; extend the per-brand interop table first if new brands exist).
2. **Phase 3 live path is audible** (the current blocker) and **Phase 4 volume** has
   landed — otherwise deleting RAOP removes the only working per-output volume for those
   receivers. (Per-output *render delay* already exists on AP2; volume does not yet —
   `ap2_volume.rs` is present but wiring is in progress.)
3. `/data/raop-outputs.json` migration (below) has been dry-run against the live
   deployment's actual store + routing/groups files.

If (1) or (2) is not true, this is a regression, not a simplification. Recovery if it
goes wrong: `git revert` the PR **and** restore `/data/raop-outputs.json` from backup
(the migration deletes it) — so **back that file up as step 0**.

---

## Grep-verified removal inventory

### A. Full-file deletions (verified RAOP-only)
- **`src/raop.rs`** — module-arg builder + `raop_node_name` + `RAOP_NODE_PREFIX` +
  `RAOP_MODULE_NAME`. Every consumer is on this list.
- **`src/outputs_store.rs`** — the RAOP output store (`OutputsStore`, `/data/raop-outputs.json`).
- **`src/discovery.rs`** — ⚠️ **correction to the original stub:** this is *not* "the
  `_raop._tcp` branch of discovery.rs"; the **entire file** is RAOP receiver discovery +
  `raop-sink` module load/unload (its own module doc line 1 says so). AP2 discovery is
  the separate `ap2_discovery.rs`; sendspin is `sendspin_discovery.rs`. Delete the whole
  file, including the `Mode` enum and `SharedDiscovered` type.

### B. Edits — excise the RAOP branch, keep the file
- **`src/routing.rs`**
  - Delete `is_raop_output` (321), `source_needs_raop_anchor` (332), `raop_uses_anchor`
    (357), `ensure_monitor_link_by_name` (399), `destroy_links_between` (420) *if* their
    only callers are RAOP (verify: `ensure_monitor_link_by_name`/`destroy_links_between`
    are called only from `sync_group.rs`'s RAOP branch — confirmed by grep).
  - `is_output_node` (92): drop `RAOP_NODE_PREFIX`; keep sendspin/AP2 prefixes.
  - `output_display_name` (96): drop `RAOP_NODE_PREFIX` from the strip list — **this is
    the collision fix** (see Hard Part 1).
  - `build_matrix` (150): drop the `configured` RAOP map and its display-name/`configured`
    badge plumbing (RAOP was the only "configured" output kind; sendspin/AP2 are all
    auto-discovered). Verify no other output kind relies on `configured`.
  - **`reconcile` (444) becomes near-dead:** its only real work was direct-linking
    non-anchored routes, and after RAOP the sole remaining outputs are virtual
    (sendspin/AP2) which have **no live node** — `node_id_for` returns `None` so the loop
    already no-ops for them, and the `raop_uses_anchor` skip goes away. **Flag, don't
    blind-delete:** confirm with a compile + a routing smoke test that nothing else
    depends on `reconcile` linking a real output; if truly dead, remove it and its
    call-sites, else leave it as a documented no-op. `ensure_link_by_name` +
    `matched_port_specs` **stay** — `sync_group` uses them to link sources *into* the
    group anchor sink.
  - Delete the RAOP unit tests (654–687).
- **`src/sync_group.rs`**
  - `compute_desired` (215): drop the `present_raop`/`raop_outputs` param + loop
    (235–248); drop `raop_node_names` from `DesiredGroup` (75/90/247/264).
  - `reconcile`: drop the `present_raop` snapshot (308) and step **d** "RAOP monitor
    links" (444–460) entirely — with RAOP gone every group member is a per-device sender,
    no monitor-attach exception.
  - `group_display` (557): drop the `.chain(d.raop_node_names…)`.
- **`src/api.rs`**
  - `/api/outputs*` CRUD (the RAOP add/remove/list handlers ~334–650): remove. Verify
    which endpoints survive for AP2/sendspin (those are read from discovery registries,
    not the store).
  - `set_output_latency` (~2034): drop the `RAOP_NODE_PREFIX` guard + the RAOP
    unload/reload block (2043–2088). The AP2 render-delay branch (above line 2034) stays
    and becomes the *only* path; update the error string at 2037.
  - Settings DTO (1261/1276/1297/1316): drop `default_raop_latency_ms`.
  - `list_outputs` node-name RAOP filters (373/388/395/2128): drop `RAOP_NODE_PREFIX`
    (2128 keeps `SENDSPIN_NODE_PREFIX`; check whether an AP2 prefix belongs there).
  - Diagnostics `raop_outputs` count field → remove (or repurpose).
- **`src/main.rs`**
  - Startup RAOP module-load loop (257–279) — remove.
  - The other `PwCommand::Load` at ~432 — grep confirms this is the **RTP source** load,
    not RAOP; **keep it**.
  - `discovery::Mode` plumbing (107/112/234) and the RAOP store construction (`store`,
    `DEFAULT_STORE_PATH` = `/data/raop-outputs.json`, line 47) — remove.
- **`src/discovery_supervisor.rs`** — ⚠️ **correction:** shared across RAOP + sendspin +
  AP2 (3-tuple of `ServiceDaemon`, line 27). **Edit, don't delete:** drop the RAOP
  daemon (→ 2-tuple), remove the `discovery::` import + the RAOP-latency input field.
- **`src/settings_store.rs`** — remove `default_raop_latency_ms` field + getter/setter
  (52/71/114/119) + tests (174/187/194).
- **`src/sync_settings.rs`** — remove `raop_latency` map + `raop_latency`/`set_raop_latency`
  (58/69/125/135). Keep `ap2_latencies`/`set_ap2_latency` (the replacement).
- **`src/config.rs`** — remove `RaopOutputConfig`, `RaopEncryption`, `default_raop_port`
  (12–57). Keep `slugify` and `AP2_DEV_PREFIX`/`SENDSPIN_*` (used everywhere).
- **`src/routing_store.rs`** — code stays (generic `(source, output)` intent store); only
  the RAOP examples in doc-comments/tests need refreshing to AP2/sendspin names. The
  live data it holds is handled by the migration (Hard Part 2).

### C. Verified to STAY (the stub flagged these as conditional — resolved: keep)
- **`src/pw_module.rs` + `PwCommand::Load`/`Unload` (`pw_thread.rs`)** — ⚠️ **KEEP.**
  Grep shows `rtp_source.rs` loads its C module through the exact same
  `PwCommand::Load`/`Unload` path (api.rs ~1015–1102, keyed `RTP_SOURCE_NODE_NAME`). The
  stub's "iff nothing else loads C modules" resolves to **something else does** → the
  runtime-module-load machinery is not RAOP-specific and remains.
- **`src/volume.rs`** — ⚠️ **KEEP entirely.** It is generic node-volume-by-`node_id` via
  SPA `Props`, used for **source ducking / announce** and general get/set (calibrate.rs,
  api.rs 2137–2342), not a "RAOP node-Props path." RAOP outputs already stopped exposing
  volume in the matrix (routing.rs 190 comment). Nothing to remove here.

### D. Frontend (`frontend/src/`)
- `components/OutputsTab.svelte` — remove the RAOP add-output form + the
  "Receiver latency (ms)" branch (236/252) and the "AirPlay (RAOP)" copy (171); AP2's
  "Render delay (ms)" becomes the only latency field. (This is also where the
  test-buttons `canTest` gets `|| o.kind === 'airplay2'` per the roadmap Phase-4 note.)
- `components/SettingsTab.svelte` — remove `raopLatency` + `default_raop_latency_ms`
  (12/35/49/109/110).
- `components/AlignTab.svelte` — remove `kind === 'raop'` member handling + the RAOP
  node_id path (272 + delay-push logic).
- `components/DiagnosticsTab.svelte` — remove the `raop_outputs` stat (68).
- `lib/api.ts`, `lib/types.ts` — drop RAOP CRUD, `default_raop_latency_ms`,
  `raop_outputs`, and the `'raop'`/`'airplay'` `AlignMemberKind`/output-kind variants
  (types.ts 21/64/76/100/126/136/141).

### E. HA Python integration (`custom_components/pipewire_audio_router/`)
- `media_player.py` — remove `RAOP_NODE_PREFIX` (48), drop it from `_OUTPUT_PREFIXES`
  (54), and remove the RAOP-specific state/volume/no-identity special-casing (77, 278,
  315, 356, 415, 445, 697). AP2 + sendspin paths remain.
- `tests/test_media_player.py` — rewrite the `raop-out-*` fixtures to `ap2-dev-*`.
- (Music Assistant's `music-assistant-server/.../airplay/*` RAOP refs are a **separate
  project** — out of scope.)

### F. Container / runtime deps
- **No dedicated package to drop.** ⚠️ **correction:** the Dockerfile has no explicit
  `module-raop-sink` package — the module ships inside the `pipewire` package (line 113),
  which we still need for everything else. So "drop the runtime dependency" is
  **comment-only**: update the RAOP mentions in `Dockerfile` (158/164),
  `rootfs/run.sh` (22/41), `rootfs/etc/pipewire/client.conf.d/50-mlock.conf` (5), and
  `config.yaml` (24/50) to reflect that no `raop-sink` module is loaded anymore. No
  apt/port/capability change (RAOP used no privileged ports; AP2's 319/320 stay).

---

## Hard Part 1 — display-name collision (resolved: delete = fix)

**Mechanism (confirmed).** Node names are already distinct (`raop-out-dusche` ≠
`ap2-dev-dusche`); routing keys off the *node name*, so the intent store itself is
unambiguous. The collision is purely in the human/display layer: `output_display_name`
(routing.rs 96) strips **both** prefixes to the same label ("Dusche"), so a UI/automation
selection by label, and the HA media_player correlation, can't tell the two apart — which
is how a music group got silently pointed at the RAOP output during Phase-3 testing.

**Decision: option (a) — delete removes the collision.** After Phase 5 each physical
receiver is reachable via exactly one output (`ap2-dev-*`); dropping `RAOP_NODE_PREFIX`
from `output_display_name` and `build_matrix` means only one output ever carries the
label. **No transition-window disambiguation is needed** *provided* the migration below
runs in the **same** deploy that removes the RAOP code, so no persisted
`raop-out-*` reference is ever left pointing at a now-deleted output kind. (Option (b),
UI "show kind", would only be needed if RAOP and AP2 had to coexist for a window — they
don't, because this lands after the hard flip.)

## Hard Part 2 — data migration (one-time, on first boot of the drop-RAOP build)

Two kinds of persisted state reference RAOP:

1. **`/data/raop-outputs.json`** (the RAOP output store). **Decision: drop it, don't
   translate the store itself.** AP2 devices are *discovered*, not stored — `_airplay._tcp`
   re-discovery repopulates `ap2-dev-*` on boot. Translating store entries into AP2
   devices would just race discovery. Back it up (Gating step 0), then delete.

2. **Routing intent (`routing_store`) + group membership (`groups_store`) links that
   name `raop-out-<slug>`.** These are the ones that would *dangle* if the output kind
   vanishes. **Decision: rewrite the prefix in place, one-time, at startup.** For each
   persisted link/member whose output is `raop-out-<slug>`, rewrite to `ap2-dev-<slug>`.
   Slug parity holds because both derive from `slugify(<mDNS instance label>)`
   (`raop.rs::raop_node_name` vs `ap2_discovery::device_node_name`, both call
   `config::slugify`) and the same physical receiver advertises the same instance label on
   `_raop._tcp` and `_airplay._tcp` in the tested fleet.
   - **Caveat to handle:** a device could advertise *different* labels on the two service
     types → slug mismatch → the rewritten `ap2-dev-*` never resolves and the link stays
     grey/offline. Since this is a single known deployment, the safe form is: run the
     rewrite, then **log every rewritten link** and **log any resulting `ap2-dev-*` that
     no discovered device matches after one discovery cycle**, so a mismatch is visible
     rather than silent. A dangling `ap2-dev-*` link is harmless (grey, no audio) and
     the user re-links once — acceptable for a hard-drop migration.

3. **Per-output latency.** Migrate `sync_settings.raop_latency[raop-out-<slug>]` →
   `ap2_latencies[ap2-dev-<slug>]`, **clamped** into AP2's render-delay range
   (`AP2_RENDER_DELAY_MIN_MS..=MAX_MS`, 200–2000 ms — RAOP's default was 1500, already in
   range; a `None`/unset RAOP latency migrates to unset, i.e. the AP2 default). RAOP
   "receiver latency" and AP2 "render delay" are not the same physical quantity, so treat
   this as a *starting point* the user re-tunes in Phase 5, not an exact transfer.

**Where the migration lives.** A one-shot function run in `main.rs` **before** the stores
are handed to the reconcilers, idempotent (a second boot finds no `raop-out-*` and does
nothing), guarded so it only rewrites — never invents links. It must run **before** first
`reconcile`/`sync_group` pass so the graph is built from migrated intent. Keep it as a
dated, self-deleting-by-idempotency shim (a `// migration: drop-raop 2026-…` comment) so
it's obvious it can be removed after the deployment has booted once.

---

## Ordering / sequencing
1. **Precondition:** Gating 1–3 all true; back up `/data/raop-outputs.json`.
2. **PR, single commit-series (RAOP and its migration land together):**
   a. Add the one-time migration shim (Hard Part 2) — lands *first within the PR* so the
      persisted state is AP2-shaped before any RAOP reader is removed.
   b. Delete/edit backend (inventory A + B), keeping C (pw_module/volume/rtp-source)
      intact. Compile — the compiler is the dead-code oracle for the "flag, don't
      blind-delete" items (`reconcile`, the `ensure_monitor_link`/`destroy_links_between`
      helpers).
   c. Frontend (D) + Python integration (E) in lockstep with the API changes.
   d. Dockerfile/rootfs/config comment cleanup (F).
3. **Verify on hardware:** both receivers still audible + in-sync via AP2; routing +
   grouping survive a daemon restart (proves the migration + no dangling links);
   per-output render delay still editable; announce/duck (once Phase-4 volume lands)
   still works. Diff `/data/*.json` before/after to confirm the rewrite.
4. Remove the migration shim in a follow-up once the deployment has booted clean (or leave
   it — it's idempotent and cheap).

## Payoff (for the PR description)
- `routing.rs`: no `is_raop_output`/`source_needs_raop_anchor`/`raop_uses_anchor`, no
  monitor-link/follower-sink helpers; `reconcile`'s direct-link path likely removable —
  the whole "direct link vs anchor" fork disappears because all outputs are virtual.
- `sync_group.rs`: single per-device-sender model, no RAOP monitor-attach branch.
- Volume: one in-band per-device model (AP2 + sendspin), no node-Props-for-RAOP split.
- Latency: one mechanism (per-device PTP render offset), no load-time `raop.latency.ms`
  reload-the-sink path.
- O-E (RAOP per-output volume/duck/announce, previously deferred) is moot — AP2 does it
  in-process.
- Discovery: one fewer `ServiceDaemon` and its whole load/unload C-module lifecycle
  (`discovery.rs`) gone; `pw_module.rs` stays but only serves the RTP source.

## References
- `docs/airplay2-roadmap.md` (Phase 6 + "What dropping RAOP simplifies").
- Memory: HA local add-on build-vs-pull; routing via automations.
