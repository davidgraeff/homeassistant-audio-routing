# Send-loop jitter & underflow analysis — RAOP-in → sendspin-out (AP2 out of scope — see banner)

**Companion diagram:** [`raop-to-sendspin-audio-path.svg`](raop-to-sendspin-audio-path.svg)
/ `.png` (source: `raop-to-sendspin-audio-path.drawio`).

**Scope:** the RAOP-input → **sendspin**-output path — its underflow points and the
architecture changes that remove them **without adding buffer** (near-realtime output),
plus general RT hygiene. **This is the sendspin/hardening brief.**

> **⚠️ AP2 is OUT OF SCOPE here — owned by the AP2 workstream
> (`airplay2-jitter-and-ui-handover.md`).** The AP2 remarks below were written
> before the AP2 file-path spike and their *causal* conclusion is superseded. The
> spike (`POST /api/spike/ap2`) proved the AP2 stack plays **audibly and in sync on
> the Pi** via `start_streaming`, which shares this doc's exact
> `run_streamer` + `bounded(8)` + `SCHED_FIFO` chain — so the send-loop jitter is
> **NOT** what makes the AP2 *live* path silent (that has a different, upstream cause:
> the live-feed path). Do **not** apply the "near-realtime / no bigger buffers"
> changes to AP2 — its ~1–2 s render buffer is by design and is exactly what makes it
> jitter-tolerant. The sendspin analysis and the cross-cutting fixes (RC2/RC3,
> M2/M4/M5/M6) below stand and are yours to pursue.

---

## TL;DR — the one root cause

The timing-critical audio work sits on the **shared general-purpose tokio
runtime**, decoupled from any realtime clock. The two ends that *are* correctly
isolated — PCM **capture** (a dedicated PipeWire RT thread) and the AP2 **final
send** (a dedicated `SCHED_FIFO` thread with `clock_nanosleep`) — are fine. The
**connective tissue between them is not**:

- **sendspin path (the subject of this doc):** the `capture_forward_task` relay
  (stamp + mix + fan-out) is a plain, unprioritized tokio task.
- **AP2 path (efficiency only — NOT the silence cause):** the `run_streamer`
  producer hands each 8 ms packet to its RT sender thread through **two**
  async-scheduler round-trips. Worth trimming as hardening, but the AP2 file-path
  spike shares this and plays fine — see the AP2 banner above.

Under load on a weak Pi (HTTP API, mDNS, discovery, multiple groups, ALAC decode)
these tasks are scheduled late by 100–294 ms (observed peak 873 ms). **For sendspin**
that exceeds the receiver's 250 ms lead → underrun → audible stutter. (The AP2
`bounded(8)` ≈ 64 ms drains too and the RT thread logs `fell behind`, but AP2's large
render buffer absorbs it — the spike confirms AP2 plays; its live-path silence is a
separate, upstream problem.)

**For sendspin, bigger buffers only hide this at the cost of latency** — the fix is
to make the feeders realtime so the *existing small buffers are enough* (the
near-realtime direction). **AP2 is the opposite:** its buffer is intentional — keep
it, and keep it *fed*.

---

## The sendspin path, stage by stage

`airplay-in` (RAOP receiver) → anchor `null-sink` → monitor → `sendspin-capture`
→ unbounded mpsc → `capture_forward_task` → per-member 32-cap → `writer_task` →
WS/TCP → ESPHome receiver (plays at the presentation timestamp, 250 ms lead).

Execution contexts (colour-coded in the diagram):

| Context | Thread | Realtime? |
|---|---|---|
| shairplay recv/decrypt/decode | tokio worker (+ AAC delivery `std::thread`) | ❌ |
| ingest **ring** → `airplay-in` | dedicated `airplay-producer` OS thread, own mainloop | ❌ (own non-RT clock) |
| anchor null-sink + monitor | system PipeWire RT thread | ✅ graph clock |
| `sendspin-capture` | dedicated OS thread, `RT_PROCESS` | ✅ graph-clock paced |
| `capture_forward_task` (stamp+mix+fan-out) | **plain tokio task** | ❌ **jitter enters here** |
| `writer_task` (per member) | tokio task | ❌ no pacing, TCP-throttled |
| receiver | device firmware | plays at timestamp, **250 ms lead is the only jitter buffer** |

### Underflow / stutter points

