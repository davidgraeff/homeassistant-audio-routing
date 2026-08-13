# PipeWire Audio Router (Home Assistant integration)

A Home Assistant custom integration that turns each output configured on
the [bridge daemon add-on](../../pipewire_audio_router/README.md) into a
regular `media_player` entity — volume, TTS/announce with real ducking,
live state, and **source routing controllable from automations** — plus
`switch`/`number` entities to turn the Bluetooth-bridge RTP source on and
set its port, all backed by the add-on's REST API.

This integration is useless without the add-on already running
somewhere reachable — install and configure that first.

## Installing

Not HACS-packaged (no `hacs.json`) — install manually:

1. Copy (or symlink) this directory into your Home Assistant config's
   `custom_components/pipewire_audio_router/`.
2. Restart Home Assistant.
3. Settings → Devices & Services → **Add Integration** → search
   "PipeWire Audio Router".
4. Enter the bridge daemon add-on's host and port (default `8099`).
   Setup actually calls the add-on (`GET /health`) to verify
   it's reachable before completing — if that fails you'll see "Could
   not reach the bridge daemon at that host/port" rather than a silent
   success.

There's no options flow for changing host/port later — remove and
re-add the integration if the add-on moves.

## Entities

The add-on's **settings** — voice ducking and the Bluetooth-bridge RTP source —
live on one device named *PipeWire Audio Router*, listed under the integration in
Settings → Devices & Services. That device page is the index for this integration:
Home Assistant groups its entities into *Controls* and *Configuration* there, shows
which add-on build is running and on what hardware, and its **Visit** link opens
the add-on's own web UI, which is where the routing matrix, the outputs and the
diagnostics actually live.

A second kind of device appears for every adopted **pw-sink host** — a PC running
`pwrouter-agent` — named the way that machine reports itself, *"david-local
(david)"*. Unlike a speaker, a PC has nothing in Home Assistant to inherit a room
from, so this is the thing you **assign a room to**; voice ducking then covers it
like any other output. It carries no entities of its own unless per-output players
are exposed, which is fine: the room assignment lives on the device and survives
restarts.

The `media_player`s deliberately stay off the settings device, because a device
prefixes its entities' displayed names and that is wrong for a speaker:

- **per-output** players join the **real speaker's** device instead (or its
  pw-sink host device), inheriting its name and area — and the area is what voice
  ducking below resolves a room against;
- **group** players stay standalone, so a group keeps the name you gave it
  (*"Everywhere"*, not *"PipeWire Audio Router Everywhere"*) in media cards,
  `tts.speak` targets and scripts.

So the six settings entities read *"PipeWire Audio Router Voice assistant
ducking"* and the like in pickers — the same shape as every other device's entities
(*"Satellite1 c4150c Temperature"*), with short names on the device page itself.
Existing entity **ids never change**; only settings entities created after this (a
fresh install) get the device name in their id too. Rename any entity in its
settings dialog to override the display name.

### Bluetooth bridge RTP source (`switch` + `number`)

Two entities let you turn on and configure the RTP source that receives the
[ESP32 Bluetooth bridge firmware](../../firmware/bt-bridge/README.md) —
without opening the add-on's web UI:

- **Switch** *"Bluetooth bridge RTP source"* — on loads the RTP source
  into the add-on (via `/api/sources`), off removes it. Once on, the
  source appears in every output's `source_list` automatically (it's just
  another routable source — pick it with `select_source`, below).
- **Number** *"Bluetooth bridge RTP port"* — the UDP port to listen on;
  must match the port the firmware sends to (default `46000`). Changing it
  while the source is on re-points it live; while off it's just remembered
  for the next enable. (The add-on's daemon only remembers a port while the
  source is enabled, so the integration persists your chosen port across
  restarts.)

Both are config entities (they show under the device's *Configuration*
section) and go **unavailable** if the add-on is unreachable or too old to
expose the `/api/sources` collection.

### Voice-assistant ducking (`switch` + `number` + `select`)

While a voice assistant in a room is talking, the router's speakers **in that
room** play quietly. No automation, no blueprint:

| Entity | What it does |
|---|---|
| `switch.voice_assistant_ducking` | Whether the feature is armed — **on by default**. It is *not* a duck: switching it on ducks nothing by itself, it just means the next voice turn will. Switch it **off** if you still run a volume-ducking blueprint, or you'll duck twice; an explicit off is remembered across restarts. |
| `number.voice_assistant_duck_level` | How quiet, as a **gain**: `0.25` = quarter volume, `1` = no ducking. (Not a divisor.) Takes effect on the next turn — an open one keeps its level rather than jumping mid-sentence. |
| `select.voice_assistant_duck_scope` | *Only the room being talked to* (`area`, the default) ducks just the satellite's own room, even mid-song inside a multi-room group. *The whole music group of that room* (`music_group`) widens it to every member of that synchronized group — for open-plan rooms where the same track next door drowns the response. |

