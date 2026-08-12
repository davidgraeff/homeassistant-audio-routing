# PipeWire-sink output — how it works, and what the design got wrong

**Another PipeWire Linux host as an independently routable output**, so a source
(or a Music Group) can be fanned to a remote room running stock PipeWire, in
sync, with per-device announce/duck. This is the AirConnect-style "bridge one
audio world into another" idea for the one target that speaks our own transport
natively — which is what made it the cheapest backend to add, and also what made
almost every *specific* mechanism in the original design wrong.

Read [`architecture.md`](architecture.md) for how it sits in the daemon (§5.3 this
backend, §5.5 announcing to an unrouted output, §5.6 the three states); the
receiver side is [`receiver-agent.md`](receiver-agent.md), which
supersedes this file wherever the two disagree about the remote host.

---

## 1. As built

| Concern | What it does |
|---|---|
| Transport | A **custom AppleMIDI/RTP audio sender** (`outputs/pwsink/applemidi.rs`), L16 (`S16BE`) 48 kHz stereo, 5 ms packets, advertising one session `pwrouter-<slug>` per target |
| Receiver | Stock `libpipewire-module-rtp-session` in discover mode, **loaded and owned by the `pwrouter-agent`** on the remote host — no drop-in, no root, no PipeWire restart |
| Existence | A pw-sink output **is** a paired agent (`Agents::connected_targets()`). mDNS discovery of `_pipewire-audio._udp` survives as a *diagnostic only*; nothing in the routing path reads it |
| Routing | One output per target (`pwsink-dev-<host>_<user>`), routable in the matrix like any other, fed off the group anchor's monitor |
| Sync (v1) | Fixed-offset: a per-target receiver jitter buffer (`pwsink_jitter`, default 100 ms, clamped 15–2000 ms). Separate-room use case |
| Announce/duck | Per target via `outputs::overlay_mixer::mix_into`, exactly like AP2 — plus an out-of-band duck of *foreign* streams on the host through its agent |
| Volume/mute | The host's own master sink, driven and reported by its agent; surfaced in the matrix, `/api/outputs`, and the HA `media_player` |

**The data path.** Routing a source to a target puts it in that source-set's
group. The group anchor's monitor is captured once (`pw/capture.rs`,
48 kHz/S16/stereo) and a `pwsink-relay` (SCHED_FIFO 40) fans it to one
`AppleMidiSender` **per target**, each fed a per-device-*mixed* copy via
`mix_into`. The target's `module-rtp-session` discovers our advertised session,
runs the AppleMIDI handshake and receives L16 RTP. Because the anchor monitor is
already the steady QUANT-1024 48 kHz mix, nothing resamples anywhere on this path.

