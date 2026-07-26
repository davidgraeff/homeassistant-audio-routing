# RTP source → RAOP routing stalls the graph (and the fix)

## Symptom

Routing the Bluetooth-bridge RTP source (`bt-bridge-rtp`, from
`libpipewire-module-rtp-source`) to a **RAOP / AirPlay output**
(`raop-out-*`, from `libpipewire-module-raop-sink`) does **not** work, and it
fails in a *loud* way: the moment the link is created, the **entire graph
component driven off the RTP source freezes** — audio to any already-working
output (e.g. sendspin) stops, and the routing-matrix level meters drop to zero.
Removing the RAOP link recovers everything.

It fails both:
- in isolation (RTP source → one RAOP sink, nothing else), and
- additively (RTP source already feeding sendspin, then add a RAOP output).

The same RTP source routes to **sendspin** outputs perfectly, and RAOP outputs
play perfectly from the **AirPlay-receive source** (`airplay-in`). So it is
specifically the pair *RTP-source → RAOP-sink* that breaks.

## Root cause: no node can drive the cycle

PipeWire schedules a connected set of nodes as one cycle led by exactly one
**driver** — the node that provides the clock/quantum. A node can only be the
driver if it is driver-capable (a real timer/hardware clock). The relevant
node properties:

| Node | module | `node.network` | can drive a cycle? |
|---|---|---|---|
| `bt-bridge-rtp` | `module-rtp-source` | `true` | **no** — it's a `SPA_IO_RateMatch` *follower* |
| `raop-out-*` | `module-raop-sink` | `true` | **no** — network sink |
| `airplay-in` | vendored shairplay source | *(unset)* | **yes** |
| `sync-grp-*` (group anchor) | `support.null-audio-sink` | *(unset)*, `node.driver=true` | **yes** |

`module-rtp-source` is implemented as an adaptive **RateMatch follower**: it
receives RTP into a jitter buffer and resamples to whatever clock drives the
graph. It never sets `node.driver` and it cannot itself provide a clock.
`module-raop-sink` is a `node.network` node and likewise does not drive.

So the component `{ bt-bridge-rtp, raop-out-* }` contains **two followers and no
possible driver**. PipeWire cannot assign a driver, the cycle never runs, and
every node in that component stalls (`QUANT 0 / RATE 0` in `pw-top`). Because
metering taps the source and sendspin shares the source's component, those
freeze too.

Why the working cases work:
- **→ sendspin**: the sendspin *group sink* is a `support.null-audio-sink` with
  `node.driver=true` — a real driver that anchors the cycle; the RTP source
  follows it. (See `bridge-daemon/src/sync_group.rs`.)
- **airplay-in → RAOP**: the shairplay source is driver-capable, so it anchors
  the cycle and the RAOP sink follows — exactly as a RAOP sink normally follows
  a playback stream.

## What was ruled out (properties on the RTP source — "option 2")

Both attempts were tested live against a suspended RAOP sink with the phone
streaming; **both still stalled**:

- `stream.props.node.network = false` — removes the network flag, but the node
  is still a RateMatch follower with no clock → still no driver → stall.
- `stream.props.node.driver = true` + high `priority.driver` — PipeWire will not
  give it a valid clock; `pw-top` shows `QUANT 0 / RATE 0` → stall.

Conclusion: this cannot be fixed with properties on the RTP source. The module
is not driver-capable, full stop.

## Fix: insert a null-sink clock anchor ("option 1") — implemented

Implemented in `bridge-daemon/src/sync_group.rs`, which unifies this RTP→RAOP
anchoring with sendspin multi-room grouping into one **sync group** model (it
subsumes the former `rtp_raop_anchor.rs` and `sendspin_group.rs`). A stateless-
per-tick reconciler runs from the same loop as `routing::reconcile`. Option 2
(properties on the RTP source) was ruled out first — see above.

Route a non-driver source to a RAOP output **through a real driver** — the same
pattern sendspin grouping uses:

```
bt-bridge-rtp ──▶ sync-grp-<hash>  (support.null-audio-sink, node.driver = true)
                       │ monitor
                       ▼
                  raop-out-*  (follows the anchor's clock, like it follows airplay-in)
```

The null sink is a genuine driver: it clocks the cycle, the RTP source feeds it
as a follower, and the RAOP sink is fed from the anchor's **monitor** and
follows that clock. No more driverless component. **One anchor per source-set**
fans out to every output routed from those sources — several RAOP sinks and/or
sendspin devices at once — so a mix of AirPlay and sendspin plays off one clock.

How the reconciler decides (see `routing::raop_uses_anchor`): a RAOP output is
anchored when **either** any feeding source is non-driver (the RTP case here)
**or** it shares its exact source-set with a sendspin device (so it joins that
group). A lone RAOP output fed only by driver-capable sources stays a direct
link — snappier, one fewer buffer. The group's presentation lead and per-member
offsets are tunable (`sync_settings.rs`, `raop.latency.ms`); see the "Outputs →
Group sync" UI.

## Latency impact

**Small and, in context, negligible.** The anchor adds one intermediary sink, so
it introduces roughly **one graph period of extra buffering** (the monitor output
lags the input by ~one quantum). At a normal quantum that is on the order of
**~20–45 ms**; if a loopback is also used on the monitor→RAOP hop, budget one
more period.

That is minor relative to the latency already inherent in this path:
- **Bluetooth A2DP** itself: ~100–200 ms (phone-side codec + BT buffering).
- **RTP jitter buffer** on the receiver: `sess.latency.msec` (100–200 ms).
- **RAOP/AirPlay** playback buffering in `module-raop-sink`: hundreds of ms
  (its cycle already ran at a large quantum, ~10912 frames ≈ 0.25 s, in the
  captures here).

So the RAOP and A2DP buffers dominate; the anchor's extra period is well under
the noise floor of the total. It is also **the same intermediary the sendspin
path already incurs**, which has been in daily use without a latency complaint —
so it is a known-acceptable cost, not a new one. If tighter latency is ever
wanted, the anchor's quantum can be pinned low (e.g. `node.latency`/
`clock.quantum` on the null sink) at some CPU cost.
