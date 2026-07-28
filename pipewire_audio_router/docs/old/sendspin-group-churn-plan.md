# Adding a device silences the whole group — analysis plan

> **ARCHIVED 2026-07-29. Closed — the cause was ours.** Kept for the measurements, the
> hypotheses and how each of them died; several conclusions in here were *wrong* and are
> struck through in place rather than deleted, because the wrong turns are the useful part.
> **Still-open work has moved to
> [../sendspin-open-items.md](../sendspin-open-items.md)** — read that first if you are
> looking for something to do. Start at §4.14 for the answer, §2d for the measurement that
> made it checkable.

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

> **Status (2026-07-29 — ROOT CAUSE FOUND, §4.14).** The tens-of-seconds silence was
> **our own server**, not the firmware and not WiFi. We rate-limited and coalesced the
> client's `client/time` requests — a 50 ms silent drop plus a single-slot reply channel —
> against a spec that puts the cadence entirely in the client's hands and requires a reply
> to each. The ESPHome player is one-request-in-flight with a 10 s timeout and no
> retransmit, and it decodes **nothing** until its opening 8-message burst completes, so
> every dropped request bought 10 s of total silence and every reconnect paid again. The
> arithmetic reproduces §2d's per-device spread (4.06 / 4.10 / 5.1 / 7.1 s per exchange)
> without invoking WiFi at all. **Fixed** in the submodule with a regression test; **not
> yet confirmed on hardware.**
>
> The daemon side was never the problem and is now measured five ways: it has the whole
> group reconnected and streaming in **205 ms – 1 s**, with the relay continuous at a
> healthy lead from the first block.
>
> Why it took so long is worth recording: `time exchange(s)` counted only the requests we
> *answered*, so our own drops were invisible and the resulting "4× discrepancy" was
> attributed to WiFi loss. §4.8's instrumentation pointed at the firmware because it was
> placed one line too late. The log now prints **received and replied**.
>
> Shipped and deployed (`0.2.20260728200055`): §4.1 membership no longer restarts the
> server, §4.2 graceful bounded teardown, §4.3 unknown-capability members, §4.4 retry,
> §4.6 send-ahead high-water mark, §4.7 no death with devices mid-stream, §4.8
> instrumentation. Fixed since, **not yet deployed**: §4.10 a static-delay change touches
> only its own device, §4.11 the deploy script no longer double-restarts, **§4.14 the root
> cause**. Still open: §4.12 the firmware pin (now **demoted** — v0.7.0 keeps the same
> burst design, so it does not address §4.14), §4.13's remaining conformance gaps, §4.15
> config-change-without-reconnect, and §4.9's firmware questions (largely moot).

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
> [rtp-input-dropouts-plan.md](../rtp-input-dropouts-plan.md)), and the first run of this
> test showed ~50 s of `3 B` immediately after the restart, which reads exactly like a
> restart-induced outage and is not one. **Any group-churn measurement over the BT
> source must independently confirm the source has audio for the whole window** — join
> the multicast group and check the RTP payload peak
> ([`firmware/pi-bridge/bluetooth-testing-app/`](../../../firmware/pi-bridge/bluetooth-testing-app/README.md)
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

- the static-delay path, where the reconnect *is* the point, can no longer work by
  clearing the remembered device set — that set is not the identity any more. It records
  intent for the next reconcile to honour, which also means its teardown goes through the
  graceful path. (It recorded a whole-group `force_restart` flag at first; §4.10 narrowed
  that to a set of *devices*, and `ServerAction` lost the input entirely.)
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

A deliberate static-delay change still takes effect, because that path asks for a
reconnect explicitly — which is the one place a reconnect is the *point*. Since §4.10 it
asks for it per *device* (`force_device_reconnect`), and the only part of a delay change
that reaches the whole group is the one that belongs to it: a delay big enough to raise
this very high-water mark.

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

1. **Does anything still reconnect?** Yes — a **static-delay change** does. It used to
   restart the whole group (§2d result 2); since §4.10 it reconnects only the edited
   device, unless its new delay raises the group's send-ahead. Membership changes no
   longer reconnect anything (§4.1), so the user-facing trigger has narrowed rather than
   disappeared: calibrating one speaker still costs *that* speaker its ~30 s.
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

1. ~~**There is no playback gate on clock-sync convergence.**~~ **WRONG — corrected in
   §4.14.** It is true that `get_covariance()` is never called outside
   `sendspin_time_filter.cpp` and that `TIME_FILTER_MIN_SAMPLES = 100` is the
   *forgetting-factor* threshold rather than a readiness test. The error was concluding
   "no gate" from the absence of *those* symbols: the gate is one level coarser and
   strictly worse. `sync_handle_load_chunk_` returns early decoding **nothing** while
   `!is_time_synced()`, and `is_time_synced()` == `has_update()` == `count_ >= 1`
   (`sendspin_media_source.cpp:571`, `sendspin_connection.h:129`,
   `sendspin_time_filter.cpp:172`) — all-or-nothing on the *first* accepted measurement,
   which arrives only when the opening burst completes. §2c's instinct was right; this
   subsection's grep was aimed at the wrong symbol. **Lesson worth keeping: "I grepped
   for the consumers and found none" is not the same as "there is no gate".**

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

3. **The intended exchange cadence is 4× faster than what we observe.** *(Cause found —
   §4.14: the gap was our own silent drops, and the counter used to measure it only
   counted requests we chose to answer. Not WiFi loss.)*
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

**B · Get the device's own log — blocked by tooling, not just log level.**

> **Update 2026-07-29.** Two further findings, and together they mean an empty device log
> proves nothing. First, `--strip-ansi-escapes` **does not exist** in the venv's
> `aioesphomeapi-logs` (it errors with a usage message); it only exists in the
> `~/.local/bin` install, so part of §2d's "empty log" was a tooling failure — the
> command in this document only works with the latter. Second, with the flag removed the
> capture handshakes fine and still delivers **zero** lines in 150 s across a full stream
> stop/start, including no reply to the `dump_config` sent at
> `LOG_LEVEL_VERY_VERBOSE`, and a capture left running for ~2 h printed nothing even
> across a reboot that should have produced a boot banner. **The API log subscription is
> not delivering, for a reason not yet determined.** Since the pinned firmware's timeout
> path logs at `ESP_LOGW` — which `debug` would pass — a working subscription would test
> §4.14's mechanism directly from the device side. Fix the delivery before drawing any
> inference from silence in the log.

The original reasoning, still valid as far as it goes: satellite1 already runs `logger: level: debug`
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

### 4.10 A static-delay change is scoped to the one device ✅

Measured (§2d result 2): changing **one** device's static delay stopped the group's
single sendspin server, so all four members dropped and reconnected — and by §4.9 each
one then pays tens of seconds of silence. The daemon cost was only 219 ms; the user cost
was a group-wide outage for a one-device calibration tweak, which is the same shape as
the bug §4.1 fixed for membership.

The restart itself is deliberate — `api.rs` notes the firmware reads the static delay
only at stream start, so a live push doesn't shift a running stream — but the blast
radius was not. **Option A is implemented: the reconnect is scoped to the device whose
delay changed.** Its delay is a property of its own per-device sender; the other
members' timeline, codec, send-ahead and timestamps are all unchanged, so there is
nothing for them to re-arm. This is the §4.1 argument applied to the delay path.

`GroupReconciler::force_server_restart` became `force_device_reconnect(node_name)`,
which marks that one device in its group's `force_device_reconnect` set instead of
flagging the whole server. `ServerAction` lost its `force_restart` input entirely —
there is no caller left that wants a whole-group re-arm for its own reasons.

**The group lead is the one part that really is group-wide**, and it is handled by the
machinery that already existed rather than by a second rule: a member's static delay
feeds `required_send_ahead_us`, so a delay big enough to push the group's requirement
past the running server's high-water mark shows up as a *stream config* change
(`sendspin_config_changed`) and takes the ordinary restart path — correct, because the
shared timeline fixes the send-ahead at construction. Anything smaller leaves the
config unchanged and only the edited device reconnects.

Two implementation details worth knowing:

- **The reconnect takes two reconcile passes**, deliberately. The first ends that
  device's stream (`stop_device` — graceful `stream/end`) and drops it from the
  remembered member set; the retry pass (§4.4, ≤3 s) sees the member set differ from
  what's desired and re-supervises it through the ordinary membership path (§4.1's
  step c2), which redials. Doing `stop_device` + `supervise` back-to-back would race:
  `stop_client` only *signals* the old supervisor, which then emits `Disconnected`,
  while `supervise` immediately spawns a fresh one for the same fullname — and both
  feed one serial event loop, so a `Disconnected` landing after the new `Connected`
  would remove the new connection's `pending`/`groups`/`client_to_node` entries and
  unregister its control sender. That is the connected-but-silent failure §4.8 needed
  instrumentation to see. The dial takes ~130 ms so the ordering is *likely* fine and
  not *guaranteed*; splitting the passes makes it structural. The ≤3 s is free against
  the tens of seconds the speaker's own resync costs, and only that speaker waits.
