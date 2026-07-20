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
| `sendspin-grp-*` | `support.null-audio-sink` | *(unset)*, `node.driver=true` | **yes** |

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
  follows it. (See `bridge-daemon/src/sendspin_group.rs`.)
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

## Fix: insert a null-sink clock anchor ("option 1")

Route a non-driver source to a RAOP output **through a real driver** — the same
pattern `sendspin_group.rs` already uses:

```
bt-bridge-rtp ──▶ rtp-anchor  (support.null-audio-sink, node.driver = true)
                       │ monitor
                       ▼
                  raop-out-*  (follows the anchor's clock, like it follows airplay-in)
```

The null sink is a genuine driver: it clocks the cycle, the RTP source feeds it
as a follower, and the RAOP sink is fed from the anchor's **monitor** and
follows that clock. No more driverless component. A single shared anchor can
fan out to several RAOP sinks (and can coexist with sendspin, which brings its
own driver sink).

Implementation sketch (daemon): when the routing intent links a **non-driver
source** (currently only the RTP source) to a **RAOP** output, transparently
insert/attach a shared `rtp-anchor` null sink instead of a direct port link, and
reconcile it away when the last such route is removed — mirroring the
group-lifecycle logic in `sendspin_group.rs`. Direct links stay the default for
driver-capable sources (`airplay-in`), which already work.

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

## Reproducing / diagnosing

On the add-on host (`ssh root@homeassistant.local`, container
`addon_local_pipewire_audio_router`, `export XDG_RUNTIME_DIR=/run/user/0`):

```
# who can drive: look for node.network / node.driver
pw-dump | jq '.[] | select(.type|endswith("Node")) | .info.props
  | {name:."node.name", network:."node.network", driver:."node.driver"}'

# reproduce the stall (WARNING: freezes the source's whole component):
pw-link bt-bridge-rtp:receive_FL raop-out-<name>:send_FL
pw-top -b -n 3        # stalled nodes show QUANT 0 / RATE 0 / ??? , music stops
pw-link -d bt-bridge-rtp:receive_FL raop-out-<name>:send_FL   # recover
```

A healthy (driver-anchored) cycle instead shows non-zero, advancing
`QUANT/RATE/BUSY` with `ERR 0`.

## Notes on measurement gotchas found during this investigation

- **`pw-top` node state `running` does not mean audio is flowing.** A stalled,
  driverless cycle still reports its nodes as `R`; the tell is `QUANT 0 /
  RATE 0` and BUSY not advancing across samples.
- `curl` is **not installed** in the add-on container; use the daemon's HTTP API
  from another host (`http://<ha-ip>:8099/...`, host-network) or `pw-link`
  directly. `python3` is also absent in the container — parse `pw-dump` output
  on the client side.
