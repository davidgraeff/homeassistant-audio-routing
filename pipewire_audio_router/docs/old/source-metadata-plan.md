# Source metadata → Home Assistant — plan

Every input the router carries already knows what it is playing, and the router shows none
of it. A phone streaming over AirPlay sends title, artist, album, cover art and progress; a
phone on the Bluetooth bridge sends title/artist/album/duration over AVRCP; the
[YouTube Music receiver](../../../docs/ytmusic-receiver.md) resolves a track it could describe.
All of it is discarded today.

This plan makes **now-playing a first-class per-source property of the daemon**, fed by
whichever producer can supply it, and consumed by the HA integration's existing output and
music-group `media_player` entities — with sendspin device displays as a second consumer
later.

The shape is deliberately **source-generic**. No producer's name appears in the daemon or
the integration: the daemon learns "source `X` is playing `Y`", and it does not matter
whether `Y` came from DMAP over RTSP, from BlueZ over D-Bus, or from mpv. This is the
condition under which the YTM plan's *"no presence in the add-on"* pillar survives — the Pi
becomes "an RTP source that also reports metadata", exactly like the Bluetooth bridge.

Read [`architecture.md`](../pipewire_audio_router/docs/architecture.md) §3 (*Sources*) and
[`api-reference.md`](api-reference.md) (*Sources*) first if the source model is unfamiliar.
Rationale that outlives this plan graduates into [`decisions.md`](decisions.md).

---

## Status

**WP0–WP4 and WP8 built and green, 2026-08-10. Not deployed.** The daemon carries per-source
now-playing metadata, the AirPlay receiver feeds it, both HA entity kinds display it, the
add-on's own routing graph shows it on each source card, and both Pi roles report. What remains is WP5 (sendspin displays, optional), WP6 (live validation with a
real phone) and deployment — see §4 for the per-WP state and §6 for what live validation has to
show.

Three design decisions were taken up front and are not open questions below:

1. **Music groups show their source's metadata** (§3.6).
2. **The Pi reports as a second *role* on the existing `_pwrouter-btbridge._tcp` advert** —
   no new service type (§3.5).
3. **Metadata is its own low-rate frame on `/api/routing/ws`**, not a field on the 250 ms
   matrix frame (§3.2).

Planning also turned up a **live bug** in the HA integration's handling of that socket, which
is now WP0 because this plan cannot land on top of it — see §4. Measuring that socket then
turned up a second, independent problem: the matrix frame is pushed at 4 Hz although **1.6 % of
it** needs that rate, and the WP0 fix is what would start making HA pay for it. That is **WP7**,
also built, and it must be **deployed together with WP0** — WP0 alone would take HA from 0.2 to
4.2 matrix frames a second. Auditing for WP7 also found `set_output_latency` never notifying
`changes`, fixed there.

---

## 1. Finding: PipeWire cannot carry this

The obvious idea — let the transport that already carries the audio carry the metadata —
does not work, for three independent reasons.

**PipeWire's two metadata mechanisms are both instance-local.** Node and stream properties
(`media.title`, `media.artist`, …) live on objects in one daemon's graph, and the `Metadata`
object that `pw-metadata` reads and writes (`default.audio.sink`, `target.node`) is a
key/value store belonging to that same daemon. Neither is transported anywhere.

**The RTP link carries audio and nothing else.** The path is `module-rtp-sink` →
`module-rtp-source`; the only out-of-band text anywhere near it is the `s=` session name in
a SAP/SDP announcement, which is fixed when the module is loaded. There is no metadata
header extension and no RTCP side channel in those modules.

