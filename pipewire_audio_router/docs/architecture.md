# Bridge daemon — architecture

The bridge daemon is the Rust binary at the heart of the PipeWire Audio
Router add-on. It runs PipeWire + WirePlumber inside the add-on container,
observes and mutates the live graph, and exposes everything as a REST +
WebSocket API. This document describes how it is put together *in detail*;
the system-wide, three-component picture (add-on + HA integration + BT
bridge firmware) lives one level up in
[`../../docs/system-architecture.md`](../../docs/system-architecture.md),
and *why* each piece is built this way lives in
[decisions.md](decisions.md). What each subsystem delivered and what is
still open is in the per-subsystem records —
[airplay2-output.md](airplay2-output.md),
[pipewire-sink-output.md](pipewire-sink-output.md),
[receiver-agent.md](receiver-agent.md),
[mic-alignment-plan.md](mic-alignment-plan.md),
[sendspin-open-items.md](sendspin-open-items.md) — and how to poke a running
instance is in [live-instance-debugging.md](live-instance-debugging.md).

---

## 1. Process shape

The daemon is a single compiled binary. `run.sh` starts only
infrastructure — a private D-Bus session bus, PipeWire, WirePlumber — and
then the daemon; there are **no supervised subprocesses** (the AirPlay
receiver, the Sendspin servers and the RTP source all run natively
in-process, so the *process* supervisor was deleted — see
[decisions.md](decisions.md#source--sendspin-processes-are-daemon-supervised-not-spawned-by-runsh)).
The `supervisor.rs` in the tree today is a different thing: it owns the one
process-wide mDNS `ServiceDaemon` and registers every browser on it (§5.4).

Threading model, top to bottom:

- **The PipeWire thread.** PipeWire's core types aren't `Send`, so all
  graph work (registry listening *and* mutation) happens on one dedicated
  thread. Other threads talk to it over a `pipewire::channel` command
  channel (`pw/thread.rs`): `Load`/`Unload` a module, `CreateSinkNode`/
  `DestroySinkNode`, `CreateLinks`/destroy a link. Registry events flow
  back out to the rest of the daemon.
- **The tokio async runtime.** The axum REST/WS server, mDNS discovery,
  liveness probes, HTTP fetches and all control-plane logic. Capped to
  `worker_threads(2)` / `max_blocking_threads(4)` in `main.rs` — safe
  because **no time-critical audio work runs on tokio** (see the RT
  section below). *Caveat:* vendored crates spawn their own runtimes, so
  the observed `tokio-rt-worker` count is higher than 2.
- **SCHED_FIFO real-time threads.** Every stage that touches steady audio
  runs at a fixed real-time priority so that general-purpose tokio work,
  mDNS, or host load can never preempt it. See §7.

### 1.1 Where the code lives

`bridge-daemon/src/` is 12 directories plus `main.rs` (boot + wiring),
`state.rs` (`AppState` and the `Shared*` aliases) and `supervisor.rs`. The
tree deliberately mirrors this document, so the sections below are also a
map of the source:

| directory | this document | contents |
|---|---|---|
| `pw/` | §2 | PipeWire plumbing with no product concepts: the graph thread and its command channel, node capture, clip playback, the Profiler FFI, peak taps |
| `audio/` | §8 | resample / wav / decode helpers, zero crate deps |
| `util/` | — | `LockRecover`, the node-name prefixes, the host RT-capability assessment |
| `store/` | — | file-backed JSON with no live handles. Membership rule: a module belongs here only if its sole dependency is `util/locks` |
| `sources/` | §3 | the source collection *and* its reconciler, plus the AirPlay receiver, its client registry, RTP, now-playing, BT-bridge discovery |
| `outputs/` | §4, §5 | `sendspin/`, `ap2/`, `pwsink/`, the shared `overlay_mixer.rs` and the `listing.rs` model |
| `routing/` | §4 | the matrix and its handlers, `routing/sync_group/mod.rs`, `sync_settings.rs` |
| `announce/` | §5.5, §9 | announcement delivery and the pure scheduling decisions (`arbiter.rs`) |
| `align/` | §11 | the manual and mic-assisted alignment cluster |
| `api/` | §9 | the route table plus one module per resource. A **leaf**: nothing in the crate depends on it |
| `spike/` | — | dev-only experiments behind `/api/spike/*` |

Each `mod.rs` carries a `//!` header stating that directory's boundary and
each child's role — the reason the tree exists at all. Two placements are
deliberate and look wrong at first glance: `sources/mod.rs` *is* the source
store (it reconciles live receivers, so it is not a pure `store/` module),
and `align/relay_delay.rs` lives with alignment even though it is *called*
from the three output relays. The refactor that produced this, and the
lessons from doing it, are in
[module-layout-plan.md](module-layout-plan.md).

## 2. The PipeWire graph

The graph is a plain many-to-many routing/mixing graph: any source node
can be linked to any sink node, including several sinks at once (the
"one AirPlay source → RAOP receiver **and** Voice PE speaker
simultaneously" scenario that motivated the project). Nothing
project-specific happens at the link layer — PipeWire does what PipeWire
always does; the daemon only decides which nodes exist and which links to
create.

Link mutation is native `pipewire-rs` (`Core::create_object` with the
`link-factory`; `Registry::destroy_global` by id), not a `pw-link`
subprocess — 0.07 ms vs ~16 ms per round trip. Idempotency and targeted
unlink are decided against the observed registry state, not by parsing
subprocess stderr. See
[decisions.md](decisions.md#link-mutation-is-native-pipewire-rs-not-a-pw-link-subprocess).

## 3. Sources (audio into the graph)

| Source | Node | How it gets into the graph |
|---|---|---|
| **AirPlay receive** | `airplay-in` | Native, in-process RAOP receiver — vendored+patched pure-Rust `shairplay` crate (`sources/airplay.rs`). Decoded f32 PCM is pushed through a jitter buffer into a PipeWire producer node. |
| **RTP** | `bt-bridge-rtp` (or similar) | PipeWire's `rtp-source` module, fed by the ESP32 / Pi Bluetooth bridge over RTP/UDP. Can bind a multicast group so several PipeWire hosts share one bridge stream. |

Sources are a **keyed collection**, not one of each: `sources/mod.rs` holds a
persisted list of sources of those two kinds (`sources.json`, presence in the
list = enabled) and reconciles the live receivers when it changes, so the user
can add and remove several of either type at runtime, each independently
routable. Each AirPlay source carries its own registry of remembered senders,
with bans and priorities (`sources/airplay_clients.rs`), and each source has a
now-playing record fed by whichever producer can say (the receiver's DMAP
callbacks locally, a Pi reporter over the API remotely). A legacy
single-source file is migrated to this shape on load, and back-compat shims
still present the old singular API to the rest of the crate.

The AirPlay path is the interesting one for the AP2 flow below:

1. **AirPlay sender** (phone / Mac / another PipeWire host) streams
   RTP/RTSP over WiFi.
2. **`shairplay`** receives UDP/TCP, decrypts, and decodes ALAC (pure
   Rust) or AAC (`symphonia`) on a tokio worker — this codec work is
   irreducible and stays in Rust.
3. A **lock-free SPSC jitter-buffer ring** (`rtrb`, interleaved f32) rides
   out clock drift/jitter between the sender and the graph: prebuffer
   `latency_msec` (default 150 ms), re-arm to a small guard after an
   underrun, drop-on-overflow. (The `Mutex` around the producer only
   serializes session teardown/replacement — the RT consumer is lock-free.
   This replaced the old `Mutex<VecDeque>`.)
4. The **`airplay-in` node's `RT_PROCESS` callback drains the ring** on the
   graph **data-loop (SCHED_FIFO 83)** and emits **F32LE 44.1 kHz stereo**.
   The `airplay-producer` OS thread that hosts the stream's mainloop is
   itself elevated to **SCHED_FIFO 45** to protect the control path (see §6).

The receiver advertises **unencrypted ALAC** (`et=0`, `cn=1`) — the one
combination a PipeWire sender drives correctly on a trusted LAN. See
[decisions.md](decisions.md#native-airplay-receive-source-vendored-shairplay-not-shairport-sync).

## 4. The anchor + per-device sender model

This is the core routing pattern, and it is **shared by both output
backends** (Sendspin and AirPlay-2). It was validated on hardware by the
Sendspin overhaul and AP2 was aligned onto it.

- **One steady PCM source per group** = the group's
  `support.null-audio-sink` **anchor** (`routing/sync_group/mod.rs`, keyed
  `sync-grp-<hash>` by source-set). It is a **QUANT-1024 steady clock
  driver** and does the graph-native mix + resample. A *standalone
  per-device* null-sink is **not** a steady driver — it produced ~1 glitch
  / 60 s (QUANT-0 dropouts) — so every group member is fed from the *one*
  anchor monitor. Do **not** give each device its own sink.
- **One capture from `anchor.monitor` per backend group** (`pw/capture.rs`,
  a graph-clock-paced `RT_PROCESS` stream) produces **S16LE** at the group's
  wire rate — Sendspin at **48 kHz** (`spawn`), AP2 at its **negotiated rate**
  (`spawn_with_rate`: 48 kHz when every member accepts it, else 44.1 kHz with
  PipeWire resampling 48→44.1 in-graph — see §8). Capture hands out a pooled
  buffer (`PooledBuf`) over a **bounded** channel with non-blocking
  drop-on-full, so a stalled consumer can't grow memory/latency without bound.
- **One `SharedTimeline`** (`vendor/sendspin/src/server/timeline.rs`,
  `CLOCK_MONOTONIC_RAW`) stamps the sample timestamp **once per chunk**, so
  every per-device sender in the group emits the identical `ts` and stays
  sample-coincident.
- **Split only the *sender* per device, not the sink.** This gives
  per-device addressability (volume, delay, duck/overlay) while keeping
  devices sample-coincident and fed from one steady driver.

**Two-tier grouping:** an **MG** (Music Group) is the routable unit — a
routing target is polymorphic (`output | MG | …`); an **AG**
(Announcement Group) is additive, priority-preempting, with per-device
duck/overlay (`announce/arbiter.rs` / `announce/mod.rs`).

### The `OutputBackend` seam (target end-state)

Historically "output" was scattered node-name-prefix `if`s across
`routing/mod.rs`, `api/`, `routing/sync_group/mod.rs`, `media_player.py`. The intended
convergence is a single trait so each backend stops touching five files
and "drop RAOP" becomes a clean delete:

```rust
trait OutputBackend {
    fn prefix(&self) -> &'static str;          // "ap2-dev-", "sendspin-dev-"
    fn kind(&self) -> &'static str;            // "airplay2" | "sendspin"
    fn devices(&self) -> Vec<OutputDevice>;
    fn make_sender(&self, device: &NodeName, timeline: Arc<SharedTimeline>) -> Box<dyn PerDeviceSender>;
    async fn set_volume(&self, device: &NodeName, vol: f32);
    async fn set_delay(&self, device: &NodeName, ms: u32);
    async fn overlay(&self, device: &NodeName, action: OverlayAction); // AG duck/announce
}
```

`routing/sync_group/mod.rs::reconcile` would iterate backends and, per group, create
one `PerDeviceSender` per member off the group's `Arc<SharedTimeline>` +
anchor capture. With RAOP gone there are exactly two impls —
`SendspinBackend`, `Ap2Backend` — and no follower-sink exceptions.

## 5. Outputs (audio out of the graph)

### 5.0 Adoption — discovery only *offers* a device

Every output kind below is mDNS-discovered, which on a real home network
also finds the neighbours' HomePods and any laptop running
`module-rtp-session`. So discovery is an **offer**, not an enrolment:
`store/outputs.rs` records one verdict per stable node name — *adopted*,
*discovered* (undecided) or *ignored* — and only an **adopted** output is
real. The gate is applied in exactly three places, all of which read
routing intent through the adopted set:

- `routing/mod.rs::build_matrix` — an unadopted device isn't in the matrix, so
  it can't be routed **and** the HA integration (which builds its
  `media_player` entities from that listing) never sees it;
- `routing/sync_group/mod.rs::reconcile` — intent whose output isn't adopted is
  dormant, so no group forms and no stream/session is ever opened to it;
- `api/outputs.rs` — `/api/outputs` returns the adopted ones,
  `/api/outputs/discovered` the rest (the Outputs page's second list).

Intent is *filtered*, never deleted, so adopting a device restores the
routing it had. The one deliberate exception to the gate is the on-demand
announce path (`ensure_announce_transport`): a discovered device can still
be sent a test tone, because that's how the user works out which speaker
`ap2-dev-living-2` actually is before adding it.

### 5.1 Sendspin (ESPHome speakers, e.g. HA Voice PE)

Devices are **auto-discovered** over mDNS (`outputs/sendspin/discovery.rs`) and
surfaced as virtual routing outputs `sendspin-dev-<slug>`. Devices routed
from the same source-set are formed into one synchronized group. Per
group:

- **`sendspin-relay`** [SCHED_FIFO 40]: consumes the capture channel,
  `timeline.stamp()` + **overlay-mix / duck** (`outputs/overlay_mixer.rs`) +
  fan-out. One PCM feed is differentiated per device *inside the Rust
  process*. Uses a reused `mix_buf` (no per-chunk `Vec`).
- **Per-device WebSocket writers** [tokio tasks] push
  `stream_start`/audio to each device over the Sendspin WebSocket via an
  embedded native server (`outputs/sendspin/server.rs`, vendored+patched
  `sendspin` crate).
- **ESPHome / Voice PE receiver**: the **send-ahead lead** is the *only*
  jitter buffer end-to-end — it converts the presentation timestamp to its
  local clock and plays, so every hiccup above that budget is audible. The
  lead is what the group's speakers ask for (`required_send_ahead_us`: each
  member's reported `min_buffer_ms` + its static delay, or the codec's decode
  floor), raised by the configured `group_lead_ms` if you want more headroom
  than that. That configured value defaults to **0** — see
  [decisions.md](decisions.md#playout-delay-one-knob-per-backend-and-every-default-adds-nothing).

Per-device **volume** is sent in-band over the protocol
(`outputs/sendspin/volume.rs`); **liveness** is connection-driven (mDNS is
discovery-only, `outputs/sendspin/liveness.rs`). See
[decisions.md](decisions.md#sendspin-auto-discovery-grouping-per-device-volume-and-connection-driven-liveness).

### 5.2 AirPlay-2 (AV receivers, e.g. Yamaha WX-021, Pioneer VSX-934)

The AP2 backend replaces the old RAOP/AirPlay-1 output. It is an
**in-process Rust AP2 sender** — vendored `lmcgartland/airplay2-rs`
(pairing / SETUP / RTP / streaming) driven by a **host-global PTP
grandmaster** from OwnTone's MIT `libairptp` via FFI. Devices are
discovered over `_airplay._tcp` (`outputs/ap2/discovery.rs`) and surfaced as
`ap2-dev-<slug>`. Per receiver, off the same anchor monitor:

- **`ap2-relay`** [SCHED_FIFO 40]: capture-forward loop. Each sender's feed
  is tagged with its `node_name`, so per chunk the relay calls
  `OverlayMixer::mix_into(node_name, …)` — a device with an active AG
  announcement gets duck(music)+overlay, its groupmates get plain music
  (symmetric with the Sendspin relay; the overlay clip is pre-matched to the
  group's rate — see §8). Reuses one `mix_buf` across chunks and devices.
- **`ap2-producer`** (`run_streamer`, vendored `streamer.rs`) [SCHED_FIFO
  48, own current-thread runtime]: **ALAC encode** (realtime type 96, cookie
  rate-matched to the group) + **ChaCha20 encrypt**.
- **`rt-sender`** [SCHED_FIFO 50]: emits **RTP** + **PT=87 anchor**
  packets, paced by `clock_nanosleep(TIMER_ABSTIME)`.
- **AP2 receiver**: renders against the shared PTP clock. A **render
  delay** (default **0**, tunable 0–2000 ms up to
  `AP2_RENDER_DELAY_MAX_MS`) is the receiver-side buffer. It is
  retuned **live** — `ap2_control` sends `SetRenderDelay` to the running
  streamer, which shifts the next PT=87 anchor — so a UI change takes
  effect mid-stream with **no reconnect**; it is deliberately *not* part of
  the group's restart identity. See
  [decisions.md](decisions.md#render-delay-is-retuned-live-not-by-reconnect).

**Restart identity + reliability.** The AP2 senders for a group are torn
down and rebuilt only when the **receiver set** or the **wire rate** (§8)
changes — never for a render-delay edit. `outputs/ap2/liveness.rs` TCP-probes each
receiver's RTSP port (3 failed ticks ⇒ demote; 5 min ⇒ remove + release its
PTP peer); the reconcile loop coalesces change bursts behind a 400 ms quiet
window; and `connect_one` retries once after ~1.5 s on a pairing failure
(receiver hadn't released the prior session).

**Graceful shutdown.** On SIGTERM the shutdown path *awaits* the RTSP TEARDOWNs
(`GroupReconciler::shutdown_ap2` → `Ap2ServerHandle::shutdown`, ~3 s per group,
groups concurrently) instead of only dropping the handles — a dropped handle just
signals its task, and on process exit nothing polls that task again. This is the
other half of the retry above: a receiver holds an unclosed session until *it*
times out, so an unclean exit is precisely what makes the next start's first
connect fail, and leaves the receiver's one AirPlay input busy for phones in
between. `run.sh` (PID 1) waits up to ~8 s for the daemon before stopping
PipeWire, so the bound fits inside the supervisor's ~10 s stop grace.

**Alignment.** AP2 members are muted/soloed via `ap2_control`
(device-authoritative mute) and each one's offset is tuned — by ear or by
microphone measurement — with its **live render delay**; there is no
node-volume path, since AP2 outputs are virtual. See §11.

### 5.3 pw-sink (another PipeWire Linux host, via its agent)

The cheap backend, because the receiver speaks our transport natively: no
codec and no Rust hot path of its own. A **paired `pwrouter-agent`** running
in a user's session on the remote host is what makes a target exist at all —
`Agents::connected_targets()` is the source of truth, and the mDNS browse of
`_pipewire-audio._udp` (`outputs/pwsink/discovery.rs`) survives as a
*diagnostic only*. Outputs are `pwsink-dev-<host>_<user>`; the identity is
machine + user, so two logged-in users on one host are two targets.

- **`pwsink-relay`** [SCHED_FIFO 40]: one capture off the group anchor's
  monitor (48 kHz/S16/stereo), fanned to one **`AppleMidiSender` per target**,
  each fed a per-device-mixed copy via `OverlayMixer::mix_into` — so per-device
  announce/duck works exactly as it does for AP2. A single shared session could
  not duck one member alone.
- **Transport**: AppleMIDI/RTP, L16 (`S16BE`) 48 kHz stereo in 5 ms packets,
  each sender advertising its own session `pwrouter-<slug>`. The receiver is a
  stock `libpipewire-module-rtp-session` that the *agent* loads into its own
  `pw_context` and dials *us* — so nothing listens on the receiver, and module
  lifetime is agent-process lifetime.
- **Playout delay** is the receiver's jitter buffer (`pwsink_jitter`, default
  100 ms, clamped 15–2000 ms in whole packet times), decided daemon-side and
  pushed by re-sending `welcome`; the agent reloads its receiver on every one,
  so retuning needs no new message type.
- **Volume/mute/duck** are out-of-band, through the agent: it walks
  `receive stream → link → target sink` and drives the **device's**
  `SPA_PARAM_Route` (node `Props` is the wrong lever for a real device sink and
  WirePlumber reverts it), reporting local changes back so HA follows the host
  rather than overwriting it. Its duck attenuates the *foreign* streams on that
  sink — the host's own music, which our mix cannot reach — and never our
  `pwsink-in` stream.
- **Two independent liveness questions**, §5.6: `target_liveness` watches the
  remote host's advert (no active probe is possible — the receiver dials us), and
  `sender_liveness` watches whether a session is actually established.

Known limitation: stock `module-rtp-session` in discover mode attaches to
*every* advertised session of the media type (the session name is never
compared), so 2+ pw-sink targets on one LAN cross-connect. Details, and the
`rtp.session` hook that would fix it agent-side, are in
[pipewire-sink-output.md](pipewire-sink-output.md) §4 and
[receiver-agent.md](receiver-agent.md) §7.1.

### 5.4 One mDNS browse per service type — a shared daemon can't be shared for browsing

The daemon runs **one** `mdns_sd::ServiceDaemon` for the whole process (that
consolidation is what fixed the multicast CPU storm), and exactly **one browse per
service type**: `sendspin_discovery` owns `_sendspin._tcp`, `ap2_discovery` owns
`_airplay._tcp`, `pw_target_discovery` owns `_pipewire-audio._udp`,
`bt_bridge_discovery` owns `_pwrouter-btbridge._tcp`. That is not
tidiness — it is required:

> mdns-sd keeps `service_queriers: HashMap<String, Sender<ServiceEvent>>`, one listener
> per type. Its own comment: *"If there is already a `listener`, it will be updated,
> i.e. overwritten."*

So a second `browse()` for a type **silently unsubscribes the first**. sendspin-rs's
`ClientManager::start` browses internally, and we run one manager per sync group plus
one per idle device — so each new server used to steal the subscription from every
earlier one *and* from our own registry. The consequences were invisible rather than
loud: a group server never learned its devices existed, so it **never dialed them and
logged nothing**, while the registry stopped seeing presence and capability updates.
On hardware this looked like "two speakers get no audio, and which two changes".

The servers therefore do **not** browse: `ClientManager::start_without_discovery` +
`supervise(fullname, url)`, fed the URLs `sendspin_discovery` resolved (it stores each
device's `ws://ip:port<path>`), re-asserted on every reconcile so an address change
redirects the supervisor without restarting the server. Upstream note + suggested fix:
`pull_request_docs/sendspin-89-shared-mdns-daemon-steals-browse-subscription.md`.

**Corollary for the log:** the vendored crate reports its whole dial loop through the
`log` crate, so `sendspin=info` belongs in the `EnvFilter` (main.rs). Without that
target, every dial, failure, retry and goodbye is dropped — which is what made the
above take a day to find instead of a minute.

**Three of the four browses find outputs; the fourth finds an input.**
`bt_bridge_discovery` is the odd one out and deliberately builds **no audio
path**: a Bluetooth→RTP bridge (`firmware/pi-bridge/setup_pi_bridge.py`)
advertises `_pwrouter-btbridge._tcp` with its stream parameters in TXT, and the
daemon uses that only to (a) offer the bridge on the Sources tab with its real
port/rate prefilled and (b) link to its diagnostics page. It exists because the
daemon otherwise cannot tell *which host* feeds an RTP source —
`module-rtp-source` exposes only the address it listens on, and sniffing the port
would take datagrams from the module. Note the direction trap this avoids:
reusing `_pipewire-audio._udp` here would list a bridge as an **output** (that is
what the type means), and a stock `module-rtp-session` in discover mode attaches
to every session of the media type — including our own `pwrouter-*` output
sessions, looping output audio back in as an input. See
[decisions](../../docs/decisions.md#raspberry-pi-bluetooth--rtp-bridge).

### 5.5 Announcing to an output with nothing routed into it

An announcement is only audible while **some per-device relay is consuming
that output's overlay slot** — `mix_into` is what turns a clip into audio, and
it runs *inside* a sender's capture-forward loop. So "can this output be
announced to?" reduces to "does it have a running sender?", and the answer
differs per backend:

- **Sendspin: connection always, audio on demand.** An ungrouped device keeps an
  **idle sender** (`routing/sync_group/mod.rs`, `IdleSender`) on its own silent
  `null-audio-sink`, so an announcement (or a volume command) never pays a cold
  dial. But the connection carries **no audio** while idle
  (`StreamPolicy::WhenAnnounced`): the device isn't in a group, so it gets no
  `stream/start` and no chunks, and an arm task (100 ms tick) puts it in one
  exactly while a clip is playing on or queued for it, then ends the stream after
  a 1.5 s drain so the send-ahead tail still renders.

  This matters for two reasons the spec makes explicit. Several servers may be
  connected to one device and **the device** decides which to keep, weighing each
  server's `connection_reason`; an idle sender that dialed `Playback` and pushed
  silence forever looked exactly like the active server, so it could stop the
  device switching to one the user asked to play. An idle sender therefore dials
  `Discovery` — the spec's own "discovery/announcement" case. And the wire format
  is PCM 48 kHz/16-bit/stereo, so the silence stream cost **~1.5 Mbit/s per idle
  device** and kept it out of WiFi power-save.
- **The dialed backends (AirPlay-2, pw-sink): on demand.** Neither can be held
  open like an idle sender — an AP2 receiver accepts **one session at a time**
  (a permanent one would block the household's phones and park AVRs on their
  AirPlay input), and a pw-sink session is an **advertised** mDNS service that
  stock `module-rtp-session` in discover mode attaches to on sight (a permanent
  advert per idle target would keep every receiver on the LAN attached to
  sessions it has no reason to be in). So an unrouted endpoint of either kind
  gets an **`AnnounceSession`** only while it's being announced to:
  `GroupReconciler::ensure_announce_transport` (called by `POST /api/announce`
  **before** the clip starts) creates a private silent sink and starts a
  single-member `ap2_server` (also publishing its wire rate to the mixer so the
  clip is rate-matched, §8) or `pwsink_server` (binding + advertising one
  `pwrouter-<slug>` session). The clip is *not* consumed while the endpoint
  connects — `mix_into` only runs for a registered sender/feed — so it still
  plays whole, a few seconds late: AP2 pays pair + SETUP + its render delay,
  pw-sink pays the target noticing the advert and initiating the AppleMIDI
  handshake. The session then lingers on a **lease** (30 s, never shorter than
  an AP2 render delay + 2 s so buffered audio renders), refreshed while a clip
  is playing *or queued* for that output, and a 1 s ticker
  (`poll_announce_sessions`) hands it back when the lease expires. `reconcile`
  drops the on-demand session as soon as the endpoint *is* routed — before the
  group senders dial/advertise for it — so the two never collide.

  Same caveat as routed pw-sink streaming: with 2+ pw-sink targets on one LAN,
  discover-mode receivers attach to *every* advertised session, so an
  announcement aimed at one can be heard by the others (the deferred
  session-scoping decision, `docs/pipewire-sink-output.md` §4).

**"Live" means connected, not dialed.** For both dialed backends, group
membership only says what the group *dialed*: `routing::sync_group::dialed_session_established`
therefore reads `Ap2Control::connected` (its sender registered a command channel)
and `PwSinkLiveness` `established` (a receiver completed the handshake), and
`has_live_sender` builds on it. A routed endpoint that never came up is reported as
such instead of counting as playable.

### 5.6 Reachable, connected, playing — three states, one rule

Every surface that shows an output's state has to answer two questions, and
conflating them is how the UI came to contradict itself (the routing graph animated
a wire to a pw-sink target that had never accepted a session, while announcements to
that same target were correctly refused):

| Question | Field | Owner |
| --- | --- | --- |
| Is it **reachable**? | `present` | the per-backend liveness tasks: `sendspin_liveness` (connection + TCP probe), `ap2_liveness` (RTSP-port probe), `pw_target_liveness` (advert withdrawal, debounced) — mDNS discovery only ever *adds* |
| Is a **session up**? | `streaming` (matrix) / `pwsink_streaming` (`/api/outputs`) | `routing::sync_group::dialed_session_established` — `Ap2Control::connected` + `PwSinkLiveness.established` |

`streaming` is `None`/absent where the question doesn't apply: sources, and sendspin
devices (which always have a sender while adopted). It matters most for pw-sink,
where the handshake is *receiver*-initiated: a target can be reachable indefinitely
without ever attaching, so the UI shows a third state ("not connected", amber)
and the graph draws that wire **without** the flow animation — an animated wire is a
claim that audio is being carried. A status flip nudges the routing-matrix
WebSocket (`PwSinkLiveness::set_change_notifier`, `Ap2Control` register/unregister),
so the graph and the Outputs page both update live rather than on the next unrelated
registry event.

**Nothing is silently swallowed.** Two mechanisms make that true: the API
answers with what will actually carry each target (targets nothing can carry are
dropped from the announcement and named in the response, so the UI toast is
honest), and the mixer runs a **stall watchdog** — `reap_stalled` drops an
overlay whose cursor hasn't moved for its grace (5 s normally, 40 s while an
on-demand session connects) and `announce/mod.rs` completes it in the scheduler.
Without that, a clip nothing consumes would hold the output occupied forever and
every later announcement to it would queue behind a clip that can never finish.

### 5.7 Voice ducking — the other producer of duck

Announcement ducking is a *side effect* of an overlay being mixed. There is a
second, independent producer: a **duck hold** (`outputs/overlay_mixer.rs`), an open-ended
lease that attenuates one output's music with no clip at all. It exists for a
voice assistant that speaks through its **own** speaker (an HA Voice PE): the
router has nothing to play, only music to get out of the way.

Three properties, each deliberate:

- **Not an announcement.** The scheduler is built for atomic clips and it
  *occupies* its targets; a hold that occupied an output would make a doorbell
  queue behind someone's voice turn. Holds live in their own map, so no overlay
  bookkeeping path (`reap_stalled`, the finished-drain, `stop`) can see them, and
  the two compose in the mix by taking the stronger (lower) gain — the clip
  itself is never attenuated.
- **Leased, renewed by the holder.** The announce tick expires overdue leases, so
  a holder that dies mid-turn (Home Assistant restarting, network dropping) costs
  one TTL of quiet music rather than silence until someone notices. This is the
  property that makes the feature safe to enable at all.
- **Rooms stay in Home Assistant.** The daemon is addressed by output name only.
  The integration (`voice_duck.py`) resolves satellite → area → outputs from HA's
  own registries, optionally widening to a whole music group, and it reuses the
  *same* device correlation as output adoption — so ducking and adoption can never
  disagree about which device an output is. Nothing about areas is duplicated
  daemon-side.

A hold on an output nothing is streaming is inaudible and harmless: holds are
keyed by output name and outlive any relay, so music that starts mid-hold comes
up already ducked.

**Agent-backed hosts need the aggregate, not the delta.** A pw-sink target is a
whole host that may be playing music of its own, outside our stream, which the
overlay mix cannot reach — so its duck is mirrored to that host's agent
(`outputs::pwsink::agent::duck_output`), which attenuates the foreign streams on its sink.
The agent takes an **absolute** depth and does no ref-counting, so every producer
of ducking re-asserts `OverlayMixer::effective_duck` (the same value the mix
applies) through `announce::sync_agent_duck` rather than clearing the host
outright. Without that, an announcement finishing on such a host would un-duck a
room whose voice assistant is still talking.

## 6. The two clocks, cleanly separated

AP2 needs *two* independent clocks, and conflating them is the classic
mistake:

| Concern | Owner | Notes |
|---|---|---|
| **RTP sample timestamp** (`ts`; who-plays-which-sample) | `SharedTimeline` (target) / per-sender RTP counter (current impl) | Stamped once per chunk so every per-device sender in a group emits identical `ts` = sample-coincident. |
| **Network / PTP clock** (shared wall clock devices slave to) | **`libairptp`, host-global** (`outputs/ap2/ptp.rs`) | Binds 319/320 **once**; every AP2 receiver across all groups is a peer of the one grandmaster. PT=87 anchors are stamped from **`CLOCK_MONOTONIC`** (via `airptp_ffi::monotonic_ns()`) — *not* `CLOCK_MONOTONIC_RAW`, not epoch. |

**PTP is host-global; RTP/grouping is per-group.** One `Ap2PtpService`
for the whole daemon; which audio a receiver plays is decided by which
group's sender streams to it (mirrors OwnTone: one libairptp + per-session
streams). gPTP lock is **required** for rendering — PT=87 anchors alone
are insufficient — and matters most for multi-room drift.

> **Current-vs-target divergence.** Phase 3 as shipped uses independent
> per-device `Connection`s that share the grandmaster `clock_id` + the
> same PCM (matching the proven spike `test_group`), **not** the
> stamp-once `SharedTimeline` / `push_encoded` path. That is sufficient
> for sync (PTP + coincident PCM); aligning AP2 onto `SharedTimeline` +
> the `OutputBackend` seam is the intended end state.

## 7. Real-time thread ladder

The whole audio data path is SCHED_FIFO end-to-end so nothing normal-
priority (tokio, mDNS, HTTP, host load) can preempt steady audio. On the
4-core Pi, with the relay and sender both at SCHED_FIFO, CPU pinning is
not required (revisit only if a stall traces to CPU contention).

| Thread | Priority | Role |
|---|---|---|
| PipeWire `data-loop.N` | **FIFO 83** | The graph RT thread; mix/resample/capture `RT_PROCESS`. |
| `libairptp` | **FIFO 55** | gPTP event loop (Sync/Follow_Up 8×/s). Timer accuracy is what makes receivers lock; a starved SCHED_OTHER loop dilated the 125 ms timer ~48×. |
| `rt-sender` | **FIFO 50** | AP2 RTP egress, `clock_nanosleep` paced. |
| `ap2-producer` | **FIFO 48** | AP2 ALAC encode + encrypt (`run_streamer`, own current-thread runtime). |
| capture / producer feeder mainloops | **FIFO 45** | `pw/capture.rs` / `sources/airplay.rs` PipeWire mainloops — low-impact for steady audio (that's on `data-loop`), protects the *control* path (reconnect/flush/stop) under load. |
| `ap2-relay`, `sendspin-relay` | **FIFO 40** | Capture→sender fan-out relays. |

`CAP_SYS_NICE` (`config.yaml` `privileged: [SYS_NICE]`) bypasses the
container's `RLIMIT_RTPRIO=0`, so these elevations succeed in-container;
they are best-effort/non-fatal without it (dev box).

## 8. Sample-rate harmonization

**All resampling happens at the PipeWire level — never in a Rust hot
path.** `pw::capture::spawn_with_rate` sets the capture stream's rate
and PipeWire does the SRC in-graph on its RT thread.

- **Internal bus (anchor + `SharedTimeline` + `OverlayMixer` + announce
  assets) = 48 kHz** (Sendspin's native rate and the overlay format).
- **Sendspin send-ahead has a device-driven floor.** A player reports
  `min_buffer_ms` (+ `required_lead_time_ms`) in `client/state`, and the spec makes
  the first one binding: "servers must schedule timestamps so each player's queued
  audio duration stays at or above its `min_buffer_ms`", with a group using "the
  maximum per-player send-ahead across grouped players". So a group's send-ahead is
  `max(configured group lead, max over members of (min_buffer_ms + that member's
  static delay))` (`outputs::sendspin::server::required_send_ahead_us`), and it is part of the
  server's **restart identity** — the timeline fixes its send-ahead at construction.
  `required_lead_time_ms` is deliberately not folded in: the spec says to extend
  toward it only for buffered sources, and this is a live stream. A player may raise
  its requirement for "codec init, decode warmup", so **the floor moves with the wire
  codec** — the UI reads it back after a codec change, and `/api/sync-settings`
  exposes `group_lead_floor_ms` / `group_lead_effective_ms` plus which device set it,
  so a value typed below the floor is shown as having no effect rather than silently
  ignored.
- **Sendspin wire codec is negotiated per group** (`outputs/sendspin/codec.rs`): PCM,
  **Opus** (vendored libopus via `opusic-sys`) or **FLAC** (pure-Rust `flacenc`) —
  the spec requires a server to support all three. The rate/depth stay at the
  capture format, so only the *framing* changes: encoding runs inline on the
  `sendspin-relay` SCHED_FIFO thread with every buffer pre-allocated and reused,
  one encoder **per member** (Opus/FLAC are predictive, and an announced-to device's
  audio diverges from its groupmates'). A `Reblocker` re-cuts capture quanta into
  legal blocks — Opus 20 ms, FLAC 1024 frames, both inside the spec's 15–150 ms
  chunk bounds — and the timeline is stamped once per *emitted block*, so members
  stay sample-coincident. The choice is per output (`SendspinCodec`, Auto = Opus
  when every member decodes it, else PCM) narrowed to what all members support;
  it's part of the server's restart identity, and FLAC's `codec_header` (base64
  `fLaC` + STREAMINFO) rides on `stream/start`. **Opus's encoder delay is
  compensated**: libopus's decoded output lags its input by `OPUS_GET_LOOKAHEAD`
  (~6.5 ms at 48 kHz) and sendspin has no pre-skip field to declare that, so the
  sender shifts its timestamps back by it (`codec_delay_us`) — without that, every
  Opus chunk is heard ~6.5 ms after the instant it asked for, a permanent error
  outside the spec's ±1 ms accuracy floor that the player's correction loop then
  fights continuously. PCM and FLAC have no such delay.
- **Wire rates are per-group and negotiated (AP2).** PipeWire bridges the
  one 48 kHz anchor to each per-device capture: Sendspin @ 48 kHz (no
  conversion); AP2 @ its group rate — **48 kHz** when every member accepts it
  (then it's 48 kHz end-to-end, no resampling anywhere), else **44.1 kHz**
  with PipeWire doing 48→44.1 in-graph. No steady Rust resampling; don't
  unify the wire rates by hand.
- **AP2 rate negotiation (`routing/sync_settings.rs`).** Each output has an
  `Ap2RateMode`: **`Auto`** (default) optimistically streams 48 kHz and, on a
  SETUP rejection (`ConnectFail { at_setup }`), learns a persisted per-device
  **44.1 kHz cap** so it doesn't re-probe; **`Fixed44100`** forces 44.1 for
  receivers known to dislike 48 kHz. Because one capture serves a whole group,
  `ap2_group_rate` = 48 kHz **iff every** member's effective rate is 48 kHz.
  The rate is part of the group's **restart identity**, so a change (UI mode
  switch or a learned downgrade) tears the senders down and reconnects once.
- **Announce asset — handled by the `OverlayMixer`.** Clips decode once to
  48 kHz stereo; the AP2 sender publishes its group's capture rate
  (`set_output_rate`), and `OverlayMixer::start` resamples the clip **once**
  to that rate (`resample::from_48k_stereo_to`, an **identity copy for 48 kHz**
  groups and Sendspin). So the per-chunk relay mix (`mix_into`) is pure
  sample-addition on the RT thread — the one-off resample is a Rust step, but
  not on the steady hot path.
- **Off-rate AirPlay senders** are the other exception: a sender that does
  not use 44.1 kHz/stereo gets a one-off linear resample in Rust at ingest
  (`sources/airplay.rs`) before the ring. The common 44.1 kHz/stereo case is
  a zero-cost passthrough, and the steady 44.1↔48 kHz conversions are all
  in-graph — so no steady Rust resampling on the hot path.

## 9. Control plane → Home Assistant

The daemon exposes state + native mutation (links, per-device volume in-band
per backend, announcements mixed per device by the relay) over
REST + WebSocket (`api/`; full list in
[`../../docs/api-reference.md`](../../docs/api-reference.md)). The Python
`custom_components` integration exposes a `media_player` per **music group**
and per **announcement group** (and, optionally, one per individual output).
`MediaPlayerEntityFeature.MEDIA_ANNOUNCE` (ducking, not replacing) lives on
the **announcement-group** entity — the intended announce/TTS target;
per-output entities carry `SELECT_SOURCE` + volume/mute only. An announcement
is a **URL** (a `media-source` id is resolved by the integration first), decoded
with `symphonia` (no `ffmpeg`) — plus the built-in `test`/`tone` diagnostic
clips. See
[decisions.md](decisions.md#ttsannounce-ducking-url-based-v1-and-wyoming-based-v2-additive).

---

## 10. End-to-end flow: AirPlay sender → Voice PE + AirPlay-2 receiver

![AirPlay-in → Voice PE + AirPlay-2](diagrams/airplay-in-to-voicepe-and-ap2.svg)

One AirPlay input fanned to a Sendspin (Voice PE) speaker **and** an
AirPlay-2 AV receiver at the same time, in sync. Colour convention:
**green** = graph-clock / SCHED_FIFO (reliable timing), **orange** = tokio
(non-RT), **blue** = RT egress thread (SCHED_FIFO), **yellow** =
buffer/queue, **purple** = receiver. The ASCII sketch below is the same
flow in text.

```
                         ┌──────────────────────────────────────────────────────────┐
 INGEST (airplay-in)     │ AirPlay sender (phone / Mac / PipeWire host)               │
                         │        │  RTP/RTSP over WiFi                                │
                         │        ▼                                                    │
                         │ shairplay: recv + decrypt + ALAC/AAC decode  [tokio]        │  sources/airplay.rs
                         │        ▼                                                    │
                         │ jitter buffer: lock-free SPSC ring (rtrb, 150 ms) [yellow]  │
                         └─────────────────────────────────────────────────────────┘
                                  ▼   (drained by the airplay-in RT_PROCESS callback)
 PIPEWIRE GRAPH          ┌──────────────────────────────────────────────────────────┐
 (data-loop, FIFO 83)    │ "airplay-in" producer node — RT_PROCESS drains ring        │
                         │   → F32LE 44.1 kHz   (mainloop thread FIFO 45)              │
                         │        ▼   (routing/sync_group/mod.rs links airplay-in INTO the anchor) │
                         │ ANCHOR support.null-audio-sink  (one per source-set)        │
                         │   · MIX inputs · resample 44.1→48 kHz · QUANT-1024 driver   │
                         │        ▼                                                    │
                         │ anchor MONITOR ports (48 kHz)                                │
                         └─────────────────────────────────────────────────────────┘
              ┌───────────────────┘        └───────────────────────────┐  (each backend captures the monitor)
              ▼  (SENDSPIN path → Voice PE)                             ▼  (AP2 path → AirPlay-2 receiver)
   ┌─────────────────────────────────────┐        ┌────────────────────────────────────────────┐
   │ sendspin-capture @ 48 kHz [FIFO 83/45]│        │ ap2 capture @ group rate [FIFO 83/45]         │
   │  pooled buf, bounded ch               │        │  48 kHz if accepted, else 44.1 (PW resamples) │
   │        ▼                              │        │        ▼                                      │
   │ sendspin-relay  [FIFO 40]             │        │ ap2-relay  [FIFO 40]                          │
   │  stamp(SharedTimeline)+overlay/duck   │        │  overlay/duck (OverlayMixer) + fan per receiver│
   │        ▼                              │        │        ▼                                      │
   │ per-device WS writers  [tokio]        │        │ ap2-producer run_streamer [FIFO 48]           │
   │        ▼                              │        │  ALAC(type 96) encode + ChaCha20 encrypt      │
   │ WebSocket / TCP → WiFi                │        │        ▼                                      │
   │        ▼                              │        │ rt-sender [FIFO 50]  RTP + PT=87 anchor        │
   │ ESPHome / Voice PE receiver           │        │  clock_nanosleep(TIMER_ABSTIME)               │
   │  send-ahead lead = what it asks for   │        │        ▼                                      │
   └─────────────────────────────────────┘        │ AirPlay-2 receiver (Yamaha / Pioneer)         │
                                                    │  render delay 0–2000 ms (dflt 0)              │
                                                    └────────────────────────────────────────────┘
                                                                    ▲
                                          gPTP 319/320 (Sync 8×/s)  │
   ┌────────────────────────────────────────────────────────────┐ │
   │ Ap2PtpService — host-global libairptp grandmaster [FIFO 55] │─┘  peers = ALL ap2 receivers
   │  clock_id 0xffff…; PT=87 anchors read CLOCK_MONOTONIC        │
   └────────────────────────────────────────────────────────────┘
```

**Key points for the diagram:**

- The two output branches share **one anchor monitor** (one steady
  QUANT-1024 driver) and each spawns its **own capture** off it — they
  diverge at the monitor. That shared origin is what keeps a Voice PE
  speaker and an AV receiver sample-coincident.
- **Inter-device sync differs per branch:** Sendspin uses the
  `SharedTimeline` stamp + the receiver's send-ahead lead (no PTP); AP2 uses
  the **PTP wall clock + PT=87 anchors** (its sample `ts` comes from a
  per-sender RTP counter, not the SharedTimeline — see the divergence note
  in §4/§6).
- **Sample rates:** 44.1 kHz at `airplay-in`, resampled to 48 kHz by the
  anchor (internal bus); the Sendspin capture is 48 kHz, the AP2 capture is
  its negotiated group rate — 48 kHz when every member accepts it (then
  end-to-end 48 kHz, no resample), else 44.1 kHz (PipeWire resamples in-graph).
  Steady rate conversion is all in the graph; the only Rust resamples are
  one-offs off the hot path (ingest fallback for non-44.1 kHz senders, and the
  announce clip matched to the group rate at overlay start — §8).
- **Everything on the steady path is SCHED_FIFO** — the ladder in §7. The
  only non-RT steady stages are the codec decode on ingest (tokio) and the
  per-device WebSocket writers (tokio, TCP-throttled).

---

## 11. Aligning outputs in time

Sample-coincident delivery (§4) is not the same as coincident *sound*: each
backend's receiver adds its own render path, and the room adds ~3 ms per metre.
So each output has one "shift this speaker in time" knob
([decisions.md](decisions.md#playout-delay-one-knob-per-backend-and-every-default-adds-nothing)),
and `align/` is the machinery for choosing its value — by ear, or by measuring
with the phone's microphone. The full design, its measured numbers and its known
blind spots are in [mic-alignment-plan.md](mic-alignment-plan.md).

**The session** (`align/calibrate/mod.rs`) owns one alignment at a time. Starting it
**holds a set of outputs**: they are given a source set nothing else has
(`align-hold-source`), which derives a temporary group with its own anchor, and
the two-tone click track (8 ms bursts at 3 kHz and 1.5 kHz, 2 s apart) is looped
into that anchor. Forming the hold costs a reconnect wave, so a multi-position
run holds the **union** of everything it will touch **once** and then scopes each
step by *audibility*, which is live and free. Levels and mutes are snapshotted and
restored, and an **idle** timeout tears the session down so a closed tab cannot
leave the room muted with a click looping.

**Audibility and level are per-output capabilities, not per-kind** — resolved in
one place and re-resolved on every change, because a receiver agent can drop
mid-run:

| | sendspin | AP2 | pw-sink |
|---|---|---|---|
| silence | in-band mute | in-band mute | the agent's mute, else **relay zero-fill** (the universal fallback, `align/relay_delay.rs`) |
| level | in-band volume | `ap2_volume`, snapshot/restore | the agent's master volume when it answers — and **no fallback**, since the relay has no gain |

**The knobs do not all have the same sign.** AP2 render delay and pw-sink playout
delay are delays; sendspin's static delay is an **advance** (the firmware
subtracts it, and `required_send_ahead_us` adds it to the group lead for exactly
that reason). So the solver intersects each member's feasible interval and picks
the common target that keeps the largest knob smallest, rather than nominating a
reference member.

**Measurement never needs a synchronised clock.** Every member renders the same
content from the same anchor, so differencing two members' arrival phases *within
one continuous microphone capture* cancels the content's own timebase, leaving
delay difference plus path difference. The mic arrives over a WebSocket
(`align/mic.rs`), the DSP is a bandpass + envelope peak with parabolic
interpolation (`align/estimator.rs`), and the run is orchestrated by
`align/measure/mod.rs`.

**Writes are deferred.** A real delay write costs a device reconnect (tens of
seconds for sendspin), so a multi-position run applies its delays
**provisionally** in the per-device relays — a ring read at an offset, upstream of
the codec, which needs no timestamp and therefore works for all three backends —
and writes the real knobs **once** at the end, through the ordinary endpoints so
the persistence and send-ahead high-water logic is not duplicated.

**An alignment run must not be interfered with, and interference must be
reported.** The hold takes an announce-arbiter **reservation**, so ordinary
announcements queue rather than cutting in — but a barge-in announcement or a
voice duck hold still **wins**, because nobody wants a fire alarm suppressed by a
calibration. The holder is notified, so the affected member's measurement is
discarded with a reason naming the cause instead of the gate blaming the user's
hand for something a doorbell did.

The wizard lives on the **Outputs** page, not on a source card: the user picks
speakers and alignment forms the group around them, which is the inverse of how
grouping normally works.
