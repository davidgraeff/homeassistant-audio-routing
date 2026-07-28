# Adding a device silences the whole group — analysis plan

**Symptom (reported 2026-07-28).** With a source playing to AP2 **and** sendspin
outputs, routing one *more* sendspin device into the group makes **every sendspin
device stop**, and it takes **>10 s** before audio is back everywhere.

Nothing here is a mystery of the "we don't know what it does" kind — the reconciler
was *designed* to restart a group's server when its membership changed. This plan is
about measuring where the 10 s actually goes and deciding which parts of that restart
are avoidable, because that design paid the full price for the cheapest possible
change.

Read [architecture.md §4](architecture.md#4-the-anchor--per-device-sender-model) and
[§5.1](architecture.md#51-sendspin-esphome-speakers-eg-ha-voice-pe) first.

> **Status (2026-07-28, after the second hardware test — §2d).** The daemon side is now
> measured five separate ways and is never the problem: it has the whole group
> reconnected and streaming in **205 ms – 1 s**, and the relay is continuous at a
> healthy lead from the first block. The remaining tens of seconds are **inside the
> firmware, after a reconnect**.
>
> **§4.8's instrumentation plus a read of the client's C++ have moved the mechanism**
> (§2d, §4.9-A). Measured: a reconnect resets the device's clock-sync exchange count to
> 1, and exchanges then accrue at only **one per ~4–7 s** (ten took 40.6 / 41.0 / 50.6 /
> 70.7 s across the four devices) — not the ~1 Hz §2c assumed. But the code says there
> is **no playback gate on sync convergence at all**; what gates audio is a hard-sync
> loop that starts armed on every fresh stream and only settles once alignment is inside
> **500 µs**, computed from a Kalman estimate that is still moving. And the client
> *intends* 8 exchanges every 10 s (~0.8/s), so the observed 0.2/s is a **4×
> discrepancy** that is itself now the sharpest open question. §4.9 has the detail.
>
> Fixed since: membership no longer restarts the server (§4.1), teardown is graceful
> and bounded (§4.2), an unknown-capability member no longer downgrades the group
> (§4.3), a failed start retries (§4.4), the send-ahead is a high-water mark so a
> *departing* member can't force a reconnect either (§4.6), and the process no longer
> dies with devices mid-stream (§4.7). `cargo test`: 126 daemon + 140 submodule
> passed. §4.1–§4.8 are **deployed** as of `0.2.20260728200055`. Four items are open and
> *not* fixed: a per-device static-delay change still restarts the whole group (§4.10),
> the deploy script restarts the add-on redundantly (§4.11), **the speakers' firmware is
> pinned to an abandoned pre-library snapshot from 2026-03-23 and is eleven
> `sendspin-cpp` releases behind (§4.12 — now the largest available lever)**, and our
> Rust server has never been checked against the protocol spec (§4.13).
>
> §4.12 also **closes the loop on the exchange-rate gap**: the pin predates
> esphome#17133 ("Suppress WiFi roam scanning while playing"), the Satellite1 config
> dropped its WiFi/LWIP buffer tuning (Satellite1-ESPHome#520, open), and the pinned
> commit's own message says the 10 s time-message timeout exists because WiFi scans drop
> and delay those messages. WiFi loss → slow time exchanges → an estimate that never
> settles → `hard_syncing` never clears is an end-to-end chain in which every link now
> has independent evidence.

---

## 1. Why it happens at all (the known part) — *historical, fixed in §4.1*

`sync_group.rs::reconcile` **treated** the member set as part of the server's restart
identity:

```rust
if d.sendspin_node_names != prev_devices || d.sendspin_codec != prev_codec || d.sendspin_send_ahead_us != prev_lead {
    rg.server = None;           // drop the whole server
    …start_server_per_device(…) // and build a new one
}
```

Dropping `SendspinServerHandle` tears down **everything** the group had, not just the
part that changed: the anchor capture, the `SharedTimeline` (so the anchor is
re-anchored from scratch), the inbound listener, the `ClientManager` and with it every
supervisor, and therefore every device's WebSocket. Each surviving device then has to:

1. be re-supervised and re-dialed (fresh TCP + WebSocket + `client/hello`),
2. re-establish clock sync (`client/time` exchanges),
3. send its initial `client/state`,
4. wait for `stream/start` (which now, correctly, comes *after* step 3 — see
   [§5.1](architecture.md#51-sendspin-esphome-speakers-eg-ha-voice-pe) and
   `spawn_membership_task`),
5. refill its buffer to the group's send-ahead before the first sample is heard.

So "one device joined" costs every other device a full reconnect. On top of that:

- **The join often happens more than once.** mDNS resolves devices one at a time, so a
  routing change can walk through several member sets. Observed on hardware:
  `supervising 1 device(s)` → `supervising 2` → `supervising 3`, each with its own
  restart and reconnect wave (15:30:47–48 in the 2026-07-28 log). The 400 ms reconcile
  debounce coalesces a *burst* of changes but not a sequence of genuinely different
  member sets.
- **AP2 restarts too, and it is the slower backend.** A new sendspin device does not
  change `ap2_identity`, so AP2 *should* be untouched — but that must be confirmed,
  because an AP2 reconnect is 2–4 s **per receiver, sequentially**
  (`AP2_CONNECT_ATTEMPTS` × `AP2_CONNECT_TIMEOUT` worst case) and would dominate the
  10 s on its own.
- **`READY_GRACE` (3 s)** is a *fallback* for firmware that never reports state; a
  healthy Voice PE reports within ~30 ms (measured), so this should contribute nothing
  — verify rather than assume.

---

## 2. What to measure first

The log already carries every phase boundary. One routing change, then attribute the
time:

```bash
ssh root@homeassistant.local \
  "docker logs --since 2m addon_local_pipewire_audio_router 2>&1 | grep -aE \
   'sync group|supervising|connected as|reported client/state|starting its stream|sendspin relay|AP2'"
```

Build a table per device for a single add:

| Phase | Log line | Δ |
|---|---|---|
| reconcile decided | `sync group '…': per-device senders on port … dialing N device(s)` | |
| supervisors placed | `sendspin server '…': supervising N device(s)` | |
| TCP+WS up | `[<fullname>] connected as <mac>` | |
| device declared itself | `sendspin '<node>': reported client/state (synchronized)` | |
| stream armed | `sendspin '<node>': starting its stream` | |
| audio actually paced | first `sendspin relay '<anchor>' [codec]: … blocks …` after that | |

**Three numbers decide the redesign:**

1. **How many restarts** one user action produces (count `per-device senders on port`
   lines). If it's >1, coalescing is the cheapest win available.
2. **Which backend dominates** — sendspin's reconnect wave or AP2's sequential
   reconnects. Grep the AP2 lines in the same window; if AP2 is restarting at all for a
   *sendspin* membership change, that is a bug, not a cost.
3. **Where the sendspin seconds sit** — dial/WS, clock sync, `client/state`, or buffer
   refill. Only the last is physics (it is the send-ahead, currently ≥250 ms for Opus);
   the rest is our sequencing.

Also worth capturing once, because it changes the target: does audio to the *unchanged*
devices actually stop, or does it merely restart? Measure at the wire, not by ear:

```bash
# per-device bytes to the speakers across the change
ssh root@homeassistant.local "timeout 20 tcpdump -i any -n -q 'tcp and host <device-ip> and port 8928'" \
  | awk '{print}' | ts   # or count packets per second
```

---

## 2b. Measured — the 15:34:52 add (2026-07-28)

Done. The reported add is in the log in full: `bt-bridge-rtp` → `sendspin-dev-satellite1_c4150c`,
joining a group of 3 Voice PEs that were already streaming Opus, plus one AP2 receiver.

| Phase | t | Δ from click |
|---|---|---|
| `USER ACTION: link 'bt-bridge-rtp' → 'sendspin-dev-satellite1_c4150c'` | 15:34:51.820 | — |
| reconcile runs (400 ms debounce) | 15:34:52.222 | 402 ms |
| new server up, `supervising 4 device(s)`, codec **opus**, send-ahead **250 ms** | 15:34:52.232 | 412 ms |
| capture re-attached to the anchor (node 67, unchanged) | 15:34:52.248 | 428 ms |
| all three **surviving** devices reconnected (`connected as …`) | 15:34:52.375–.395 | 555–575 ms |
| all three reported `client/state (synchronized)` | 15:34:52.386–.400 | 566–580 ms |
| all three `starting its stream` | 15:34:52.434 | 614 ms |
| the **new** device connected → state → `starting its stream` | 15:34:52.593–.633 | 773–813 ms |

**Total daemon-side cost: 813 ms**, plus the 250 ms send-ahead refill ≈ **1.05 s to audible**.
The relay's own accounting agrees: the 10 s interval spanning the restart shows 497 blocks
instead of 501 — an ~80 ms hole in the wire stream, and the lead was back at 220–263 ms
immediately.

Answers to the three questions in §2:

1. **One restart per user action**, not several (`per-device senders on port` appears once).
2. **Neither backend dominates, because AP2 never restarted.** No AP2 line and no
   `airplay_audio` warning anywhere in the window. AP2's underruns in this log belong to
   15:37:31, where the *AP2 receiver set* itself changed (1 → 2 receivers) — the identity
   checks behave exactly as designed in both directions.
3. **The sendspin seconds do not exist.** Dial+WS 150 ms, `client/state` 6–24 ms,
   arm 40 ms, refill 250 ms.

**So the >10 s is not in the daemon.** Nothing in this add cost more than a second, and
the whole point of the redesign (H1) is worth ~800 ms of that. Whatever produced ">10 s
before audio is back everywhere" is downstream of `starting its stream` — see H6, which is
now the hypothesis that matters.

The 15:30:47→15:30:48 pair, by contrast, is real churn and a *different* cause than any
hypothesis below assumed: two restarts 914 ms apart, `codec pcm, send-ahead 100 ms` then
`codec opus, send-ahead 250 ms`. See H2.

---

## 2c. The hardware test that settles it (2026-07-28, 19:36–19:39)

Three scenarios, all reported as ~30 s of sendspin silence, all with the §4.1–§4.4 fixes
deployed. In each one the daemon finishes in about a second:

| Scenario | Daemon work | Silence heard |
|---|---|---|
| Add-on start (19:36:38.1) | process start → all 4 devices discovered, dialed, `client/state`, `starting its stream` by **19:36:39.15** — **1.0 s**; AP2 playing at 19:36:39.2 | ~30 s |
| Remove one member (19:38:18.3) | `stopping sendspin server` → all 3 survivors reconnected + armed by **19:38:18.56** — **210 ms** | ~30 s |
| Add it back (19:39:12.1) | decision → all 4 armed by **19:39:12.36** — **205 ms** | ~30 s |

And the relay was healthy throughout: the first 10 s window after the restart logged 494
blocks (vs 501 nominal — a ~140 ms hole), then 501–502 blocks every window, `ts gap`
exactly 20000 µs, `lead 269..313 ms`. We were putting well-formed Opus on the wire at a
correct lead within ~200 ms and never stopped.

**So the ~30 s is firmware-side, after a (re)connect, and it does not depend on how the
previous session ended.** That last part matters because it falsifies the specific form of
H6 this document was betting on: the 19:38 and 19:39 restarts *did* send `stream/end`
first (§4.2 was deployed; the `GRACEFUL_END` warning never fired), the devices
reconnected in 130 ms, reported `client/state (synchronized)` within 5 ms, were sent
`stream/start`, and were then silent for half a minute anyway.

Two consequences:

1. **A sendspin reconnect is expensive — treat it as tens of seconds, not milliseconds.**
   That reprices every remaining restart trigger, which is what §4.6 and §4.7 are about.
   The *first* connect after an add-on start is unavoidable and costs the same 30 s; that
   is a firmware property, not something the daemon can shorten.
2. **The mechanism is still unproven**, and the leading candidate is clock-sync
   convergence: a player may only schedule audio once its `client/time`/`server/time`
   filter has converged, the spec's cadence is ~1 Hz, and ~30 exchanges at 1 Hz is ~30 s.
   *(Superseded by §2d: the measured cadence is one exchange per ~4–7 s, not ~1 Hz, so
   the arithmetic here is right in shape and wrong in scale — far fewer exchanges are
   needed to spend half a minute. Read §2d before relying on this paragraph.)*
   The `client/state (synchronized)` a device sends 5 ms after connecting is then a stale
   or aspirational claim, not evidence of a usable offset. §4.8 adds the instrumentation
   that decides this.

---

## 2d. The instrumentation's verdict (2026-07-28, 20:02–20:36, deployed build)

`0.2.20260728200055` — the first build carrying §4.6–§4.8 — plus a deliberate
static-delay change as the restart trigger. Four results.

**1 · Add-on start: sendspin is not late relative to AP2 at all.** From the startup log:

| Time (UTC) | Event |
|---|---|
| 20:02:04.338 | dialing 4 devices, send-ahead 320 ms |
| 20:02:04.363 | AP2 senders "streaming to 2 receiver(s)" |
| 20:02:04.491–.535 | 3 devices connected **and** `client/state (synchronized)` |
| **20:02:04.564–.565** | **sendspin `starting its stream`** |
| 20:02:04.829 | AP2 → Dusche actually streaming |
| 20:02:05.115–.164 | 4th device connects + starts |
| 20:02:06.911 | AP2 → Pioneer actually streaming |

Sendspin starts **265 ms before the first AP2 receiver and 2.35 s before the second**,
with no errors, no re-dials, one time exchange each. So "sendspin joins way later than
AP2" is entirely about when the *speaker* starts emitting, not when we start sending.

**2 · A static-delay change: 219 ms, and it restarts the whole group.**

```
20:35:13.507  stopping sendspin server (a static-delay change needs a reconnect)
20:35:13.726  starting its stream        ×4
```

All four members dropped and reconnected for a change to **one** device's delay —
see §4.10. Server-side that is a fifth of a second.

**3 · Our own output does not gap across the restart.** With the source verified to be
carrying audio, the relay logged 534–635 B `largest packet` continuously straight
through the restart — not one silent window.

> **Methodological trap, and it caught this test first time round.** The relay's
> `largest packet` proxy measures the *source's* audio content, not the speaker's. The
> Bluetooth source has its own unrelated silence fault (full-rate aptX decoding to
> zeros — see
> [rtp-input-dropouts-plan.md](rtp-input-dropouts-plan.md)), and the first run of this
> test showed ~50 s of `3 B` immediately after the restart, which reads exactly like a
> restart-induced outage and is not one. **Any group-churn measurement over the BT
> source must independently confirm the source has audio for the whole window** — join
> the multicast group and check the RTP payload peak
> ([`firmware/pi-bridge/bluetooth-testing-app/`](../../firmware/pi-bridge/bluetooth-testing-app/README.md)
> does this live). The re-run with the source confirmed at peak 12k–18k is result 3.

**4 · Clock-sync exchanges are far slower than assumed — the mechanism candidate.**
Every reconnect resets a device to `1 time exchange(s), 0.0s since connect`. The
`time exchange(s)` lines then arrive every ten exchanges:

| Device | 10 exchanges after reconnect | implied interval |
|---|---|---|
| `20:F8:3B:09:3C:A8` (093ca8) | 40.6 s | ~4.1 s |
| `20:F8:3B:09:62:87` (096287) | 41.0 s | ~4.1 s |
| `98:A3:16:C4:15:0C` (satellite1) | 50.6 s | ~5.1 s |
| `20:F8:3B:09:66:F3` (0966f3) | 70.7 s | ~7.1 s |

Steady state before the restart was the same cadence (n=20–40 at ages 82–143 s), so
this is a cold start, not a fault. **§2c assumed ~1 Hz and therefore ~30 exchanges for
~30 s; the real cadence is 0.14–0.25 Hz**, so even a handful of required exchanges
costs tens of seconds — and the 1.7× spread across devices matches the reported
"different on each speaker" exactly.

This does not *prove* the player gates on convergence — and the code read that followed
(§4.9-A) shows **it does not**: nothing consults the sync uncertainty before playing.
What the measurement does establish is that the only clock the player has is settling on
precisely the timescale of the symptom, that `client/state (synchronized)` 5 ms after
connect cannot be reporting a usable offset, and — once compared against the client's
own `BURST_SIZE = 8` / `BURST_INTERVAL_MS = 10000` — that exchanges are arriving **4×
slower than the firmware intends**. §4.9-A carries that forward.

**5 · The device's own log was empty — but not simply because the level is low.**
`aioesphomeapi-logs` against satellite1 across **two** complete stream stop/starts
produced **zero** lines beyond the handshake banner. The device already runs
`logger: level: debug`; the time-sync lines are `ESP_LOGV` (**VERBOSE**), which `debug`
suppresses — so `level: VERBOSE` plus a reflash is what the direct route needs
(§4.9-B). Seeing *nothing at all* is itself odd, though: at `debug` an ESPHome device
normally chatters, and the burst code's timeout path logs at `ESP_LOGW`, which `debug`
would show. So confirm the API log subscription is actually delivering before drawing
conclusions from an empty log.

```bash
aioesphomeapi-logs 192.168.178.99 --noise-psk "ZqYcIe6Ptvy2DRCIYmSS6UdOP04SLVVftwUU3I8Z2PA=" --strip-ansi-escapes
```

## 3. Hypotheses, most likely first

**H1 — the restart is unnecessary for a pure membership change.** *Confirmed feasible, but
worth ~800 ms, not 10 s.* The vendored crate
now has everything needed to add a member to a *running* server: `ClientManager::
supervise(fullname, url)` (already called on every reconcile, idempotent) and
`Group::with_timeline(...)` + `add_member` on the shared `SharedTimeline`, which
explicitly supports mid-stream joins (the server-role PR demonstrates it, and the spec
requires late joiners to get future timestamps). Nothing about a new device forces the
existing devices' streams to end. Both halves of the removal path exist too:
`ClientManager::stop_client(fullname)` (`vendor/sendspin/src/server/manager.rs:306`) ends one
device's supervision gracefully, which is exactly the operation whose absence used to force
"drop and rebuild the whole manager".

Note also that the promotion path is *already* fully dynamic: `spawn_membership_task` polls
`pending` and gives any newly-connected client its own `Group::with_timeline` on the running
shared timeline (`sendspin_server.rs:960`). A device added via `supervise` on a live server
would therefore arm itself with no further plumbing — the restart is buying nothing that the
membership task doesn't already do.

*Test:* stop treating `sendspin_node_names` as restart identity; restart only when the
**stream config** changes. Concretely, the identity should be
`(codec, send_ahead_us, codec_header)` — everything a `stream/start` carries — because
that is what the timeline fixes at construction. Membership becomes: supervise the new
fullnames, `stop_client` the departed ones.

**H2 — a new device legitimately changes the stream config, so a restart is correct but
should be rarer.** *Confirmed, and it has a worse form than this described.* Both the codec
(`resolve_codec` intersects across members: a new device without Opus forces the whole
group to PCM) and the send-ahead floor (`required_send_ahead_us` takes the max across
members) depend on membership.

The part that was wrong: **a second identical Voice PE does change both — the first time.**
`supported_codecs` and `min_buffer_ms` are not mDNS facts; they are learned from the
device's own `client/hello` / `client/state` and live only in the in-memory registry
(`sendspin_discovery.rs:177` seeds them empty; `sendspin_server.rs:826` and
`record_timing_request` fill them in on connect). So routing a device that has never
connected in this daemon's lifetime goes:

1. identity grows → the new member counts as **PCM-only with no buffer requirement** →
   the group restarts at `codec pcm, send-ahead 100 ms`, dragging every existing member
   down off Opus;
2. the device connects and reports Opus + its `min_buffer_ms` → `report_format_support`
   calls `notify_reconcile` → the group restarts **again** at `codec opus, send-ahead
   250 ms`.

That is exactly the 15:30:47→48 pair in §2b, and it is structural, not incidental. What
saved the 15:34:52 add was the **idle sender**: satellite1 had been connected as an idle
device, so its codecs and buffer floor were already cached and the identity didn't move.
A re-resolve preserves the cache (`sendspin_discovery.rs:144-165`), so this bites once per
device per daemon lifetime — but a daemon restart re-arms it for every device, so the first
route after a deploy always pays it.

*Fix candidate (independent of H1, and cheap):* don't let a member with no learned
capabilities lower the group's config — treat "unknown" as "no constraint" until the device
has spoken, rather than "PCM-only". Or persist the learned `(supported_codecs,
min_buffer_ms)` next to the other per-device settings so a cold daemon starts from what it
already knew.

**H3 — the sequence of member sets multiplies the cost.** *Falsified for this add* — one
`per-device senders on port` line per user action (§2b.1). The 15:30 double restart was H2,
not an mDNS trickle.

One correction worth keeping, because it bears on any fix here: the parenthetical
"(`compute_desired` skips devices whose URL hasn't resolved)" is **not** true of the restart
identity. `compute_desired` pushes every routed device into `sendspin_node_names`
unconditionally (`sync_group.rs:795`) and gates only `sendspin_members` on a resolved URL
(`:798`). The identity is `node_names`, so a device that is discovered but not yet
resolvable already restarts the group without being dialable — "hold a newly-desired member
out of the identity until its URL is known" is work to do, not a property we have.

**H4 — AP2 is dragged in.** *Falsified with evidence.* No AP2 restart and no
`airplay_audio` warning anywhere in the 15:34:52 window; the anchor kept its node id (67)
and the new sendspin capture re-linked to it 16 ms after the old one went away. AP2 holds a
separate capture on the same anchor (`ap2_server.rs:361`) and rode through the sendspin
capture being destroyed and recreated without complaint. AP2's underruns in this log come
only from a genuine AP2 receiver-set change (15:37:31).

**H5 — the readiness gate adds a fixed 3 s.** *Falsified.* `client/state` arrived 6–24 ms
after each reconnect; `READY_GRACE` never engaged, and no device logged the "no
client/state within the grace" variant.

**H6 — the outage is device-side recovery from an un-announced disconnect.** *Falsified
in its specific form by §2c, and superseded by §2d.* The teardown is now graceful and
bounded (§4.2, deployed) — §2d watched a restart send `stream/end`, reconnect all four
devices and re-arm them in **219 ms** with no gap in our own output, and the devices were
silent for tens of seconds anyway. So the outage is device-side but **not** recovery from
an abrupt disconnect; §2d result 4 puts clock-sync convergence in its place. The rest of
this entry is kept because it is the reasoning that produced §4.2, which is worth having
regardless.

Originally: *now the hypothesis that matters*, because §2b leaves ~9 s unaccounted for. A restart tells the
devices nothing: `SendspinServerHandle::drop` aborts the accept/event tasks and
`ClientManager`'s own `Drop` calls `managed.handle.abort()` on every supervisor
(`vendor/sendspin/src/server/manager.rs:334`), so each device's WebSocket dies mid-stream
with **no `stream/end` and no WS close frame** — while that device has an active stream.
Contrast the deliberate demote path, which is careful to `broadcast_stream_end()` first
(`sendspin_server.rs:995`). If ESPHome's player needs seconds to abandon a stream that was
killed under it — or refuses the new `stream/start` until it has — that is the 10 s, and it
is invisible here because from our side everything was healthy at 813 ms.

*Test:* the §2 `tcpdump`, but read for the *device's* behaviour: does it accept the new
`stream/start` promptly and stay silent anyway (device-side decoder/buffer stall), or does
it not come back promptly at all? Cross-check the speaker's own ESPHome log across the
change — the one vantage point this investigation never had.

*Fix candidate, cheap enough to do regardless of H1:* make teardown graceful —
`broadcast_stream_end()` on every group and close the sockets before dropping the manager,
instead of `abort()`. If H6 holds, that alone recovers most of the 10 s, and it is also
what H1 needs for a departing member.

---

## 4. Fixes — §4.1–§4.8 shipped and deployed; §4.9–§4.13 open

Ordering note that still stands: the daemon-side restart was ~800 ms, so H1 alone was never
going to explain a 10 s report. Everything below was done anyway, because each item is
independently right; what remains is the device-side measurement (§4.9-B).

### 4.1 The restart identity is now the stream config ✅ (H1)

`sync_group.rs` step (c). The identity is `(codec, send_ahead_us)` — what `stream/start`
carries and what the shared timeline fixes at construction. Membership is applied to the
**running** server in step (c2): `supervise` for arrivals and re-resolved addresses,
`SendspinServerHandle::stop_device` for departures. A join now costs the existing members
nothing; they are not told and do not notice.

The decision moved out of the reconcile body into `sendspin_server_action`, a pure function
over `(routed, have_server, config_changed, force_restart)`, so the rule is unit-testable
without a live PipeWire graph (§5).

Two consequences worth knowing:

- `force_server_restart` (the static-delay path, where the reconnect *is* the point) can no
  longer work by clearing the remembered device set — that set is not the identity any more.
  It sets a `force_restart` flag that the next reconcile honours, which also means its
  teardown goes through the graceful path.
- The H3 correction is now moot: a discovered-but-unresolved device no longer restarts
  anything, because it is not in the identity at all. It gets supervised when its URL
  resolves, and until then it only sets the retry flag (§4.4).

### 4.2 Teardown is graceful, and bounded ✅ (H6's fix half)

`SendspinServerHandle::shutdown()` — `broadcast_stream_end()` to every group, then
`stop_client` on every supervisor, then `abort().await` on the tasks. Used by every
deliberate teardown: a config-change restart, a group going away, and an idle sender being
superseded by its group sender. `Drop` remains as the fallback and is documented as such.

The await on the aborted accept task is also what closes the **port rebind race**: the
listener socket lives inside that task's future, and `SO_REUSEADDR` does not permit two live
listeners, so "abort then immediately rebind the same port" was a race. It never showed up
in the log, but it is gone rather than merely unlikely.

`stop_device` is the same idea for one member: `stream/end`, then `stop_client`.

**One risk this introduced, and how it is bounded:** `broadcast_stream_end()` awaits the
write reaching each member's socket, and a device that vanished mid-stream (powered off,
WiFi gone) may not fail that write until the kernel's TCP retransmit budget runs out —
minutes. The teardown runs on the reconcile task, which holds the group lock, so that would
have frozen all routing. Both paths are wrapped in a `GRACEFUL_END` (300 ms) timeout and log
when they hit it. A healthy device acks in well under a millisecond.

### 4.3 An unknown-capability member no longer downgrades the group ✅ (H2)

`resolve_codec` skips members with an empty codec list instead of treating them as PCM-only.
Routing a never-before-connected speaker into a live Opus group therefore keeps Opus; if the
newcomer's `client/hello` proves it can't decode it, `report_format_support` nudges a
reconcile and *that* restarts the group — at most one restart, and only for hardware that
really can't. `device_supports` is unchanged, so the UI's per-device picker still says
"unknown ⇒ only PCM is assured".

Not done, and not needed for this: persisting the learned capabilities across daemon
restarts. With the optimistic rule a cold daemon no longer pays a downgrade for its
ignorance, which was the only reason to want the cache on disk.

### 4.4 A failed start now retries ✅ (latent issue)

`GroupReconciler::retry_wanted()` is set by every path that failed in a way only a retry can
fix — sendspin/AP2/pw-sink senders that didn't start, an anchor or idle sink that didn't
appear, a device whose URL hasn't resolved — and the reconcile task in `main.rs` then waits
on the change channel with a 3 s timeout instead of indefinitely. The debounce is unchanged;
a retry that fires with nothing to coalesce skips it.

### 4.6 The send-ahead is a high-water mark ✅ (the 2026-07-28 regression)

This is what the hardware test caught, and §4.1 alone did not cover it. The group's
send-ahead is derived from membership (`required_send_ahead_us` takes the max across
members, plus each member's static delay), so it moves *both ways* when membership changes.
Observed: removing `home-assistant-voice-096287` dropped the group from 300 ms to 250 ms,
and re-adding it raised it back — each one a "stream config changed" restart, i.e. exactly
the reconnect §4.1 was supposed to stop, arrived at by the back door.

`server_send_ahead_us` is now compared **one-way**: only a *higher* requirement restarts the
server. The send-ahead is a floor we must clear, so keeping a departed member's larger lead
is always correct — it costs latency, not correctness — and 50 ms of latency against 30 s of
silence is not a close call. The mark is reset to 0 when the server stops, so the next one
starts from the real requirement. And because it is a high-water mark, the reported
remove-then-add sequence now restarts **nothing at all**: the mark stays at 300 ms, so
re-adding the device matches it.

A deliberate static-delay change still takes effect, because that path calls
`force_server_restart` explicitly — which is the one place a reconnect is the *point*.

The restart log now says which of the four reasons fired, and says out loud what it costs.

### 4.7 The process no longer dies with devices mid-stream ✅

`shutdown_sendspin()`, called from the SIGTERM path in `main.rs` next to `shutdown_ap2()`,
sends `stream/end` and closes every group server *and* idle sender before the process
exits. The comment it replaces — "sendspin group servers tear down with the process" — was
accurate and was the reason an add-on restart left every speaker holding a dead stream.

Honest caveat: §2c shows the devices' 30 s does *not* depend on how the last session ended,
so this is unlikely to shorten the startup case. It is correct regardless, and it is what
the spec asks of a server that is going away.

### 4.8 Instrumentation to close the mechanism out ✅ (deployed; verdict in §2d)

Two blind spots made §2c's conclusion take three test scenarios instead of one:

- **Clock-sync progress was invisible.** `client/time` is consumed inside the connection
  layer and never surfaced. It now counts exchanges per connection and logs the first and
  every tenth with seconds-since-connect (`[<mac>] 20 time exchange(s), 19.8s since
  connect`). If audio starts after ~30 exchanges, convergence is the answer and the number
  is right there next to the first audible-block timestamp.
- **Per-device audio loss was `trace`-only.** `Group::push_at` now returns a `PushOutcome`
  (`queued` / `dropped` / `disconnected`), and the relay aggregates drops per device into
  its 10 s stats line as a `warn`. A device whose write backlog fills gets *nothing* while
  every whole-group number stays perfect — the one failure mode that looks exactly like
  healthy streaming from the server side. That interval now logs on the PCM path too when
  anything was dropped.

**Answered — see §2d result 4:** the exchanges are ~0.2 Hz (not ~1 Hz) and there are no
drops, i.e. the clock-convergence branch. The rest of this subsection is the reasoning
that got there.

Next test should answer it from the log alone: if the silent window shows ~1 Hz time
exchanges and no drops, it is firmware clock convergence and the daemon is done; if it
shows drops for the silent device, the device stopped reading and the question becomes why.

### 4.9 The mechanism, now measured — and what closes it ⬜

§4.8's instrumentation answered §2c's open question (§2d, result 4): **a reconnect
resets the device's clock-sync exchange count to 1, and exchanges accrue at one per
~4–7 s.** §2c guessed ~1 Hz; the truth is 0.14–0.25 Hz, so even a modest convergence
requirement costs tens of seconds, and the 1.7× spread between devices (40.6 → 70.7 s
for ten exchanges) explains why the silence is a different length on each speaker.

Both of §2c's checks are now discharged:

1. **Does anything still reconnect?** Yes — a **static-delay change** does, and it
   restarts the whole group (§2d result 2, §4.10). Membership changes no longer do
   (§4.1), so the user-facing trigger has moved rather than disappeared.
2. **The startup case, from the log.** Done: exchanges are ~0.2 Hz, no drops, relay
   continuous. That is the "clock convergence in the player" branch — with the caveat
   that the cadence is far slower than the spec's ~1 Hz, which is itself worth
   understanding.

**What is still not proven** is that the player *gates audio on* convergence. The
evidence is circumstantial-but-tight: the only clock it has is converging on exactly
the timescale of the symptom, and `client/state (synchronized)` arrives 5 ms after
connect so it cannot be reporting a usable offset. Two ways to finish it, cheapest
first:

**A · Read the ESPHome client's C++ — done 2026-07-28, and it moves the answer.**

The exact code running on these speakers is the ESPHome `sendspin` external component,
pinned — in the **sibling `satellite1` checkout**, not this repo, at
`satellite1/Satellite1-ESPHome/config/common/sendspin.yaml` — to
[`kahrendt/esphome`](https://github.com/kahrendt/esphome) @
`7a6cf5c8472b7e2fa18ee0fc314f66a80d249e32` ([ESPHome
PR #14933](https://github.com/esphome/esphome/pull/14933)), components
`[const, media_source, sendspin]`. The protocol/client library it tracks is
[Sendspin/sendspin-cpp](https://github.com/Sendspin/sendspin-cpp). The pinned copy is
already on disk after any build, which is the fastest way to read what actually runs:

```
~/Entwicklung/home_assistant/satellite1/Satellite1-ESPHome/config/.esphome/
    external_components/<hash>/esphome/components/sendspin/
```

Four findings, all from that source (code reading — **not** runtime-verified).

> **These describe the *pinned* firmware, not current sendspin-cpp.** v0.5.0's #44
> rebased the time filter wholesale (§4.12), so the constants below are historical for
> anyone on 0.7.0. They are still the right description of the speakers as they run
> *today*.

1. **There is no playback gate on clock-sync convergence.** `get_covariance()` is never
   called outside `sendspin_time_filter.cpp`, and `TIME_FILTER_MIN_SAMPLES = 100` is the
   *forgetting-factor* threshold (`min_samples_for_forgetting_`), not a readiness test.
   **So §2c's and §2d's leading candidate — "the player waits for the filter to
   converge before scheduling audio" — is not supported by the code.** Nothing consults
   the sync uncertainty before playing.

2. **What does gate it is a hard-sync alignment loop**, and it starts armed on every
   fresh stream (`sendspin_media_source.h:76`, `bool hard_syncing{true}` — "Starts true
   so initial sync uses tight settle threshold"):

   | Constant | Value | Role |
   |---|---|---|
   | `HARD_SYNC_THRESHOLD_US` | 5000 µs | drift that *triggers* a hard sync |
   | `HARD_SYNC_SETTLE_THRESHOLD_US` | **500 µs** | must be met to *leave* hard-sync |

   `hard_syncing` is cleared only once alignment is inside 500 µs
   (`sendspin_media_source.cpp:641`). The alignment target is computed from the Kalman
   filter's offset estimate — so while that estimate is still moving, the tight 500 µs
   settle threshold is hard to hit, and the player keeps re-syncing instead of playing.
   **That is the mechanism to test: not "waits for convergence" but "cannot settle
   inside 500 µs until the estimate stops moving".** Same practical consequence, but a
   different place to look and a different fix.

3. **The intended exchange cadence is 4× faster than what we observe.**
   `sendspin_time_burst.h`: `BURST_SIZE = 8`, `BURST_INTERVAL_MS = 10000` — eight
   exchanges, then 10 s from burst *completion*, i.e. ~0.8 exchanges/s. Our server saw
   **10 exchanges in 40.6–70.7 s (~0.2/s)** (§2d result 4). Something is delaying or
   losing them, and the code offers a candidate: `RESPONSE_TIMEOUT_MS = 10000`, where a
   timed-out message advances the burst by one (`sendspin_time_burst.cpp:44–46`) — eight
   timeouts would stretch one burst to ~80 s. The other candidate is a starved ESP loop.
   **This discrepancy is now the most concrete open question**, because it is on the
   critical path for finding 2: a slow-moving estimate is exactly what keeps
   `hard_syncing` armed.

   Note the timeout path logs at `ESP_LOGW`, so it should be visible even at the
   device's current level — and we saw nothing (§2d result 5), which argues against
   timeouts and toward the estimate simply taking that long to settle. Not conclusive:
   see B on why we may not have been receiving device logs at all.

4. **A clean `stream/end` and a socket close are not obviously different** to the
   client's audio path, so §4.2's graceful teardown should not be credited with fixing
   this symptom (consistent with §2c, which measured exactly that).

**B · Get the device's own log — the level is the problem, and it is a smaller one than
§2d assumed.** satellite1 already runs `logger: level: debug`
(`satellite1-c4150c.yaml:145`), so §2d result 5's "level too low" is only half right:
the normal time-sync line is `ESP_LOGV(TAG, "Sent time message %u/%u")` — **VERBOSE**,
which `debug` suppresses. What is needed is `level: VERBOSE` (globally or just for the
sendspin component via `logs:`), then a reflash:

```bash
cd ~/Entwicklung/home_assistant/satellite1 && . ./.venv/bin/activate \
  && esphome compile Satellite1-ESPHome/config/satellite1-c4150c.yaml \
  && esphome upload Satellite1-ESPHome/config/satellite1-c4150c.yaml --device 192.168.178.99
```

and watch it across a restart:

```bash
aioesphomeapi-logs 192.168.178.99 --noise-psk "ZqYcIe6Ptvy2DRCIYmSS6UdOP04SLVVftwUU3I8Z2PA=" --strip-ansi-escapes
```

> The `--noise-psk` above is satellite1's ESPHome API encryption key, in the clear on
> purpose so this procedure is runnable as written. It is **not treated as a secret**: it
> is already committed in plaintext in the device's own config
> (`satellite1/Satellite1-ESPHome/config/satellite1-c4150c.yaml`, `api: encryption: key:`), it only
> grants access to one speaker on the LAN, and the owner rotates it at will. If it is
> rotated, take the new value from that file. Per-device keys for the other speakers live
> in their own configs.

That also makes it cheap to **add temporary instrumentation to the component itself** —
one `ESP_LOGI` where `hard_syncing` is cleared, and one logging the filter's
`get_covariance()` per burst, would settle findings 2 and 3 outright. The component is a
plain external source tree, so a local patch plus this compile/upload cycle is the whole
loop.

Worth remembering that we saw **zero** device lines at `debug` — not even ESPHome's
usual chatter — so before trusting an empty log, confirm the API log subscription is
actually delivering (a WARN-level line from the timeout path would prove it).

**On the regression.** The report is that this did not always happen. If the client's
gate is unchanged, the change is on our side — either that a delay edit now forces a
group-wide restart, or that the lead/identity computation made reconnects more
frequent. `git log -S force_server_restart` and the §4.6 send-ahead history are the
places to look.

### 4.10 A per-device static-delay change restarts the whole group ⬜

Measured (§2d result 2): changing **one** device's static delay stops the group's
single sendspin server, so all four members drop and reconnect — and by §4.9 each one
then pays tens of seconds of silence. The daemon cost is only 219 ms; the user cost is
a group-wide outage for a one-device calibration tweak, which is the same shape as the
bug §4.1 fixed for membership.

The restart is deliberate — `api.rs` notes the firmware reads the static delay only at
stream start, so a live push doesn't shift a running stream — but the blast radius is
not. Two directions:

- **Scope the restart to the one device.** Its delay is a property of its own sender;
  the other members' streams do not change. This is the §4.1 argument applied to the
  delay path.
- **Prefer the live push where the firmware supports it.** The `sendspin_delay_live`
  setting already skips the restart for firmware that honours a live `SetStaticDelay`;
  finding out whether current firmware does (a §4.9-A question) may remove the restart
  entirely.

Note the group *lead* genuinely is group-wide (it is a high-water mark over members —
§4.6), so a delay change that raises the floor must still re-arm everyone. A change
that does **not** move `group_lead_effective_ms` should not.

### 4.11 The deploy script restarted the add-on twice ✅

`scripts/deploy-dev.sh` bumps the version and runs `ha store reload`, which makes the
supervisor pull and **start** the new container; the script then unconditionally runs
`ha apps restart`, producing a second stop/start. Observed in the supervisor log:

```
22:01:45.656  Stopping addon_local_pipewire_audio_router      (old: 0.2.20260728193511)
22:01:59.712  Cleaning …                                       ← 14.06 s
22:02:00.691  Starting …:0.2.20260728200055
22:02:01.248  /addons/local_pipewire_audio_router/restart      ← the script
22:02:01.261  Stopping …                                       ← again, 0.6 s later
22:02:02.184  Starting …
```

Mostly cosmetic (0.5 s in there is nothing to tear down) but it doubles the disruption
per deploy, and it makes the add-on log unreadable for exactly the restart questions this
document keeps asking — the new container is killed ~0.5 s into startup, mid-handshake
with the speakers.

**Fixed.** `scripts/deploy-dev.sh` no longer restarts unconditionally; it starts the app
only when it isn't already running:

```bash
state="$(ssh … "ha apps info $ADDON_SLUG --raw-json | jq -r '.data.state'" || true)"
case "${state:-unknown}" in
  started | startup) echo "already running — no restart needed" ;;
  *)                 ssh … "ha apps start $ADDON_SLUG" ;;
esac
```

Three details that make it correct rather than just shorter:

- **`ha apps update` already restarts an app that was running**, which is why the extra
  restart was redundant — but **`ha apps install` leaves it stopped**, so a first install
  still needs a start. Checking the state covers both without special-casing.
- **`startup` counts as running.** It is the transient state between the supervisor's
  start and the app reporting ready; treating it as stopped would re-introduce the very
  race this removes.
- **`|| true` on the assignment.** Under this script's `set -euo pipefail`, a plain
  `VAR=$(…)` *is* subject to `set -e` (unlike the `if [ "$(…)" ]` form used for the
  install check just above), so a momentary ssh failure would otherwise abort the deploy
  after the image was already pushed. Falling back to `unknown` takes the start path,
  which is the safe default when the state can't be read.

Verified against the live host: the running case reports "already running (state=started)
— no restart needed", and an unreachable host yields `unknown` without aborting.

Two related observations from the same log, both worth keeping in mind when judging
§4.2/§4.7 on real deployments:

- **The old container's stop took 14.06 s**, over run.sh's own ~8 s wait *and* Docker's
  10 s default. So that shutdown did **not** finish in budget — either the daemon
  outran run.sh's window or the supervisor SIGKILLed it. Whether §4.7's graceful
  sendspin teardown completed is therefore unknown for that deployment.
- **A graceful-shutdown fix cannot help the deployment that ships it.** The shutdown
  code that runs belongs to the container being *stopped* — here
  `0.2.20260728193511`, which predates §4.7. The earliest §4.7 can be judged is the
  *next* restart after it lands.

### 4.12 The firmware is pinned to an abandoned pre-library snapshot ⬜

This is now the **largest single lever** available, and it was invisible until the pin
was dated.

| | |
|---|---|
| Pinned | `kahrendt/esphome` @ `7a6cf5c8472b7e2fa18ee0fc314f66a80d249e32`, **2026-03-23**, branch `sendspin-dev-snapshot2` |
| That PR | [esphome#14933](https://github.com/esphome/esphome/pull/14933) — **CLOSED, never merged** (WIP) |
| `sendspin-cpp` v0.1.0 | **2026-04-01** — nine days *after* the pin |
| ESPHome upstream today | component **merged** (#15924, #15929, #15950 …), pinning `sendspin/sendspin-cpp` ref **0.7.0** as an IDF component |

So the speakers run a **vendored, pre-library snapshot**: the time-sync and audio code
lives *inside* the component. Upstream's component no longer contains any of it — only
`sendspin_hub.{cpp,h}`, `automation.h` and the platform subdirectories remain; the rest
became [Sendspin/sendspin-cpp](https://github.com/Sendspin/sendspin-cpp), which has had
**eleven releases** since (v0.1.0 → v0.7.0, 2026-04-01 → 2026-07-22).

**Fixes between the pin and 0.7.0 that bear on this document.** Grouped by the symptom
they touch; issue numbers are sendspin-cpp PRs unless noted.

| Area | Fix |
|---|---|
| Initial silence / stutter | **#69 "Reduce initial playback stutter"** (v0.7.0, breaking) · #92 "Remove hello message initial delay" (v0.7.0) · #35 "Drop initial part of decoded frame if it now will be late" (v0.4.0) |
| **Reconnect — this document's trigger** | **#60 "Fix message-send races on reconnect and handoff"** (v0.6.1) · **#43 "Clears all states on a server disconnect to avoid exposing stale values"** (v0.5.0) · #79 connection-layer hardening, #80 reap connections that never complete the hello handshake, #81 prove-then-admit lifecycle (v0.7.0) · #57 httpd session owns the connection (v0.6.0) · #30 "crash when closing a connection" (v0.4.0) |
| Timing arithmetic | **#58 "Fix 32-bit overflow in `AudioStreamInfo::frames_to_microseconds`"** (v0.6.1) |
| Time sync — **the code §4.9-A read** | **#44 "Rebase time filter on upstream reference repo"** (v0.5.0) · #53 remove the interpolation buffer, #39 fixed write timeout in the sync task, **#50 "Improve sync task logging"** (v0.5.0) · #16 "Simplify time message handling" (v0.2.0) |
| Observability we lack | **#67 "Report error state when out of sync"** (v0.7.0) — the device would *tell* us, instead of us inferring it from exchange counts |
| Task priority / starvation (§4.9-A finding 3's other candidate) | #34 sync task priority 18 (v0.4.0) · #66 httpd priority 17 → 5 (v0.7.0) · #89 bit-gated `loop()` drains (v0.7.0) |
| **Static delay (§4.10)** | #29 "static delay no longer applied when not adjustable" (v0.3.1) · #17 "Expose static delay setting to sendspin server" (v0.2.0) — these may decide whether `sendspin_delay_live` can be turned on and the restart dropped entirely |
| Defaults that affect *our* server | #61: `server_max_connections` **2 → 4**; `client_id` now falls back to the interface MAC, new `mac_address` field (#65) — cf. esphome#16331 "Fix client_id MAC mismatch with ethernet" |

**And the WiFi chain, which closes §4.9-A finding 3.** Three independent pieces agree:

1. **The pinned commit's own message** is about exactly this: *"increasing time message
   response timeout to 10 seconds. Especially useful during wifi scans which delays
   things significantly causing much higher latencies … many packets being dropped and
   they need to be resent."* `RESPONSE_TIMEOUT_MS = 10000` is a **workaround for WiFi
   loss**, not a healthy design point.
2. **[Satellite1-ESPHome#520](https://github.com/FutureProofHomes/Satellite1-ESPHome/pull/520)**
   (open, 2026-05-29): the sendspin migration **dropped the WiFi/LWIP/SPIRAM sdkconfig
   tuning**, leaving tiny LWIP/WiFi buffers — "unstable Wi-Fi and unreliable OTA …
   device intermittently drops off the network". The owner independently reports seeing
   WiFi drops.
3. **esphome#17133 "Suppress WiFi roam scanning while playing"** (merged 2026-07-10) —
   the upstream fix for the roam-scan half. The pin predates it by four months.

Which gives an end-to-end chain in which every link now has its own evidence:

```
WiFi buffer starvation (#520) + roam scans (esphome#17133)
  → client/time messages delayed or dropped          (pinned commit's own message)
    → 0.2 exchanges/s observed vs 0.8/s intended      (§2d result 4 vs §4.9-A finding 3)
      → the Kalman offset estimate settles slowly
        → hard_syncing cannot reach its 500 µs settle threshold  (§4.9-A finding 2)
          → tens of seconds of silence, different per device      (the reported symptom)
```

Two of the three fixes already exist upstream. **Suggested order:**

1. **#520 first.** Cheapest, addresses a confirmed root cause, independently
   symptom-matched, and it is a config-only change.
2. **Then move off the abandoned WIP pin** onto upstream ESPHome + sendspin-cpp 0.7.0.
   A bigger jump with real breaking changes, but it collects the ~20 fixes above at once.
3. **Re-measure §2d.** Several of §4.9's open questions may simply evaporate — the 4×
   exchange gap most of all.
4. §4.13, independently.

> **Scope warning for §4.9-A.** That subsection's constants and control flow describe the
> **pinned** firmware. v0.5.0's #44 replaced the time filter wholesale, so those numbers
> are not current sendspin-cpp. Do not invest in patching the old snapshot — the
> instrumentation idea in §4.9-B is only worth it if the upgrade is deferred, and #50/#67
> may make it unnecessary anyway.

### 4.13 Our Rust server has never been checked against the spec ⬜

The daemon's sendspin server was written by converting a Python library's behaviour, not
by reading the protocol specification — so conformance is unverified by construction,
and a non-conformant server is a live candidate for firmware-side symptoms we have been
attributing to the firmware.

There is an authoritative source, and it is not the Python library:

| Repo | What it is |
|---|---|
| **[Sendspin/spec](https://github.com/Sendspin/spec)** | Specification of the Sendspin protocol — **the thing to diff against** |
| [Sendspin/aiosendspin](https://github.com/Sendspin/aiosendspin) | Async Python library — almost certainly what our server was converted from |
| [Sendspin/sendspin-rs](https://github.com/Sendspin/sendspin-rs) | "Sendspin Rust Library (WIP)" — upstream of the fork we carry as a submodule |
| [Sendspin/sendspin-cpp](https://github.com/Sendspin/sendspin-cpp) | The client library the speakers run (see §4.12) |

Worth checking first, because each would produce exactly the class of symptom this
document chases:

- **`client/time` handling** — cadence expectations, whether the server is allowed to
  coalesce or reorder, and what a client may assume about response latency. §2d measured
  10 exchanges where the client intended ~32; if any part of that is our server's doing
  rather than WiFi, it is here.
- **`stream/start` / `stream/end` / `stream/clear` semantics** — v0.5.0's #54 moved
  `stream/clear` handling *into* the library, so the spec's expectations may have moved
  under us.
- **What `client/state (synchronized)` is actually asserting.** We treat it as "may now
  be streamed to" (§2c) and separately observed it arriving 5 ms after connect, which
  cannot describe a converged clock. If the spec says something narrower, our readiness
  gate is reading a field that does not mean what we think.
- **Timestamp/presentation semantics for the send-ahead**, against §4.6's high-water mark.

### Not a fix

Lowering the send-ahead to shorten the refill. The floor is protocol- and hardware-driven
(see [architecture.md §8](architecture.md#8-sample-rate-harmonization)); trading it away to
mask a restart would reintroduce the stutter class we just closed.

Coalescing membership growth (H3) — one restart per user action was measured, and with §4.1
a membership change restarts nothing at all, so there is nothing left to coalesce.

---

## 5. Regression guards — added ✅

- `sync_group::tests::membership_alone_does_not_restart_the_sendspin_server`: a
  membership-only change yields `KeepRunning`; a codec/send-ahead change and a forced
  reconnect each yield `Start`.
- `sync_group::tests::the_server_follows_whether_anything_is_routed`: first device routed
  ⇒ `Start`, last device unrouted ⇒ `Stop`, neither ⇒ `Idle` (an AP2-only group).
- `sendspin_server::tests::an_unknown_member_does_not_drag_the_group_off_its_codec`: an
  unknown member imposes nothing, a member that *has* spoken and lacks the codec still
  vetoes it, and an explicit pick is unaffected either way. This is the one that would have
  caught the 15:30 double restart.
- `sendspin_server::tests::a_device_we_have_never_connected_to_is_only_pcm_assured` keeps the
  display-side rule pinned, so the two semantics can't silently converge again.
- The phase log lines from §2 are unchanged, and the membership path adds one of its own
  (`sendspin membership now N device(s) (+X/-Y) — no restart`), so the next "adding a
  speaker is slow" report is still answerable from one log grep — and now the log
  distinguishes a restart from a join.
  A future "adding a speaker is slow" report should be answerable from one log grep.