- **"Does the lead move" is judged against the running server's high-water mark**, not
  against the freshly-computed `group_lead_effective_ms` that `GET /api/sync/settings`
  reports. Those diverge after a member with a large requirement leaves (§4.6: the API's
  number drops, the mark stays). So a delay edit can change the *reported* effective lead
  and still — deliberately — not re-arm the group. That is the §4.6 trade, not a new
  approximation.

Option B is **not** implemented and is still open: **prefer the live push where the
firmware supports it.** The `sendspin_delay_live` setting already skips the reconnect
entirely for firmware that honours a live `SetStaticDelay`, and it is left working as
it was; whether current firmware does is a §4.9-A / §4.12 question (see #29 and #17 in
§4.12's table). If the answer is yes, this whole path becomes a no-op — but scoping was
worth doing regardless, because it is right even on firmware that needs the reconnect.

Not deployed / not measured on hardware.

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

### 4.12 The firmware pin — phase 1 done, migration assessed and demoted ◐

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
| Defaults that affect *our* server | `server_max_connections` **2 → 4** (v0.7.0's breaking-changes summary attributes this to #65/#61; #61 itself is titled "Make WebSocket server port configurable", `server_port` default 8928, so check the config struct rather than the PR title); `client_id` now falls back to the interface MAC, new `mac_address` field (#65) — cf. esphome#16331 "Fix client_id MAC mismatch with ethernet" |

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
3. **esphome#17133 "Suppress WiFi roam scanning while playing"** (merged 2026-07-08) —
   the upstream fix for the roam-scan half. The pin predates it by four months. Note it
   is **already in ESPHome 2026.7.0**, i.e. available a release *earlier* than the
   sendspin-cpp 0.7.0 pin, so the roam-scan half can be had without the full ladder.

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

#### Phase 1 executed (2026-07-29): PR #520 applied, flashed, measured

**Applied and flashed successfully.** `config/common/core_board.yaml` only, verbatim from
`gh pr diff 520` — the LWIP/WiFi/SPIRAM sdkconfig restoration (TCP buffers 65535, recv
mboxes 64, AMPDU RX/TX with 32-slot BA windows, RX buffers 16/128, TX 64, SPIRAM
placement for wifi+lwip+mDNS). Compile 203 s; the generated
`sdkconfig.satellite1-c4150c*` was **spot-checked to confirm the keys actually landed**
rather than being silently ignored. OTA 3.3 MB in 14.5 s; device answered ping 6 s after
reboot and re-attached ~25 s later. No USB recovery needed. Not committed.

**Result: null, exactly as predicted once §4.14 was found.** Seconds to reach 10
exchanges after a reconnect — and note three devices were left unflashed as a **control
group**, which is what makes this readable:

| Device | firmware | pre-flash | post-flash |
|---|---|---|---|
| satellite1 `98:A3:…` | **reflashed** | 21.8, 61.0 | 41.2, 61.1, 51.4, 61.0 |
| 093ca8 | old (control) | 41.5, 31.7 | 31.6, 41.5 |
| 096287 | old (control) | 31.6, 31.6 | 31.7, 31.9 |
| 0966f3 | old (control) | 21.4, 41.6 | 21.7, 41.6 |

Neither the flashed device nor the controls moved. **Keep #520 anyway** — it compiles,
the sdkconfig verifiably applies, and the owner's independent WiFi-drop and
OTA-reliability reports justify it on its own merits. It is simply not this symptom.

**But the measurement produced one unambiguous positive finding, and it is the strongest
independent evidence for §4.14.** All **18** reconnect samples land on
**`k × 10 s + 1.0–1.9 s`**, with k ∈ {2,3,4,5,6} — 21.4/21.7/21.8, 31.6/31.6/31.6/31.7/
31.7/31.9, 41.2/41.5/41.5/41.6/41.6, 51.4, 61.0/61.0/61.1 — **zero exceptions**, with
steady state occasionally reaching 12.2 s (k=1, the healthy floor). A cadence limited by
*WiFi loss* would be continuously distributed. A cadence quantised to exact 10 s steps is
a **discrete count of 10 s response timeouts** — i.e. requests that were never answered.
That is §4.14's mechanism measured from the outside, without reading any code.

**Caveat that bounds what this test proved: the source was not merely silent, it was
absent.** The phone is disconnected at the Pi, so `bt-bridge-rtp` emitted **no packets at
all** (observer: `pkts=0 peak=0` for every one of 3 525 s; relay `largest packet 3 B`
throughout). So these numbers come from an *unloaded* WiFi path — ~150 B/s of Opus
silence instead of ~26 kB/s of music. Consequences: the before/after is internally valid
(identical conditions both sides, plus the control group), but it is **not comparable to
§2d's 40.6/41.0/50.6/70.7**, and it **under-tests PR #520 specifically**, since buffer
starvation is what a *loaded* RX path causes. Redo it with the phone connected. (Two
workarounds — injecting a format-identical RTP stream, and connecting a dev box's own
Bluetooth to the Pi's sink — were attempted and correctly refused as out of scope.)

#### ⚠️ The flash left a lasting artifact — and it is not #520's doing

The speaker's identity changed: `satellite1-c4150c` → **`satellite1-c4150c-c4150c`**
(proven by the pre- and post-flash API handshakes naming each). The cause is **the
owner's own uncommitted rename reaching hardware for the first time**, not the PR:
`common/satellite1.base.yaml:42` sets `name_add_mac_suffix: true`, and the new untracked
`config/satellite1-c4150c.yaml:53` sets `name: satellite1-c4150c` explicitly — so the MAC
suffix now doubles it. The deleted `satellite1.yaml` set no explicit name, so the device
was `satellite1` + suffix.

Live consequences, confirmed against `/api/outputs`: the daemon lists **both**
`sendspin-dev-satellite1_c4150c` and `sendspin-dev-satellite1_c4150c_c4150c` for
192.168.178.99. The old (stale-mDNS-cached) entry holds the working connection and is the
sync-group member; the new one is unrouted, carries a phantom idle Opus relay burning
~500 blocks/10 s of encode, and re-dials ~0.5×/min with occasional
`Connection reset without closing handshake`. The host absorbs it (87.5 % idle).

**The risk is deferred, and it is a landmine:** on the next daemon restart the cached old
entry disappears and the sync group loses satellite1 until the new name is adopted. Fix
is one line in the untracked config — drop the explicit `name:`, or set
`name_add_mac_suffix: false` — plus a reflash. Left for the owner, since the file is
theirs and mid-edit.

#### Migration assessment: **do not do it for this symptom**

Verified against v0.7.0 sources, not release notes: `time_burst.h` still has
`burst_size_{8}`, `DEFAULT_RESPONSE_TIMEOUT_MS = 10000`,
`DEFAULT_BURST_INTERVAL_MS = 10000`; a timed-out message is still *skipped, never
retransmitted*; `is_time_synced()` is still `time_filter_->has_update()`, with
`sync_task.cpp`'s `handle_load_chunk()` decoding nothing while false. **And it is worse
than the pinned snapshot in one respect:** `time_filter->update()` runs only at burst
*completion*, so all 8 slots must resolve — worst case **8 × 10 s ≈ 80 s** of gated
silence per reconnect. v0.7.0 does newly expose `time_burst_size` /
`time_burst_interval_ms` / `time_burst_response_timeout_ms` in `SendspinClientConfig`, but
**ESPHome does not surface them in YAML**, so tuning needs an upstream PR or a patched
component.

The dominant cost is **the version ladder, not sendspin**: the component shipped in
ESPHome **2026.5.0**, **2026.7.0 pins sendspin-cpp 0.6.1**, and **0.7.0 is only on `dev`**
(esphome#17781, so ≥2026.8). The box runs **2026.4.5**, so getting 0.7.0 means moving the
*entire* Satellite1 config forward — XMOS flasher, `fusb302b`, `tas2780`,
`satellite1_radar`, the `http_request` pin and a local TAS2780 boot patch. Docs still
label the component experimental.

On the plus side the YAML mostly *shrinks*: the whole `external_components:` block and
`http_request:` go away, and `const` / `media_source` / `speaker_source` already exist
natively in 2026.4.5 — so the current fork pin is overriding two components for no
reason. Behavioural changes to absorb: repeat/shuffle move to controller state (#45),
`stream/clear` handled in-library (#54), `static_delay_ms` now reports the *effective*
value and 0 when not adjustable (#29 — directly relevant to §4.10/§4.15), `client_id`
defaults to the interface MAC with a `mac_address` override (#65).

### 4.13 Our Rust server checked against the spec — audit done ◐

The daemon's sendspin server was written by converting a Python library's behaviour, not
by reading the protocol specification — so conformance was unverified by construction,
and a non-conformant server was a live candidate for firmware-side symptoms we had been
attributing to the firmware.

> **Audit done 2026-07-29 against `Sendspin/spec` HEAD `aa752f6`.** That suspicion was
> exactly right: the audit found the root cause (**§4.14**, fixed). Five further
> deviations were found and each was checked against the *pinned firmware* rather than
> assumed — all are currently **inert**, so they are correctness debt, not live bugs:
>
> | | Deviation | Live impact today |
> |---|---|---|
> | F3 | `server_transmitted` missing from `stream/start` / `stream/end` / `stream/clear` (spec makes it non-optional, and `roles/player/v1.md` defines the lead budget against it) | none — this firmware only requires it on `server/time` |
> | F4 | `ClientSyncState` has no `Error` variant and no `#[serde(other)]`, so a `client/state` with `state: "error"` fails to parse and the **whole message** is discarded — volume, mute and static-delay deltas with it | none yet, **but** it would silently discard sendspin-cpp #67 "report error state when out of sync", which §4.12 lists as the upgrade's headline observability win |
> | F5 | We add a client to `ready` for *any* `client/state`, so an `external_source` device is streamed to anyway and never gets `stream/end` (spec: MUST NOT) | none — these devices report `synchronized` |
> | F7 | `buffer_capacity` is parsed and never read; our `MAX_QUEUED_AUDIO_FRAMES = 32` is a local invention where the spec supplies a negotiated number | none — the Voice PE advertises far more than we use |
> | F8 | `required_lead_time_ms` excluded from the send-ahead — correct against spec HEAD, a violation against the era spec | none — this firmware reports neither field; **becomes live on upgrade** |
>
> Also checked and **compliant**: `stream/end` used only for genuine termination, chunk
> duration bounds (≤150 ms / ≥15 ms) including Opus re-blocking, late joiners getting
> future timestamps with no timeline re-anchor, `server/time` stamped immediately before
> the write, handshake ordering, `active_roles`, `connection_reason`, PCM endianness and
> the audio frame layout.
>
> One thing the audit *cleared*: our readiness gate keys on the **arrival** of
> `client/state`, not its value, which is what the spec licenses. It is only the log
> string that implied convergence — and that phrasing is what sent §2c down the
> convergence path. (On the pinned firmware `synchronized` is a compile-time default
> published unconditionally at handshake, so it asserts nothing about the clock.)
>
> F4 is worth fixing before §4.12, since it would silently eat the signal that upgrade is
> being bought for.

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

### 4.14 ROOT CAUSE: our server silently dropped the client's clock-sync requests ✅

**This is the answer.** It was ours, not the firmware's, and every hypothesis in this
document looked in the wrong direction because the one instrument we trusted could not
see it. Found by auditing our Rust server against
[Sendspin/spec](https://github.com/Sendspin/spec) (§4.13) after §2d had already measured
the symptom precisely enough to check the arithmetic against.

**What the spec says.** Every revision, including all our server could have been written
against: *"The frequency of these messages is determined by the client based on network
conditions and clock stability"* and *"Once received, the server responds with a
`server/time` message."* The cadence is the client's choice; a reply to each is the
server's obligation. The spec grants a server **no** licence to rate-limit, coalesce or
ignore `client/time` — the only such licence anywhere is for a player's *timing updates*
in `client/state`, a different message. The reference implementation the spec points at
sends **8 back-to-back, each awaiting its reply**.

**What we did.** Two independent defects, both of which had to be fixed:

1. `MIN_TIME_REPLY_INTERVAL_US = 50_000` in `writer.rs`, with `connection.rs` doing a
   bare `continue` — **no reply at all** — for any request arriving within 50 ms of the
   previous one. Its comment justified this with *"the spec's cadence is about one
   `client/time` per second"*, which the spec has never said. Not present in upstream
   `Sendspin/sendspin-rs` (0 hits): our fork's invention.
2. The reply travelled a **single-slot `watch` channel**, documented as *"a correctness
   property, not an optimisation"* on the grounds that a reply derived from the latest
   request makes earlier ones redundant. Same mistake in a second place: the client
   matches each reply to its request by `client_transmitted` and derives that
   measurement's `max_error` from that specific round trip, so a superseded request is
   not answered redundantly — it is **not answered at all**.

**Why it cost tens of seconds.** Every link verified in the firmware source on disk:

```
our 50 ms drop / single-slot coalesce
  → client is one-request-in-flight, RESPONSE_TIMEOUT_MS = 10000, NO retransmit
    → each unanswered request costs the client a full 10 s
      → time_filter->update() runs only when the whole 8-message burst completes
        → is_time_synced() == has_update() == count_ >= 1  → false until then
          → sync_handle_load_chunk_ returns early, decoding NOTHING
            → total silence for k x 10 s
              → init_time_filter() runs per connection, so every reconnect pays again
```

The arithmetic reproduces §2d result 4 quantitatively. With an answered fraction `f`,
seconds per *logged* exchange ≈ `(1/f − 1)·10 + 10/(8f)`:

| f | predicted s/exchange | measured (§2d) |
|---|---|---|
| 0.80 | 4.06 | 093ca8 **4.06**, 096287 **4.10** |
| 0.72 | 5.2 | satellite1 **5.1** |
| 0.66 | 7.1 | 0966f3 **7.1** |

i.e. 1.6–2.7 dropped requests per burst → a first burst of 16–27 s. **The per-device
spread needs no WiFi explanation**: it is each device's main-loop turnaround straddling
our 50 ms threshold differently. That also explains the intermittency, and why the
symptom never depended on how the previous session ended (§2c).

**Independent confirmation, from outside the code (§4.12's phase-1 measurement).** Across
**18** reconnect samples on four devices, every single one landed on
**`k × 10 s + 1.0–1.9 s`** with k ∈ {2,3,4,5,6}, with steady state occasionally reaching
12.2 s (k=1, the floor). **Zero exceptions.** WiFi loss would give a continuous
distribution; quantisation to exact 10 s steps is a *discrete count of 10 s response
timeouts* — unanswered requests, counted. That measurement was taken before this fix was
deployed and without reference to the code, so it is genuinely independent of the audit
that found the cause.

**Why our instrumentation could not see it (§4.8's blind spot).** The
`time exchange(s)` counter incremented *after* the drop check, so it counted only the
requests we chose to answer. §2d's "0.2/s observed vs 0.8/s intended, a 4×
discrepancy" was therefore measuring **our own replies**, and the gap was our drops —
not the packet loss it was attributed to. A counter placed one line earlier would have
ended this in an afternoon.

**Fixed.** In the submodule:

- `MIN_TIME_REPLY_INTERVAL_US` and the silent-drop branch are **gone**; every
  `client/time` is answered.
- The time lane is a **bounded queue** (`MAX_QUEUED_TIME_REPLIES = 32`) instead of a
  single slot. The writer still takes exactly **one reply per loop pass**, so the
  starvation concern the old comment raised is handled by bounding the queue rather than
  collapsing it — the time lane still cannot hold more than one frame's worth of
  priority over audio.
- Overflow is a **logged WARN**, never a silent drop, and does not increment `replied`.
- The log now prints **received and replied**: `N time exchange(s) (received M)`. They
  should stay equal; a divergence is now visible in one line instead of requiring a code
  read to rule out.

Regression guard: `tests/server_listener.rs::every_client_time_is_answered_even_back_to_back`
sends 8 requests with no pacing and asserts 8 replies, each echoing its own
`client_transmitted`. **It failed against the rate-limit removal alone** — which is how
the single-slot coalescing (defect 2) was caught, after it had been reasoned away as
harmless on the grounds that the real client is one-in-flight. Worth remembering: the
real client's politeness was hiding half the bug.

`cargo test`: submodule **312 passed / 0 failed**, daemon **129 passed / 0 failed**,
no new warnings.

#### Confirmed on hardware (2026-07-29, `0.3.20260728232459`) ✅

The owner reports reconnecting is now **fast**. The log agrees, and the prediction was
exact:

| | before (§2d / §4.12) | after |
|---|---|---|
| 10 exchanges after a reconnect | 40.6 / 41.0 / 50.6 / 70.7 s | **10.1 / 10.2 / 10.4 / 10.5 / 10.6 s** |
| effective cadence | 0.14–0.25 /s | **~0.93 /s** |
| per-device spread | 1.7× | **gone** (10.1–10.6 s across all four) |
| `replied` vs `received` | not measurable | **equal in every sample, 0 divergences** |
| `time-reply queue full` warnings | — | **0** |

Three things worth drawing out:

1. **~0.93 exchanges/s is the firmware's intended cadence**, not merely "better":
   `BURST_SIZE = 8` every `BURST_INTERVAL_MS = 10000` predicts 10 exchanges per ~10.7 s,
   and that is what the log shows. The client was never slow; we were not answering it.
2. **The per-device spread vanished.** That spread (1.7×) was the whole reason this looked
   like a per-device firmware bug — it was each device's main-loop turnaround straddling
   our 50 ms window differently. Remove the window and four different devices become
   indistinguishable. That is the cleanest possible confirmation of the mechanism.
3. **The `k × 10 s` quantisation is gone.** §4.12's 18-for-18 quantised samples were a
   count of 10 s response timeouts; there are now none to count.

So §4.14 is closed end to end: spec → code → arithmetic → prediction → measurement.

### 4.15 Next: stop reconnecting for a config change at all ⬜

§4.13's F2. The spec's `stream/start` explicitly supports re-configuring a live stream:
*"If sent for a role that already has an active stream, **updates the stream
configuration without clearing buffers**."* Nothing in the protocol asks a server to drop
the WebSocket to change codec, format or lead.

Our only `stream/start` path is `Group::add_member` on a freshly-dialled connection, so
every config change becomes a reconnect. With §4.14 fixed a reconnect is cheap again, so
this is no longer urgent — but it is still the right shape, and it would reduce §4.10's
remaining single-device gap to zero. Sequence it after §4.14 is confirmed on hardware, so
the two changes are measured separately.

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
- `sync_group::tests::a_static_delay_change_within_the_running_lead_touches_only_that_device`
  (§4.10): composes the real `required_send_ahead_us` for a two-member Opus group and
  shows that giving one member 40 ms leaves the group's send-ahead — and therefore
  `sendspin_config_changed` and `ServerAction` — untouched, so the server keeps running;
  plus that `force_device_reconnect` marks that device and *only* that device. Also pins
  §4.6's one-way rule from this angle: a delay *reduction* can't re-arm anything either.
- `sync_group::tests::a_static_delay_change_that_raises_the_group_lead_re_arms_every_member`
  (§4.10, the other side): 250 ms on the same member pushes its requirement past the
  group floor, so the config *has* changed and every member must re-arm — the guard that
  stops the scoping above from being applied too eagerly.
- `sync_group::tests::a_forced_reconnect_is_scoped_to_one_device_and_one_group`: a delay
  edit doesn't disturb a co-existing group (different source-set, own anchor and server),
  and a device no running group has returns false rather than silently marking something.
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
