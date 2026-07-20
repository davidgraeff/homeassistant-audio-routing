# Design decisions

Reference material for *why* the system is built the way it is. Each
entry was an actual investigation with a concrete finding, not a
preference — most link to a `spikes/*.md` write-up with the full
evidence trail (packet captures, signal measurements, exact error
messages). If you're about to second-guess one of these, read the
linked spike first; there's a good chance the "obvious" alternative was
already tried and failed for a specific, documented reason.

## Why replace Music Assistant at all

MA's audio engine is pure Python (asyncio + soundfile/ffmpeg
subprocesses), which on a Raspberry Pi 4 caused audible stutter,
multi-second stream startup, and noticeable output delay. PipeWire's
graph is a compiled C daemon with realtime scheduling (rtkit) and mature
routing tooling — the goal was to keep MA's *idea* (unified sources →
mixed room outputs → HA entities) while reimplementing the engine on top
of PipeWire instead of Python audio processing.

## Container base: Ubuntu 26.04 LTS, not Debian

Debian bookworm ships PipeWire 0.3.65, which creates RAOP sink nodes
fine but **never performs the actual RTSP handshake with a real
receiver** — a silent, version-specific bug found via packet capture,
not a config problem. Ubuntu 26.04 LTS (PipeWire 1.6.2 / WirePlumber
0.5.13) works end-to-end against real hardware. Debian trixie (1.4.2)
also works but Ubuntu 26.04 was chosen for the better freshness/support
horizon. See `spikes/02-raop-static-sink.md`.

## Bridge daemon language: Rust, not Python

