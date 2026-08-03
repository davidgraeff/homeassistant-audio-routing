# Voice-assistant ducking in the add-on (replacing the HA blueprint)

**Goal.** When a voice assistant in a room starts a turn, the music the router is
playing **on the speakers in that room** ducks, and un-ducks when the turn ends —
with no automation, no blueprint and no YAML in the user's setup.

**Today** this is done by a community blueprint
(`~/Entwicklung/home_assistant/blueprint-volume-duck.yaml`): it watches one
`assist_satellite` entity, templates the list of `media_player`s in the
satellite's area that are `playing`, divides each one's `volume_level` by a
factor via `media_player.volume_set`, waits for `idle` (hard 20 s timeout), then
multiplies back — but only for players whose volume still reads exactly the
ducked value.

**The response audio is not ours.** The HA Voice PE speaks through its own
speaker; the router is only in the path of the *music* being ducked. So this is
not an announcement — there is no clip for the daemon to play. That single fact
drives the whole design below.

This plan also removes the legacy node-volume duck/announce API, which this work
makes definitively dead — see §7.

## 1. Why this belongs in the add-on

Everything the blueprint does poorly is poor *because* it can only act through
HA's `media_player` volume API:

| Blueprint behaviour | Cause | In-add-on behaviour |
|---|---|---|
| Restore silently skipped if the user nudges the volume during the turn (`state_attr(mp) == vol_anon` guard) | HA has no other way to know whether its duck is still the current value | Duck is a **mixer gain**, never the device's volume — nothing to restore, nothing to lose a race with |
| Un-ducks after 20 s even if the turn is still open | `wait_for_trigger` needs a timeout or it leaks | Lease with renewal; expiry is a safety net, not the normal path |
| Two rooms talking at once breaks it (`mode: single`) | One automation instance | Ref-counted holds, per output |
| Ducks the *whole* synced group, since an MG is one `media_player` entity | Volume is per entity | Ducks **only the room's speaker** inside a multi-room group — the per-device relay already differentiates one PCM feed per device. Whole-group ducking stays available, as a *choice* (§3.2) |
| Music that starts mid-turn is never ducked | The player list is templated once, up front | A hold sits on the output; a relay that starts later picks it up |
| Volume-ramp audible on the device; HA volume state churns on every turn | `volume_set` round trip per player | Applied on the next audio chunk (~10–20 ms), invisible to HA's volume state |

There is also no native hook to lean on instead: as of the `homeassistant-core`
checkout at `4c41f56079f` (2026-07-11) neither `assist_satellite`,
`assist_pipeline` nor `media_player` has any ducking concept.

## 2. Finding: there is only one duck mechanism to build

The first sketch of this design carried a fallback for "outputs with no
per-device relay, duck them via the output node's PipeWire volume
([`volume.rs`](../pipewire_audio_router/bridge-daemon/src/volume.rs))". **That
case no longer exists.** Checked:

- **RAOP output was hard-dropped** on 2026-07-26 — `raop.rs`, the RAOP-only
  `discovery.rs` and the `/api/outputs` CRUD around it are deleted, no fallback
  path retained ([decisions.md](decisions.md) "Hard-drop RAOP, no fallback";
  [airplay2-roadmap.md](airplay2-roadmap.md) "RAOP is already removed").