**Liveness is two separate questions**, and the UI answers both (the rule is in
[architecture.md §5.6](architecture.md#56-reachable-connected-playing--three-states-one-rule)):

- `present` = **reachable**, owned by `outputs/pwsink/target_liveness.rs`. No
  active probe is possible — the receiver dials *us*, so there is no port of ours
  on the target to poke, and probing the host generally would only prove the
  machine answers. Instead discovery timestamps a `ServiceRemoved` (goodbye, or
  SRV expiry ~2 min after the host goes quiet) in `PwTarget::withdrawn_since`, and
  the task demotes after a 45 s grace / removes after 5 min — unless a session is
  established, which outranks the advert.
- `streaming` = **a session is up**, from `PwSinkLiveness.established`, shared with
  the announce arbiter by `routing::sync_group::dialed_session_established`. A
  reachable-but-unattached target gets a drawn-but-not-animated wire and an amber
  "not connected", instead of being called offline or being animated as if it were
  carrying audio. Getting *out* of that state is not the receiver's job either: it
  invites once per mDNS resolve, so every new sender has to **ask** its agent to
  rebuild the receive side ([receiver-agent.md](receiver-agent.md) §7.4) — without
  that, an add-on restart or any group rebuild left the target amber indefinitely.

**The jitter buffer is the daemon's decision, not the host's.** It is stored per
output as `pwsink_jitter` (`routing/sync_settings.rs`), set through the same
`PUT /api/outputs/{node}/latency` endpoint and the same slider as the AP2 render
delay, and pushed to the host by re-sending `welcome` — the agent reloads its
receiver on every one by design, so retuning needed no new message type and works
against already-deployed agents. It is clamped to whole packet times, floor
**15 ms** (`PWSINK_JITTER_MIN_MS` = 3 × `PACKET_MS`): the module refuses a buffer
below `rtp.ptime` and warns unless it is an integer multiple of it, and the
sender's catch-up burst has to fit inside the buffer — which is what makes the
floor three packets rather than one.

---

## 2. What the spikes overturned

The plan for this backend was "load `libpipewire-module-rtp-sink` per target plus
one `rtp-sap` announcer, build a native per-device mix bus out of a loopback and a
null-sink, browse `_workstation._tcp` for candidate hosts, gate them behind user
approval, and get liveness from RTCP receiver reports." Almost none of that
survived contact with real hosts. Each correction is worth keeping because each
one is a thing that *looks* obviously right.

**SAP multicast does not cross typical consumer LANs.** IGMP snooping drops the
group, and PipeWire's rtp-sap *receiver* cannot cleanly take a unicast
announcement (connected socket). So auto-configuration of the receiver via SAP
discovery is not available at all.

**Stock `module-rtp-session` refuses plain RTP**, and it is the only
mDNS-discoverable stock receiver. That is what forced the pivot to an
**AppleMIDI/RTP** sender: the handshake is the price of admission to the one
receiver a user does not have to hand-configure. It was proven end-to-end against
a stock receiver before anything else was built.

**There is no RTCP.** The original liveness design (RTCP receiver reports) is not
merely awkward, it is impossible: PipeWire's RTP modules implement no RTCP at all.
There is also no reliable TCP port to probe — SAP is passive multicast and
PipeWire's native transport is a Unix socket. That is what pushed liveness onto
advert withdrawal plus session establishment (§1), and ultimately onto the agent's
own connection, which is the honest signal.

**Approval was solving a problem that went away.** `_workstation._tcp` is Avahi's
generic "any Linux host" advertisement, so it lists the whole LAN and needed an
approval step to filter noise. `_pipewire-audio._udp` is advertised only by a
configured `module-rtp-session` host — a strong signal — so targets became directly
routable like sendspin and AP2 devices, and the approval + config-persistence
steps were dropped. The adoption gate then arrived generically for every output
kind, and the agent's pairing became the real gate
([receiver-agent.md](receiver-agent.md) §3).

**The per-device mix bus was unnecessary.** The plan built ducking out of graph
nodes — a `module-loopback` per target carrying group music with its
`channelVolumes` as the duck control, a `null-sink` summing it with the announce
stream, and the `rtp-sink` on the end — precisely because a follower-sink is *in*
the graph. With per-target senders fed from one capture, `overlay_mixer::mix_into`
already does per-device duck+overlay in the relay, exactly as AP2 does, so three
extra graph nodes and a set of link mutations per target buy nothing. The
loopback-and-links topology is obsolete for this path.

**Per-target sessions, not per-group.** One `AppleMidiSender` per target is what
makes per-device announce/duck work at all; a single shared session could not duck
one member alone.

What *did* survive from the original design: the transport format (S16/48000/2
matching the anchor bus, so no resampling), one node per target so it is
independently routable, the jitter buffer as the per-target alignment knob, and
the observation that a native-speaking receiver means no Rust codec and no second
hot path. `outputs/pwsink/module_args.rs` still holds the `rtp-sink` + `rtp-sap`
SPA-JSON args, now used only by `spike/pwsink.rs`.

---

## 3. Announcing to an unrouted target

A target only hears an overlay while a sender is feeding it, so an unrouted one
used to get nothing. It now gets an **on-demand announce session**: a private
silent sink plus one `pwsink_server` advertising `pwrouter-<slug>`, opened by
`GroupReconciler::ensure_announce_transport` before the clip and dropped after a
30 s lease (`BY` + advert withdraw). Deliberately not permanent — a standing
advert per idle target would keep every discover-mode receiver on the LAN attached
to sessions it has no reason to be in. The clip is not consumed until the receiver
attaches, so it plays whole, a moment late. Full mechanism:
[architecture.md §5.5](architecture.md#55-announcing-to-an-output-with-nothing-routed-into-it).

---

## 4. Open items

- **Multi-target session scoping.** Stock `module-rtp-session` in discover mode
  connects to *every* discovered session of the media type — compatibility is
  decided by service type plus the TXT format fields, and the session *name* is
  never compared — so 2+ pw-sink targets on one LAN cross-connect, and an
  announcement aimed at one is heard by the others. It cannot be fixed with module
  arguments; the usable hook is that every session's streams are stamped
  `rtp.session = <name>`, so the agent can identify its own and tear down or mute a
  foreign router's links. Agent-side work, still undecided between teardown and
  mute ([receiver-agent.md](receiver-agent.md) §5). The single
  separate-room target — the primary use case — is unaffected.
- **Same-room sample-lock via shared PTP.** v1 holds phase at *start* but the two
  hosts' sample clocks free-run; PipeWire's adaptive resampler on the receiver
  keeps it click-free while the target slowly phase-drifts against AP2/sendspin.
  Inaudible in a separate room, not good enough for same-room sample-tight sync.
  Both halves of the fix exist — we already run a host-global gPTP grandmaster
  (`outputs/ap2/ptp.rs`, libairptp via FFI), and PipeWire's RTP modules can slave
  to a PTP clock (`sess.ts-refclk` / AES67-style `rtp.ptp`) — but whether the two
  PTP stacks co-exist cleanly on one host is unknown and wants a spike before
  anything is committed. Days-to-weeks; not scoped.
- **Live end-to-end validation of the agent-driven path** on the Fedora box: the
  control plane was verified live and the audio path was verified in the spike,
  but the *join* between them (an agent controlling the master volume of the sink
  *its own* session lands in) still needs a deployed add-on that actually
  advertises a session to that agent. Details and the rest of the agent's
  unverified list: [receiver-agent.md](receiver-agent.md) §7.

**Closed since the original design:** the HA `media_player` now covers pw-sink
outputs (`VOLUME_SET` / `VOLUME_MUTE` against the host-reported level, plus
`SELECT_SOURCE` from the generic outputs list), and the level/mute read path is
surfaced everywhere — the routing matrix carries `volume`/`muted` for
`pwsink-dev-*` from the agent's `HostState`, and the Outputs page and flow graph
render a control for them. A host with no agent answering correctly shows no
control rather than a slider that silently does nothing.

> **The *write* path from the web UI was missing until 2026-08-12**, and it failed in
> the most confusing way available: the Outputs page and the flow graph each chose the
> endpoint themselves with `kind === 'airplay2' ? ap2 : sendspin`, so a
> `pwsink-dev-*` name went to `PUT /api/sendspin/volume`. That endpoint *stores* a
> level for a device that has not connected yet and answers `ok: true`, so the click
> looked accepted — and then the next pushed matrix frame, which carries what the host
> reports, put the old value straight back. Hence "the mute button flips itself back"
> and "the slider moves and nothing happens", with the agent never asked for anything.
> The read side worked throughout, which is what made it look like a daemon bug.
>
> The mapping now lives in `frontend/src/lib/outputs/level.ts`, once, keyed off the
> node-name prefix: a new output kind adds one branch and both call sites get it, and
> a missing branch is a loud mistake in one place instead of a
> wrong-but-successful write in two. `PUT /api/pwsink/volume|mute` answers **503**
> when no agent is connected (there is nothing to save for later), so the optimistic
> mute flip is now also rolled back on failure rather than left claiming a mute that
> never landed.
