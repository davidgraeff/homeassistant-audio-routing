# Adding a device silences the whole group — analysis plan

**Symptom (reported 2026-07-28).** With a source playing to AP2 **and** sendspin
outputs, routing one *more* sendspin device into the group makes **every sendspin
device stop**, and it takes **>10 s** before audio is back everywhere.

Nothing here is a mystery of the "we don't know what it does" kind — the reconciler
is *designed* to restart a group's server when its membership changes. This plan is
about measuring where the 10 s actually goes and deciding which parts of that restart
are avoidable, because the current design pays the full price for the cheapest
possible change.

Read [architecture.md §4](architecture.md#4-the-anchor--per-device-sender-model) and
[§5.1](architecture.md#51-sendspin-esphome-speakers-eg-ha-voice-pe) first.

---

## 1. Why it happens at all (the known part)

`sync_group.rs::reconcile` treats the **member set as part of the server's restart
identity**:

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

**H6 — the outage is device-side recovery from an un-announced disconnect.** *Now the
hypothesis that matters*, because §2b leaves ~9 s unaccounted for. A restart tells the
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

## 4. Candidate fixes, in the order they should be considered

Reordered after §2b: the daemon-side restart is ~800 ms, so H1 alone cannot be the answer
to a 10 s report. Confirm where the other 9 s live before spending the H1 refactor on it.

0. **Find the missing 9 s** (H6). Until the device side is measured, everything below is
   optimising a second out of ten. Cheapest first step: make teardown graceful
   (`broadcast_stream_end()` + a real WS close before dropping the manager) and re-measure
   the same add. That is a small, self-contained change and it is a prerequisite for H1's
   member-removal path anyway.
1. **Stop unknown-capability members from downgrading the group** (H2). Structural, cheap,
   and it removes a *guaranteed* double restart — including the PCM downgrade that hits
   every existing member of an Opus group on the first route after a deploy.
2. **Narrow the restart identity to the stream config** (H1). Still the right shape, and the
   pieces already exist — but it is worth ~800 ms, so schedule it on that basis. Care needed
   on: dropping a departed member's `Group` and calling `stop_client`; making sure
   `client_to_node` / `pending` / `ready` entries are cleaned per device rather than per
   server; and keeping `supervise` the only path that learns new addresses.
3. **Coalesce membership growth** (H3) — not observed in this log; do it only if a
   multi-device route shows it. It would also reduce the number of `stream/start`s a device
   sees during such a route.
4. **Don't bother** shaving the per-device reconnect: dial+WS was 150 ms and `client/state`
   6–24 ms. Smallest term, riskiest place to fiddle, and now measured as noise.

Two latent issues found while reading, neither implicated in this event, both worth a note:

- **A failed restart has no retry.** The reconcile task is purely event-driven — a change
  notification with a 400 ms debounce, no periodic tick (`main.rs:359-372`). If
  `start_server_per_device` fails, `rg.server` stays `None` and the group has **no** sendspin
  output until some unrelated change happens to fire a notification.
- **The restart re-binds the same port immediately** after `abort()`ing the task that owns
  the old listener, and abort is not synchronous. `SO_REUSEADDR` does not permit two live
  listeners, so there is a race window whose loss mode is the bullet above. It has never
  appeared in the log (no sendspin `failed to bind` in the full history), so this is a
  latent risk, not a diagnosis.

Explicitly **not** a fix: lowering the send-ahead to shorten the refill. The floor is
protocol- and hardware-driven (see
[architecture.md §8](architecture.md#8-sample-rate-harmonization)); trading it away to
mask a restart would reintroduce the stutter class we just closed.

---

## 5. Regression guards to add with the fix

- A unit test that a membership-only change does **not** change the restart identity,
  and that a codec/lead change does.
- A unit test that a member with **no learned capabilities** does not pull the group's
  resolved codec/send-ahead below what the known members support (H2). This is the one that
  would have caught the 15:30 double restart.
- Keep the phase log lines from §2 — they are the only reason this is measurable at all.
  A future "adding a speaker is slow" report should be answerable from one log grep.
