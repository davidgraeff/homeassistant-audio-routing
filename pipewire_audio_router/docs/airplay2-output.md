# AirPlay-2 output — what shipped, and what is left

The AirPlay-1 / RAOP **output** path has been replaced by an in-process Rust
**AirPlay-2 sender**. The build-out is **done and deployed**; this file records
what that delivered, what hardening was done or deliberately declined, and the
handful of items still open. The how and why live elsewhere:

- **How it is built** (the per-device sender model, the two clocks, PTP,
  sample-rate negotiation, the real-time thread ladder, reliability/liveness, the
  full AirPlay-in → Voice PE + AP2 flow): [architecture.md](architecture.md).
- **Why** (hard-drop RAOP + the node-name migration, GPL-2.0 vendoring, libairptp
  in-daemon and its two load-bearing patches, codec choice, live render delay,
  mDNS and RT decisions): [decisions.md](decisions.md).
- **How to poke a running instance** (the spike A/B oracle, PTP-lock health, chrt
  inventory, mDNS/CPU diagnosis): [live-instance-debugging.md](live-instance-debugging.md).

Older companion write-ups (superseded, kept as evidence) are in [`old/`](old/),
including the per-track specs `ap2-track-p{1,2,3,4}-*.md` — P4 is the RAOP-drop
design.

---

## 1. What shipped, and what it proved

Everything in phases 0–6: discovery over `_airplay._tcp`, the host-global PTP
grandmaster, transient pairing (PIN 3939), realtime ALAC (type 96), ChaCha20,
PT=87 anchors, synchronized multi-room playback, and the full control
plane — per-device volume/mute, live render delay, the per-group wire rate
(44.1 kHz, with 48 kHz opt-in per device — §5), announcement-group duck+announce,
HA `media_player` adoption.
Reliability came with it: liveness/demotion, reconcile debounce, pairing retry,
and an awaited-TEARDOWN graceful shutdown. RAOP was then **hard-removed**, with a
one-shot node-name migration so existing routing and groups survived.

**Proven on hardware:** Yamaha WX-021 + Pioneer VSX-934 on a Raspberry Pi 4 that
the host assessment rates "marginal"; the user's verdict was "better than
AirPlay 1."

Two findings from that work outlived it and are recorded in
[decisions.md](decisions.md): the **render delay must be retuned live**, not by
reconnect (a reconnect "succeeded" on the wire while a flaky receiver silently
stopped rendering, taking the play-tone and announce paths down with it), and
**a live gPTP clock is required for rendering** — PT=87 anchors alone are not
enough, which is what made libairptp's `SCHED_FIFO` 55 and its removed staleness
send-gate load-bearing patches rather than tuning. "Lock" there means *our Sync is
reaching the receiver*, not *the receiver is answering*: some never do (§5).

---

## 2. The open acceptance gate: daily-driver validation

RAOP is already gone, so this is acceptance rather than a pre-removal gate. Run
the real speakers through AP2 as the daily driver and confirm, by ear and on
hardware:

- Initial-session reliability (no manual reconnect), no dropouts, multi-room sync.
- A receiver going offline and back demotes and recovers cleanly (the liveness
  task).
- **Rate**: a device runs at 44.1 kHz by default; opting one into **48 kHz** (and back)
  restarts its group at the new rate with no audible break, and any 44.1 kHz member
  holds the whole group at 44.1 kHz. A receiver that *rejects* 48 kHz at SETUP is capped
  automatically; one that accepts it and then goes quiet is caught by the keepalive
  (§5) — confirm both by ear on the WX-021, which is the device that does the latter.