- **Every routable output is now virtual and served by a per-device relay.**
  `build_matrix` says so in as many words ("Every output is now virtual +
  auto-discovered … audio reaches them via a group sink, not a live node here",
  [routing.rs:208](../pipewire_audio_router/bridge-daemon/src/routing.rs#L208)),
  and the matrix is narrowed to adopted outputs, which are exactly the three
  `-dev-` prefixes from
  [config.rs](../pipewire_audio_router/bridge-daemon/src/config.rs):
  `sendspin-dev-`, `ap2-dev-`, `pwsink-dev-`.
- **All three relays already call `OverlayMixer::mix_into` per chunk** —
  sendspin ([sendspin_server.rs:772](../pipewire_audio_router/bridge-daemon/src/sendspin_server.rs#L772)),
  AP2 ([ap2_server.rs:399](../pipewire_audio_router/bridge-daemon/src/ap2_server.rs#L399)),
  pw-sink ([pwsink_server.rs:192](../pipewire_audio_router/bridge-daemon/src/pwsink_server.rs#L192)).
  A duck applied in the mixer therefore reaches every output kind there is.

**Consequence:** one duck mechanism, no branch on output kind, no restore-state
bookkeeping, no risk of a duck leaking into a device's user-visible volume. And
the node-volume path becomes removable outright (§7).

The one thing that follows from "duck lives in the mixer": an output whose relay
is not running cannot be ducked — but it also has no music playing, so this is a
no-op rather than a gap. A hold placed on such an output must simply **persist**,
so that if music starts mid-turn the newly-started relay picks the duck up. It
does: holds are keyed by output name in the process-global `OverlayMixer`, whose
lifetime is independent of any relay.

## 3. Design

**Knowledge in HA, mechanism in the daemon.** HA owns areas and satellite state;
the daemon owns audio. Neither learns the other's job.

### 3.1 Daemon: a duck **hold** (not an announcement)

Deliberately *not* built on the AG path. The announce scheduler
([announce_arbiter.rs](../pipewire_audio_router/bridge-daemon/src/announce_arbiter.rs))
is built for **atomic clips the daemon owns**: whole-or-nothing, TTL, queue,
barge-in, and ducking that exists only as a side effect of an overlay being
mixed. A voice turn has no clip and no known duration, so an AG-based version
would need a silent clip of unknown length and would fight the stall reaper. It
would also be actively wrong: an AG announcement **occupies** its outputs, so a
doorbell would queue behind someone's voice turn instead of playing over the
already-ducked music.

So: a separate, additive concept, in a **separate map** from the overlay slots.

```rust
// overlay_mixer.rs — new, alongside `slots`
struct DuckHold { id: u64, level: f32, expires: Instant }
ducks: Mutex<HashMap<String /* output */, Vec<DuckHold>>>,
```

- **Aggregate gain per output** = `min` over that output's live holds, and if an
  announcement overlay is also active, `min(overlay.duck, holds…)` — strongest
  duck wins, and duck+announcement compose (music quiet, clip audible).
- `mix_into` returns `true` when *either* a slot or a hold exists; with a hold
  only, it writes `music * gain` and touches no cursor.
- Keeping holds out of `slots` means `reap_stalled`, `take_finished` and `stop`
  need **no changes** and cannot interact with holds. A duck-only entry has no
  cursor to advance, so it must never be reachable by the progress watchdog —
  separation buys that structurally instead of by a flag check.
- **Lease, not fire-and-forget.** Every hold carries a TTL and is expired in the
  existing 150 ms `AnnounceCoordinator::poll` tick
  ([main.rs:423](../pipewire_audio_router/bridge-daemon/src/main.rs#L423)). If HA
  restarts, loses the network, or the integration is reloaded mid-turn, music
  un-ducks on its own within one TTL. This is the single most important property
  of the design — a duck that can leak is worse than no duck.

API (mirrors the `/api/announce` conventions — explicit `targets` or a named
group, `duck` defaulting to the daemon setting):

| Method | Path | Body / effect |
|---|---|---|
| `POST` | `/api/duck` | `{ targets[] \| announcement_group, level?, ttl_ms? }` → `{ ok, hold_id, ducked[], level }` |
| `POST` | `/api/duck/{hold_id}` | renew (extend by `ttl_ms`); 404 once expired so the caller re-`POST`s |
| `DELETE` | `/api/duck/{hold_id}` | release now |
| `GET` | `/api/duck` | live holds — for the UI badge and for debugging |

`level` defaults to `settings.default_duck()` (already the `/api/announce`
default). `ttl_ms` defaults to 30 s.

The API takes **output names only** — it knows nothing about areas or scopes.
Both scopes in §3.2 resolve to a list of outputs on the HA side, so neither the
endpoint nor the daemon's stores change when the user switches scope.

### 3.2 HA: automatic voice ducking

A new `voice_duck.py` in the integration:

1. `async_track_state_change_event` over the whole `assist_satellite` domain (no
   per-satellite config — new satellites are covered automatically).
2. Non-idle (`listening` / `processing` / `responding`) → resolve the
   satellite's area: entity registry `area_id` if set, else its device's
   `area_id`. No area ⇒ do nothing (and log once at debug).
3. Resolve the target outputs by **scope** (below).
4. `POST /api/duck`; renew every `ttl/3` while non-idle; `DELETE` on
   `idle`/`unavailable`.

**Scope, user-selectable, default `area`:**

| Scope | Targets | For |
|---|---|---|
| **`area`** (default) | This integration's `media_player` outputs whose area is the satellite's area | The normal case: only the room you are talking in goes quiet, even mid-song inside a multi-room group |
| `music_group` | Those outputs, plus every other member of any music group they belong to | Open-plan rooms, where the same track playing next door drowns the response |

`music_group` expansion is pure HA-side arithmetic — the coordinator already
holds the MG list with members (`coordinator.music_groups`), so it maps
area-outputs → containing MG → all members, and posts that list.

Area resolution needs no new mapping code: output entities adopt the real
speaker's device and inherit its area
([media_player.py:83](../custom_components/pipewire_audio_router/media_player.py#L83),
[media_player.py:307](../custom_components/pipewire_audio_router/media_player.py#L307)),
so the entity registry already knows. A user who disagrees with the adopted area
can override it on the entity.

Two decisions that fall out of the mixer-side mechanism, both departures from the
blueprint:

- **No filtering on `playing`.** A hold on an idle output is inaudible and costs
  nothing, it covers music that starts *during* the turn, and it removes a
  dependency on entity state — which matters, because `_is_virtual` currently
  omits `pwsink-dev-`, so those entities report `state = None`
  ([media_player.py:341](../custom_components/pipewire_audio_router/media_player.py#L341))
  and a state filter would silently skip them.
- **Do *not* exclude the satellite's own speaker.** The blueprint's
  `select('ne', device_entities(device_id(assist)) …)` excludes the satellite's
  *response player* — the ESPHome `media_player` that is about to speak. Our
  `sendspin-dev-<pe>` output is a different thing: it carries **music** to that
  same physical speaker, so it is the single most important output to duck, not
  one to skip. (If a user instead routes the assist response *through* the router
  as an AG announcement, that path brings its own duck and composes via `min` —
  no conflict.)

**No areas in the daemon.** Adding an `area` to outputs or groups in
[groups_store.rs](../pipewire_audio_router/bridge-daemon/src/groups_store.rs)
would create a second source of truth to keep in sync with HA's registry, and
the daemon still could not see `assist_satellite` states. The daemon stays
addressed by `node_name`.

### 3.3 Configuration surface

Three entities on the existing config entry — no options flow, and all three are
automatable:

- `switch.…_voice_ducking` — **off by default**, so upgrading users who still
  have the blueprint enabled don't get double ducking. Turning it on is the cue
  to delete the blueprint.
- `number.…_voice_duck_level` — the mixer gain (0.05–1.0, default from the
  daemon's `default_duck`). Note this is a *gain*, not the blueprint's divisor;
  the UI text must say so.
- `select.…_voice_duck_scope` — `area` (default) | `music_group`. New platform
  (`select.py`, added to `PLATFORMS`); `switch`/`number` already exist.

## 4. Work breakdown

| ID | Scope | Files | Done when |
|---|---|---|---|
| **VD1** | Duck holds in the mixer | `overlay_mixer.rs` | `ducks` map + aggregate gain + expiry; unit tests: hold-only ducks, hold+overlay takes the min, two holds refcount, expiry restores full gain, hold survives with no relay and applies once one starts, `reap_stalled`/`take_finished` unaffected |
| **VD2** | Hold registry + expiry tick | `announce.rs` (or a small `duck_holds.rs`), `main.rs` | ids allocated, TTL expiry driven from the existing 150 ms poll, `GET` snapshot |
| **VD3** | REST API | `api.rs`, `docs/api-reference.md` | 4 endpoints, `announcement_group` resolution reusing the `/api/announce` code path, one `USER ACTION: duck -> N target(s)` log line per call |
| **VD4** | HA voice ducking | `voice_duck.py`, `api.py`, `__init__.py`, `switch.py`, `number.py`, `select.py`, `const.py`, `strings.json` | satellite→area→outputs resolution, both scopes, renewal, release on idle; pytest: area via device, area overridden on entity, satellite with no area (no-op), scope `music_group` expands to all MG members, scope `area` does **not**, satellite's own output *included*, two satellites in different areas concurrently, release on `unavailable` |
| **VD5** | UI + docs | `frontend/src/components/OutputsTab.svelte`, `docs/architecture.md` §5.3, `README.md` | a "ducked" badge on an output with a live hold; architecture note that duck now has two producers (AG overlay, duck hold) |
| **L1–L6** | Legacy removal (§7) | see §7 | separable; do after VD3 lands, before or after VD4 |

VD1–VD3 are independently testable without HA (`curl` a hold onto a playing
output). VD4 is where the blueprint actually dies.

Rough size: ~350 lines of Rust with tests, ~250 lines of Python with tests, plus
the deletions in §7 (net negative).

## 5. Risks / open questions

- **`responding` also ducks.** The PE speaks its response on its own speaker, so
  the room's music must stay ducked until `idle` — same as the blueprint. Costs
  nothing, but a long TTS keeps the duck for its full length; renewing at `ttl/3`
  covers this.
- **Latency of the trigger.** Ducking starts on the state transition to
  `listening`, i.e. after on-device wake-word detection has been reported to HA.
  Expected well under the ~250 ms sendspin send-ahead, but the duck lands a
  fraction of a second into the utterance — measurable on the live instance, and
  nothing in this design can improve it without a signal earlier than HA's state
  machine.
- **Announcement + voice turn on the same output.** Compose by design
  (`min(gain)`); worth one live check that a doorbell is still intelligible over
  doubly-ducked music.
- **`music_group` scope and MG exclusivity.** An output belongs to at most one MG
  (enforced in `groups_store.rs`), so expansion is unambiguous. An output in no
  MG expands to itself — the two scopes then behave identically, which is correct
  and worth a test.

## 6. Acceptance (live instance)

1. Music on the room's speaker, say the wake word → music quiet within a fraction
   of a second, response intelligible, music back at full level after `idle`,
   **and HA's volume slider for that player never moves**.
2. Scope `area`, speaker inside a multi-room MG → only the satellite's room
   ducks; the other members stay at full level and stay sample-coincident (no
   re-anchoring, no drift — the timeline is untouched, this is a gain change
   inside one device's frame).
3. Scope `music_group`, same setup → every member of that MG ducks together.
4. Kill the HA integration (reload the config entry) mid-turn → music un-ducks
   within one TTL on its own.
5. Two satellites in two areas, overlapping turns → each room ducks and restores
   independently.
6. Doorbell announcement to the same output during a turn → clip audible, music
   ducked once, output released normally afterwards.
7. Start music *during* a turn → it comes up already ducked.

## 7. Legacy removal: the node-volume duck/announce cluster

§2 establishes that no matrix output is backed by a real PipeWire node any more,
which makes L1–L4 dead code paths kept alive by callers that never fire. L5
(Wyoming TTS) is a different case — a live but redundant feature, removed because
HA already does the job. Verified caller-by-caller; each item lists its full blast
radius.

**L1 — `GET /api/media_players`.** The handler already documents itself as
"normally empty on the current architecture, and that is correct" — it filters on
`SENDSPIN_NODE_PREFIX` (`sendspin-out-`), which nothing creates
([api.rs:2779](../pipewire_audio_router/bridge-daemon/src/api.rs#L2779)). It is
still polled every 5 s by the integration, and **no entity consumes the result**:
state comes from `coordinator.routing`, volume from `outputs_meta` /
`sendspin_volumes`. Removing it drags: `MediaPlayerInfo`; `api.py`'s
`async_get_media_players` + the `MediaPlayerState` dataclass; the coordinator's
generic type and the first block of `_async_update_data`
([\_\_init\_\_.py:107](../custom_components/pipewire_audio_router/__init__.py#L107))
— note the coordinator must keep *some* update that can raise `UpdateFailed`, so
promote the settings/outputs fetch to the authoritative one; `config_flow.py:33`,
which uses this call as its **connectivity probe** (switch to
`async_get_settings`); the unused `api.ts` `mediaPlayers()` + `MediaPlayerInfo`
type; and the `async_get_media_players` patch targets in three integration test
modules (mechanical).

**L2 — `GET`/`POST /api/media_players/{node_id}/volume`.** Node-id addressed;
`volume.rs` only. Callers: `api.py`'s `async_set_volume` (defined, never called)
and `api.ts`'s `setVolume(nodeId)` (no Svelte component calls it). Drags
`VolumeResponse` + `SetVolumeRequest` and the frontend types.

**L3 — `POST /api/media_players/{node_id}/announce`.** The v1 ducked announce:
duck every source linked into a real sink via node volume, play the clip with
`player.rs`, restore. Superseded by `POST /api/announce` (AG path, per-device
overlay). Callers: `api.py`'s `async_announce` / `async_announce_wyoming` /
`_async_announce` — all three defined, none called (`media_player.py` uses
`async_announce_group`). Drags `AnnounceRequest`/`AnnounceResponse`, the
duck/restore block ([api.rs:2995](../pipewire_audio_router/bridge-daemon/src/api.rs#L2995)),
`player.rs::play_wav_to_target` (**keep** `play_loop_to_target` — used by
`calibrate.rs` and `pw_sink_spike.rs`), the api-reference section, and
`tests/test_addon_announce_ducking_e2e.sh` (it tests the removed mechanism).
`decode.rs`, `wav.rs` and `resample.rs` all stay.

**L4 — delete `volume.rs`.** After L1–L3 all four public functions are unused
(the two blocking ones are called only by the module's own async wrappers). Drop
the `mod volume;` line. The empirical finding it encodes — PipeWire Links have no
gain stage, so ducking must be a node/mix operation — is already recorded in
`decisions.md` and `spikes/05-tts-ducking-mechanism.md`; keep those.

**L5 — `wyoming.rs` and the `wyoming` announce source.** Unlike L1–L3 this one
has a real live caller, so it is a *feature removal*, not dead-code deletion —
but the feature is fully replaceable and duplicates HA's own TTS.

- **Not in the add-on UI**, as observed: the only frontend trace is a stale
  comment in `api.ts:130`; no component offers it.
- **Live path:** the AG `media_player`'s `async_play_media` honours
  `extra.wyoming` ([media_player.py:678](../custom_components/pipewire_audio_router/media_player.py#L678)),
  reaching `POST /api/announce`'s `wyoming` source
  ([api.rs:2152](../pipewire_audio_router/bridge-daemon/src/api.rs#L2152)). It is
  documented for users in
  [the integration README](../custom_components/pipewire_audio_router/README.md)
  §"Using Wyoming TTS instead of a rendered URL".
- **Full replacement:** `tts.speak` / `play_media` with a `media-source` or
  `tts_proxy` URL — the same entity already resolves media-source ids and
  processes the URL right next to the wyoming branch. Cost: one extra HTTP round
  trip, and HA renders the clip (which it does for every other TTS consumer
  anyway). Gain: no second TTS configuration path where a Piper host/port/voice
  is pinned inside automations, bypassing HA's TTS entity, its voice selection
  and its caching — the original decision entry already noted "repeating the same
  `wyoming` text re-synthesizes it every time".

Blast radius. **Daemon:** delete `wyoming.rs` + its `mod` line;
`WyomingAnnounceRequest` and `default_wyoming_port`; the `wyoming` field on
`AgAnnounceRequest` ([api.rs:2093](../pipewire_audio_router/bridge-daemon/src/api.rs#L2093));
the wyoming arm of `acquire_announce_pcm` (so its error text becomes "provide
exactly one of: test, tone, url"); the v1 handler's wyoming branch (already going
with L3). No Cargo dependency drops — the module uses only tokio and
`serde_json`; `wav::build_wav`/`read_pcm16`, `decode.rs` and `resample.rs` all
stay (test/tone/url, `calibrate.rs`, the spikes). **Integration:** the `wyoming`
kwarg on `async_announce_group`, `async_announce_wyoming` (dead, goes with L3),
the `extra.wyoming` branch in `media_player.py`, the README section, the tests
README line, and any test exercising it. **Tests:** delete
`tests/test_addon_announce_wyoming_e2e.sh` — with the source gone there is
nothing left for it to cover. **Docs:** the api-reference `wyoming` blocks; mark
`decisions.md` §"TTS/announce ducking: URL-based (v1) and Wyoming-based (v2),
additive" **superseded** (keep the rationale as history — the AudioFormat
`width`-is-bytes finding is the kind of thing worth not rediscovering);
`architecture.md:489-491`.

After this, `POST /api/announce` has exactly three sources — `test`, `tone`,
`url` — of which `url` is the only dynamic one, and the daemon speaks no TTS
protocol at all.

**L6 — `SENDSPIN_NODE_PREFIX` / `routing::is_output_node`** — *deferred, not part
of this work.* With L1 gone the prefix survives only in
[routing.rs:142](../pipewire_audio_router/bridge-daemon/src/routing.rs#L142),
where `is_output_node` splits registry nodes into outputs vs. sources. It is
already always-false in practice (so `present_outputs` is always empty and every
output's `node_id` is already `None`), but removing it changes `build_matrix`'s
classification rule rather than deleting a dead branch, and matrix semantics are
worth their own change with its own test pass. Its doc comment claiming it is
"shared with sources_store.rs" is already stale — no such reference exists.

**Sequencing.** L1–L5 are independent of VD1–VD5 and can land in either order;
doing them *after* VD3 means the new duck endpoint is in place before the old
duck code goes, so there is never a window with no duck mechanism documented.
Per the project's standing "no upgrade migration — start clean" convention, no
compatibility shims for the removed endpoints.

L5 is the only one that can break a working setup (an automation passing
`extra.wyoming`), so it is the only one that needs a user-facing note: replacing
the integration README's Wyoming section with the `tts.speak` equivalent *is* that
note. L1–L4 remove things nothing calls, so they need none.