**PipeWire *can* span hosts** — `module-protocol-native` over TCP exposes a whole graph to a
remote client — but that would mean handing the add-on unauthenticated control of the Pi's
entire graph in order to read one string (`media.title` on mpv's stream node). Not a
transport; a liability.

And the reason it could never have worked anyway: **Bluetooth metadata never enters the
PipeWire graph.** AVRCP is delivered by BlueZ on the *system* D-Bus as
`org.bluez.MediaPlayer1` — properties `Track` (`Title`, `Artist`, `Album`, `Duration`,
`TrackNumber`), `Status`, `Position`. PipeWire sees PCM frames on an A2DP transport and
nothing else.

**Consequence:** the transport has to be ours. §2 shows we have already built one twice.

---

## 2. Finding: four producers, three consumers — and the richest one needs no transport

### Producers

| Producer | Carries | State today |
|---|---|---|
| **AirPlay source** (in the add-on) | title, artist, album, genre, duration, track/disc no., **JPEG/PNG cover art**, progress | **already arriving and dropped on the floor** |
| **BlueZ on the Pi bridge** | title, artist, album, duration, track no., play state, position | nothing reads it; no D-Bus code in [`setup_pi_bridge.py`](../firmware/pi-bridge/setup_pi_bridge.py) |
| **pi-ytmusic** | mpv `media-title`, duration, position, play state; artwork URL derivable from the video id | not built ([`mpv.js`](../firmware/pi-ytmusic/receiver/mpv.js) observes no properties yet) |
| [`player.rs`](../pipewire_audio_router/bridge-daemon/src/player.rs) | knows exactly what it is playing | n/a (announcements — probably never wanted here) |

The AirPlay one is the strongest argument for putting the model in the daemon, because it is
**local**: the vendored library already parses DMAP and already offers the hooks.
`vendor/shairplay/src/proto/dmap.rs` defines `TrackMetadata { title, artist, album, genre,
duration_ms, track_number, disc_number }` with a `from_dmap` parser, `handlers_ap1.rs`
already routes RTSP `SET_PARAMETER` to it, and the `AudioHandler` trait already declares

```rust
fn on_metadata(&self, _metadata: &TrackMetadata) {}
fn on_coverart(&self, _coverart: &[u8]) {}
fn on_progress(&self, _start: u32, _current: u32, _end: u32) {}   // RTP timestamps @44100
```

— three default no-ops. `airplay_source.rs`'s `Handler` implements `AudioHandler` (for
`authorize_session`, `on_client_connected`, …) and simply does not override them. On the AP2
path, `ap2_server.rs` correspondingly passes `raop_metadata_types: None`.

So WP1 lights up the richest producer in the system by implementing three trait methods, with
**no new transport, no firmware change, and no protocol work** — which is also why it is the
right place to prove the whole chain.

### Consumers

- **The HA integration.** [`media_player.py`](../custom_components/pipewire_audio_router/media_player.py)
  already resolves, for every entity, which source is feeding it:
  `PipewireRouterMediaPlayer.source` reads the persisted links, and
  `MusicGroupMediaPlayer._member_sources()` does the group equivalent. Metadata display is
  then a handful of properties on entities that already exist (§3.6).
- **sendspin outputs.** The protocol has a `metadata@v1` role, and the vendored submodule
  already models it: `MetadataState { timestamp, title, artist, album_artist, album,
  artwork_url, year, track, progress }` in `submodules/sendspin/src/protocol/messages.rs`.
  ESPHome devices with a display could show now-playing. Entirely unused capability today.
- **RAOP / AP2 outputs**, by sending DMAP back out. Later, if ever.

### Prior art worth remembering

The **ESP32 bridge already did this** and it was lost in the move to the Pi:
[`bt-bridge.yaml`](../firmware/bt-bridge/bt-bridge.yaml) declares `track_title` and
`track_artist` ESPHome text sensors, and `a2dp_bridge.cpp` polls AVRCP metadata every 5 s,
publishes only on change, and blanks both sensors on disconnect. So the feature is known to
be wanted; the only real question was the **route**: edge→HA directly (what ESPHome did) or
edge→daemon→HA (this plan). Edge→daemon→HA wins because the AirPlay producer and the sendspin
consumer exist only on that route — a Pi that publishes straight to HA leaves both stranded
forever.

One improvement over the ESP32 for free: BlueZ emits `PropertiesChanged`, so the Pi needs
**no 5-second poll**.

---

## 3. Design

### 3.1 The model: `NowPlaying`, keyed by source node name

```rust
pub struct NowPlaying {
    pub state: PlaybackState,        // playing | paused | stopped
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    pub position_ms: Option<u32>,
    pub position_updated_at: Option<SystemTime>,  // → HA's media_position_updated_at
    pub artwork: Option<Artwork>,    // Url(String) | Bytes { rev: u32, mime, len }
    pub updated_at: Instant,         // for the staleness TTL
}
```

Keyed by **source node name** (`sources_store::source_node_name` — `airplay-in`,
`rtp-in-<id>`, …), because that is the key the routing matrix, the persisted routing intent
and the HA integration already share. Stored in a `Mutex<BTreeMap<String, NowPlaying>>`
behind an `Arc`, with the existing `ChangeNotifier`, in the shape
`airplay_clients.rs` / `sendspin_volume.rs` already use.

Two hard rules:

- **Nothing on this path may touch an audio thread.** Metadata is a tokio/HTTP concern.
  The relay threads run `SCHED_FIFO` and the ingest resampler is already the known
  bottleneck; a title string must never be allocated, parsed or logged on either.