The original plan defaulted to Python ("fast iteration... performance
not critical, it's control-plane only"). Spike 4
(`spikes/04-graph-control.md`) changed that once actually measured:

- Python shelling out to `pw-link`/`pw-cli`/`pw-dump`: ~8ms state read,
  ~16ms per link create+destroy round trip. Fine for a human clicking a
  button, but no native Python PipeWire binding exists worth using.
- Rust via `pipewire-rs` (the official upstream binding): full
  node/port/link/registry control, **0.07ms** per link round trip via a
  persistent connection — roughly 230x faster — plus live registry
  state via event listeners instead of polling `pw-dump`.
- Go: no viable binding exists (checked two candidates — one
  early-stage/playback-only, one a read-only `pw-dump --monitor`
  wrapper).

Rust's web ecosystem (`axum`, `serde`) meant the *entire* daemon —
graph control, REST/WS API, static UI — could ship as one compiled
binary, not just the PipeWire layer. (The sendspin sink adapter was
originally kept as a separate Python process per output — "rewriting that
in Rust wasn't justified" — but that conclusion was later reversed; see
"Sendspin sink adapter rewritten in Rust, embedded in bridge-daemon" below.)

## No MQTT `media_player` platform — a custom integration is required

Home Assistant core has **no MQTT `media_player` platform at all** —
confirmed against `homeassistant/components/mqtt/const.py`'s
`ENTITY_PLATFORMS` list (no `media_player` entry) and a 404 on the
would-be docs page. This is a 5+ year open, unresolved community feature
request, not a gap in this project's research — people who want an
"MQTT media player" today reach for third-party `custom_components`
that wrap MQTT themselves. So a `custom_components/` Python integration
was never optional.

**Consequence, not just a cost:** the custom integration gets real
`MediaPlayerEntityFeature.MEDIA_ANNOUNCE` support, which MQTT could
never have expressed regardless. Investigated directly against HA core:
the `announce` kwarg on `play_media` is forwarded straight into the
entity's `async_play_media(...)` with **no core-provided pause/duck/
resume logic** — every integration that supports it (Sonos, ESPHome's
`speaker`/`speaker_source`) implements its own overlay/duck/restore,
same as this project's announce-ducking design. Also note: HA core has
**no distinct "announcing" state** — even with `MEDIA_ANNOUNCE`
declared, nothing in HA's state machine reflects "this output is
mid-announcement" unless the integration adds its own attribute for it
(not currently done here).

## RAOP quirks found only by testing against real hardware

Two assumptions from PipeWire's own example configs turned out to be
wrong against real AV receivers (Pioneer VSX-934, a "Dusche" ESP
device), both in `spikes/02-raop-static-sink.md`:

- **`raop.port` is not reliably 5000.** Both real devices advertise RTSP
  on port **7000** via mDNS. A static config generator can't hardcode
  5000 — it needs the real port from mDNS discovery, or an explicit
  field. The failure mode is silent: writing audio into the sink
  reports success regardless, since the RTSP handshake fails
  independently with no error propagated back.
- **`raop.encryption.type` is not reliably `"none"`.** Both devices
  return `403 Forbidden` on `ANNOUNCE` with `"none"` or `"RSA"` — they
  require the Apple device-verification handshake (`"auth_setup"`, a
  real certificate exchange). The add-on defaults to `auth_setup` and
  treats `none`/`RSA` as fallbacks, not the other way around.

## PipeWire has no per-link volume

The original TTS-ducking design assumed a `volume` property could be
set on a **Link** (source→sink connection) to duck just that one path.
Spike 5 (`spikes/05-tts-ducking-mechanism.md`) found this doesn't exist:
PipeWire Links carry no Props/gain stage at all (Format param only) — a
`volume` property set via `pw-link -p` is stored but has zero audible
effect. The real mechanism is **per-source-**node** volume** — the node's
SPA `Props` `channelVolumes`, set natively (`volume.rs`), already used by
the daemon's `/api/media_players/:id/volume` endpoint. A real A/B/restore signal test confirmed this ducks only the
intended source while a second source mixed into the same sink is
unaffected, with a clean restore to the original level. A related real
bug caught during end-to-end testing: a stereo source contributes two
`Link` objects (FL+FR) sharing one output node — ducking/restoring per
*link* instead of per distinct *node* double-applied the duck/restore
and had to be fixed to dedupe by node id.

## Loading PipeWire modules at runtime

The native PipeWire *wire protocol* has no "load module" RPC:
`pw_core_methods` in `pipewire/core.h` is exactly eight methods —
`add_listener`, `hello`, `sync`, `pong`, `error`, `get_registry`,
`create_object`, `destroy` — with no `load_module`. So a connected
client genuinely cannot tell the *daemon process* to `dlopen` a module.
That part was confirmed by reading the header directly.

An earlier version of this project drew the wrong conclusion from that
fact — "so modules can only be loaded from the daemon's startup config,
outputs can't change without a restart." That is **false**, and worth
correcting because it drove a design choice. Runtime module loading
plainly exists, verified against the live PipeWire (1.6.7) on the dev
machine two ways:

- **`pw_context_load_module()`** (`pipewire/impl-module.h`) loads a
  module into *the calling process's own* `pw_context`. That's what
  `pw-cli load-module` uses. Verified: a non-interactive
  `pw-cli load-module` returns and the node vanishes immediately —
  because the module is hosted *in pw-cli's process* and dies with it —
  but a **long-lived** client that keeps its context alive keeps the
  module (and the node it creates) alive too.
- **`pactl load-module`** works as well, against `pipewire-pulse` — a
  separate, long-lived PulseAudio-compatibility server whose protocol
  *does* have a load-module command. Verified: `pactl load-module
  module-null-sink …` returns a module index and the sink persists,
  hosted by the `pipewire-pulse` process.

So the real constraint is **ownership/lifetime, not possibility**: a
hot-loaded module is owned by whichever process loaded it, never adopted
into the PipeWire daemon's own lifetime.

That is exactly the lever the bridge daemon uses for **hot-reloadable
RAOP outputs**. The daemon is itself a long-lived PipeWire client, so it
loads `libpipewire-module-raop-sink` into its *own* context at runtime —
one module per output — and adding/removing/changing an output
loads/unloads just that one module, with no PipeWire restart and no
disturbance to audio already flowing through the other outputs. Because
`pipewire-rs` 0.10 doesn't wrap `pw_context_load_module` (it lives in
`impl-module.h`, outside the crate's bindgen surface), the daemon
declares the two C functions (`pw_context_load_module` /
`pw_impl_module_destroy`) itself and calls them on its PipeWire thread
via a `pipewire::channel` command channel — see
`bridge-daemon/src/pw_module.rs` and `pw_thread.rs`. Status and control
surface: [roadmap.md](roadmap.md#raop-output-hot-reload--done).

Sendspin outputs never had this constraint anyway — their sink node is a
plain `create_object` node (the native protocol method that *is* in
`pw_core_methods`), not a loaded module. The daemon now creates it natively
on its PipeWire thread (`pw_thread.rs`'s `CreateSinkNode`); see "Sendspin
sink adapter rewritten in Rust, embedded in bridge-daemon" below.

## Link mutation is native `pipewire-rs`, not a `pw-link` subprocess

Spike 4 measured native link creation at 0.07ms vs. ~16ms for shelling
out to `pw-link` — a 230x difference. An earlier iteration nonetheless
kept the mutation endpoints (`POST /api/links`, `POST /api/routing/link`/
`unlink`) on `pw-link`, judging the thread-safety work not worth it for
human-paced UI clicks. That's since been done: the daemon already owns a
`pipewire::channel` command channel into its PipeWire event-loop thread
(for runtime module loading, above), so routing link create/destroy now
go through it natively — `Core::create_object` with the `link-factory`,
and `Registry::destroy_global` by link id. Idempotency (creating a link
that already exists) and targeted unlink are decided against the observed
registry state, not by matching subprocess stderr for `"File exists"`.
Combined with native volume (`Props`/`channelVolumes`) and native announce
playback (`pw::stream`), the daemon now speaks one native PipeWire API
with no `pw-link`/`pw-cat`/`wpctl`/`ffmpeg` subprocesses left. See
`bridge-daemon/src/pw_thread.rs`, `routing.rs`, `volume.rs`, `player.rs`.

## Source & sendspin processes are daemon-supervised, not spawned by `run.sh`

> **Update (fully superseded):** there are no supervised subprocesses left.
> Sendspin outputs became an embedded native server (see "Sendspin sink
> adapter rewritten in Rust" below), and the AirPlay-receive source became a
> native in-process receiver too (see "Native AirPlay-receive source" below),
> so `supervisor.rs` was removed entirely. `run.sh` is now just infrastructure
> (a D-Bus session bus, PipeWire, WirePlumber) + the daemon — the native
> AirPlay receiver also dropped the avahi + system-D-Bus requirement (all mDNS
> is `mdns-sd` now). The paragraph below is kept for historical context on the
> interim supervised design.

`shairport-sync` (the AirPlay-receive source) and `sendspin-adapter.py`
(one per sendspin output) are external processes — unlike RAOP outputs,
which are native PipeWire modules the daemon loads in-process (above),
these have no in-daemon equivalent. An earlier iteration had `run.sh`
spawn them once at boot from a `bridge-daemon runtime-plan` dump of
`options.json`, so changing the source name or a sendspin output required
an add-on restart. The daemon now **supervises** them itself
(`supervisor.rs`): it spawns them from a persisted `/data/sources.json`
store at startup, reconciles them live on API changes
(`/api/source/airplay`, `/api/sendspin_outputs`), and kills them on
removal and on its own graceful shutdown (SIGTERM). This makes *every*
user-facing config runtime-managed and persisted, matching RAOP outputs;
`run.sh` is left with just infrastructure (D-Bus, PipeWire, WirePlumber,
avahi) + the daemon, and the `runtime-plan` subcommand is gone. A crashed
child is reported as not-running rather than fatal (the same `nofail`
stance as a bad RAOP device); the daemon exiting still restarts the
container.

## No `options.json` seeding — runtime config only

For a while the daemon's stores were *seeded* once from the add-on's
`options.json` (`outputs`, `airplay_source_name`, `sendspin_outputs`) on
first run, then ignored. User testing found this genuinely confusing: the
fields stayed visible and editable in the add-on's Configuration tab but
had no effect after the first start — people edited them expecting a
change and got none. So the options (and their `schema`) were removed from
`config.yaml` entirely, along with the `AddonOptions` parsing and the
`--options` flag. The stores (`/data/raop-outputs.json`,
`/data/sources.json`) now start empty on a fresh install and are populated
only at runtime — via the REST API / web UI, and (for RAOP) mDNS
auto-discovery. Existing installs keep whatever their stores already hold;
nothing re-reads `options.json`. The trade-off — no declarative up-front
config in the add-on UI — is acceptable because the whole point of this
project is live, no-restart reconfiguration, and one clear source of truth
beats two competing ones.

## `host_network: true` is required, not a convenience

Both RAOP output and RTP-sourced input need to send/receive *unsolicited*
traffic on the LAN. Default Docker bridge networking isolates the
container from the host's other interfaces by design — confirmed
necessary empirically in `spikes/02-raop-static-sink.md` and
`spikes/03b-rtp-pc-transfer.md`, and consistent with every other
real-device test in this project. Multicast in particular has no "port"
to map, so port-mapping doesn't help either — only host networking (or
a macvlan/ipvlan network) removes the isolation.

## `shairport-sync` needs a real D-Bus system bus + avahi

> **Update (superseded):** the AirPlay-receive source no longer uses
> shairport-sync at all — it's a native in-process receiver (see "Native
> AirPlay-receive source" below), which removes this D-Bus-system-bus
> requirement. Kept for the historical finding.

Discovered in `spikes/shairport-sync-source.md`: shairport-sync
hard-requires a working `avahi-daemon` and a real D-Bus **system** bus
to even start (fatal exit otherwise) — the private session bus this
project's containers already set up for PipeWire's own portal/rtkit
probing isn't enough. PipeWire/WirePlumber themselves don't need the
system bus. Its PipeWire node also only exists while a session is
actively playing (lazy activation, same pattern as RAOP/RTP), which the
real bridge daemon handles fine (it holds a live registry listener) but
means "link ahead of time" isn't possible — the daemon must react to
the node appearing.

## TTS/announce ducking: URL-based (v1) and Wyoming-based (v2), additive

`POST /api/media_players/:node_id/announce` accepts either `url` (fetch
+ decode to WAV via `symphonia` — pure Rust, no system dependency —
works with HA's standard `tts.speak` contract unchanged) or `wyoming`
(direct synthesis via the Wyoming protocol, no decode step at all) —
mutually exclusive, chosen per call via HA's standard
`play_media` `extra` dict (the same mechanism other integrations use for
implementation-specific options), not a daemon-wide mode switch or
transparent interception. Either way, the daemon ducks every distinct
source node currently linked into the target sink, plays the clip, and
unconditionally restores original volumes even on failure.

The Wyoming client (`wyoming.rs`) buffers the full `audio-chunk` stream
into memory and builds one WAV before playback, rather than streaming
chunk-by-chunk into a live buffer — a deliberate simplification once it
came time to build it: for a short announce clip this buffering is
milliseconds, so the latency win over v1 (skipping the render-to-file-
then-HTTP-fetch round trip) is preserved without needing a push/callback
mixer interface. **Still unaddressed**: HA's built-in TTS cache (keyed
on engine+text+language+options) only exists in the `url`/file-based
code path — repeating the same `wyoming` text re-synthesizes it every
time, with no caching layer of our own. Low-priority as long as usage
stays occasional announcements rather than frequently-repeated phrases.

## Decoding announce audio: `symphonia`, not an `ffmpeg` subprocess

The `url` announce path (above) originally shelled out to `ffmpeg -i
<fetched> <wav>` to decode whatever format HA's TTS integration
happened to render (usually mp3). Investigated why the runtime image was
864MB — roughly double what its actual contents (a Rust binary, a
Python sendspin adapter, PipeWire/WirePlumber) should need.

**`ffmpeg` alone accounted for ~250-300MB**, confirmed two ways: (1)
`docker history` on the built image showed the apt-get layer at 524MB
total; (2) installing *only* `ffmpeg` via `--no-install-recommends` on a
clean `ubuntu:26.04` container pulled `libllvm21` (130MB!),
`mesa-libgallium` (48MB), `libplacebo360` (9MB, GPU color management),
`libx265` (8.5MB, HEVC), `libcodec2` (16.5MB), plus X11/OpenCL/Kerberos
libraries — Ubuntu's `ffmpeg` package is built with full GPU-accelerated
video transcoding and broad protocol support, none of which this project
uses. Our only use of it was decoding a short spoken-word clip to WAV.

(The other major layer, ~212MB of `pip install aiosendspin[server]`
pulling `av`/`numpy`/`pillow`, is **not** the same kind of waste —
verified directly by uninstalling them and trying to import
`SendspinServer`: `pillow` is a hard import-time dependency of the
artwork/album-art role, `numpy` of the visualizer role. Both are baked
into the same shared library Music Assistant depends on; not something
to patch around for a packaging win.)

**Fix**: replaced the `ffmpeg` subprocess with `symphonia`
(`decode.rs`), a pure-Rust decoder with zero system dependencies —
probes the format from content (mp3/wav/aac/ogg/flac all work
unmodified) and decodes to a `SampleBuffer<i16>`, which `wav.rs` (shared
with the Wyoming path) turns into a WAV. Removing `ffmpeg` from the
Dockerfile and adding the `symphonia` crate (`features = ["all"]` — the
cost of enabling every codec is a bit more compiled Rust in our own
~8MB binary, not a system dependency tree, so there's no reason to
hand-pick a narrower set) brought the image from **864MB to 456MB**.
Verified functionally identical, not just smaller: the same real-signal
e2e test (`tests/test_addon_announce_ducking_e2e.sh`) produced the exact
same baseline/ducked/restored RMS measurements before and after the
swap.

**Considered and rejected**: rewriting `aiosendspin`'s server essentials
(the other ~212MB) in Rust/Go/C++, prompted by finding
[sendspin-rs](https://github.com/Sendspin/sendspin-rs). That project
implements the **client/receiver** role (a sendspin *player*), not the
**server** role this project needs (accepting ESP32 connections, clock
sync across speakers, pushing audio) — wrong half of the protocol, and
still WIP even for what it does cover. A from-scratch server
reimplementation's real risk isn't library availability (Rust/Go/C++ all
have adequate WebSocket/audio libraries) but multi-room clock
synchronization correctness — the genuinely hard part of this protocol,
currently handled by a shared library Music Assistant also depends on.
Not worth that risk for a packaging win of this size; revisit only if
sendspin-rs's server side matures or the image size becomes a real
problem for the deployment target. (Superseded: this was reversed once a
server role was actually built and tested — see the next section.)

## Sendspin sink adapter rewritten in Rust, embedded in bridge-daemon — supersedes the two entries above

Both "Bridge daemon language: Rust, not Python" and the "Considered and
rejected" paragraph just above concluded a sendspin server rewrite wasn't
worth it, on the same premise: [sendspin-rs](https://github.com/Sendspin/sendspin-rs)
only implemented the client role, so a from-scratch server meant redoing
multi-room clock synchronization from zero — the genuinely hard, risky part.
That premise changed, not the risk calculus around it: a server role was
built and validated on a fork (`server-role-prototype` branch,
`github.com/davidgraeff/sendspin-rs`) — handshake, clock-sync echo,
multi-client synchronized groups, mDNS advertise *and* discover/dial-in for
clients that only run their own embedded server, reconnect-with-backoff —
and tested against real Home Assistant Voice PE hardware. The hard part got
derisked by actually building and testing it, not by re-estimating it.

`sendspin-adapter.py` (a Python subprocess per output, wrapping
`aiosendspin[server]`) is replaced by an embedded native server
(`sendspin_server.rs`), pinning the fork by commit hash (`sendspin = { git =
"...", rev = "..." }` — unreviewed/unmerged, so pinned to an exact commit,
not a branch). This also picks up the two shellouts the Python adapter still
had: sink-node creation now goes through `pw_thread.rs`'s
`CreateSinkNode`/`DestroySinkNode` (the native equivalent of `pw-cli
create-node`, alongside the existing `Load`/`CreateLinks` commands there),
and continuous capture is native PipeWire (`sendspin_capture.rs`, mirroring
`player.rs`'s stream setup but `Direction::Input` with
`STREAM_CAPTURE_SINK`) instead of a `pw-record` subprocess. No longer
daemon-supervised as an external process — and once the AirPlay source went
native too (below), `supervisor.rs` was removed entirely.

> **Update:** this "one server per configured output" model was itself
> superseded — sendspin outputs are no longer manually configured. Devices
> are auto-discovered and servers are formed per synchronized *group* from
> the routing intent; see "Sendspin: auto-discovery, grouping, per-device
> volume, and connection-driven liveness" below.

**Payoff, measured**: image size **456MB → 193MB**; `python3`/`pip`/
`aiosendspin` fully gone. `sendspin-rs` still compiles in `cpal`/`alsa` (its
client-role audio module, which this project's server-only usage never
calls), but the linker drops the dependency entirely — confirmed via `ldd`
on the built binary showing zero ALSA runtime link.

**Scope decision made alongside this**: no historical-replay/late-join
catch-up buffer. A `Group` member added after streaming has already started
gets `stream_start` plus only subsequent audio (covered by a
`sendspin-rs` test), so a late joiner is in sync within about the next
buffer's worth of audio (~100ms) — an accepted trade-off, not a gap.
Discovery is deliberately unfiltered (`ClientManager::start`, not
`start_filtered`) to preserve current behavior exactly: aiosendspin's
`SendspinServer.start_server()` already defaults `discover_clients=True`,
so today's production adapter already has every output/process discover and
dial every such device on the network — this port doesn't introduce a new
device-to-output assignment problem, whatever already arbitrates which
server's connection a device keeps continues to do so unchanged.

## Native AirPlay-receive source: vendored shairplay, not shairport-sync

The AirPlay-receive source was a supervised `shairport-sync` subprocess. On
the Ubuntu base image that build has **no PipeWire output backend** (alsa/
pipe/stdout only), so its decoded audio never reached the graph — the source
could never appear in the routing matrix or be routed. It's replaced by a
**native, in-process RAOP receiver**: a pure-Rust `shairplay` crate whose
`AudioHandler` callback hands us decoded f32 PCM, which we push through a
jitter buffer into a PipeWire source node (`airplay_source.rs`). This removes
the last supervised subprocess *and* the D-Bus-system-bus + avahi requirement
shairport-sync needed.

The crate is **vendored** (`bridge-daemon/vendor/shairplay/`, a `path`
dependency) so it can be patched for PipeWire-sender interop, which needed
three findings (all reproduced locally against PipeWire 1.6.7 with a spike
receiver + `shairport-sync` as a working reference + a loopback `tcpdump`):

1. **A `Server:` header on every RTSP reply.** Without it PipeWire's sender
   sends `OPTIONS`, gets our `200`, and then silently never sends `ANNOUNCE`.
2. **Default to advertising `et=0` (no encryption).** PipeWire picks the
   highest offered encryption, and its **RSA path is broken** in this build
   (it stalls before `ANNOUNCE`), so offering `1` wedges the sender.
   Unencrypted is the reliably-driven path on a trusted LAN. (The advertised
   codecs/encryption are builder-configurable; `auth_setup` — what real
   AirPlay-2 gear negotiates via `et=0,4` — is also handled, but isn't the
   default.)
3. **Advertise `cn=1` (ALAC), not `cn=0,1`.** PipeWire picks the first codec;
   the receiver decodes ALAC (and now raw L16), and PipeWire's "PCM" mode is
   uncompressed-ALAC on the wire anyway.

So the receiver advertises **unencrypted ALAC** — the one combination
PipeWire senders drive correctly on a trusted LAN. Tradeoff: real iOS senders
(which require encryption) can't cast to it; fine for this PipeWire-fed router.
A configurable producer **jitter buffer** (`/api/source/airplay` `latency_msec`,
default 150 ms) rides out clock drift between the receiver and the graph.

## Sendspin: auto-discovery, grouping, per-device volume, and connection-driven liveness

Sendspin outputs were manually created one-per-output. They're now
**auto-discovered** over mDNS (`sendspin_discovery.rs`) and surfaced as
virtual routing outputs (`sendspin-dev-<slug>`), mirroring how RAOP devices
appear. Devices a user routes from the **same set of sources** are formed into
one **synchronized group** automatically (`sendspin_group.rs`): a single sink
+ one embedded server dialing exactly those devices, so "one source → several
speakers in sync" needs no manual group setup and is visible as a group badge
in the UI.

**Per-device volume** (`sendspin_volume.rs`): these virtual outputs have no
PipeWire node volume, so volume is sent in-band over the protocol
(`ServerSender::send_player_command`). Mapping a connection to a device needed
a patch to the (also vendored, `bridge-daemon/vendor/sendspin/`) `sendspin`
crate: its `ClientEvent::Connected` only exposed the client's `client_id`,
which for ESPHome devices is an opaque MAC that doesn't match the advertised
name — the patch adds the dialed mDNS `fullname` so the daemon can map a
connection to the right discovered device.

**Liveness — mDNS is discovery-only.** An mDNS `ServiceRemoved` is usually a
TTL-expiry flap (WiFi power-save on the speaker), not a real departure; acting
on it tore down live groups (and raced the group's server-port rebind). Now a
device is *present* if it has a **live server connection** or an active **TCP
probe** succeeds; mDNS only ever *adds* (`sendspin_liveness.rs`). A device is
demoted to offline (grayed) only after sustained failure, and removed only
after a long grace — so a flap no longer disturbs a playing group. RAOP
discovery gets the same treatment: after a grace-debounce on mDNS removal it
**TCP-probes the receiver's last-known address** (`discovery.rs`) and only
unloads when that *also* fails, re-probing while the receiver stays
mDNS-absent-but-reachable. RAOP has no per-device connection like sendspin's to
lean on, so the probe is the sole liveness signal past the debounce — a receiver
that stops announcing over mDNS but still answers on the wire stays loaded
(previously it was unloaded 90s after the mDNS flap, then reloaded minutes later
when it re-announced). The vendored sendspin server
also binds with `SO_REUSEADDR` so a group recreated on the same port can't
lose the port-rebind race.

## Runtime image trimming: `systemd-standalone-sysusers`, not full `systemd`

Investigated whether the 193MB runtime image (above) could shrink further,
given the container only actually needs PipeWire, D-Bus, a shell, and their
real dependencies. `dpkg-query -Wf` sorted by installed size turned up two
full-size `systemd`/`libsystemd-shared` packages (~18MB combined) that don't
appear in the bare `ubuntu:26.04` base image at all — i.e. something in our
own `apt-get install` line was pulling them in.

**Cause**: `pipewire`'s own Depends is an alternative, `systemd |
systemd-standalone-sysusers` — either can satisfy its install-time need to
create system users/groups. Neither was already installed, so apt defaulted
to the first (full `systemd`), even though this container never runs systemd
as PID 1 (`run.sh` is) and uses nothing else from it. Listing
`systemd-standalone-sysusers` explicitly (340KB, a standalone `sysusers`
binary built for exactly this non-systemd-init case) satisfies the same
dependency; confirmed via `apt-get install --no-install-recommends -s` with
the full package list that nothing else about the resolved set changes.
Paired with stripping `/usr/share/{doc,man,locale}` in the same `RUN` layer
(dead weight for a container that only speaks REST and logs in English) —
**measured 192,750,570 → 175,922,196 bytes** (~16.8MB) on the otherwise
identical image from the sendspin-native-rewrite entry above.

**Investigated and deliberately kept**: `dbus-x11`. It looks like it's only
there for `run.sh`'s own `dbus-launch` call (spec: cheap way to give
PipeWire's portal/rtkit probing a session bus per the entry above), so
dropping the apt package and having `run.sh` call plain `dbus-daemon
--session --fork --print-address` instead looked like a free ~2.8MB win
(dbus-x11 itself plus its `libx11-6`/`libx11-data` chain). Building that and
checking `dpkg -l` in the result showed dbus-x11 still present — `apt-get
install -s` on the package set traced it to `wireplumber` itself, which
hard-Depends on some session-bus provider (`<default-dbus-session-bus>` →
`dbus-user-session` or `dbus-x11`), independent of anything `run.sh` does.
The only other option, `dbus-user-session`, hard-Depends on `systemd` and
`libpam-systemd` — reinstalling the exact ~18MB the fix above just removed.
So `dbus-x11` (118KB installed) plus its X11 chain is genuinely the smaller
of the only two real choices here, not an oversight; `run.sh` keeps
`dbus-launch` rather than churning to `dbus-daemon --session` for no
measurable benefit. Also checked and ruled out: dropping
`pipewire-audio-client-libraries` (its only unique contribution beyond what
`shairport-sync`'s own hard Depends already pull in is the ~746KB
`pipewire-alsa`/`pipewire-jack` shims — not worth the risk of retesting
AirPlay-receive for that little).

> **Update:** `shairport-sync` was later removed from the image entirely
> (AirPlay is now the native in-process receiver, above), so this
> package-baseline reasoning is superseded — the avahi/D-Bus chain is now
> pulled only by the daemon's own mDNS + PipeWire's needs, not shairport. The
> image-size *conclusions* still hold; only the "what already pulls X in"
> attribution changed. Not re-measured here.

## Bluetooth bridge box: hardware and firmware constraints

- **Chip variant is not negotiable.** Confirmed via `esptool chip-id`
  and Espressif's own SoC comparison page: only the original ESP32
  (`ESP32-D0WDQ6` here) has classic Bluetooth (BR/EDR) with
  A2DP(SNK)/AVRCP(CT) at all. ESP32-S2 has no Bluetooth; S3/C3/C6/H2 are
  BLE-5-only with no classic radio — confirmed dead by
  `espressif/esp-idf#16232`, closed **Won't Do**. The USB-UART chip
  visible in `lsusb` (e.g. CP210x) says nothing about which ESP32
  variant is on a board — verify per-board with `esptool chip-id`.
- **No conflict between ESPHome's normal features and owning the
  classic-BT radio.** ESPHome only initializes the Bluetooth
  controller/host stack when YAML includes `esp32_ble:`,
  `esp32_ble_tracker:`, `bluetooth_proxy:`, or `esp32_improv_ble:` —
  confirmed directly against `esphome/components/esp32_ble/ble.cpp`.
  Omit all four and the radio is free for a custom classic-BT component.
  WiFi provisioning (fallback AP + captive portal) is pure WiFi with no
  BLE involvement either, so "WiFi/OTA/HA API" and "own the BT radio"
  were never in tension.
- **`sendspin-cpp` was investigated and ruled out** as the audio-out
  transport from this box. It implements only the sendspin *player*
  role (receives a pushed stream), matching `aiosendspin`'s
  server→client direction — there is no way to use it to *upload*
  captured A2DP audio to a server. The sendspin protocol spec does
  define a `source@v1` role for exactly this, with one WIP reference
  client (`Sendspin/sendspin-jack-bridge`), but `aiosendspin` (the
  server library this project already depends on) has no server-side
  implementation of that role. RTP was the lower-risk choice, reusing
  the already-proven `rtp-source` receiving path instead of building
  both a new ESP32 client and new upstream-style server support.
- **Vendoring a self-contained ESP-IDF component is unsupported by
  ESPHome.** The firmware is a plain `external_components` package
  (own `.cpp`/`.h` + `__init__.py`) calling `esp_a2dp_api.h`/
  `esp_avrc_api.h` directly, following the same pattern ESPHome's own
  `esp32_ble` component uses internally — not a vendored
  `CMakeLists.txt`/`idf_component.yml` directory (tracked as an open,
  unimplemented ESPHome feature request, `esphome/feature-requests#1605`).
- **Real hardware surfaced three bugs no amount of code review would
  have caught:**
  1. `esp_bt_controller_enable` failed at first boot
     (`ESP_ERR_INVALID_ARG`) — the Kconfig `choice` controlling the
     controller's mode defaults to **BLE Only** since nothing in
     ESPHome ever needed classic BT before. Fixed by forcing
     `CONFIG_BTDM_CTRL_MODE_BR_EDR_ONLY=y`.
  2. The BT stack itself warned that AVRC must be initialized before
     A2DP. Fixed by reordering initialization.
  3. PipeWire's `rtp-source` module session-latches onto the first SSRC
     it sees and silently rejects every packet from a later reboot's
     different (randomly generated per-boot) SSRC — invisible anywhere
     except `journalctl --user -u pipewire`. Originally worked around on
     the receiving side with `sess.ignore-ssrc = true`. That has since
     been superseded at the source: the firmware now derives its SSRC
     from the factory MAC (`esp_efuse_mac_get_default` in
     `a2dp_bridge.cpp`), so it is **stable across reboots**. With a stable
     SSRC the receiver can instead run `sess.ignore-ssrc = false` and get
     a useful property — it latches onto that one sender and rejects any
     *other* box hitting the same port, so a stray or second sender can't
     interleave into and corrupt the stream. Both modes are exposed
     (`ignore_ssrc` on `PUT /api/source/rtp`, the "Source" radio in the
     web UI); `true` (accept any sender) stays the default so an install
     with a not-yet-reflashed bridge can't go silent.
- **If BLE is ever added to this same device later**, note that
  ESPHome's `esp32_ble` component — even though it defaults to
  Bluedroid, which supports classic+BLE simultaneously in principle —
  explicitly sets `CONFIG_BT_CLASSIC_ENABLED = False` and reclaims
  classic-BT memory via `esp_bt_controller_mem_release(...)`. That
  default would actively kill the A2DP sink unless both the sdkconfig
  override and the mem-release call are explicitly prevented first.

## Raspberry Pi Bluetooth → RTP bridge

A second, independent Bluetooth-bridge implementation living beside the
ESP32 one: a Raspberry Pi (developed on a Zero 2 W) as an A2DP sink that
forwards to the *same* `bt-bridge-rtp` add-on source. It exists because a
Linux SBC needs **no custom firmware** (BlueZ + PipeWire do A2DP-sink and
RTP-send as pure configuration), gets **real codecs** (aptX/AAC/SBC vs the
ESP32 sink's effectively-SBC-only), and directly replaced a user's prior
high-latency PulseAudio → `darkice` → Icecast setup on the same box. The
deliverable is `firmware/pi-bridge/setup_pi_bridge.py`, an idempotent
configurator — see `firmware/pi-bridge/README.md`. Everything below was a
wall hit during real bring-up (verified end-to-end streaming aptX from a
Samsung S23 → RTP → add-on → speakers), not a preference.

- **WirePlumber 0.5's "seat monitoring" silently disables Bluetooth on a
  headless box — this was the single biggest blocker.** WP 0.5 only
  manages Bluetooth for the user on the *active login seat*
  (`monitor.bluez.seat-monitoring`, backed by `support.logind`). A
  lingering headless session has **no seat**, so the bluez monitor loads
  but registers **no A2DP media endpoint**: the adapter advertises AVRCP
  (`0000110c`/`110e`) but not **Audio Sink (`0000110b`)**, and phones pair
  fine yet show a device with *no audio service*. Fixed by disabling
  `monitor.bluez.seat-monitoring` + `support.logind` for the `main`
  profile. This is also exactly why **WirePlumber 0.4 (Debian bookworm)
  "just worked"** with no such setting — the seat gate didn't exist yet.
  Note bookworm's own Pi archive ships PipeWire **1.2.7** (not stock
  bookworm's broken 0.3.65), so the RAOP-era version concern (see
  "Container base") does not apply to the sender side.
- **`JustWorksRepairing = never` (BlueZ default) blocks re-pairing.** Once
  a device is bonded, BlueZ *refuses* a new "just works" pairing from it
  (an anti-MITM default). So after a phone unpairs (dropping its key) and
  tries to pair again, the Pi rejects it because it still holds the old
  bond — re-pairing fails until the bond is manually purged from
  `/var/lib/bluetooth` (`bluetoothctl remove` alone leaves the `cache/`
  entry behind). Setting `JustWorksRepairing = always` lets a known device
  re-pair and overwrite the stale bond seamlessly.
- **WiFi power-save drops the Bluetooth link on the shared radio.** The
  BCM43xx combo chip shares one 2.4 GHz radio between BT and WiFi;
  NetworkManager's default WiFi power-save parks the radio and
  starves/drops the BT connection, producing intermittent pairing and mid-
  stream drops. Disabled via `wifi.powersave = 2` (+ a boot oneshot). The
  user's prior PulseAudio setup proving BT+WiFi coexist here is what ruled
  out a hardware/antenna limit and pointed at this.
- **A fresh image comes up with the controller unpowered.** Raspberry Pi
  OS defaults `[Policy] AutoEnable` off (commented) in
  `/etc/bluetooth/main.conf`; without it the adapter is `Powered: no` at
  boot, WirePlumber sees no adapter, and no A2DP sink is offered. The
  script sets `AutoEnable = true`.
- **Priority-based capture binding beats `target.object` on WP 0.5.** The
  loopback bridges the phone source → RTP sink; its capture must attach to
  the phone, not the sink's own monitor (a feedback loop). Pinning the
  capture with `target.object = <bluez node>` **backfired on WP 0.5**: it
  added that link *and* kept a fallback link to the sink monitor, mixing
  two clocks and stalling the graph. The robust approach is to give the
  bluez source a high `priority.session` so it always outranks the monitor,
  and let the (default-following) capture bind to the single highest-
  priority source — which also re-binds cleanly on reconnect/re-pair.
- **Restarting PipeWire/WirePlumber orphans the live A2DP transport.**
  After a session restart, bluetoothd still reports the device
  "connected", but the audio transport is dead and the source node is
  gone; neither replaying nor a Pi-side reconnect reliably rebuilds it —
  only a **clean boot** (or a full BT disconnect+reconnect) does. The
  appliance is meant to run from boot, so this is a debugging hazard, not a
  runtime one: don't hot-restart the audio stack under a live phone.
- **`tcpdump` cannot see the RTP egress on this WiFi driver.** Locally-
  generated multicast TX is offloaded past tcpdump's capture hook, so
  tcpdump reports **0 packets even while audio is audibly streaming**.
  Confirm egress with the `wlan0` `tx_packets` counter, the `pw-link`
  capture binding, or the add-on actually playing — not tcpdump. (Several
  bring-up mis-diagnoses came from trusting tcpdump's false zero.)
- **Unicast is more robust than multicast for one Pi → one add-on.**
  Multicast (`239.255.42.42`) fans out to several receivers but depends on
  IGMP group membership that a receiver must re-join after a restart (the
  add-on's **Enable** button does a full `module-rtp-source` reload =
  re-join, so no dedicated "rejoin" control is needed). A point-to-point
  link has no group to lose, so `destination.ip = <add-on IP>` + the add-on
  source set to *Accept all* just resumes after either side restarts.
