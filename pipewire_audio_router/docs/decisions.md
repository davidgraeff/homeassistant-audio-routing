# Bridge daemon — design decisions

Reference material for *why* the bridge daemon (the PipeWire Audio Router
add-on) is built the way it is. Each entry was an actual investigation
with a concrete finding, not a preference — most link to a `spikes/*.md`
write-up or a companion doc with the full evidence trail (packet captures,
signal measurements, exact error messages). If you're about to
second-guess one of these, read the linked evidence first; there's a good
chance the "obvious" alternative was already tried and failed for a
specific, documented reason.

For decisions about the *other* components (the HA integration, the ESP32
and Raspberry Pi Bluetooth bridges) and the project premise, see
[`../../docs/decisions.md`](../../docs/decisions.md). For how the daemon is
put together, see [architecture.md](architecture.md); for what's done vs.
planned, [airplay2-roadmap.md](airplay2-roadmap.md).

---

## AirPlay-2 output — decisions locked in

**Replace the AirPlay-1 / RAOP output path with an in-process Rust AP2
sender.** RAOP (`libpipewire-module-raop-sink` per speaker) is dropped in
favour of `lmcgartland/airplay2-rs` (pairing / SETUP / RTP / streaming) +
OwnTone's MIT `libairptp` via FFI (a conformant gPTP grandmaster). AP2
gives real multi-room sync plus per-output volume/duck/announce that RAOP
could only ever have gotten via deferred work. Proven on Yamaha WX-021 +
Pioneer VSX-934; the user's verdict was "better than AirPlay 1."

**Decisions locked in (2026-07-25):**
- **GPL-2.0 vendoring is fine** — vendor-and-patch like
  `sendspin-rs` / `shairplay-rust`. Keep only pairing/SETUP/RTP/streaming;
  the crate's own Rust-PTP code is **dead** (PTP is driven by libairptp).
- **Hard-drop RAOP, no fallback** — all target receivers are AP2-capable.
  **Done 2026-07-26** (plan: [ap2-track-p4-drop-raop-design.md](ap2-track-p4-drop-raop-design.md)):
  `raop.rs`/`outputs_store.rs`/the RAOP-only `discovery.rs` deleted, the
  `/api/outputs` CRUD + `raop_uses_anchor`/monitor-anchor logic removed,
  `pw_module.rs`/`volume.rs` kept (RTP source + source-ducking still use
  them). **Migration is by node-name rewrite, not store translation**
  (`raop_migration.rs`, one-shot at startup, idempotent): a persisted
  `raop-out-<slug>` link becomes `ap2-dev-<slug>` (same slug —
  `_raop._tcp` and `_airplay._tcp` advertise the same instance label), so
  routing/groups survive the switch; AP2 devices are re-discovered, not
  translated, and the old `/data/raop-outputs.json` is ignored. Once
  removed there is no fallback.
- **`libairptp` is hosted in-daemon**, not as a separate `airptpd`
  sidecar. It binds privileged ports 319/320 once for the whole daemon,
  so the add-on manifest must grant `CAP_NET_BIND_SERVICE` and open
  `319-320/udp`.
