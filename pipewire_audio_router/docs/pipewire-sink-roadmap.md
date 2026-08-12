# PipeWire-sink output — roadmap

Adding **another PipeWire Linux host as an independently routable output**,
so a source (or a Music Group) can be fanned to a remote room running stock
PipeWire, in sync, with per-device announce/duck. This is the AirConnect-style
"bridge one audio world into another" idea, but for the one target that speaks
our own transport natively — so it is by far the cheapest backend to add.

Read [`architecture.md`](architecture.md) first; section numbers below refer to
it. Design rationale that outlives this roadmap should graduate into
[`decisions.md`](decisions.md).

---

## 1. Why this backend is different (and cheap)

Sendspin and AP2 are expensive because the receiver cannot take a PipeWire
stream: each needs a Rust relay (FIFO 40), an encoder, a `SharedTimeline`
stamp, and per-device writers (§4, §5, §7). A remote PipeWire host is the
opposite — it speaks PipeWire/RTP natively. So the sender **is** a native
PipeWire module (`libpipewire-module-rtp-sink` + `-rtp-sap`) that appears as a
real sink node fed off the group's `anchor.monitor` (§4). This is the
"follower-sink" shape the old RAOP output had before it was removed — but here
we extend it with a per-device native mix bus so it keeps full AG
announce/duck (§4 two-tier grouping) without re-adding a Rust hot path.

**Design pillars**

| Pillar | Choice |
|---|---|
| Transport | Native PipeWire `rtp-sink` + `rtp-sap`, S16/48000/2 (matches the 48 kHz anchor bus, §8 — no resample) |
| Routing | One node per target → independently routable `pw-dev-<slug>` in the matrix |
| Sync (v1) | Fixed-offset alignment via a per-target jitter buffer (`sess.latency.msec`); separate-room use case |
| Sync (v2, future) | Shared PTP clock — see §7 |
| Announce/duck | **Native per-device mix bus** — a hard requirement, done in-graph, no Rust relay (§4 below) |
| Discovery | mDNS `_workstation._tcp` candidates → user-approved → host-reachability liveness (RTCP ruled out — see §4 + spike-results) |
| Volume | Native `Props`/`channelVolumes` on the sink node (§9) — free, no in-band protocol |

**Non-goals (v1):** same-room sample-tight lock with an AP2 receiver (needs
PTP, §7); auto-configuring the *remote* host's PipeWire (documented for the
user via a help link instead).

---

## 2. Routing path — the follower-sink off the anchor

No per-device Rust sender. Per routed target `X` in group `G` (anchor `A_G`,
one `support.null-audio-sink` per source-set, §4):

1. `rtp-sink` module loaded as a real node `pw-dev-<slug>` with a **unicast
   destination** = the target's IP and a SAP announcement (SDP `c=IN IP4
   <target>`), `sess.latency.msec = <configurable JB>`, format S16/48000/2.
2. The remote host, running `libpipewire-module-rtp-sap` in listen mode,
   auto-instantiates the matching `rtp-source` and plays.
3. `routing/sync_group.rs::reconcile` links `A_G.monitor → pw-dev-<slug>` (a monitor
   link — the RAOP-style follower-sink step that was deleted; we re-introduce
   exactly one such branch). Load on first route, unlink + unload on unroute.

Because the anchor monitor is already the steady QUANT-1024 mix at 48 kHz
(§8), the sink needs no resampling and no Rust relay — PipeWire does the egress
on the data-loop (FIFO 83).

---

## 3. Per-device announce/duck — native, in-graph (**required**)

Ducking lives in the Rust relay for Sendspin/AP2 only because those senders
consume a raw PCM channel (§4/§5). A follower-sink is *in* the graph, so the
per-device mix happens **in** the graph. Between the anchor monitor and the
sink we insert a per-device mix bus:

```
 A_G.monitor ─► [loopback  pw-music-<X>] ──────────────► ┐
                 channelVolumes = DUCK gain               │
                                                          ├─► [null-sink pw-mix-<X>] ─monitor─► [rtp-sink pw-dev-<X>] ─► RTP/SAP ─► remote
 announce pw::stream ─(linked only to targeted devices)─► ┘        channelVolumes = USER volume