**Nothing to set up:** the rooms, the satellites and the speakers all come from
registries Home Assistant already has, which is why this ships switched on.

To watch it work without saying anything, set a satellite's state by hand
(*Developer Tools → States*, `assist_satellite.…` → `listening`, then `idle`) —
that is the same trigger a real turn fires. The add-on's *Outputs* tab shows a
`ducked NN%` badge on each held output while it lasts, and `GET /api/duck` lists
the live holds. If a room never ducks, turn on debug logging for
`custom_components.pipewire_audio_router.voice_duck`: it says whether the
satellite had an area and whether any output was found in it.

Every `assist_satellite` is covered automatically, including ones added later.
The room comes from Home Assistant's areas: the satellite's area (its entity's
override if set, else its device's), matched against each output's area. A
satellite in no area ducks nothing, and neither does an output in no area — no
per-output entities are needed for any of this, because an output's area is read
from the device it belongs to:

| Output | Whose area | If it has none |
|---|---|---|
| `sendspin-dev-*` | the speaker's ESPHome device, matched by its mDNS hostname (or the MAC fragment that name ends in) | give the ESPHome device a room |
| `ap2-dev-*` | the HA device whose integration talks to the same IP (MusicCast, Onkyo, …) | give that device a room |
| `pwsink-dev-*` | the host device this integration creates for that PC | **give it a room — it starts with none** |

Why this beats ducking with `volume_set`: the add-on applies a gain inside its
mix, so **your speakers' volume never moves** (no slider jumping, nothing to
restore, no race if you change the volume mid-sentence), the duck lands within
milliseconds, overlapping turns in different rooms are independent, and a single
speaker of a synced group can duck while the others keep playing. The duck is
held on a lease the integration renews; if Home Assistant restarts or the network
drops mid-turn, the add-on un-ducks by itself within ~30 s.

The satellite's *own* speaker is ducked too, on purpose: the add-on may be
streaming music to it while the device speaks locally.

### Outputs (`media_player`)

One `media_player` entity per output in the add-on's routing matrix — the
auto-discovered virtual AirPlay-2 (`ap2-dev-*`) and sendspin (`sendspin-dev-*`)
devices — named by stripping the prefix and title-casing the rest (e.g.
`sendspin-dev-kitchen` → **Kitchen**). Playing/idle state and volume are
polled every 5 seconds; the routing (`source`) is **pushed live** over the
add-on's `/api/routing/ws` WebSocket, so re-wiring shows up immediately.

- **State**: `playing` if the add-on reports any source linked into
  that output, else `idle`.
- **Volume**: `media_player.volume_set` → the add-on's per-output
  `wpctl`-backed volume.
- **No play/pause**: deliberately not implemented — a passive routing
  sink has no queue of its own to pause. This is a design choice, not a
  missing feature (see [../../docs/decisions.md](../../docs/decisions.md)).
- **Announce / TTS**: declares `MediaPlayerEntityFeature.MEDIA_ANNOUNCE`
  (and `PLAY_MEDIA`, required alongside it — HA's `play_media` service
  rejects calls without it). `media_player.play_media` targeting one of
  these entities ducks whatever's currently playing on that output,
  plays the clip, and restores volume afterward.
- **Source (wiring)**: declares `MediaPlayerEntityFeature.SELECT_SOURCE`.
  `source_list` is the routable sources the add-on reports (plus `None`),
  `source` is whatever is currently linked into the output, and
  `media_player.select_source` re-wires it. Routing is **one source per
  output**: selecting a source unlinks whatever was feeding the output
  first; selecting `None` disconnects it. Automations key off source
  *names* (stable) — the add-on's ephemeral node ids are re-resolved on
  every call, so a routing automation keeps working across a module
  reload. The current wiring is kept in sync over the `/api/routing/ws`
  WebSocket (with automatic reconnect), so changes made elsewhere — the
  add-on's own web UI, another automation — reflect here without waiting
  for a poll.

## Dashboard card

The integration ships a Lovelace card that shows the whole house's routing at a
glance and lets you re-wire it by tapping:

```yaml
type: custom:pipewire-router-card
```

No resource to register — the integration serves the card and loads it itself, so
`custom:pipewire-router-card` is available on every dashboard as soon as the
integration is set up (it is in the card picker as **PipeWire Audio Routing**).

Inputs are on the left, where they play on the right — one node per **music
group**, plus one per output that isn't in a group — and one wire per route:

- **Tap an input, then tap where it should play.** Tapping a destination that is
  already playing it removes it again. The other direction works too: tap a
  destination first, then the input it should play.
- **Tap a wire to remove that route.** No confirmation — redrawing it is the same
  two taps.
