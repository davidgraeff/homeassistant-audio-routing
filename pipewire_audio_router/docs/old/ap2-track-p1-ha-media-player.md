# AP2 Track P1 — expose `ap2-dev-*` as HA media_players (+ area/identity adoption)

**Parallel-safe.** Owns the **Python HA integration only**; does NOT touch the Rust
daemon (`bridge-daemon/`). No conflict with the AP2 audio-path work.

## Goal
The daemon already surfaces AirPlay-2 receivers as virtual outputs (`ap2-dev-<slug>`,
`kind:"airplay2"` in `GET /api/outputs`, and as routable columns in the routing
matrix). Make them appear in Home Assistant as `media_player` entities, on par with the
sendspin devices, and adopt the correct HA device/area so they land in the right room.

## Context / what already exists
- Daemon endpoints (unchanged, just consume them):
  - `GET /api/outputs` → each AP2 output: `{ node_name: "ap2-dev-…", kind: "airplay2",
    display_name, present, … }`.
  - `GET /api/media_players`, `/api/routing` (matrix + links), the routing
    `link`/`unlink` services — same ones the sendspin entities already use.
- The HA integration is the Python side under `custom_components/` (the piece
  `deploy-dev.sh integration` rsyncs). The media_player platform already handles
  sendspin (`sendspin-dev-` prefix) — see the prefix check around `media_player.py`
  (roadmap references `media_player.py:47`).
- **Adoption precedent (reuse this):** sendspin entities adopt the HA device/area by
  matching the full mDNS hostname (embedded in the ESPHome entity unique_ids) → linking
  via the device's full MAC. See memory note *"Sendspin media_player adopts HA device
  name/area"* and the sendspin adoption code path.

## Work
1. **Enumerate AP2 outputs** alongside sendspin in the media_player platform: treat
   `kind == "airplay2"` / prefix `ap2-dev-` the same way sendspin outputs are turned
   into entities (source selection, volume once P-volume lands, routing via the matrix
   services). Factor the prefix handling so it's not sendspin-specific.
2. **Identity/area adoption (the design question to resolve).** Yamaha/Pioneer are NOT
   ESPHome, so the ESPHome-uid trick doesn't apply directly. Investigate the cleanest
   correlation to an existing HA device for the receiver:
   - Match on the receiver's IP / mDNS instance name / model (the daemon has
     `Ap2Device{fullname, display_name, model, addr}` — expose whatever's needed via the
     API if it isn't already).
   - Common case: the receiver already has an HA integration (MusicCast for Yamaha,
     Onkyo/Pioneer, or an `_airplay._tcp` HomeKit/AirPlay device). Prefer linking the
     `ap2-dev` media_player to that existing HA **device** (so it shares the area),
     rather than creating an orphan. Fall back to a standalone device with a
     user-settable area if no match.
   - Document the chosen rule; keep it a small, testable function.
3. **Volume/announce:** stub or defer the volume slider until the daemon's AP2 volume
   control (Phase 4, `ap2_volume.rs`) is wired — coordinate the API shape then. Routing
   (select source / group membership) should work now via the existing services.

## Acceptance
- Each present `ap2-dev-*` receiver shows as a `media_player` in HA, routable to sources
  via the matrix, and lands in a sensible area (adopted or user-set).
- No changes under `bridge-daemon/`.

## References
- `docs/airplay2-roadmap.md` (Phase 4 — control plane / HA adoption).
- Memory: sendspin media_player adoption; sendspin volume/media_players.
- Existing sendspin path in the media_player platform (mirror it).