- **RC1 — non-RT relay starvation (primary).** `capture_forward_task` competes
  with all other daemon async work. A scheduling gap > the receiver's remaining
  lead → the receiver underruns → silence. `timeline.stamp()` re-anchors forward
  when the lead has decayed below 125 ms, but the receiver has already run dry.
  *(`sendspin_server.rs` capture-forward loop.)*
- **RC2 — ingest ring re-prebuffer amplifier.** On any single ring underrun the
  `airplay-producer` emits silence **and resets to a full 150 ms prebuffer**
  (`airplay_source.rs:447-460`). One late/lost network packet becomes ~150 ms of
  silence injected at the source — hitting **every** output of the group.
- **RC3 — uncompensated clock drift.** The producer runs on its own mainloop
  clock; the graph runs on the system clock; nothing rate-matches them. The ring
  slowly empties → recurring underruns over minutes. (PipeWire resamples
  44.1→48 kHz in-graph, but does *not* track the producer's push cadence.)
- **RC4 — fat critical section in the relay.** The forward loop holds the
  `groups` (tokio) + `client_to_node` (std) mutexes across the whole fan-out and
  allocates a fresh `Vec` per chunk in `overlay_mixer::mix`. Accept/event tasks
  contend for the same locks and stall delivery.
- **RC6 — silent per-member drop.** `MAX_QUEUED_AUDIO_FRAMES = 32`
  (`connection.rs:33`): a slow member socket fills the backlog, then every
  further chunk is dropped (`group.rs:166`) = holes at fixed timestamps the
  receiver can't fill.
- **RC5 — writer TCP stall.** `sink.send().await` has no pacing; a full TCP
  window (classically WiFi radio power-save sleep) backs the 32-cap up into drops.

---

## The AP2 path — corrected by the file-path spike (do not act on this here)

> Kept for context only. **AP2 is owned by the AP2 workstream** — see the banner at
> top and `airplay2-jitter-and-ui-handover.md`. The efficiency notes are valid; the
> original "jitter → underrun → silence" conclusion is **not**.

AP2's final send is done *right*: `sender_thread_main` is a `SCHED_FIFO` prio-50
thread sleeping to absolute deadlines with `clock_nanosleep(TIMER_ABSTIME)`
(`streamer.rs:161`). Genuine efficiency waste upstream of it (worth removing as
hardening, low risk):

- The producer does `spawn_blocking(|| tx.send(msg)).await` **and** `yield_now().await`
  per 8 ms packet — **two scheduler round-trips** (`streamer.rs:1194,1264`).
- Handoff is `crossbeam bounded(8)` ≈ 64 ms (`streamer.rs:582,723`); the producer
  holds the `inner` `Mutex` across the whole encode+encrypt (`streamer.rs:991-1254`).

**But this is not the cause of the AP2 live-path silence.** Proof: `start_streaming`
(file) — line 559 — and `start_streaming_live` (capture-fed) — line 659 — spawn the
**same** `run_streamer` (637 / 773) through the **same** `bounded(8)` (582 / 723) to
the same RT thread, and both log identical `fell behind` jitter. The file path (the
`/api/spike/ap2` tone) is **audible and in sync**; only the live path is silent. So
the shared producer/`bounded(8)` chain cannot be the differentiator.

The **live-specific** difference is the feed *into* the decoder: `start_streaming`'s
`AudioDecoder` decodes ahead from a complete source (never starves), whereas
`LiveAudioDecoder::create_pair(…, capacity)` is a small `bounded::<LivePcmFrame>(16)`
that `ap2_server`'s forward loop fills with `try_send` (**drops when full**). When the
producer is scheduled late this channel overflows and captured PCM is *lost* upstream
of the `AudioBuffer` — a lossy point the file path simply doesn't have. AP2's 2 s
`AudioBuffer` + ~1 s prefill + 1500 ms render delay are **correct and intentional**
(that's how AirPlay-2 tolerates jitter); the fix is to keep that buffer reliably fed,
**not** to shrink it. Details + the exact failure mode (drop-induced gaps vs
timestamp discontinuity) are tracked in the AP2 handover doc, using the spike as the
A/B oracle.

---

## Architecture changes (near-realtime; no bigger buffers)

Ranked; measure with the existing `TIMING STATS` / `fell behind` logs and the
sendspin receiver after each.

- **M1 — Realtime relay (kills RC1, the primary cause).** Do
  `timeline.stamp()` + mix + fan-out **on the `sendspin-capture` RT thread**
  (already graph-clock paced), or on a dedicated `SCHED_FIFO` relay thread (prio
  just below capture / the AP2 sender). Delete the unbounded capture→relay mpsc —
  there is then no async hop between capture and the wire, and general-purpose
  tokio work cannot preempt audio. *(Caveat: the WS writers are async; hand off to
  the per-member writers via a lock-free queue, or make the writers blocking on
  their own threads.)*
- **M3 — AP2 (OWNED BY THE AP2 WORKSTREAM — hardening only, NOT the silence fix).**
  The **quick interim** is worth doing as pure efficiency: replace
  `spawn_blocking(|| tx.send()).await` with a direct `try_send`/blocking `send` and
  drop `yield_now` — removes 2 scheduler hops/packet (low risk). But do **not** fuse
  the producer into the sender thread or shrink AP2's buffers to chase "near-realtime":
  the file-path spike plays fine with today's producer + `bounded(8)`, so that is not
  the AP2 silence cause. The real AP2 bug is the live feed (upstream), tracked in
  `airplay2-jitter-and-ui-handover.md`. **Coordinate before touching `ap2_server.rs`
  or the vendored streamer on AP2's behalf.**
- **M4 — Fix the ingest ring (kills RC2 + RC3).** On underrun, fill only the
  missing samples (or a one-quantum guard) and keep draining — do **not** reset
  to a full 150 ms prebuffer. And replace the hand-rolled ring + separate
  producer mainloop with a **graph-clock-locked adaptive resampler** (a DLL, or
  PipeWire's own rate-matching) so the producer consumes at exactly the graph's
  pull rate and never drifts.
- **M5 — Shrink the relay critical section (kills RC4).** Encode/mix once;
  snapshot the member list and release the `groups` + `client_to_node` locks
  **before** fanning out; reuse a preallocated mix buffer instead of a `Vec` per
  chunk.
- **M6 — Drop-to-live, never replay-late (addresses RC6).** When a member backlog
  is behind, skip forward to the newest chunk instead of sending the stale
  backlog (which arrives past the receiver deadline *and* grows latency). Log
  every drop — today AP2's `try_send` and the 32-cap drop silently.
- **M2 — Real RT scheduling + isolation (backstop for RC1/RC5).** `chrt`
  `SCHED_FIFO` + CPU affinity: pin the PipeWire-RT, capture, and relay/sender
  threads to dedicated cores; keep the tokio worker pool off those cores. Verify
  no priority inversion (`chrt` / `ps -eLo …,rtprio`). Disable WiFi power-save
  host-wide (AP2 already does it per-stream).

**Sequencing suggestion:** M3-interim + M1 are the highest impact for the least
code and directly target RC1 (the dominant term). M4 removes the source-side
amplifier that makes every downstream margin worse. M5/M6/M2 are hardening.

---

## Implementation status (sendspin / hardening workstream)

**Done (compiles; needs on-Pi measurement to confirm the audible win):**

- **M1 — realtime relay.** `sendspin_server.rs`: the capture→wire relay
  (`timeline.stamp` + overlay mix + `push_encoded` fan-out) moved off the shared
  tokio runtime onto a **dedicated OS thread** (`"sendspin-relay"`) that
  `blocking_recv`s the capture channel and runs `SCHED_FIFO` priority 40
  (`set_relay_realtime_priority`; best-effort — logs and continues at normal
  priority without `CAP_SYS_NICE`). The per-server `groups` map was converted
  from a `tokio::sync::Mutex` to a `std::sync::Mutex` (all its call sites are
  brief and synchronous — verified none held it across an `.await`) so the relay
  can touch it without an async hop. The relay thread stops when the capture
  handle's Drop closes the PCM channel. General-purpose async work (HTTP/mDNS/
  discovery/other groups) can no longer preempt the relay → RC1 addressed.
- **M4 (part 1) — ingest ring hysteresis re-arm.** `airplay_source.rs`: a
  mid-stream underrun now re-arms the jitter buffer to a small `AIRPLAY_REARM_MSEC`
  (40 ms) guard instead of the full `DEFAULT_AIRPLAY_LATENCY_MSEC` (150 ms)
  cold-start prebuffer. A transient late/lost packet costs ~one quantum + 40 ms
  instead of ~150 ms of source-injected silence fanned to the whole group → RC2
  bounded. (Clamped so a low configured latency can't make the guard exceed the
  cold-start prebuffer.)
- **M5 (partial).** The relay holds the `groups` + `client_to_node` locks only
  for the brief synchronous fan-out — no encode/mix work precedes the locks.
  Full mix-buffer reuse in `overlay_mixer` is **deferred**: `mix()` only runs
  while an announcement overlay is active, i.e. not on the steady-state music
  hot path, so its per-chunk `Vec` alloc is not a steady-state cost.

**Deferred (scoped; not yet done):**

- **M4 (part 2) — adaptive resampler (RC3).** Replacing the hand-rolled ring +
  separate producer mainloop with a graph-clock-locked adaptive resampler is a
  larger rewrite of `airplay_source.rs`; the hysteresis above only bounds
  *transient* gaps, not chronic producer↔graph drift.
- **M6 — drop-to-live + logging (RC6).** Lives in the **vendored** sendspin
  crate (`connection.rs` `MAX_QUEUED_AUDIO_FRAMES = 32` ≈ 680 ms; `group.rs`
  logs drops at `trace`). A full drop-oldest/resync-to-live needs a small change
  to the `ServerSender` backlog model and wants on-Pi tuning of the cap toward
  the receiver's ~250 ms lead — do it as its own change with hardware in the loop.
- **M2 (host part).** The relay's in-daemon `SCHED_FIFO` is done; the host-level
  `chrt`/CPU-affinity isolation and host-wide WiFi power-save disable are
  deployment/runtime config, not daemon code.

---

## AP2 → shared-capture coordination (2026-07-25, from the AP2 workstream)

AP2 live output now **plays** (the silence bug was a prefill issue in the vendored
streamer, fixed separately). But it **drops out intermittently** (Dusche: 3–30s then
silence; inconsistent), and the evidence points at the **capture→consumer feed being
uneven** — the shared path you own. Flagging for coordination; I could NOT confirm the
exact source, so this is a question, not a conclusion.

**Evidence (AP2 side):**
- tcpdump of outbound audio to the receiver (baseline pcaps on the HA host:
  `/tmp/ap2-{file,live,dropout2}.pcap`): steady 125 pkt/s **with scattered ~2s gaps**
  (per-second count drops to 0 for ~2 seconds, irregularly). Each gap drains the
  receiver's ~1.5s render buffer → audible dropout.
- Inbound from the Yamaha is MusicCast **status JSON**, not RTCP retransmit requests —
  so it's **sender-side starvation**, not packet loss.
- The vendored streamer logs `Decode took 70–211 ms (blocking send loop!)` — **~530
  times in 60s**. That timer wraps `LiveAudioDecoder::decode_resampled`, which loops a
  blocking 2 ms `recv_timeout` on the PCM feed. Capture is 44.1k passthrough (no
  resample), so the time is spent **waiting for feed that dribbles in unevenly**, and
  the vendored streamer's *blocking* decode in its async task amplifies a short feed
  gap into a multi-hundred-ms send-loop stall.

**What I could NOT confirm:** no `airplay_source` ingest underrun / 150 ms
re-prebuffer (RC2) or drift (RC3) log lines fired during the dropouts. So either the
unevenness is upstream but unlogged, or it's at the PipeWire capture/anchor level, or
it's partly AP2-specific (the blocking decode). The AP2 group has its **own**
`sendspin_capture` instance on the (shared) source-set anchor null-sink.

**Questions for the shared-capture/ingest owner:**
1. Does `sendspin_capture` deliver **evenly** (one chunk per graph quantum), or can it
   burst/gap? Any per-chunk cadence instrumentation? A ~2s delivery gap on the AP2
   capture is what we'd need to explain the outbound gaps.
2. Can the **anchor null-sink** stall/suspend its monitor for ~seconds when the source
   (airplay-in) has ingest jitter — starving *all* captures on it? (sendspin may mask
   this via its own relay/timeline; AP2's blocking decode does not.)
3. Do RC2/RC3 (ingest re-prebuffer / drift) affect **capture-delivery steadiness**
   downstream of the anchor, and would your ingest fixes steady it?

**Repro:** route `airplay-in → ap2-dev-dusche`, play audio, watch
`docker logs -f <addon> | grep "Decode took"` (spams during dropouts) and tcpdump
`udp and host <receiver> and host <daemon>` for the outbound gaps.

**AP2-side mitigation under consideration (if the feed can't be made even):** have the
AP2 relay keep the decoder fed with **silence during capture gaps** (steady, non-
blocking feed) so uneven capture can't stall the send loop — decoupling AP2 from feed
jitter. Noting it so we don't both fix the same thing two ways; happy to defer to a
shared-capture steadiness fix if that's cleaner.
