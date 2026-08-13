# Music-group presets

**What it does.** A *preset* is a named grouping of the house: which speakers sit in
which music group, and what each group plays. Switching preset is one click — "House
party" collapses five rooms into one group on one source, "Normal" puts them back —
instead of dragging speakers between groups twice per party.

**What it is not.** Not a scene, not a snapshot of everything. A preset carries
membership and (per group) a source. It does not carry volumes, delays, alignment or
announcement groups. §11 says why for each.

---

## 0. Status

**Built end to end and green, 2026-08-13. Never run against a real daemon.**
P1–P8 are done: daemon store + API, the add-on UI, the Home Assistant service and
select entity, and the dashboard card. **P9 (live acceptance, §12) is the gate** —
every claim below rests on unit tests and headless screenshots, and three things in
particular are unmeasured: whether activation really costs nothing when the source
partition doesn't change (§4.3), what a real switch sounds like when it does, and
whether write-through (§4.2) captures what people expect it to.

- daemon `cargo test` **536 passed / 0 failed**, `cargo fmt` clean on the touched
  files, `cargo clippy --all-targets` at exactly its pre-existing warnings
- integration `pytest` **123 passed** (14 new: `tests/test_presets.py` plus the
  card's preset cases), frontend `npm run check` clean
- the add-on UI verified by headless screenshot in both states — editing the active
  preset (indistinguishable from the old page) and editing an inactive one

Not built, deliberately: §11's deferrals, and the "cost" hint in the UI (§4.3),
which needs a measurement from P9 before it can say anything true.

### Decisions taken

| Decision | Where | Why |
|---|---|---|
| A music group is **identity** (`id`, `name`); a preset owns the **membership** | §2 | The identity is an HA entity (`<entry>_mg_<id>`). If presets owned the group *list*, every switch would create and destroy `media_player` entities and break automations, dashboards and voice aliases |
| Presets always exist internally; `Default` is created by migration and cannot be deleted | §2.2 | One code path. "Presets off" is not a second model, it is a UI that shows one preset |
| Exclusivity is **per preset** | §3 | Global exclusivity is exactly what makes party mode tedious: the old group must be emptied before the new one may claim a speaker |
| A preset entry carries a **source** per group, `null` = leave as is | §4.1 | Rewriting membership moves no links, so a preset without sources lands every regrouped group in `⚠ Mixed` — the state the UI otherwise cannot produce |
| Routing an *active* preset's group **writes through** into that preset | §4.2 | Makes "return to Normal" restore the music too, without a save step. Same reason the group editor is write-through |
| Activation is **one daemon operation** | §4.3 | N calls from the UI would pass through states that violate exclusivity and churn senders twice |
| The add-on UI is gated behind a **`Work with presets`** checkbox; off = today's page, byte for byte | §6.1 | The feature's cost is UI complexity for people who will never use it |
| Turning the checkbox **off force-activates `Default`** | §6.1 | Otherwise a non-Default grouping stays in force with no visible thing explaining it and no UI to fix it |
| A chip selects a preset **for editing**; activating is a separate, explicit action | §6.3 | Building Friday's party layout must not play it. But then the page's live controls must not be pointed at an inactive preset either — §6.4 |
| The routing graph stays bound to the **active** preset, always | §6.4 | It is a picture of reality. An editor may show intent; a graph may not lie |
| HA gets **both** a service and a `select` entity | §7 | The service is the automation ergonomics (address by name, no `entity_id`); the select is the only way a template or a stock dashboard can *read* which preset is on |

---

## 1. The user goal

Verbatim: *a specific situation (house party) requires a different set of music
groups, and when the situation is over you want your normal set back.* Today that is
manual: empty the old groups, refill the new ones, then reverse it tomorrow.

So the feature is judged on two clicks — "party", and "normal" — and on everything
else in the house staying exactly as it was for users who never open it.

## 2. Data model

### 2.1 Shape

`groups.json` (store/groups.rs) grows two keys and loses one:

```jsonc
{
  "music": [ {"id": "kitchen_zone", "name": "Kitchen"},      // identity only — no members
             {"id": "bath",         "name": "Bath"},
             {"id": "everywhere",   "name": "Everywhere"} ],
  "presets": [
    { "id": "default", "name": "Default", "groups": {
        "kitchen_zone": {"members": ["sendspin-dev-a", "sendspin-dev-b"], "source": "airplay-in"},
        "bath":         {"members": ["ap2-dev-dusche"],                   "source": null} } },
    { "id": "party", "name": "House party", "groups": {
        "everywhere":   {"members": ["sendspin-dev-a", "sendspin-dev-b", "ap2-dev-dusche"],
                         "source": "airplay-in"} } }
  ],
  "active_preset": "default",
  "announcement": [ /* unchanged */ ]
}
```

A group identity absent from a preset simply has no members there. `Everywhere` is
empty in `Default` and `Kitchen`/`Bath` are empty in `party` — both entities exist
throughout.

### 2.2 What the rest of the daemon sees

`GroupsStore::music()` returns `Vec<MusicGroup>` — `{id, name, members}` with the
members **of the active preset**. That is the same shape `/api/groups/music` returns
today, so `media_player.py`, `ws_api.py`, the card, `voice_duck.py`, the route
endpoints and `FlowGraph` need no change to keep working. Presets are additive
surface (§5), not a migration of the read path.

Membership writes take a preset scope: `update_music(id, name, members, preset)`,
defaulting to the active one. A rename hits the identity and is therefore
preset-independent.

### 2.3 Migration

In `GroupsStore::load`, not `store/migration.rs`: this is a shape upgrade of the
file the struct owns, and it must have happened before any reader sees the store.
The stored identity keeps a legacy `members` field (serde-renamed, dropped on
write); when `presets` is empty, load builds one preset `default` / `"Default"` from
those members, makes it active, and persists. Idempotent, and a `groups.json`
written by an older daemon upgrades on first boot.

Downgrade is *not* graceful: an older daemon reading the new file finds
`music[].members` gone and comes up with empty groups. Recorded here rather than
defended against — writing the effective members back into the file would make the
store two sources of truth about the same fact.

## 3. Exclusivity moves inside the preset

`exclusivity_conflict` today scans every music group. It becomes a scan of the
groups *of one preset*, so `bath` may hold `ap2-dev-dusche` in `Default` while
`everywhere` holds it in `party`, and neither edit has to make room for the other.
That is the whole tedium, removed.

## 4. Applying a preset

### 4.1 Membership is not routing

Links live per output. Rewriting membership moves nothing, so a preset that only
regroups leaves each speaker on whatever it was fed from, and any group whose
members disagree renders as `⚠ Mixed`. Hence the per-group `source`:

- `Some(name)` — link every member to that source exclusively (drop other sources
  feeding those members). Exactly `route_music_group`'s body, which P2 extracts.
- `None` — leave the members' links alone. A preset does not touch what it does not
  mention, so a speaker in no group keeps playing.

A `source` naming a node that no longer exists is skipped with a log, like any other
dangling routing intent.

### 4.2 Write-through

While a preset is active, `POST /api/groups/music/{id}/route` (the group's Source
dropdown, the card's wire, and HA's `select_source` — all one call) also records the
source in the active preset's entry for that group; `DELETE …/route` records `null`.
So `Default` learns what the house normally plays without a save step, and
activating it later restores both the grouping and the music.

### 4.3 Cost

Sync domains are keyed by each output's **source set** (`sync_group/desired.rs`), not
by music group. So a preset switch that leaves every speaker on the source it
already had is free — collapsing three groups on one source into one group causes no
sender churn at all. Moving a speaker to a *different* source does, and a
reconnecting sendspin device is a ~30 s absence (see the group-churn work). Worth
saying in the UI once measured; not worth trying to avoid.

Activation is a single store write plus one `changes.send(())`, so the reconciler
sees one transition.

## 5. Daemon API

Additive; nothing existing changes shape.

```
GET    /api/presets                 -> { active: "default", presets: [Preset] }
POST   /api/presets                 { name, copy_from?: preset_id }  -> { ok, preset }
PUT    /api/presets/{id}            { name? }
DELETE /api/presets/{id}            (refused for `default`; if active, `default` is activated)
POST   /api/presets/{id}/activate
PUT    /api/groups/music/{id}       { name?, members?, preset? }     // preset defaults to active
POST   /api/groups/music            { name, members?, preset? }
PUT    /api/settings                { presets_enabled? }
```

`copy_from` defaults to the active preset — the real workflow is "party is Normal
with everything in one group", and starting empty means re-assigning every speaker.

## 6. Add-on UI

### 6.1 The gate

`presets_enabled` joins `settings.rs` beside `expose_outputs_as_media_players`, and
appears as a `Work with presets` checkbox in the Music-groups intro card — where it
is discoverable at the moment the thought occurs, unlike the Settings tab. Off is
the default and renders today's page unchanged.

Turning it off while a non-Default preset is active activates `Default` first
(applying §4.1), with the confirm dialog saying so.

### 6.2 The chip bar

One full-width card directly under the intro card, only when enabled: a `+ New
preset` button, then one chip per preset in creation order with `Default` first.
Chip states are two independent things and must look different:

- **active** — filled, with a `▶` marker. The grouping in force.
- **selected** — ring. What the group cards below are editing.

On load, selected = active, which is the state that behaves exactly like today.
Each chip carries `✕` (confirm dialog; `Default` has none). Deleting a preset never
touches group identities — the HA entities survive — and deleting the active one
activates `Default`.

### 6.3 Editing vs activating

Selecting a chip changes only what is edited. The selected chip grows an
**Activate** button, which is also the only way to switch preset from the add-on UI.

### 6.4 What the page shows while editing an inactive preset

The group cards' Source dropdown, the `⚠ Mixed` pill and its breakdown all describe
*live* link state. Pointed at a preset that is not in force they would be wrong at
best and would apply the wrong grouping at worst. So when selected ≠ active the
cards drop to a membership editor: names, chips, drag-and-drop, delete — no Source
row, no Mixed pill, plus a banner `Editing "House party" — not active` with the
Activate button.

`FlowGraph` keeps rendering the **active** preset's groups regardless, labelled
`Active: Normal`. It is the one honest picture of what is playing where, and it stays
that.

## 7. Home Assistant integration

- **Service `pipewire_audio_router.activate_preset`**, field `preset` (id or name,
  case-insensitively), optional `entry_id` — resolved like `ws_api._resolve`: with
  one daemon configured nothing else is needed, with several it must be named.
- **`select` entity** `Music group preset`, `EntityCategory.CONFIG`, options = preset
  names, state = active. Both it and the service call one coordinator method.
  Added when `presets_enabled` is on (at setup, or later from the coordinator
  listener like `pwsink_hosts`); it reports `unavailable` if the flag goes back off
  rather than deleting itself. Option labels are user data, so unlike
  `voice_duck_scope` there is no `translation_key` to give them (see the note in
  `select.py`).
- **Coordinator** gains `presets`, `active_preset`, `presets_enabled`, polled in the
  same best-effort block as the groups.

## 8. The card

`ws_api._snapshot` gains `presets: [{id, name}]` and `active_preset`, both empty
when the flag is off, plus a `set_preset` command beside `route_group`. The card
draws a `<select>` in its header **only when more than one preset exists** — a
dropdown with one option is noise for exactly the user the gate protects.

`www/pipewire-router-card.js` is a committed build artifact, so P8 includes
rebuilding it.

## 9. Work packets

| P | Scope | Gate | |
|---|---|---|---|
| **P1** | `store/groups.rs`: identities + presets + migration + per-preset exclusivity + `remove_output` across all presets | `cargo test`; new tests per §10 | ✅ |
| **P2** | `api/groups.rs`: extract `route_members`, write-through (§4.2), `preset` on create/update | `cargo test` | ✅ |
| **P3** | `api/presets.rs` + routes: list/create/rename/delete/activate | route-shape test; live in P9 | ✅ |
| **P4** | `presets_enabled` in `settings.rs` / `SettingsInfo` / `set_settings` incl. the force-Default (§6.1) | `cargo test` | ✅ |
| **P5** | Frontend `lib/types.ts` + `lib/api.ts` | `npm run check` | ✅ |
| **P6** | `PresetBar.svelte` + the checkbox + `MusicGroupsTab` edit mode (§6.2–6.4) + the docs dialog | `npm run check`, headless screenshot | ✅ |
| **P7** | Integration: `api.py`, coordinator fields, `select.py`, service + `services.yaml`/`strings.json` | `pytest` (`test_presets.py`) | ✅ |
| **P8** | Card: `ws_api.py`, `card/types.ts`, `model.svelte.ts`, `RoutingCard.svelte`, rebuild the artifact | `npm run check`, `pytest` | ✅ |
| **P9** | Live acceptance on the instance: party ↔ normal, both directions, with music playing | manual, §12 | ☐ |

Two notes for whoever picks up P9. The **apply path is the untested one**: the store
hands out a plan (unit-tested) and `api/presets.rs` walks it through
`route_members`, but building an `AppState` in a test costs more than it would
prove, so nothing has yet watched it move a real link. And every existing test file's
offline patcher had to learn `async_get_presets` — a new one that forgets it fails
with *"the test opens sockets"* rather than anything about presets.

## 10. Tests worth having

Store level, because that is where the invariants are:

- the same speaker may sit in different groups in two presets, and neither edit
  conflicts (§3)
- a pre-presets `groups.json` migrates to one `Default` holding its memberships, and
  re-loading twice changes nothing
- `music()` follows the active preset, and a group absent from it reports no members
- `remove_output` strips a speaker from every preset, and keeps every group
- deleting the active preset activates `Default`; deleting `Default` is refused
- `copy_from` yields an independent copy (editing the copy leaves the original)
- activation returns the (group, source) plan, `None` entries included as "leave"

API level: activate applies the plan to the routing store and drops the old
source of a moved member; write-through records a route into the active preset only.

## 11. Deferred, with reasons

- **Announcement groups in presets.** They overlap freely, so nothing forces the
  user to dismantle one to build another — there is no tedium to remove.
- **Per-preset volume** ("party is louder"). The most defensible next addition, and
  a separate decision about whether a preset may move sliders.
- **Per-preset alignment / delays.** Alignment is a property of the room and the
  hardware, not of the situation.
- **Scheduling** (a preset per time of day). An automation plus §7's service already
  does it, and better.

## 12. Live acceptance (P9)

With music playing in two rooms: switch to a party preset that puts both rooms plus
a third on one source, confirm all three play in sync and that the two that were
already on that source did not drop out (§4.3's prediction). Switch back, confirm the
two rooms return to their own groups and their own sources. Then the same from an
automation via the service, and from the card's dropdown, and confirm HA's select
entity follows in all three cases.
