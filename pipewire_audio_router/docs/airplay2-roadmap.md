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
- AP2 **announcements** (per-output Play tone / Play announcement) play + duck.
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

### Real-time & allocation hardening

The AP2 data path is already SCHED_FIFO end-to-end (ladder in
[architecture.md](architecture.md#7-real-time-thread-ladder)). Remaining:

- **AP2 relay forward-loop allocations** (`ap2_server.rs`, the `blocking_recv`
  loop): each chunk does `pcm.chunks_exact(2)…collect::<Vec<i16>>()` **and**
  `samples.clone()` per receiver — N+1 heap allocations per chunk on the RT
  relay. Reuse one `Vec<i16>` scratch + fan out via a shared `Arc<[i16]>`
  (mirror the sendspin relay's reused `mix_buf`).
- **Vendored streamer per-packet clones** — `run_streamer` clones
  `frame.samples` every packet even with no EQ and rebuilds a `wire_packets`
  Vec per packet. Encode from a borrow + reuse a per-connection scratch where
  the vendored API allows. Lower priority than the relay allocations.
- **Structural producer fuse (optional)** — the fully-fused form (encode on the
  sender thread, no channel) is optional; the `crossbeam bounded(8)` split is
  kept for backpressure and is sufficient now.
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