```

Three daemon-created nodes per target (same class as the anchor — auto-managed,
not user config):

- **`pw-music-<X>`** (`module-loopback`): carries group music from
  `A_G.monitor`; its `channelVolumes` **is the duck control** (1.0 normal,
  ~0.2 during announce, ramped).
- **`pw-mix-<X>`** (`support.null-audio-sink`): sums ducked-music + announce;
  monitor feeds the sink.
- **`pw-dev-<X>`** (`rtp-sink`): RTP/SAP egress; its `channelVolumes` = per-device
  **user volume**.

**Overlay** = link the existing announce `pw::stream` (§9) into the mix bus of
*only the targeted devices* — that is the per-device addressing (symmetric with
`OverlayMixer::mix_into` per `node_name` in the relays, §5, but done with links).
**Duck** = step `pw-music-<X>.channelVolumes` down/up. **Volume** =
`pw-dev-<X>.channelVolumes`. All are native `Props`/link mutations the control
plane already performs (§9) — **no Rust hot-path mixing**, all on the data-loop.

Design notes:
- A *separate* loopback for music is required (not port-volume on the null-sink):
  a null-sink's inputs share one `channelVolumes`, so music and announce gains
  can't be independent unless music passes through its own node first.
- User volume on the sink and duck on the music loopback compose multiplicatively
  and independently — announcements are not attenuated by the duck.
- **Verify in the spike:** glitch-free duck *ramp*. PipeWire exposes no volume-ramp
  param; the daemon steps `channelVolumes` over ~150 ms from a control-plane timer
  (same mechanism as §9 per-node volume). Confirm no zipper noise.
- Cost: 3 nodes + a few links per target on the RT graph — negligible for a
  handful of targets, and no Rust CPU on the steady path.

---

## 4. Discovery, approval, liveness

Mirrors the Sendspin philosophy — "mDNS is discovery-only; liveness is
connection-driven" (§5.1).

> **Spike correction (see [spike-results](pipewire-sink-spike-results.md)):**
> **multicast SAP does not cross typical consumer LANs** (IGMP snooping drops the
> group), and PipeWire's rtp-sap *receiver* can't cleanly take a unicast
> announcement (connected socket). So the receiver is **not** rtp-sap discovery —
> it is a **static `rtp-source`** with the daemon's fixed wire format
> (L16/48k/2, `audio.format=S16BE`, `node.always-process=true`) on a known
> per-target port. `_workstation._tcp` still drives *candidate* discovery of
> reachable hosts; the SDP is not needed because the format is fixed.

1. **Candidates** — browse `_workstation._tcp.local.` (Avahi's default
   advertisement; some distros ship it disabled — the help link covers that),
   optionally enriched by `_ssh._tcp` / `_device-info._tcp`. This yields
   *candidate Linux hosts*, **not** proof that rtp-sap is configured — there is
   no standard "I am a PipeWire RTP sink" mDNS record.
2. **User approval** — candidates surface as `unapproved`; approval is persisted
   to config. Unapproved hosts never receive audio.
3. **Liveness — RTCP ruled out (spike-results §4).** The original plan (RTCP
   receiver reports) is **impossible**: PipeWire's RTP modules implement no RTCP
   at all. There is also no reliable TCP port to probe (SAP is passive multicast;
   PipeWire's native transport is a Unix socket). Revised options, **decision
   deferred**: (a) host reachability — mDNS presence + periodic TCP/ICMP probe
   ("host up", not "playing"); (b) a receiver-announced heartbeat SAP session the
   daemon discovers (needs extra receiver config); (c) accept fire-and-forget and
   surface "configured/announced" instead of "connected". Pick during P1.
4. **Help button** in the discovery listing → docs for the *remote* setup: load
   `libpipewire-module-rtp-sap` in listen mode (a `pipewire.conf.d/` drop-in or
   `pactl load-module`). This is the only manual step, and it is on the remote —
   nothing manual in this daemon or its graph.

---

## 5. Jitter buffer & sample rate

- **JB** = the module's `sess.latency.msec`, exposed as a **per-target setting**.
  Tune it so the target's fixed total latency lines up with the group's
  presentation offset (the Sendspin send-ahead lead / AP2's render delay, §5).
  Account for the two extra graph quanta from the loopback + mix bus (§3).
  **DONE** (2026-08-10): stored per output as `pwsink_jitter`
  (`routing/sync_settings.rs`, default `DEFAULT_PWSINK_JITTER_MS` = the module's own
  100 ms), set through the same `PUT /api/outputs/{node}/latency` endpoint and
  the same slider as the AP2 render delay, and pushed to the host by re-sending
  `welcome` (the agent reloads its receiver on every one). Clamped to 15–2000 ms in
  whole packet times: the module refuses a buffer below `rtp.ptime` and warns unless
  it is an integer multiple of it, our sender's ptime is 5 ms, and the sender's
  catch-up burst has to fit inside the buffer (hence three packets, not one, as the
  floor).
- **Rate**: S16/48000/2 end-to-end — the anchor bus is 48 kHz (§8), so nothing
  resamples anywhere on this path.
- **v1 sync ceiling**: fixed-offset alignment holds phase at *start*, but the two
  hosts' sample clocks free-run; PipeWire's adaptive resampler on the receiver
  keeps it click-free while the target slowly phase-drifts vs AP2/Sendspin.
  Inaudible in a separate room; not good enough for same-room sample-tight sync.
  → §7.

---

## 6. Work breakdown (file-level)

Mirrors the established discovery/output pattern.

| File | Change | Mirrors |
|---|---|---|
| `outputs/pwsink/discovery.rs` (new) | Browse `_workstation._tcp`, populate `SharedPwTargets` (`pw-dev-<slug>`, `approved` flag), fire `changes` | `outputs/sendspin/discovery.rs` |
| `supervisor.rs` | Register the browser on the shared LAN-restricted daemon in `start()` | existing browsers |
| `util/node_names.rs` / `store/outputs.rs` | `PW_DEV_PREFIX`; persist approved targets + per-target `jb_msec`, dest IP/port | `RaopOutputConfig` |
| `rtp_sink.rs` (new) | Load `rtp-sink` (+ SAP announce) as a real node via `pw_thread` `Load`/`Unload`; build the per-device mix bus (loopback + null-sink) | old `raop.rs` module-load; anchor `CreateSinkNode` |
| `routing/sync_group.rs` | Re-introduce **one follower-sink branch**: ensure per-target nodes + monitor links on route; tear down on unroute; wire announce-stream links into `pw-mix-<X>` for AG targets | deleted RAOP monitor-link step (§4) |
| `outputs/pwsink/target_liveness.rs` (new) | host-reachability liveness → output health (RTCP ruled out, §4) | `outputs/sendspin/liveness.rs` |
| `api.rs` | Candidate list + `approve` endpoint + per-target JB setter + help URL; expose `pw-dev-*` as routable output & `media_player` | §9 outputs derivation |
| `frontend/` (Svelte) | Discovery listing: approve + help button; per-target JB slider | existing admin console |

Freebies from it being a real node: per-target **volume** works via native
`Props` (§9) with no in-band protocol; the HA `media_player` appears
automatically from the generic outputs list (§9) — no per-backend Python.

---

## 7. Future extension — same-room sample-lock via shared PTP

v1 is separate-room (JB alignment). For same-room, sample-tight sync against an
AP2 receiver, both ends must slave to one clock:

- We already run a **host-global gPTP grandmaster** (`Ap2PtpService`, `libairptp`
  via FFI, §6).
- PipeWire's RTP modules can slave to a PTP clock (`sess.ts-refclk` / AES67-style
  `rtp.ptp`).
- **The spike**: bridge `libairptp`'s gPTP domain to PipeWire's PTP integration
  so the remote host and our graph share a clock domain. Unknown whether the two
  PTP stacks co-exist cleanly on one host — prove it before committing. Estimated
  days-to-weeks, tracked here as **Phase 3**, not scoped into v1.

---

## 8. Phases

- **P0 — Transport spike — DONE** (local mechanics + real-LAN `/api/spike/pw-sink`;
  see [spike-results](pipewire-sink-spike-results.md)). Confirmed: SAP
  announce→discover auto-creates the receiver source with session identity +
  JB; per-target key is `rtp.destination.ip` and unicast media isolates targets;
  exact params (S16LE/48k/2, `sess.name`, `sess.sap.announce`). Corrected: **no
  RTCP** (liveness rethought, §4); single-box multicast/CLI can't drive audio
  flow (real-LAN spike is the oracle).
- **P1 — Routable output (~2–3 days).** Discovery browser + `SharedPwTargets` +
  approval + config persistence; `rtp_sink.rs` node load/unload; the
  follower-sink branch in `routing/sync_group.rs`; expose in API + matrix + media_player;
  frontend listing with approve + help.
- **P2 — Per-device native announce/duck (~1–2 days).** The mix-bus topology
  (§3): loopback duck gain + announce-stream links + ramp. AG duck/overlay parity
  with Sendspin/AP2. **Required for done.**
- **P3 — PTP sample-lock (future).** §7.

Rough v1 (P0–P2): **~1 week**.

---

## 9. Open questions / risks

- **P0 blockers**: unicast SAP per-target filtering; duck-ramp glitch-freeness (§3).
- **Latency budget**: loopback + mix bus add ~2 quanta; fold into the JB default.
- **Announce fan-out**: linking one announce `pw::stream` to many `pw-mix-<X>`
  nodes — confirm the stream's output port count / auto-linking behaves, or use a
  small tee. (Sendspin/AP2 fan in Rust; here we fan by links.)
- **Discovery noise**: `_workstation._tcp` lists *all* Avahi hosts, not just
  audio-capable ones — approval gating (§4) is what keeps the list meaningful.

---

## 10. Phase B — IMPLEMENTED (as-built, 2026-07-27)

The backend is built and compiles/tests green. It **supersedes parts of §8's P1/P2
plan** — the transport pivoted from `rtp-sink` + SAP to a custom **AppleMIDI/RTP
audio sender** (the only mDNS-discoverable stock receiver, `module-rtp-session`,
refuses plain RTP; SAP multicast doesn't cross consumer routers). The sender was
proven end-to-end against a stock receiver (`E@440`; see
[spike-results](pipewire-sink-spike-results.md) "PROVEN" section).

**Files.** `outputs/pwsink/applemidi.rs` (transport, frozen interface — Task 1),
`outputs/pwsink/discovery.rs` (discovery), `outputs/pwsink/target_liveness.rs` (presence),
`outputs/pwsink/server.rs` (per-group audio path),
`outputs/pwsink/sender_liveness.rs` (session-status registry), plus wiring in `routing/sync_group.rs`,
`supervisor.rs`, `routing.rs`, `api.rs`, `main.rs`.

**Data path.** Discovery browses `_pipewire-audio._udp` → `SharedPwTargets`
(`pwsink-dev-<slug>`) → shown as a matrix column + `/api/outputs`. Routing a
source to a target makes `sync_group` put it in the source-set's group; the group
anchor's monitor is captured once (`sendspin_capture`, 48k/S16/stereo) and a
`pwsink-relay` (SCHED_FIFO) fans it to one `AppleMidiSender` **per target**. Each
sender advertises its own session `pwrouter-<slug>`; the target's
`module-rtp-session` discovers it, runs the AppleMIDI handshake, and receives L16
RTP. Liveness = `AppleMidiSender::status().established`, polled into
`pw_sink_liveness` and surfaced as `/api/outputs` `pwsink_streaming`.

**Decisions taken (were deferred to the implementer):**
- **No approval / no store.** Unlike the old `_workstation._tcp` sketch (a generic
  "any Linux host" signal that needed approval to filter noise), `_pipewire-audio.
  _udp` is advertised *only* by a configured `module-rtp-session` host — a strong
  signal — so targets are directly routable, exactly like sendspin/AP2 devices.
  (§4/§8's approval + config-persistence steps are intentionally dropped.)
- **Per-target sessions (not per-group).** One `AppleMidiSender` per target, each
  fed a per-device-**mixed** copy of the capture via `outputs::overlay_mixer::mix_into`, so
  per-device announce/duck (the must-have, §3/P2) works for free — the same shape
  AP2's relay uses. A single shared session couldn't duck one member alone.
- **Announce/duck reuses `overlay_mixer`** — no separate mix-bus topology needed
  (§3's loopback/link plan is obsolete for this path).
- **Announcing to an *unrouted* target works too (2026-07-28).** A target only
  hears an overlay while a sender is feeding it, so an unrouted one used to get
  nothing. It now gets an **on-demand announce session** — a private silent sink
  plus one `pwsink_server` advertising `pwrouter-<slug>`, opened by
  `GroupReconciler::ensure_announce_transport` before the clip and dropped after a
  30 s lease (`BY` + advert withdraw). Not permanent, precisely *because* of the
  discover-mode behaviour below: a standing advert per idle target would keep every
  receiver on the LAN attached. The clip isn't consumed until the receiver attaches,
  so it plays whole, a moment late. "Is it live?" reads `PwSinkLiveness`
  `established` (not group membership), so a routed-but-unattached target is
  reported honestly instead of swallowing the clip. Full mechanism:
  [architecture.md §5.4](architecture.md#54-announcing-to-an-output-with-nothing-routed-into-it).
  **Same cross-talk caveat**: with 2+ targets on one LAN, an announcement aimed at
  one is heard by the others until session scoping exists.

**Deferred (documented, not blocking single-room):**
- **Multi-target routing scoping.** Stock `module-rtp-session` in discover mode
  connects to *every* discovered session of the media type (no name filtering), so
  2+ pw-sink targets on one LAN cross-connect (each receiver hears both sessions).
  The single separate-room target (the primary use case) is unaffected. A scoping
  mechanism (per-receiver session binding) is future work.
- **PTP sample-lock** (§7) — unchanged, future.

**Presence vs delivery (done 2026-08-05).** These are two questions and the UI now
answers both, everywhere, by the same rule:
- `present` = **reachable**. `outputs/pwsink/target_liveness.rs` owns it (the counterpart to
  `sendspin_liveness`/`ap2_liveness`, which this backend had lacked, so a target
  seen once stayed "online" for the daemon's lifetime). No active probe is possible
  — the receiver dials *us*, so there's no port of ours on the target to poke, and
  probing the host generally would only prove the machine answers. Instead
  `pw_target_discovery` timestamps a `ServiceRemoved` (goodbye, or SRV expiry ~2 min
  after the host goes quiet) in `PwTarget::withdrawn_since`, and the task demotes
  after a 45 s grace window / removes after 5 min — unless a session is established,
  which outranks the advert.
- `streaming` = **a session is up**, from `PwSinkLiveness.established` (AP2's half
  is `Ap2Control::connected`), shared by `routing::sync_group::dialed_session_established`
  with the announce arbiter. Carried on the routing matrix (`RoutingNode.streaming`)
  and `/api/outputs` (`pwsink_streaming`), so the routing graph draws a wire to a
  reachable-but-unattached target *still* (amber "not connected") instead of
  animating it, and the Outputs page shows the same third state rather than calling
  it "offline". A status flip nudges the matrix WebSocket, so both update live.

**Not yet done:** live end-to-end deploy validation (needs the Fedora box running
`module-rtp-session` + a routing link created); `media_player` exposure parity
(outputs appear via `/api/outputs`; HA `media_player` bridging follows the
sendspin/AP2 pattern).