- **Entries expire and are cleared explicitly.** A TTL (~60 s without an update) *plus* an
  explicit clear when a source's session ends or the source is deleted. The ESP32 learned
  this the hard way — it blanks the sensors on disconnect for exactly this reason. Without
  both, HA cheerfully shows last night's song forever.

### 3.2 Publish path: its own low-rate frame on the routing socket

`/api/routing/ws` is **already a multiplexed, typed protocol running at two different
rates**, so metadata joins it as a third rate rather than riding the hot frame.
`routing.rs`'s `Frame` enum is internally tagged (`type`), and today carries:

| Frame | Rate |
|---|---|
| `matrix` | **every 250 ms**, unconditionally — peak levels move without graph changes, so a timer pushes a fresh snapshot while any client watches |
| `outputs` / `discovered` / `agents` | on connect, then **only when the serialized payload differs** from the last one sent (`SentListings` + `push_if_changed`), coalesced onto that same tick |

So: **a new `Frame::NowPlaying`**, pushed through the exact same `push_if_changed` dedupe
with one more `SentListings` slot. On change only; position rate-limited to ~5 s (HA
extrapolates from `media_position_updated_at` between updates, so nothing is gained by
sending more). When nothing is playing it costs zero bytes.

**Do not put `now_playing` on `RoutingNode`.** That was the first sketch here and it is
wrong: the matrix frame is rebuilt and re-serialized *four times a second*, so embedding
titles, album names and artwork revisions would put the descriptive payload — the part that
changes once per song — onto the hottest frame in the system, and hand it to every web-UI
client too. The routing graph already repaints at 250 ms; keeping that frame lean is the
point.

**WP7 weakens that argument but does not overturn it.** Measuring the matrix frame (numbers in
WP7) showed that only 1.6 % of it needs 4 Hz at all, so the fix is to stop pushing the *matrix*
at that rate rather than to route around it. Once that lands the matrix frame is no longer hot
— but metadata still belongs on its own frame, because a ~5 s position tick should not re-send
the whole graph, and an artwork revision has nothing to do with routing. After WP7 the reason
is "different lifecycle", not "don't touch the hot path".

The reason the HA side stays cheap is unchanged: the integration already holds this socket
open (`async_routing_ws_loop` in
[`__init__.py`](../custom_components/pipewire_audio_router/__init__.py)), so metadata arrives
**pushed, not polled**, with no new transport and no change to the coordinator's 5-second
`/api/media_players` poll. But see **WP0** — the integration cannot currently receive a second frame type at all.

Two further constraints:

- **Do not gate it like the meters.** Source `peak` metering and profiler xrun sampling are
  armed by the first matrix watcher and disarmed by the last, deliberately. Metadata must not
  acquire that coupling: HA is a permanent watcher and the UI an occasional one, and metadata
  has to be correct for whoever asks `GET /api/routing` cold.
- **`GET /api/routing` needs a companion.** The matrix REST payload deliberately does not
  carry this, so add `GET /api/sources/{id}/now_playing` (or a `now_playing` block on `GET
  /api/sources`) for cold reads, debugging and `curl`-level acceptance testing.

### 3.3 Artwork

Three cases, and they are genuinely different:

- **URL** (pi-ytmusic): `https://i.ytimg.com/vi/<video_id>/hqdefault.jpg`, derived from the
  id with no API call.
- **Bytes** (AirPlay): `on_coverart(&[u8])` hands us a JPEG. The daemon serves it at
  `GET /api/sources/{id}/artwork?rev=N`, holding **one** current image per source in memory
  (they are ~50–200 kB; never written to disk, never accumulated).
- **None** (Bluetooth): AVRCP cover art is the 1.6 BIP/OBEX feature and BlueZ does not
  expose it. Write this down now so it is not discovered as a bug later.

The `rev` counter is what busts HA's image cache on a track change.

**Set `media_image_remotely_accessible = False` in all cases**, including the public
`i.ytimg.com` one. HA then fetches the image server-side and serves the browser its own
`/api/media_player_proxy/…` URL, which means the daemon's port never has to be reachable from
a phone on the LAN, an ingress-only setup keeps working, and the two artwork cases behave
identically.

### 3.4 Ingest

**Local producers** call into the store directly — the AirPlay `Handler`'s three trait
methods, and (if ever wanted) `player.rs`.

