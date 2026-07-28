# AirPlay-2 output — roadmap

The AirPlay-1 / RAOP **output** path has been replaced by an in-process Rust
**AirPlay-2 sender**. The build-out (Phases 0–6) is **done and deployed**; this
file now tracks only **what remains**. The how and why live elsewhere:

- **How it's built** (per-device sender model, the two clocks, PTP, sample-rate
  negotiation, the real-time thread ladder, reliability/liveness, the full
  AirPlay-in → Voice PE + AP2 flow): [architecture.md](architecture.md).
- **Why** (hard-drop RAOP + the node-name migration, GPL-2.0 vendoring,
  libairptp in-daemon + its patches, codec choice, live render delay,
  mDNS/RT decisions): [decisions.md](decisions.md).
- **How to poke a running instance** (spike A/B oracle, PTP-lock health, chrt
  inventory, mDNS/CPU diagnosis): [live-instance-debugging.md](live-instance-debugging.md).

Older companion write-ups (superseded, kept for evidence) live in
[`old/`](old/). Per-track specs: `ap2-track-p{1,2,3,4}-*.md` (P4 =
[the RAOP-drop plan](ap2-track-p4-drop-raop-design.md)).

---

## Status

Phases 0–6 **done**. Discovery, the host-global PTP grandmaster, transient
pairing (PIN 3939), realtime ALAC (type 96, 48 kHz confirmed by ear),
ChaCha20, PT=87 anchors, synchronized multi-room playback, the full control
plane (per-device volume/mute, live render delay, per-group 48 kHz/44.1 kHz
negotiation, AG duck+announce, HA `media_player` adoption), reliability
(liveness/demotion, reconcile debounce, pairing retry), and the **hard RAOP
removal** (+ Align-page AP2 alignment) are all shipped. **Proven on hardware**:
Yamaha WX-021 + Pioneer VSX-934 on a "marginal"-verdict Raspberry Pi 4; user
verdict "better than AirPlay 1." Mechanisms: [architecture.md](architecture.md).

The **one open acceptance gate** is daily-driver confirmation (below); the rest
is polish/hardening and forward-looking design.

---

## Remaining work

### Daily-driver validation (the open acceptance gate)

Run the real speakers through AP2 as the daily driver and confirm on hardware.
RAOP is already removed, so this is now acceptance, not a pre-removal gate:

- Initial-session reliability (no manual reconnect), no dropouts, multi-room sync.
- A receiver going offline/online demotes/recovers cleanly (liveness task).
- **Rate negotiation**: an `Auto` device runs at 48 kHz; switching one to
  **Fixed 44.1 kHz** (or a receiver that rejects 48 kHz) restarts its group at
  44.1 kHz with no audible break.