- **Announcements** play and duck — including on a receiver with **no input routed
  to it**, which opens an on-demand session for the clip and hands it back after a
  30 s lease
  ([architecture.md §5.5](architecture.md#55-announcing-to-an-output-with-nothing-routed-into-it)).
  Confirm three things by ear: a tone on an unrouted receiver (audible a few
  seconds later — the pair + render-delay cost), a second tone within the lease
  (fast, warm session), and that the receiver frees its AirPlay input afterwards
  so a phone can connect.
- **Alignment**: soloing two receivers and sweeping one's render delay brings them
  into coincidence (now on the Outputs page's alignment card, not the source card
  — see [mic-alignment-plan.md](mic-alignment-plan.md) §12.1).
- Routing and grouping survive an add-on restart, with no dangling links. The
  `store/migration.rs` shim was a verified no-op on the live store; it is kept for
  safety and can be deleted after one clean boot.
- Then tune the render delay down from 1500 ms as far as the live feed allows.

---

## 3. Productionize backlog

Small, all still open, all verified present in the tree as of 2026-08-12:

- Remove `outputs/ap2/server.rs`'s `#![allow(dead_code)]` (and `ptp.rs`'s), and use
  **discovered** features instead of the fixed sender-side features string
  (`Features::from_txt_value("0x4A7FCA00,0x3C354BD0")`).
- Revert the widened `airplay_audio` / `airplay_client` `EnvFilter` targets in
  `main.rs`, which were raised for debugging.
- Excise the dead Rust-PTP code in the vendored `airplay2-sender` crate — PTP is
  driven by libairptp, so that code cannot run.
- Add tests / CI for the AP2 path. (The per-brand quirk table is now in §5 — keep it fed.)
- Fold the vendored `with_daemon` mDNS patches and the libairptp patches into the
  `pull_request_docs/` upstream mirrors, per the vendoring convention.

### The PTP-lock signal was measuring the wrong thing ✅ (2026-08-21)

The lock watchdog below shipped on a signal that could not carry the meaning it was
given, and on an x86 host it turned into a **two-minute reconnect loop** against a
receiver that was playing fine — ~1.4 s of silence each time (1 s teardown budget +
~0.35 s reconnect), nine times before it was caught.

Two facts, both measured on the wire rather than inferred:

- **libairptp seeds `peer->last_seen` with `time(NULL)` in `daemon_peer_add`.** So
  `peer_lock_age` answered *"how long since we registered this peer"*, not *"how long
  since the receiver spoke to us"*. A receiver that has never sent one gPTP packet read
  as "locked 0 s ago" immediately after connect and "lock lost" 30 s later — and the
  watchdog's own `remove_peer` + `add_peer` re-seeded the timestamp, so every recovery
  attempt manufactured the evidence that armed the next one. `ptp::heard_age` now
  discounts anything not newer than the add, with the trap written down in
  [live-instance-debugging.md](live-instance-debugging.md#ptp-lock-health).
- **Receivers come in two gPTP classes** (90 s, both streaming in one group): a Pioneer
  VSX-934 (`PTPInfo: SUE PTP 1.0`) sent **717** `Delay_Req`, ~8/s, matching our Sync
  rate; a Yamaha WX-021 (`PTPInfo: AirPlay;2.0.4`) sent **zero** and rendered perfectly
  throughout. One-way receivers slave to Sync/Follow_Up and never answer, so they have
  no observable lock and nothing to recover.

So the watchdog is now gated on `PeerPtp::two_way` — *we have genuinely heard gPTP from
this device* — which the recovery path cannot fabricate (`remove_peer` keeps the class,
`forget_peer` drops it only when the device leaves). The Pioneer keeps its watchdog; the
Yamaha is left alone. `/api/outputs` gained `ptp_responsive` so the badge can say "PTP
one-way" plainly instead of alarming, and the Diagnostics table with it.

### Lost PTP lock is recoverable per receiver ✅ (2026-08-12)

A receiver that PTP-locks and then loses the lock renders **nothing**: our PT=87 anchors
are timestamps in the grandmaster's timeline, and a clock that has drifted off it cannot
place them. Nothing in the sender noticed — the group task connected once and then only
applied volume — so the only recoveries were restarting the add-on or power-cycling the
receiver. Both work for the same reason: they build a new session. Observed repeatedly on
the Pioneer VSX-934; the Yamaha WX-021 never locks at all and plays fine, which is the
whole reason the check has to be about runtime state rather than advertised capability.

(The paragraph above says the Yamaha "never locks at all" — read that as *never answers*,
per the 2026-08-21 section; it was the right instinct on a signal that could not carry it.)

Three pieces, all per receiver — its groupmates keep streaming throughout:

- **`Ap2Command::Reconnect`** and the group task's `reconnect_member`: drop that member's
  feed, release its session (bounded FLUSH + TEARDOWN, then close), `remove_peer` +
  `add_peer` on the libairptp grandmaster so its Announce/Sync sequence restarts, then a
  fresh `connect_one` through the same retry path as an initial connect. `attach_member`
  is shared with the connect loop, so a reconnected receiver lands in exactly the state a
  first connect leaves it in.
- **`POST /api/ap2/resync`** and a **Resync** button on each present AirPlay-2 output —
  the same button the sendspin outputs have, for the same symptom, sending what that
  transport needs.
- **A watchdog in `outputs/ap2/liveness.rs`**, on the existing 12 s tick: a receiver that
  *was* locked (`peer_lock_age ≤ 5 s` at some point this session), still has a live sender,
  and has gone quiet for ≥ 30 s gets its session rebuilt — at most one attempt every two
  minutes, with the reason published to `ap2_health` so the UI says what happened. A
  receiver that never locked is never touched. Decision extracted as `ptp_recovery_due`
  and unit-tested, since every clause exists to stop it firing on something healthy.

Not yet validated on hardware: the automatic path needs a real lock loss to fire, which is
not something we can provoke on demand. The manual button is the fallback if it does not.

### The one real defect: shutdown during a group's initial connect does not TEARDOWN

`Ap2ServerHandle::shutdown` — the graceful-exit path
([architecture.md §5.2](architecture.md#52-airplay-2-av-receivers-eg-yamaha-wx-021-pioneer-vsx-934))
— signals the group task and awaits it for ~3 s, but the task only observes that
signal *after* its sequential `connect_one` loop, and each member costs up to
`AP2_CONNECT_TIMEOUT × AP2_CONNECT_ATTEMPTS` plus backoff. So a SIGTERM landing
while a group is still coming up times out and exits without closing those
sessions — leaving exactly the stale session the shutdown path exists to avoid,
after which the next start's first connect fails and retries.

That makes it a *missed improvement* rather than a regression (it is the behaviour
that existed before the shutdown path). The fix is to make the connect loop
cancellable: `tokio::select!` the shutdown receiver against each `connect_one`, and
TEARDOWN whatever is already connected. Deliberately not done blind — it touches
the proven-on-hardware connect path, so it wants a live re-validation of
initial-session reliability and the pairing retry alongside it.

---

## 4. Real-time and allocation hardening — done, and deliberately not done

The AP2 data path is SCHED_FIFO end to end (the ladder is in
[architecture.md §7](architecture.md#7-real-time-thread-ladder)) and the steady hot
path is now allocation-free. Two allocations were removed:

- **The relay's forward loop** (`outputs/ap2/server.rs`) fans out with zero
  steady-state allocation: each sender hands back a recycled `Vec<i16>` via
  `LiveFrameSender::take_buffer()` (a bounded free-list; the decoder returns
  drained buffers), captured PCM is a pooled buffer, and the overlay `mix_buf` is
  reused across chunks and devices.
- **The vendored streamer's per-packet PCM clone** is gone: `run_streamer` encodes
  straight from the frame's shared `Arc<Vec<i16>>` on the common no-EQ path,
  removing a ~1.4 KB/packet clone. (Only the EQ path, unused here, still needs an
  owned copy.)

Three things were considered and **declined**, which is worth recording so they
are not re-litigated as oversights:

- **Reusing the streamer's `wire_packets`.** Each packet still allocates the outer
  `Vec<Vec<u8>>` (length 1 per per-device connection) and the encrypted RTP
  `Vec<u8>` from `prepare_audio`. Reusing them needs a sender→producer recycle
  channel *and* an encrypt-into-buffer change to `prepare_audio` — a deeper change
  to a proven encrypt path for a few hundred bytes per packet now that the dominant
  clone is gone. Not worth the RT-path risk.
- **A structural producer fuse** (encode on the sender thread, no channel). The
  `crossbeam bounded(8)` split is kept **on purpose** for backpressure and is
  sufficient.
- **CPU affinity / host isolation.** Pinning is not required with the relay and
  sender both at SCHED_FIFO; revisit only if a stall traces to CPU contention.
  WiFi power-save is disabled per-stream by the vendored sender; a host-wide
  disable is the more robust form if it ever matters.

---

## 5. Per-brand interop and quirks

**Interop is won device by device.** Proven on Yamaha + Pioneer only. Validate each
brand before trusting the "all AP2-capable" premise, and add to this table as you go.

Both units are `AirTunes/366.0`, both advertise the identical
`features=0x445F8A00,0x1C340`, and both set `keepAliveLowPower` +
`keepAliveSendStatsAsBody` — so **nothing they advertise separates them.** Everything
below had to be measured.

| | Yamaha WX-021 "Dusche" | Pioneer VSX-934 |
|---|---|---|
| `PTPInfo` / `sdk` | `AirPlay;2.0.4` | `SUE PTP 1.0` / `AirPlay;2.0.2` |
| gPTP direction | **one-way** — 0 `Delay_Req` in 90 s while playing | **two-way** — 717 in 90 s (~8/s) |
| `ptp_locked` when healthy | permanently `false` (nothing to observe) | `true`, age ~0 |
| PTP watchdog | never armed (`ptp_responsive: false`) | armed — a real lost lock is recoverable |
| `audioFormats` in `/info` | **absent** | **absent** |
| 48 kHz realtime ALAC | **accepts the SETUP, then stops rendering after seconds** | ran at 48 kHz without complaint |
| TEARDOWN on reconnect | routinely misses the 1 s budget | answers promptly |
| Firmware measured | 2.06 (`Dec 4 2023`), Linux 3.10.100 | `1.08/21609ALP` |
| Other | — | AirTunes can wedge until a **mains** power cycle ([probe.rs](../bridge-daemon/src/outputs/ap2/probe.rs)) |

Two consequences are now baked into the code:

- **Sample rate is 44.1 kHz by default and 48 kHz is opt-in per device**
  (`Ap2RateMode::Hz44100` / `Try48000`). It cannot be negotiated: neither receiver
  publishes an `audioFormats` capability list, and the mDNS `features` bitmask says
  nothing about rates — so the only way to learn is to try, and the failure is silent.
  The old `Auto` mode guessed 48 kHz and only fell back on a *SETUP rejection*, which
  the WX-021 never issues: it accepted every 48 kHz SETUP (log-confirmed) and then went
  quiet. Guessing costs a silent speaker the user has to diagnose; the upside was one
  skipped resample in our graph.
- **A `POST /feedback` keepalive every 2 s per session**, which both receivers ask for
  and we sent none of. It doubles as the only detector of a session dying under us — the
  RTSP socket is write-only after RECORD, and the liveness probe opens its *own*
  connection, so it happily reports a receiver that has dropped our session as fine.
  Two consecutive failures rebuild that member's session; if the group was on 48 kHz by
  opt-in, it caps the receiver at 44.1 kHz instead, since a reconnect at the same rate
  would fail the same way. **Limit worth knowing:** this catches the session *dying*.
  A receiver that keeps the session and merely stops rendering is still invisible —
  with `keepAliveSendStatsAsBody: true` the receiver's feedback *body* is the place that
  would show it, and the vendored client discards it today.
- **Mixed sendspin + AP2 group coincidence.** The two backends reference different
  clocks (sendspin's `CLOCK_MONOTONIC_RAW` send-ahead vs AP2's libairptp
  `CLOCK_MONOTONIC` + PT=87 anchors). Pure-AP2 and pure-sendspin groups are fine;
  cross-protocol *acoustic* coincidence is an extra alignment task — deferred, and
  now measurable with the alignment feature rather than by ear.
- **Converge AP2 onto `SharedTimeline` + `OutputBackend`.** The shipped path uses
  independent per-device `Connection`s sharing the grandmaster `clock_id` plus
  identical PCM, which is sufficient for sync but is not the stamp-once seam the
  other backends use
  ([architecture.md](architecture.md#the-outputbackend-seam-target-end-state)).
  Sequence it after the sendspin per-device-sender overhaul and MG/AG routing-target
  polymorphism stabilise, rather than building a parallel AP2 abstraction.
- **Codec, optional.** Realtime ALAC (type 96) is accepted by the test devices and
  needs no `SETRATEANCHORTIME`. Buffered AAC (type 103) remains a later option,
  not a requirement.