- **Align with the Sendspin per-device-sender overhaul** — land AP2 on
  the same `SharedTimeline` / `OutputBackend` / `PerDeviceSender` seam and
  MG/AG routing-target polymorphism, not a parallel AP2 abstraction. See
  [architecture.md](architecture.md#the-outputbackend-seam-target-end-state).

**Codec: realtime ALAC (type 96).** Accepted by the test devices and
needs no `SETRATEANCHORTIME`. Buffered AAC (type 103) is a later option,
not required.

**gPTP is host-global; RTP/grouping is per-group.** One `Ap2PtpService`
grandmaster for the whole daemon; every AP2 receiver across all groups is
a peer of that one clock (mirrors OwnTone: one libairptp + per-session
streams). gPTP lock is **required** for rendering — PT=87 anchors alone
are insufficient — and matters most for multi-room drift. The PT=87 anchor
MUST read `CLOCK_MONOTONIC`, not `CLOCK_MONOTONIC_RAW` and not epoch.
Mechanism in [architecture.md](architecture.md#6-the-two-clocks-cleanly-separated).

**FairPlay / encryption not needed as a sender.** Transient pairing (PIN
3939) + ChaCha20 is sufficient for the third-party AV receivers targeted;
real iOS-grade FairPlay is a receiver concern, not ours.

**Per-device interop is won device-by-device.** Proven on Yamaha +
Pioneer only; validate each brand before trusting the "all AP2-capable"
premise, and keep a per-brand quirk table.

### Vendored `libairptp` patches (why they exist)

Two patches to the vendored `libairptp` are load-bearing and must be kept
(and upstreamed per the `pull_request_docs/` convention):

- **The gPTP event loop runs at `SCHED_FIFO` 55**
  (`vendor/libairptp/src/daemon.c`, top of `run()`). libairptp is supposed
  to emit 8 Sync/s (`AIRPTP_INTERVAL_MS_SYNC = 125`); on a normal-priority
  pthread it was starved by the RT audio threads on the 4-core Pi, dilating
  the timer ~48× so receivers never locked. `CAP_SYS_NICE` bypasses the
  container `RLIMIT_RTPRIO=0`. See the ladder in
  [architecture.md](architecture.md#7-real-time-thread-ladder).
- **The staleness send-gate is removed** (`ptp_msg_handle.c`
  `peers_msg_send()`). It skipped sending Sync to a peer not heard from in
  15 s, but a third-party receiver only replies (Delay_Req) once it is
  *already* locking onto our Sync — a chicken-and-egg that stopped Sync
  15 s after peer-add so the receiver could never lock. We manage peers
  explicitly (add on discovery, remove when gone), so we keep offering the
  clock unconditionally. Wire-confirmed: 0 → ~177 gPTP pkts/6 s.

## Near-real-time principle: small buffers + real-time feeders, not bigger buffers

The end-to-end jitter tolerance for a Sendspin output is the receiver's
**250 ms send-ahead lead** — the only jitter buffer in that path. The
recurring failure mode was timing-critical work sitting on the shared
general-purpose tokio runtime, decoupled from any real-time clock, and
getting preempted past that 250 ms budget. The **decided fix is to make
the connective feeders real-time, not to grow the buffers** (bigger
buffers just add latency and mask the disease). Concretely: the
capture→sender relays and the AP2 producer are dedicated `SCHED_FIFO`
threads, the unbounded capture→relay channel became bounded drop-on-full,
and the AP2 producer moved off the tokio pool onto its own FIFO thread.
The full RT ladder and the reasoning are in
[architecture.md](architecture.md#7-real-time-thread-ladder); the original
hazard analysis is [audio-jitter-analysis.md](audio-jitter-analysis.md).

## Anchor model: one steady driver per group, split only the sender

A hardware finding (Sendspin S3 spike) inverted the naive "a PipeWire node
per device" instinct: a **standalone per-device null-sink is not a steady
clock driver** — it produced ~1 audio glitch / 60 s (QUANT-0), while a
single shared anchor monitor showed zero downstream xruns. So the decided
shape is **feed a sync group from ONE anchor `support.null-audio-sink`
monitor (QUANT-1024 steady driver), and split only the *sender* per
device** — which still gives per-device volume/delay/duck. This is why
both the Sendspin and AP2 backends fan out from a single anchor rather
than owning per-device sinks. Detail in
[architecture.md](architecture.md#4-the-anchor--per-device-sender-model).

## Render delay is retuned live, not by reconnect

Decided 2026-07-26, after a render-delay change silenced a receiver. The AP2
render delay was originally part of the group's **restart identity**, so every
edit tore the RTSP session down and reconnected it. The reconnects "succeeded"
on the wire (RECORD acked, RTP flowing, PTP locked) but a flaky receiver (the
Pioneer) accepted the new session without actually rendering it — and the play
tone / announce paths, riding that same dead session, went silent too. **The
render delay is just the PT=87 anchor offset the streamer reads per packet**, so
there is no reason to reconnect for it: `ap2_control` now sends a live
`SetRenderDelay` to the running streamer (`Connection::set_render_delay_live`),
which shifts the next anchor, and the value is dropped from the restart identity
(only receiver-set + wire-rate changes reconnect). This also makes the delay
actually *sweepable* by ear — which is what the alignment panel's AP2 support
relies on. The persisted value is still used as the initial delay on the next real
(membership/rate) reconnect.

## Sample-rate harmonization: no steady Rust resampling; the graph carries rate conversion

Decided 2026-07-26: **no resampling on the steady hot path — the PipeWire
graph does it.** The internal bus (anchor + `SharedTimeline` + `OverlayMixer`
+ announce assets) is 48 kHz. PipeWire bridges the one 48 kHz anchor to each
per-device capture at that group's wire rate, so the per-quantum relay/mix
never resamples in Rust. Do **not** unify wire rates by hand.

**Wire rate is per-group and negotiated (updated as Phase 4 shipped).**
Sendspin is always 48 kHz. AP2 is per-output `Ap2RateMode`: `Auto` (default)
optimistically streams **48 kHz** and, on a SETUP rejection, learns a
persisted **44.1 kHz** cap for that receiver; `Fixed44100` forces 44.1. A
group captures at 48 kHz iff *every* member's effective rate is 48 kHz (one
capture serves the group), else 44.1 kHz with PipeWire doing 48→44.1
in-graph. So the old "48 kHz AP2 end-to-end" is no longer an open
optimization — it's the default when the receivers accept it (both the
tested Yamaha + Pioneer do), and there is then no resampling anywhere.

**Two one-off resamples remain, both off the hot path and in Rust — not
PipeWire (a deliberate deviation from the original plan).** (1) The AG
announce asset: clips decode once to 48 kHz, and `OverlayMixer::start`
resamples each clip **once** to the target output's published capture rate
(`resample::from_48k_stereo_to`, an identity copy for 48 kHz groups /
Sendspin) so the per-chunk `mix_into` is pure sample-addition. The planned
"pre-resample via PipeWire, cached" wasn't needed — a one-shot Rust resample
at overlay start is simpler and is not per-quantum. (2) An off-rate AirPlay
*ingest* sender (not 44.1 kHz/stereo) gets a one-off linear resample in
`airplay_source.rs` before the ring; the common case is a passthrough.
Full rules in [architecture.md](architecture.md#8-sample-rate-harmonization).

## mDNS: one shared LAN-restricted daemon, not one per browser/advertiser

A 100% CPU / dead-UI incident (2026-07-26) forced this. With
`host_network: true`, `mdns-sd` had joined the multicast group on **all**
host interfaces (11 `veth*` + bridges + the real LAN NIC), so every query
echoed out ~10 veths and back — a self-amplifying storm (~11.5k pkt/s),
multiplied by having **6** separate `ServiceDaemon`s (three of our browse
modules + the vendored sendspin/shairplay advertisers). The decided design:

- **Restrict mDNS to the primary LAN interface** —
  `disable_interface(IfKind::All)` + `enable_interface(IfKind::Addr(lan_ipv4))`,
  where `lan_ipv4` comes from a UDP-`connect`-to-`8.8.8.8:53` route probe
  (no packet sent) that picks the default-route iface and excludes the
  Docker bridges + IPv6.
- **Consolidate to two process-wide daemons** — one toggleable browse
  daemon (all discovery modules `browse()` on it) and one process-lifetime
  advertise daemon injected into both the Sendspin server and the shairplay
  receiver via `with_daemon`/`mdns_daemon()` dependency-injection
  constructors. This required bumping bridge-daemon's `mdns-sd` 0.13 → 0.20
  so all three (bridge-daemon, vendored sendspin, vendored shairplay) share
  one identical `ServiceDaemon` type.
- **`stop()` calls `daemon.shutdown()`**, not drop — mdns-sd's run loop
  only exits on an explicit `Command::Exit`, so drop-based stop leaked the
  daemon thread.

Verified live: 2 LAN-pinned idle daemons; 5353 traffic ~12,000 → normal;
load ~20 → ~1.2. Diagnosis recipe in
[live-instance-debugging.md](live-instance-debugging.md). (Note: HA Core is
*also* `network_mode: host`, so any residual `172.30.32.1` mDNS after this
is HA Core's zeroconf, not the add-on's.)

---

# Bridge daemon internals

*The following were moved here from the top-level
[`../../docs/decisions.md`](../../docs/decisions.md); they concern the
daemon / add-on container specifically.*

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
SPA `Props` `channelVolumes`, set natively — at the time via `volume.rs` and
the daemon's `/api/media_players/:id/volume` endpoint, both since removed (every
output is virtual now, and the per-device relay ducks inside the mix instead).
A real A/B/restore signal test confirmed this ducks only the
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
`bridge-daemon/src/pw_module.rs` and `pw/thread.rs`. (Note: RAOP outputs
are being replaced by the in-process AP2 sender — see the AP2 decisions
above and [airplay2-roadmap.md](airplay2-roadmap.md) — but `pw_module.rs`
stays, because the RTP source and source-ducking use it too.)

Sendspin outputs never had this constraint anyway — their sink node is a
plain `create_object` node (the native protocol method that *is* in
`pw_core_methods`), not a loaded module. The daemon now creates it natively
on its PipeWire thread (`pw/thread.rs`'s `CreateSinkNode`); see "Sendspin
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
`bridge-daemon/src/pw/thread.rs`, `routing.rs`, `volume.rs`, `pw/player.rs`.

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
a macvlan/ipvlan network) removes the isolation. (This is also what
exposes the daemon to the mDNS veth-amplification storm — see the mDNS
decision above.)

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

> **Superseded 2026-08-03 — both halves of this are gone.** The v1 endpoint
> (`POST /api/media_players/:node_id/announce`, node-volume ducking) was removed
> with the node-backed output API; announcements now go to `POST /api/announce`,
> which ducks and overlays per device inside the relay. The **Wyoming source was
> removed too** (`wyoming.rs`, the `wyoming` request field, and the integration's
> `extra.wyoming` branch): Home Assistant's own TTS entity already renders to a
> URL — with voice selection *and* the cache the last paragraph below calls out
> as missing — so keeping a second synthesis path meant pinning a Piper
> host/port inside automations to get a strictly worse result. `url` (plus the
> built-in `test`/`tone` clips) is the whole surface now.
>
> Kept because the reasoning below is still the record of *why* the shape was
> chosen, and because two findings in it outlive the code: Wyoming's
> `AudioFormat.width` is **bytes**, not bits, and buffering a whole clip before
> playback is fine at announcement lengths.

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
(`audio/decode.rs`), a pure-Rust decoder with zero system dependencies —
probes the format from content (mp3/wav/aac/ogg/flac all work
unmodified) and decodes to a `SampleBuffer<i16>`, which `audio/wav.rs` (then shared
with the since-removed Wyoming path) turns into a WAV. Removing `ffmpeg` from the
Dockerfile and adding the `symphonia` crate (`features = ["all"]` — the
cost of enabling every codec is a bit more compiled Rust in our own
~8MB binary, not a system dependency tree, so there's no reason to
hand-pick a narrower set) brought the image from **864MB to 456MB**.
Verified functionally identical, not just smaller: the same real-signal
e2e test (then `tests/test_addon_announce_ducking_e2e.sh`, since removed with
the node-volume announce path) produced the exact same
baseline/ducked/restored RMS measurements before and after the swap.

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
had: sink-node creation now goes through `pw/thread.rs`'s
`CreateSinkNode`/`DestroySinkNode` (the native equivalent of `pw-cli
create-node`, alongside the existing `Load`/`CreateLinks` commands there),
and continuous capture is native PipeWire (`pw/capture.rs`, mirroring
`pw/player.rs`'s stream setup but `Direction::Input` with
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
lose the port-rebind race. (AP2 receivers get the analogous treatment via
`ap2_liveness.rs` — a periodic TCP probe of the RTSP port.)

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

---

## Playout delay: one knob per backend, and every default adds nothing

Each output path has exactly one per-output "shift this speaker in time" dial,
and the shipped default of each adds no latency of its own:

| Backend | Dial | Default | Applied |
|---|---|---|---|
| AirPlay 2 | render delay (PT=87 anchor shift) | **0 ms** | live on the running stream |
| Sendspin | static delay, over the group lead | 0 (lead **0**) | in-band on (re)connect |
| pw-sink | the receiver's jitter buffer (`sess.latency.msec`) | 100 ms (the module's own) | agent reloads `module-rtp-session` |

The sendspin group lead is a *floor*, not the value used: `required_send_ahead_us`
raises every group's send-ahead to the largest member requirement (a reported
`min_buffer_ms` + that member's static delay, or the wire codec's decode floor),
so 0 means "what the members ask for, nothing more".

Reading a receiver that is out of step:

- **AirPlay 2 goes silent rather than early** when its delay is too low. Packets
  arriving past their play deadline are dropped by the receiver, not played late,
  and the negotiated `latency_min` is 22050 frames (≈500 ms at 44.1 kHz) — so a
  receiver that refuses at 0 needs `ap2_stream_config`'s minimum lowered as well
  as the delay raised.
- **A pw-sink target crackles** when its buffer is too small for the network hop
  and the remote host's scheduling.

pw-sink's dial is the *only* lever on that path's delay, which is why it exists at
all: `welcome` carries the value, and changing it re-sends `welcome` rather than
adding a message type, because the agent reloads its receiver on every one by
design (receiver-agent-plan §13.4). That works against already-deployed agents.

## No unbounded queues

A queue that can grow is latency that can grow, and in an audio daemon that
usually surfaces as delay rather than as memory. Every channel in the add-on is
bounded; what differs is the behaviour when one is full:

- **Audio/PCM lanes drop.** Latency *is* queue depth here, so the realtime-correct
  answer is to discard and log rather than block or grow: `sendspin_capture`
  (32 chunks), the pw-sink relay→sender feed (`PCM_FEED_DEPTH`), sendspin's
  per-connection audio lane (`MAX_QUEUED_AUDIO_FRAMES`, an admission counter so
  the caller can be told *which* member is not draining).
- **Control lanes are capped generously and complain.** Dropping an unduck or a
  release is worse than briefly queueing one, so these get far more room than any
  real burst (`AGENT_MSG_DEPTH`, `REMOTE_DUCK_DEPTH`, `AP2_CMD_DEPTH`) — but a
  ceiling all the same, because a peer that stops reading its socket must not be
  able to grow a queue until the daemon dies. Hitting it means that connection is
  wedged, which is logged and reported as failure.
- **Senders `try_send`; they never `await` a send.** A fan-out must not let one
  slow member stall the others, and callers holding the registry lock must not
  wait on a socket at all.

### Throughput must not depend on the wakeup rate

Bounding a queue only helps if the consumer can keep up, and for the pw-sink
sender two things decide that:

1. **The send thread runs at `SCHED_FIFO` 50**, matching the AP2 path's
   `rt-sender`. It wakes every `PACKET_MS` to put packets on the wire, so it
   belongs on the same ladder as the capture (45), the relay (40) and PipeWire's
   data loop (83).
2. **Each wakeup emits every packet whose deadline has passed**, not one. One
   packet per wakeup would cap throughput at `wakeups/s × PACKET_MS` of audio per
   second whatever the thread was fed; the shortfall would then surface as growing
   delay or as continuous dropping, depending only on the queue policy. Sending
   all due packets makes a late wakeup cost a burst the receiver's jitter buffer
   absorbs instead of costing audio.

`BacklogLimits::for_playout` derives both limits from the receiver's configured
buffer, because that buffer is the physical constraint on both: a burst has to fit
it, and a backlog worth holding has to be absorbable by it. A burst gets two
thirds of the buffer, and the drop ceiling is three bursts, so a gap too large for
one wakeup is still caught up over the next few. The ceiling is always above one
full burst — a lower one would discard audio the loop is about to deliver. That
derivation is also why `PWSINK_JITTER_MIN_MS` is three packet times: at two, the
minimum burst would be the entire buffer.

The queue bound and the rate-matching trim (±0.5 % on the packet interval, which
the receiver's adaptive resampler absorbs) cover the two remaining cases: clock
difference between PipeWire's graph and this thread's monotonic clock, and gaps
too large to catch up. Tests assert the ceiling-above-burst and
burst-inside-buffer invariants across the whole range the API accepts.

**Still unbounded, and knowingly**: the sendspin submodule's *control* and
inbound-message lanes (`submodules/sendspin`, upstream PR pending). Its audio lane
is capped as above, and the inbound one carries a note that hardening it is a DoS
question rather than a latency one.

## The Opus send-ahead floor is a setting

A sendspin group's send-ahead is `max(configured lead, per-member requirement)`,
and a member that reports no `min_buffer_ms` — which is every Voice PE and
satellite1 here — falls back to a per-codec floor. PCM and FLAC impose none. Opus
imposes `DEFAULT_OPUS_FLOOR_MS`, **40 ms**, two 20 ms blocks: measured to play
cleanly on this hardware over 2.4 GHz WiFi.

That floor sets the latency of every output aligned to the group, so it is
`opus_floor_ms` in sync_settings, editable beside the group lead. It is
configurable because the network half of the budget belongs to the site: a
congested band spends more of it on retransmissions.

**Its lower bound is arithmetic.** `opus_floor_lower_bound_ms` returns the Opus
block size — 20 ms at 48 kHz, the frame length sendspin-cpp's decoder is built
around. The encoder emits nothing before a whole block exists, so audio captured
at `C` leaves no earlier than `C + 20 ms`; a 20 ms send-ahead has it arriving
exactly when it is due to play, leaving no window for the network hop, the MCU's
decode or its scheduling. The API clamps there.

Which side the floor protects is worth stating, because it is easy to assume it is
our own encoder: **it is not**. Encoding happens here, ahead of sending, and
libopus's algorithmic lookahead is compensated separately (`codec_delay_us`). The
floor buys time for the *receiver* — the network hop, the ESP32's decode and its
scheduling.

A device that reports its own `min_buffer_ms` bypasses the floor in **both**
directions: a firmware announcing 80 ms puts its group at 80 ms. That is the
protocol's way to set this per device rather than guessing on the device's behalf.