- AP2 **announcements** (per-output Play tone / Play announcement) play + duck —
  including on a receiver with **no input routed to it**, which now opens an
  on-demand session for the clip and hands it back after a 30 s lease
  ([architecture.md §5.4](architecture.md#54-announcing-to-an-output-with-nothing-routed-into-it)).
  Confirm by ear: tone on an unrouted receiver (audible a few seconds later, the
  pair + render-delay cost), a second tone within the lease (fast, warm session),
  then that the receiver frees its AirPlay input afterwards (a phone can connect).
- **AP2 alignment**: on the Align page, soloing two receivers and sweeping one's
  render delay by ear brings them into coincidence.
- Routing + grouping survive an add-on restart (the `raop_migration` shim + no
  dangling links). *(Migration was a verified no-op on the live store — kept for
  safety; it can be deleted after one clean boot.)*
- Then tune the render delay down from 1500 ms as far as the live feed allows.

### Productionize

- Remove `ap2_server`'s `allow(dead_code)`; use **discovered** features instead
  of the fixed features string.
- Revert the widened `airplay_audio` / `airplay_client` EnvFilter logging in
  `main.rs`.
- Excise the dead Rust-PTP code in the vendored `airplay2-sender` crate (PTP is
  driven by libairptp).
- Add tests / CI for the AP2 path; keep a per-brand quirk table.
- Fold the vendored `with_daemon` mDNS patches + the libairptp patches into the
  `pull_request_docs/` upstream mirrors.
- **Shutdown during a group's initial connect doesn't TEARDOWN.**
  `Ap2ServerHandle::shutdown` (the graceful-exit path,
  [architecture.md §5.2](architecture.md#52-airplay-2-av-receivers-eg-yamaha-wx-021-pioneer-vsx-934))
  signals the group task and awaits it for ~3 s, but the task only observes that
  signal *after* its sequential `connect_one` loop — each member costs up to
  `AP2_CONNECT_TIMEOUT × AP2_CONNECT_ATTEMPTS` plus backoff. So a SIGTERM that
  lands while a group is still coming up times out and exits without closing those
  sessions, leaving exactly the stale session the shutdown exists to avoid (the
  next start's first connect then fails and retries, as before this path existed —
  so this is a *missed improvement*, not a regression). Fix = make the connect loop
  cancellable: `tokio::select!` the shutdown receiver against each `connect_one`,
  and TEARDOWN whatever is already connected. Deliberately not done blind — it
  touches the proven-on-hardware connect path, so it wants a live re-validation
  (initial-session reliability + the M2 retry) alongside it.

### Real-time & allocation hardening

The AP2 data path is SCHED_FIFO end-to-end (ladder in
[architecture.md](architecture.md#7-real-time-thread-ladder)) and now
allocation-free on the steady hot path:

- **AP2 relay forward-loop allocations** — ✅ **Done.** The relay
  (`ap2_server.rs`) fans out with zero steady-state allocation: each sender hands
  back a recycled `Vec<i16>` via `LiveFrameSender::take_buffer()` (a bounded
  free-list; the decoder returns drained buffers), captured PCM is a pooled
  buffer, and the overlay `mix_buf` is reused across chunks and devices.
- **Vendored streamer per-packet PCM clone** — ✅ **Done.** `run_streamer`
  (`streamer.rs`) no longer clones `frame.samples` on the common no-EQ path — it
  encodes straight from the frame's shared `Arc<Vec<i16>>` (the ~1.4 KB/packet
  clone is gone; only the EQ path, unused here, still needs an owned copy).
- **Streamer `wire_packets` reuse** — deferred, deliberately. Each packet still
  allocates the outer `Vec<Vec<u8>>` (len 1 per per-device connection) and the
  encrypted RTP `Vec<u8>` from `prepare_audio`. Reusing them needs a
  sender→producer recycle channel **and** an encrypt-into-buffer change to
  `prepare_audio` — a deeper change to a proven-on-hardware encrypt path for a
  few-hundred-bytes/packet gain now that the dominant clone is gone. Not worth the
  RT-path risk yet.
- **Structural producer fuse** — not doing it. The fully-fused form (encode on
  the sender thread, no channel) is optional; the `crossbeam bounded(8)` split is
  kept **on purpose** for backpressure and is sufficient.
- **CPU affinity / host isolation (optional)** — pinning isn't required with
  relay + sender both at SCHED_FIFO; revisit only if a stall traces to CPU
  contention. WiFi power-save is disabled per-stream by the vendored sender; a
  host-wide disable is the more robust form.

### Open design / interop

- **Per-brand interop table** — proven on Yamaha + Pioneer only; validate each
  brand before trusting the "all AP2-capable" premise, and record quirks.
- **Mixed Sendspin + AP2 group coincidence** — two clock references (sendspin
  `CLOCK_MONOTONIC_RAW` send-ahead vs AP2 libairptp `CLOCK_MONOTONIC` + PT=87);
  pure-AP2 and pure-sendspin groups are fine, cross-protocol acoustic
  coincidence is an extra alignment task — defer and measure.
- **Converge AP2 onto `SharedTimeline` + `OutputBackend`** — the shipped path
  uses independent per-device `Connection`s sharing the grandmaster `clock_id`
  + identical PCM (sufficient for sync). Align it onto the shared seam
  ([architecture.md](architecture.md#the-outputbackend-seam-target-end-state))
  alongside the Sendspin per-device-sender overhaul + MG/AG routing-target
  polymorphism, rather than a parallel AP2 abstraction. Sequence after that API
  stabilizes, or co-develop.

### Codec (optional, later)

Realtime ALAC (type 96) is accepted by the test devices and needs no
`SETRATEANCHORTIME`. Buffered AAC (type 103) is a later option, not required.