- A **dashed** wire is a group only partly on that input (something else mixed
  it: the add-on's expert view, or an automation). Tapping completes it.
- A **gray** wire, and gray text, mean an endpoint isn't there right now. The
  route is kept and re-applied when it comes back.

Routing a group is exclusive — the same reconciling call as its
`media_player.select_source` — so the card cannot leave a group whose speakers
disagree about what they play. A lone output is additive, so two inputs can
deliberately be mixed into one.

It has a **visual editor** — the card's "Edit" pane, no YAML needed. Every option
is optional:

| Option | Default | Meaning |
| --- | --- | --- |
| `show_title` | `true` | Show the heading row. |
| `title` | `Audio routing` | The heading. (`title: ""` also hides it, as it did before `show_title` existed.) |
| `show_hint` | `true` | Show the instruction line under the graph. It comes back by itself while an input is held — that line is the only thing saying what the next tap does and how to cancel. |
| `entry_id` | the only one | Which router, if you have configured more than one. The editor offers a picker; the card says so if it's ambiguous. |

Deliberately *not* in this card: volumes, level meters, now-playing, per-speaker
rows, xrun badges. It answers "what is playing where, and change it" from a
phone; the add-on's own web UI is where the rest lives.

The card talks to Home Assistant, never to the add-on directly (see `ws_api.py`)
— so it works over HA Cloud and behind a reverse proxy, where the add-on's
ingress API is not reachable from the browser. It is built from
`pipewire_audio_router/frontend/src/card/` with `npm run build:card` and the
result is **committed** to `www/pipewire-router-card.js`, because HACS copies this
directory verbatim and runs no build step. CI fails if the two disagree.

## Routing from automations

The idiomatic path is `media_player.select_source`:

```yaml
action: media_player.select_source
target:
  entity_id: media_player.kitchen
data:
  source: shairport-sync        # a name from the output's source_list, or "None"
```

For anything beyond one-source-at-a-time (e.g. deliberately mixing two
sources into one output), two additive primitives are available — these
do **not** unlink existing sources the way `select_source` does:

```yaml
# Add a source without disturbing what's already linked
action: pipewire_audio_router.link
target:
  entity_id: media_player.kitchen
data:
  source: bt-bridge

# Remove one source, or omit `source` to disconnect all of them
action: pipewire_audio_router.unlink
target:
  entity_id: media_player.kitchen
data:
  source: bt-bridge             # optional
```

### Announcing TTS

Announcements take a **URL** (or a `media-source` id, which is resolved for
you), so Home Assistant's own TTS is the whole story:

```yaml
action: tts.speak
target:
  entity_id: tts.piper                # your TTS entity — Piper, Cloud, …
data:
  media_player_entity_id: media_player.downstairs_announcements
  message: "Front door opened"
```

The add-on fetches and decodes the rendered clip (symphonia — mp3, wav, aac,
ogg, flac), then mixes it over the ducked music per device.

> Earlier versions also accepted `extra.wyoming` to make the add-on synthesize
> against a Wyoming server (Piper) itself. That was removed: it duplicated a job
> Home Assistant already does better — its TTS entity handles voice selection
> and caching, whereas the add-on re-synthesized identical text on every call —
> and it meant pinning a Piper host/port inside automations. Use `tts.speak` as
> above.

## Testing

Real tests against actual HA internals (config flow machinery,
`DataUpdateCoordinator`, entity-platform forwarding, the real state
machine, real service-call dispatch) via
`pytest-homeassistant-custom-component` — only the network layer
(`PipewireRouterApiClient`) is mocked, HA itself is not:

```
pip install pytest-homeassistant-custom-component homeassistant
python3 -m pytest custom_components/pipewire_audio_router/tests/ -p pytest_homeassistant_custom_component
```

See `tests/README.md` for what each test actually covers.

## Files

```
__init__.py       setup/teardown, coordinator (5s poll: players + RTP source; routing pushed via /api/routing/ws), platform forwarding
config_flow.py     single-step host/port form, validates by calling the add-on
media_player.py    the MediaPlayerEntity subclass described above (incl. select_source + link/unlink services)
ws_api.py          WebSocket API behind the dashboard card: routing subscription + link/unlink/route_group
frontend.py        serves www/pipewire-router-card.js and loads it into the frontend
www/               the built dashboard card (committed; built from ../../pipewire_audio_router/frontend)
switch.py          RTP-source enable/disable + voice-ducking on/off switches
number.py          RTP-source port/latency + voice duck level numbers (restore-backed)
select.py          voice duck scope: area | music_group (restore-backed)
voice_duck.py      watches assist_satellite states, resolves area -> outputs, holds the daemon-side duck
api.py             thin async HTTP client for the add-on's REST API (media players, routing matrix, RTP source)
const.py           domain, default port(s), poll interval, service/source names, voice-duck defaults
services.yaml      link/unlink service descriptions for the automation editor
strings.json       UI strings (config flow + service descriptions)
manifest.json      integration metadata (config_flow: true; depends on frontend/http/websocket_api for the card)
```
