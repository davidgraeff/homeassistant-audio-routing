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

## 3. Hypotheses, most likely first

**H1 — the restart is unnecessary for a pure membership change.** The vendored crate
now has everything needed to add a member to a *running* server: `ClientManager::
supervise(fullname, url)` (already called on every reconcile, idempotent) and
`Group::with_timeline(...)` + `add_member` on the shared `SharedTimeline`, which
explicitly supports mid-stream joins (the server-role PR demonstrates it, and the spec
requires late joiners to get future timestamps). Nothing about a new device forces the
existing devices' streams to end.

*Test:* stop treating `sendspin_node_names` as restart identity; restart only when the
**stream config** changes. Concretely, the identity should be
`(codec, send_ahead_us, codec_header)` — everything a `stream/start` carries — because
that is what the timeline fixes at construction. Membership becomes: supervise the new
fullnames, `stop_client` the departed ones.

**H2 — a new device legitimately changes the stream config, so a restart is correct but
should be rarer.** Both the codec (`resolve_codec` intersects across members: a new
device without Opus forces the whole group to PCM) and the send-ahead floor
(`required_send_ahead_us` takes the max across members) depend on membership. This is
*by design* and unavoidable — but it means H1's win only materialises when the new
device doesn't move either value. Worth knowing how often that's true in practice: a
second identical Voice PE changes neither.

*Test:* log the old→new `(codec, send-ahead)` on every restart and see whether the
observed restarts were config changes or pure membership churn.

**H3 — the sequence of member sets multiplies the cost.** If mDNS resolution or the
`url`-present gate (`compute_desired` skips devices whose URL hasn't resolved) admits
devices one at a time, we restart per device. Even with H1 fixed this matters for the
*first* route of several devices.

*Test:* count restarts per action (§2.1). *Fix candidate:* hold a newly-desired member
out of the identity until its URL is known (already the case) **and** widen the
reconcile debounce for membership-only growth, or settle-wait ~500 ms after the first
new member appears.

**H4 — AP2 is dragged in.** If AP2 senders restart when only sendspin membership
changed, the 10 s is mostly AP2 pairing. The identity check should already prevent it
(`ap2_identity` is AP2 node names only), but the *anchor* is shared: if the anchor node
were recreated the AP2 capture would break too. Confirm the anchor survives (its
`node_id` should be unchanged across the event).

**H5 — the readiness gate adds a fixed 3 s.** Only if devices are slow to report
`client/state` after a reconnect. Measured ~30 ms on healthy firmware; falsify with the
§2 table before touching `READY_GRACE`.

---

## 4. Candidate fixes, in the order they should be considered

1. **Narrow the restart identity to the stream config** (H1). Biggest win, and the
   pieces already exist. Care needed on: dropping a departed member's `Group` and
   calling `stop_client`; making sure `client_to_node` / `pending` / `ready` entries are
   cleaned per device rather than per server; and keeping `supervise` the only path that
   learns new addresses.
2. **Coalesce membership growth** (H3) — cheap, and it also reduces the number of
   `stream/start`s a device sees during a multi-device route.
3. **Only then** look at shaving the per-device reconnect (H5, dial/handshake time).
   This is the smallest term and the riskiest place to fiddle.

Explicitly **not** a fix: lowering the send-ahead to shorten the refill. The floor is
protocol- and hardware-driven (see
[architecture.md §8](architecture.md#8-sample-rate-harmonization)); trading it away to
mask a restart would reintroduce the stutter class we just closed.

---

## 5. Regression guards to add with the fix

- A unit test that a membership-only change does **not** change the restart identity,
  and that a codec/lead change does.
- Keep the phase log lines from §2 — they are the only reason this is measurable at all.
  A future "adding a speaker is slow" report should be answerable from one log grep.