**Remote producers dial out.** The Pi opens a WebSocket to the daemon and pushes; the daemon
never connects to the Pi. This is the same decision, for the same reasons, as
[`receiver-agent-plan.md`](../pipewire_audio_router/docs/receiver-agent-plan.md) §5: nothing
has to listen on the Pi, no firewall hole, and a changed Pi address costs nothing. That
agent's protocol file states the two properties worth copying verbatim — a `PROTOCOL_VERSION`
checked in the first message, and **data/commands as an enum, not a passthrough**.

**Which source does a reporter annotate?** The Pi does not know the daemon's source ids, and
must not have to. `bt_bridge_discovery.rs` already solves this: the advert carries
`rtp_port` + `rtp_dest`, and the daemon already matches a bridge to a configured RTP source by
that pair. The reporter therefore identifies itself by the same key and the daemon resolves
it to a source node name. A reporter whose key matches no configured source is accepted and
ignored (logged once) — the Pi may be set up before the source is added.

**Authentication.** Recommended: *none of its own*, matching the rest of the daemon's HTTP
API, which is already unauthenticated on the LAN — but bounded deliberately. The channel is
**write-only, data-only** (it cannot route, configure, or play anything), rate-limited, and it
can only annotate a source whose stream parameters the reporter already matched. Worst case,
a LAN neighbour makes a speaker display the wrong song title. If that is judged unacceptable,
the drop-in fix already exists: `pwsink_agent.rs`'s `Agents` store — pair on first connect
with a human-verified code shown in the UI, mint a token into `/data/agents.json`. That is a
strictly larger piece of work (a UI approval flow on the Pi's behalf) and is deliberately not
in WP3.

### 3.5 mDNS: a second role on the existing advert — and **do not bump `ver`**

Per the decision, the Pi keeps advertising `_pwrouter-btbridge._tcp` and simply says it can do
more. `avahi_service_xml()` already emits a `role` key:

```
ver=1                       ← MUST STAY 1
role=rtp-sender,metadata    ← extended value
meta_ver=1                  ← metadata contract version, independent
rtp_port=… rtp_dest=… rate=… fmt=… channels=… diag_port=… diag_path=…
```

The `ver=1` constraint is not cosmetic. `bt_bridge_discovery.rs::parse_txt` does

```rust
let ver: u32 = get("ver")…?;
if ver > SUPPORTED_TXT_VERSION { return None }   // SUPPORTED_TXT_VERSION = 1
```

— it **skips** an advert from a newer bridge wholesale. Bumping `ver` to 2 would make an
updated Pi vanish from every add-on that has not been updated in lockstep, taking the
existing Bluetooth discovery and adoption with it. Unknown TXT keys, by contrast, are ignored
by that same parser, and `role` is not read at all today, so both additions above are safe in
either direction. `meta_ver` carries metadata-contract versioning instead.

Note what discovery is and is not for here, consistent with the existing rule in that module
("no audio path is built from an advert"): the advert says *this host can report metadata*
and identifies which stream it belongs to. The metadata itself never travels in TXT.

### 3.6 HA side

A single mixin supplies the display properties, and both entity classes get it:

| HA property | Source |
|---|---|
| `media_title` / `media_artist` / `media_album_name` | `now_playing` of the resolved source |
| `media_duration` / `media_position` / `media_position_updated_at` | ditto; HA extrapolates position |
| `media_image_url` (+ `media_image_remotely_accessible = False`) | §3.3 |
| `media_content_type` | `MediaType.MUSIC` when anything is known |

- `PipewireRouterMediaPlayer` resolves its source exactly as its existing `source` property
  does (persisted links, by stable name — so it stays right while the output is briefly
  offline).
- `MusicGroupMediaPlayer` **reuses `_member_sources()` / its existing `source` property
  verbatim** — decision 1. Reusing the same resolution rather than writing a second one is
  the point: the chip label and the metadata then cannot disagree, including in the
  "several sources linked additively, report the first" case.
- **Announcement groups get nothing.** They do not carry a source.
- No new `MediaPlayerEntityFeature` bits: display needs none.
- When the resolved source has no `now_playing`, every property returns `None` so the
  media card collapses instead of going stale.

Optional refinement, worth doing only if it reads well: today the group's `state` is
`PLAYING` whenever a present source is linked. With `NowPlaying.state` it could report
`PAUSED` honestly. Small, but it changes a state machine that automations may already depend
on — keep it separate from the display work.

---

## 4. Work breakdown

**WP0 — Make the integration frame-type-aware (prerequisite, and a live bug).**
✅ **Fixed in the working tree, 2026-08-10** — `async_routing_ws_messages()` now switches on
`type`, with
[`tests/test_routing_ws.py`](../custom_components/pipewire_audio_router/tests/test_routing_ws.py)
covering it (5 tests; 3 of them fail against the pre-fix code). **Not yet deployed** to the
live instance, which still runs an older copy of the integration.
`async_routing_ws_messages()` in
[`api.py`](../custom_components/pipewire_audio_router/api.py) parses **every** text frame as a
matrix — it never looks at `type`. So a listing frame reaches `_parse_routing_matrix`, whose
`_node()` does `item["display_name"]`, and `OutputInfo` has `node_name`/`name` and no
`display_name`.

This is not hypothetical. On the live instance, right now:

```
2026-08-10 04:01:36.640 ERROR … unexpected error in routing websocket loop
  File ".../api.py", line 393, in async_routing_ws_messages
    yield _parse_routing_matrix(msg.json())
  File ".../api.py", line 126, in _node
    display_name=item["display_name"]
KeyError: 'display_name'
```

— repeating at 04:01:36, :41, :46, :51, :56, 04:02:01. Exactly 5 s apart, which is
`ROUTING_WS_RECONNECT_SECONDS`. The daemon sends `push_listings` immediately after the first
matrix on connect, so the loop is: connect → matrix frame (applied) → `outputs` frame →
`KeyError` → the loop's `except Exception` → 5 s backoff → reconnect. **The routing socket
never survives its first second**, so the push path has silently degraded to 5-second
polling, with an ERROR traceback every 5 s.

Fix: switch on `type` (treating a missing `type` as a matrix, which preserves the
forward-compat rule the `Frame` enum's doc comment describes) and ignore unknown frames. Two
reasons it is WP0 rather than a footnote: a `now_playing` frame carries neither `sources` nor
`outputs`, so under today's code it would parse as an **empty matrix** and blank every
entity's source until the next 250 ms matrix frame — and the bug is worth fixing on its own
merits regardless of this plan.

**WP1 — Daemon model + the AirPlay producer.** ✅ **Built 2026-08-10.**
[`now_playing.rs`](../pipewire_audio_router/bridge-daemon/src/now_playing.rs) holds the model,
the per-source store, the TTL and the explicit clear (13 unit tests); `airplay_source.rs`'s
`Handler` implements `on_metadata` / `on_coverart` / `on_progress`; `routing.rs` gained
`Frame::NowPlaying` with its own dedupe slot; api.rs gained the read, artwork, ingest and
report routes (documented in [api-reference.md](api-reference.md#now-playing-per-source-metadata)).

Verified against a locally-run daemon, not only in unit tests: a self-identifying report
resolves to the right source, a position-only `PUT` keeps the title, an unknown node and an
unknown port both 404, an empty `PUT` is 400, an empty report clears, deleting the source drops
its metadata, and on the socket the `now_playing` frame appears **once** per change (a repeated
identical report produces no second frame). Embedded cover art is the one path only a real
AirPlay sender can exercise — WP6.

**WP2 — HA integration.** ✅ **Built 2026-08-10.** The client yields a `NowPlayingFrame`
alongside matrices, the coordinator keeps `now_playing` by source node name and exposes one
`now_playing_for()` lookup, and `_SourceMetadataMixin` in
[`media_player.py`](../custom_components/pipewire_audio_router/media_player.py) supplies the
`media_*` properties to **both** the per-output and the music-group entity. Each resolves its
source through a shared `_linked_source_names()`, so the `source` chip and the media card cannot
disagree. 8 tests in
[`tests/test_now_playing.py`](../custom_components/pipewire_audio_router/tests/test_now_playing.py).

**WP3 — The Pi reporter (Bluetooth).** ✅ **Built 2026-08-10, not deployed.**
[`bt_metadata_reporter.py`](../firmware/pi-bridge/bt_metadata_reporter.py) watches
`org.bluez.MediaPlayer1` (`PropertiesChanged` + `InterfacesAdded`/`Removed`, no polling) and
posts to `/api/now_playing/report`; `setup_pi_bridge.py` installs it as
`bt-metadata-reporter.service`, adds `python3-dbus`/`python3-gi`, extends the advert to
`role=rtp-sender,metadata` + `meta_ver=1` **keeping `ver=1`**, and removes it again on
`--disable` or `--no-metadata`. A daemon-side test pins the advert compatibility
(`a_metadata_capable_bridge_still_parses`). Three device-level unknowns, settled on the Pi
before writing it:

- **No D-Bus bindings are installed.** `python3 -c "import dbus"` and `import gi` both fail
  on the Pi today (Raspbian Trixie). So the reporter needs `python3-dbus` or `python3-gi`
  via apt (preferred — the setup script already installs apt packages), or a pip dependency,
  or it is not Python at all.
- **`mpris-proxy.service` is already running** in the user session (a stock BlueZ unit, not
  ours). It may expose the connected phone as an MPRIS player, which would be a friendlier
  interface than raw BlueZ — but verify empirically with a phone connected before designing
  around it. `org.bluez.MediaPlayer1` on the system bus is the authoritative source either
  way.
- **The player object is transient.** `/org/bluez/hci0/dev_XX_../playerN` exists only while
  an AVRCP target is connected — with both phones away, `busctl tree org.bluez` shows the
  two `dev_*` nodes and no player. The reporter must watch `InterfacesAdded`/`Removed` and
  must not assume a path.

**WP4 — The pi-ytmusic producer.** ✅ **Built 2026-08-10, not deployed.**
[`receiver/metadata.js`](../firmware/pi-ytmusic/receiver/metadata.js) plus a new
`observeProperty` on the mpv client (replayed after an mpv respawn, or reporting would go silent
after one crash); the player hands it the video id so artwork is known before yt-dlp has
resolved a title, and the setup script passes `YTCR_ADDON_HOST`/`YTCR_ADDON_API_PORT`/
`YTCR_RTP_PORT` (absent ⇒ reporting off, so this role can still run as a pure RTP sender).
Smoke-tested against a local HTTP server: the posted bodies match the daemon's contract,
identical reports are deduped, and shutdown clears. The design as planned:

`observe_property` on `media-title`/`duration`/`pause` in
[`mpv.js`](../firmware/pi-ytmusic/receiver/mpv.js), plus `Player.on('state')` from
`yt-cast-receiver` for status/position, plus the derived `i.ytimg.com` artwork URL — reusing
WP3's dial-out client. Note the finding from the YTM plan: the Lounge protocol carries **no**
metadata (a `Video` is `{id, client, context}`; the only `thumbnail` fields in that library
are Google-account avatars), so mpv/yt-dlp is the only source. `media-title` is one combined
string; splitting artist/album needs a second resolve (`yt-dlp --dump-single-json` or an
innertube library) and is explicitly out of scope until it annoys.

**WP8 — Now playing in the routing graph.** ✅ **Built 2026-08-10.** The store keeps a
`nowPlaying` slice (replaced wholesale, so a cleared source disappears) with `nowPlayingOf` /
`artworkOf` helpers, and each **source card in the routing graph grows a second row**: cover
art, a paused glyph, title, artist, and the full string in the row's tooltip.

Two details worth keeping:

- **The left column is now laid out like the right one.** Source cards used to be a fixed
  `ROW_SRC` and their handle positions pure `i * (ROW_SRC + GAP)` arithmetic; a card that grows
  only when its input reports a track needs the cumulative `srcLayout` the target column
  already had. The handle stays on the **name row's** center, not the card's, so gaining a
  track line does not drag a wire down.
- **`Artwork::Embedded.path` is why the UI needed no new knowledge.** It is daemon-relative and
  rev-stamped, so it drops straight into an `<img src>` and works behind ingress; a producer's
  absolute URL works the same way. Both are decoration — a broken image hides itself rather than
  leaving a hole where the title goes.

Verified by screenshot in **both themes** with the headless harness (mock daemon + `firefox
--headless --screenshot`), including the case where one source has art and another has none.

**WP5 — sendspin displays (optional, later).** `metadata@v1` / `MetadataState` out to adopted
sendspin devices for the source routed to them.

**WP6 — Validation.** §6.

**WP7 — Split the routing socket's hot frame (measured 2026-08-10).**
✅ **Built and green, 2026-08-10. Not deployed.** Not required for the feature, but it is the
same socket, and one of its numbers changes WP0's deployment story — so **deploy it with WP0,
not after WP6**, despite being written last here. What shipped is below the measurements, under
*What was built*.

Measured against the live instance, per client, at idle:

| | |
|---|---|
| matrix frame | **2 210 B, 75 fields** (3 sources, 7 outputs, 7 links) |
| rate | **4.2/s**, per socket — each has its own `interval(250ms)` (verified: 16 clients × 108 frames / 25 s) |
| the part that needs that rate | **`peak`, 3 floats, 36 B — 1.6 % of the frame** |
| static config + `links` | 1 650 B = **73 %**, re-serialized 4×/s |
| consecutive frames content-identical | **49 of 49** over 12 s; exactly 50 matrix frames, i.e. the `changes` notifier fired ~0 times |
| sustained wire cost | **9.0 KiB/s per client** for zero information (a peaks-only frame: 0.14 KiB/s) |

**Where the cost is not.** Daemon CPU: baseline 37.4 ticks/s (≈37 % of one core of four,
mostly audio); adding 16 watching clients — 64 full `build_snapshot()`s per second — moved it
to 40.5, i.e. **≈0.2 % of a core per client**, ~0.5 ms per rebuild. Do not argue this on CPU.
The real daemon-side objection is that each of those rebuilds takes three async mutexes
(`sendspin_control`, `ap2_control`, `agents`) plus ~8 sync ones **including the PipeWire
registry lock**, four times a second per client, to produce a byte-identical frame.

**Where the cost is.**

- **The browser.** `frontend/src/lib/routing.ts` publishes a fresh state object per frame, so
  `$routing.matrix` changes identity, `FlowGraph.svelte`'s `S`/`O`/`links` `$derived` all
  invalidate, and the graph's layout recomputes 4.2×/s permanently. The only genuine 4 Hz
  consumers are the source meter bars (FlowGraph + `SourcesTab.svelte`), the wire-flow
  animation (`peak > 0.02`), and xrun rising-edge detection.
- **HA, but not yet — this is the sequencing finding.** `_apply_routing` →
  `async_update_listeners()` re-renders all 7 registry entities (4 `media_player`, 2 `number`,
  1 `switch`) per frame. No database churn: HA core's `async_set` sees `same_state and
  same_attr` and fires only `EVENT_STATE_REPORTED`, which the recorder does not subscribe to —
  so the cost is ~29 pointless state writes/s in the event loop. The live instance is not
  paying it yet **only because of the WP0 bug**: the socket dies on the first `outputs` frame
  every 5 s (confirmed again 05:35, 2026-08-10), so HA currently receives ~0.2 matrix frames/s
  by reconnect-polling. **Deploying WP0 alone takes that from 0.2 to 4.2 Hz — a 21× jump.**

**The split**, reusing the mechanism the listing frames already proved:

- **`Frame::Meters`** — `{node_name: peak, xruns}` for present nodes, on the existing 250 ms
  tick. Needs only `MeterHub` plus the xrun map: no `build_snapshot`, no registry lock, no
  async mutexes. ~90 B instead of 2 210, and it can go silent entirely once every peak has
  settled — send the decay-to-zero frame, then nothing until something moves.
- **`Frame::Matrix`** — on `changes` only, through `push_if_changed` with one more
  `SentListings` slot. The dedupe is what makes "on change" honest, since that notifier fires
  for *any* daemon change; and comparing the serialized payload is what keeps anyone from
  having to maintain a which-event-affects-which-frame table.

Two things not to break: the `Frame` enum is internally tagged so an older cached UI ignores an
unknown `meters` frame — its meters freeze while the matrix still updates on change, which is
acceptable because daemon and UI ship together — and the "first watcher arms metering and
profiling, last disarms" gating hangs off socket open/close, so it is untouched.

Also worth noting: `xruns` is absent from the live payload entirely (every node `None`), so
either the deployed daemon predates `profiler.rs` or nothing populates it. Settle that while
building the meters frame, since xruns is the other field that wants the fast lane.

### What was built

Both frames as designed. `Frame::Meters` is `{nodes: {<node_name>: {peak?, xruns?}}}`, built by
`meter_samples()` from the meter hub and the profiler map only — no `build_snapshot`, no
registry lock, no async mutexes. `Frame::Matrix` is change-driven through `push_matrix()`.
**Every frame on the socket now goes through `push_if_changed`**; the unconditional
`send_frame` path is gone. `api-reference.md` documents both, and the four new unit tests pin
the properties that matter (206 → 207 daemon tests, 58 integration tests, `npm run check`
clean).

Three things came out of building it that the plan above did not anticipate:

1. **Absent means zero, for both fields.** Omitting `peak` at 0.0 and `xruns` at 0 is what
   makes an idle house cost nothing: a silent system sends `{"type":"meters","nodes":{}}` once
   and then stops. It also keeps the profiler's zero-valued entries — which it reports for
   *every* active node in the graph — from padding the frame. The client must therefore read
   absence as zero, which is how a level decaying to silence is expressed. Documented in
   `api-reference.md` and asserted in `zero_xruns_are_not_reported`.
2. **The frame is bounded by the last matrix.** The profiler's map is graph-wide, so the socket
   keeps the node names from the matrix it last sent and the meters frame may only mention
   those. Without that, splitting the frame could have made it *larger* than what it replaced.
3. **The audit found a bug, and there is deliberately no net under it.** The matrix is now
   pushed only when something notifies `changes`, so every mutation path touching a matrix
   field was checked. All the ones that matter do notify — link/unlink,
   adopt/ignore/unpair/remove, rename, the sendspin and AP2 volume/mute stores (including
   `note_reported_volume`, i.e. a knob turned on the speaker itself), liveness, discovery,
   pairing — **except `set_output_latency`**, the AP2 render delay, which had been reaching the
   UI only because the old 250 ms push covered for it. Fixed with an explicit notification.

   A periodic re-check was built as a safety net and then **removed on purpose**: a missing
   notification must fail *visibly*. A stale graph is a bug report; a two-second self-heal is a
   bug that ships, and `latency_ms` is the proof — it survived precisely because an
   unconditional timer was papering over it. What replaces the net is an **invariant stated in
   `routing.rs`'s module header**: anything that changes a `RoutingNode` field or the link set
   must notify `AppState::changes`, because nothing else will.

**Still to confirm on the live instance** (it needs the deploy, so it belongs with WP6): that
the meters frame really does fall silent at idle and resume with audio, that the graph's wire
animation and meter bars are indistinguishable from before, and whether `xruns` populates at
all once a daemon that *has* `profiler.rs` is running — the open question above is unresolved
and cannot be settled from here.

WP1+WP2 are the whole feature for AirPlay, which is the source with the best metadata in the
house. WP3/WP4 are additive and independent of each other. WP7 is independent of all of them
except in its scheduling against WP0.

---

## 5. Risks and open questions

- **Staleness is the main way this looks broken.** TTL plus explicit clear plus "blank on
  disconnect" (§3.1). Test it deliberately: pull the phone off the network mid-track.
- **Position leads the sound** by the jitter buffer plus output latency (order 200–400 ms for
  RTP; AirPlay has its own producer prebuffer). Do not correct it — same accepted drift as
  the YTM plan's WP2.
- **Fan-out semantics.** One source feeds N outputs, so N entities show the same track; one
  output has at most one source. Both are fine and need no rule. Music groups follow
  decision 1.
- **Keep it off the hot frame** (§3.2). The socket's `matrix` frame already goes out four
  times a second for the meters, so the risk is not repaint cost — it is letting per-song
  descriptive data ride a 250 ms frame. Its own dedupe-gated frame is the design; anything
  that quietly moves a field back onto `RoutingNode` undoes it.
- **Bluetooth has no cover art**, ever (§3.3).
- **Unauthenticated ingest** is a deliberate, bounded tradeoff (§3.4), with a known upgrade
  path.
- **Scope guard: display only.** Control flowing *back* — HA pausing the phone over AVRCP, or
  driving the Lounge session — is a much larger feature touching two protocols and a
  permission model. If it is ever built, the enum-command discipline in the receiver agent's
  protocol is the precedent to copy, not a generic passthrough.
- Open: does `player.rs` (announcements) want to report anything, or would that just fight
  with the source metadata of whatever it is ducking? Assume no.
- ~~Does anything need per-source metadata in the **web UI**, or is HA the only consumer?~~
  **Answered yes, 2026-08-10 — WP8.** The prediction held: the frame was already on the socket
  the UI reads, so the whole change was a store slice plus a row. The Sources tab still shows
  nothing; the routing graph is where "what is this input playing" is actually a question.

---

## 6. Acceptance (live instance)

- AirPlay from a phone into `airplay-in`, routed to Dusche: HA shows title, artist, album and
  **cover art** on that output's `media_player`, updating on track change within a second.
- The same source added to a music group: the group entity shows identical metadata to its
  members (decision 1), and the group's `source` chip and its metadata never disagree.
- Bluetooth phone on the Pi: title/artist/album/duration appear; no artwork; no 5-second
  polling in the reporter (`PropertiesChanged`-driven).
- Stop the sender: metadata clears within the TTL, and the media card collapses rather than
  freezing on the last track.
- Restart the daemon while a source is playing: metadata reappears without a source reload.
- **The socket stays up.** With WP0 in, the HA log shows no `routing websocket loop`
  traceback and no 5-second reconnect cadence — i.e. the matrix is genuinely pushed again,
  which is a fix this plan inherits rather than causes.
- **The hot frame did not grow.** Watch `/api/routing/ws` with a source playing: `matrix`
  frames still arrive at 250 ms and are byte-identical to before, while `now_playing` frames
  appear only on track change plus the ~5 s position tick.
- **No audio regression:** run the RTP + AirPlay sources with metadata flowing and confirm the
  profiler's xrun badges stay clean.
