# AirPlay-2 sender — handover: fix send-loop jitter + wire render delay to the UI

**Audience:** another agent instance picking up two self-contained workstreams on the
in-daemon AirPlay-2 sender. The primary author is continuing on core AirPlay-2 work,
so these two tasks are yours to own end-to-end.

**Status at handover (2026-07-25):** The AP2 sender path is fully wired and, on the
wire, *correct*. The two real receivers are Yamaha "Dusche" @192.168.178.165 and
Pioneer VSX-934 F11B89 @192.168.178.35.

**DECISIVE NEW RESULT — the stack is proven on the RPi.** A synchronized **440 Hz
test tone streamed via the file path** (`AudioDecoder` + `Connection::start_streaming`,
behind the new `POST /api/spike/ap2` endpoint) is **audible and in sync on BOTH
receivers on the Pi**. So pairing, PTP, ALAC, encryption, the render buffer, and
multi-room sync all work on this hardware. The **live output path**
(`start_streaming_live`, fed from PipeWire capture) is the *only* thing still silent.

**This reframes Workstream A.** The RT sender-thread jitter (`fell behind` 8–200 ms)
is present on the file path **too** — yet the file path plays fine. So jitter alone
is **not** the cause of the live-path silence. The real differentiator is that the
**file decoder buffers ahead** (decodes on demand from a full in-memory/file source,
so the streamer's internal audio buffer never empties), whereas the **live decoder is
fed at exactly realtime from capture with only a small buffer** — so any producer
scheduling delay drains it, the streamer underruns, and the receiver (tight realtime
anchor) gets gaps/mutes. Workstream A is therefore: **keep the live streamer's buffer
full**, not just "reduce jitter." The 1500 ms render delay helps but does not fix an
*underrunning source*. Use the spike endpoint as your A/B oracle: file path = plays,
live path = silent; make the live path behave like the file path.

---

## 0. What is already PROVEN — do not re-investigate

These were verified directly on the live daemon; treat them as settled facts:

- **gPTP works.** `tcpdump` on the HA host (`udp port 319 or 320`) shows a complete,
  healthy exchange: we (`192.168.178.22`, grandmaster clock `0xffffa1b2c3d4e5f6`) send
  Sync + Follow_Up; the **Pioneer replies with Delay_Req** (its clock `0x…f11b89`).
  A receiver only sends Delay_Req once it has accepted us as PTP grandmaster. The
  receivers' clocks are locked to ours.
- **The RTP stream is correct.** Encrypted ALAC, `pt=96`, marker bit on first packet,
  PT=87 sync packets carrying the right grandmaster `clock_id` on the `CLOCK_MONOTONIC`
  timeline. Real audio reaches the streamer (`DIAG PCM frame … rms>0`).
- **The source/anchor path is healthy.** A sendspin (Voice satellite) output on the
  *same* anchor plays the same source audio fine. So capture → anchor → monitor works.
- **Routing must use node names, not display names.** Both receivers expose a RAOP
  output *and* an AP2 output that collapse to the same UI display name ("Dusche",
  "Pioneer VSX-934 F11B89"). The music group bound them to the RAOP outputs. For AP2
  testing, link by unambiguous node name: `ap2-dev-dusche`,
  `ap2-dev-pioneer_vsx_934_f11b89`. (Longer term: RAOP is being dropped, which removes
  the collision — out of scope here.)
- **The resample is already off the send loop.** Capture now runs at 44100 so PipeWire
  resamples in-graph; `LiveAudioDecoder::decode_resampled` takes its identity
  passthrough branch. The old `Decode took 12–41ms (blocking send loop!)` warnings are
  gone. Do **not** reintroduce in-Rust resampling.
- **The file path plays; the live path is silent (the whole ballgame).** `POST
  /api/spike/ap2` streams a sine tone via `start_streaming` (file decoder) — **audible
  and in sync on both receivers**. `start_streaming_live` (capture-fed) is silent with
  the *same* PTP/encryption/render-delay/receivers. The `fell behind` RT jitter occurs
  on **both** paths, so jitter is not the discriminator. The difference is **buffer
  fullness**: the file decoder decodes ahead so the streamer never starves; the live
  decoder is fed at realtime with a small buffer and underruns. **This is the bug to
  fix.**

---

## Workstream A — make the LIVE path keep its buffer full (fix the silence)

> **Primary goal:** make `start_streaming_live` (capture-fed) behave like
> `start_streaming` (file-fed), which the spike proves is audible+in-sync on the Pi.
> The A/B oracle is the tone spike: `POST /api/spike/ap2` (plays) vs a routed
> `ap2-dev-*` output (silent). Reducing RT jitter is secondary hardening — it exists
> on *both* paths, so it is not what makes the live path silent.

### Symptom
With `airplay_audio=info` logging enabled, the streamer logs `Sender thread fell
behind by 135–294 ms` and `TIMING STATS … avg_jitter=0.480ms max_jitter=873ms`.
BUT the file-path spike logs the same and still plays. So the audible difference must
be **live-decoder buffer underrun**: when the streamer pulls a packet and the live
buffer is empty (producer/capture scheduled late), it emits silence/stalls; on a
tight realtime anchor the receiver hears gaps or mutes entirely. The file decoder
never hits this because it decodes ahead from a complete source.

### First things to try (buffer fullness — most likely the fix)
- **Enlarge the live decoder buffer + prefill.** `ap2_server.rs` calls
  `LiveAudioDecoder::create_pair(AP2_RATE, CHANNELS, 16)` — the `16` is a small
  capacity. Raise it, and check `start_live`'s prefill target (streamer.rs ~677–719,
  currently ~1 s / 50%). A deeper steady-state buffer absorbs producer jitter.
- **Instrument underruns.** The streamer tracks `underruns` (passed into
  `run_streamer`); log it, or watch for `DIAG PCM frame … rms=0` bursts *while real
  audio should be flowing* — that's the live buffer running dry. Compare the same
  counter under the file-path spike (should stay ~0).
- **Pace the capture→forward feed.** `ap2_server`'s forward loop fans each captured
  chunk to every `LiveFrameSender` via `try_send` (drops if full). Confirm it isn't
  dropping (bounded live buffer full) or delivering burstily. Feeding a slightly
  larger, steady lead into the live decoder is the goal.

### Secondary: RT send-thread jitter (hardening, present on both paths)

### Architecture (where the jitter comes from)
File: `bridge-daemon/vendor/airplay2-sender/crates/airplay-audio/src/streamer.rs`

- `sender_thread_main` (line ~161): a **dedicated OS thread**, `SCHED_FIFO` prio 50
  (confirmed applied on the Pi: log `Set real-time priority (SCHED_FIFO, priority 50)`).
  It only receives pre-built `SenderMessage::Packet { wire_packets, sync_data }`, sleeps
  until an absolute deadline (`precise_sleep_until`, `clock_nanosleep TIMER_ABSTIME`),
  and sends. It does no encoding. The `fell behind` warning (line ~348) fires when it
  wakes and finds `now > next_deadline + burst_duration` — i.e. it had nothing to send
  in time, or was preempted.
- The **producer** is the async `run_streamer` task (same file). It locks the streamer
  mutex, pulls PCM, ALAC-encodes, encrypts, and hands packets to the sender thread.
- **Handoff channel:** `crossbeam_channel::bounded::<SenderMessage>(8)` (lines ~582 and
  ~723). Only **8 packets ≈ 64 ms** of slack between producer and sender thread.
- **Per-packet handoff cost:** the producer does
  `tokio::task::spawn_blocking(move || tx_clone.send(msg))` **per 8 ms packet**
  (streamer.rs ~line 1194). Dispatching to the tokio blocking pool every packet is
  heavy and itself a scheduling-jitter source.

### Root cause (hypothesis, well-supported)
The RT sender thread is fine. The **producer can't feed it steadily**: it's an async
task competing with the whole daemon's other async work (PipeWire event handling, API,
mDNS/discovery, sendspin, etc.) on the shared tokio runtime, and it pays a
`spawn_blocking` dispatch every packet. Under load the producer is scheduled late and
bursty; the bounded(8) channel drains; the sender thread starves → "fell behind".
Two concurrent receivers (two producers + two sender threads) roughly double the load.

### Suggested fixes, ranked (measure after each with the TIMING STATS + `fell behind` logs)
1. **Remove the per-packet `spawn_blocking`.** crossbeam `try_send`/`send` is cheap and
   non-blocking-ish; sending directly from the producer (or from a tiny dedicated
   thread) avoids the blocking-pool dispatch. If the bounded(8) is the reason for
   `spawn_blocking` (to absorb backpressure), pair this with #2.
2. **Deepen the handoff buffer.** Raise `bounded(8)` to e.g. 64–128 (≈0.5–1 s) so
   transient producer stalls don't starve the sender thread. Cheap, low-risk, likely
   high-impact. (Note: this is buffering *before* the wire; it does not change the
   receiver-side render delay.)
3. **Isolate the producer from the shared runtime.** Run the AP2 producer loop on a
   dedicated thread / its own current-thread tokio runtime (ideally elevated priority,
   but below the sender thread's FIFO 50) so daemon async load can't delay packet
   generation. This is the most invasive but most principled fix.
4. **Cut hot-path logging.** `airplay_audio=info` currently logs `DIAG …` and per-sync
   lines; formatting/IO on the Pi adds jitter. Once debugging is done, drop the default
   filter back (see `main.rs` EnvFilter, below) and/or gate the DIAG logs behind a
   feature/lower frequency. Keep the `fell behind` WARN while tuning.
5. **Pin/prioritize.** Consider CPU affinity for the sender thread, and confirm no
   higher-priority FIFO thread (PipeWire) is starving it (`chrt`, `ps -eLo …,rtprio`).

These live in the **vendored** crate (`vendor/airplay2-sender/…`), which we own and
patch freely. Keep patches minimal and commented (there's an upstreaming convention in
`~/Entwicklung/home_assistant/pull_request_docs/`, but these Pi-specific RT changes may
stay local).

### Definition of done
A source routed to `ap2-dev-*` is **audible and in sync on both receivers** (the live
path matches the file-path spike). Bonus: live-decoder underruns ~0 during playback,
`max_jitter` well under the render delay with no sustained `fell behind`, and the
render delay reducible from 1500 ms without dropouts.

---

## Workstream B — wire the render delay to the per-output latency UI field

### Current state (hard-coded)
File: `bridge-daemon/src/ap2_server.rs`
```rust
const AP2_RENDER_DELAY_MS: u32 = 1500;   // <-- hard-coded
...
conn.set_render_delay_ms(AP2_RENDER_DELAY_MS);   // in connect_one(), before setup()
```
`ap2_server::start(members: Vec<(String, IpAddr)>, sink_node_id: u32, clock_id: u64)`
applies the same constant to every receiver.

### The existing UI mechanism to reuse
The UI already has a generic per-output latency field. For RAOP it is fully wired;
for AP2 it currently returns `None`, so the UI shows nothing.

- `OutputInfo.latency_ms: Option<u16>` — `bridge-daemon/src/api.rs:352`. Populated for
  RAOP from `raop_latencies.get(&node_name)` (api.rs ~408). The AP2 branch sets
  `latency_ms: None` (api.rs ~438). **→ populate it for AP2.**
- Storage: `sync_settings.rs` holds per-node RAOP latency overrides:
  `raop_latencies() -> Map<String,u16>` and `set_raop_latency(node_name, Option<u16>)`.
  **→ either generalize this to AP2 node names or add a parallel `ap2_latencies` map.**
  (Generalizing is less code; just make the key any output node name and rename in a
  follow-up. A separate map is cleaner semantically. Your call — note it in the commit.)
- Handler: `set_output_latency` (PUT `/api/outputs/:node_name/latency`), api.rs ~line
  where it currently **rejects non-RAOP**:
  ```rust
  if !node_name.starts_with(RAOP_NODE_PREFIX) {
      return BAD_REQUEST "… is not a RAOP output";
  }
  ```
  **→ add an `AP2_DEV_PREFIX` branch** (`config::AP2_DEV_PREFIX = "ap2-dev-"`). For AP2
  there's no PipeWire module to reload; instead persist the value and **nudge the
  reconciler** so the AP2 group restarts with the new delay. The reconcile nudge is
  `state.changes.send(())` (used elsewhere in api.rs, e.g. `routing::link`).

### Plumb the stored value into the sender
The reconciler builds AP2 members and starts the sender:
- `bridge-daemon/src/sync_group.rs`:
  - `DesiredGroup.ap2_members: Vec<(String, IpAddr)>` (line ~78), filled at ~254
    (`g.ap2_members.push((dev_node.clone(), addr.ip()))`).
  - AP2 (re)start at ~463–475: `ap2_server::start(d.ap2_members.clone(), anchor_id, clock_id)`.
- **Change `ap2_members` to carry a per-device delay**, e.g.
  `Vec<(String, IpAddr, Option<u16>)>`, or pass the `sync_settings` handle into
  `reconcile` and look up each node's delay there. Then extend
  `ap2_server::start(...)` / `connect_one(...)` to take the per-device delay and call
  `conn.set_render_delay_ms(delay.unwrap_or(DEFAULT))` instead of the const. Keep the
  `const` as the default when no override is stored.
- `reconcile` is already called from `main.rs` with the shared state it needs; if you
  pass `sync_settings` in, thread it through the same way `ap2_devices`/`ap2_ptp` are.

### Gotchas
- **Restart on change.** Changing an AP2 receiver's render delay means tearing down and
  reconnecting its RTSP session (the delay is set before `setup()`/`start_streaming_live`).
  The reconciler already drops+restarts the AP2 group when membership changes; make a
  delay change trigger the same restart. Avoid thrashing: each restart re-pairs, and a
  too-fast reconnect can hit a transient `Pairing error M2` (receiver hasn't released
  the prior session). A short debounce or a "changed delay ⇒ restart once" guard helps.
- **Bound the value** to the negotiated window: `StreamConfig.latency_max = 88200`
  (≈2 s at 44.1 kHz) in ap2_server. A render delay ≥ latency_max won't fit the
  receiver's buffer. Clamp UI input to e.g. 200–2000 ms.
- **UI type is `u16` ms.** 1500 fits. Keep units consistent (ms) end to end.

### Definition of done
The AP2 outputs show their render delay in the same UI field RAOP uses; editing it
persists, restarts just that group's senders, and changes the audible buffer. Default
stays 1500 ms when unset.

---

## Build / deploy / test loop

- **Compile (fast, on dev box):**
  `cd bridge-daemon && cargo build` (the vendored crates compile too; ignore the
  pre-existing C `__thread` warnings from `vendor/libairptp`).
- **Deploy to live HA (~10–15 min, cross-build → GHCR → Supervisor pull):**
  `cd homeassistant-pipewire-audio-routing && ./scripts/deploy-dev.sh addon`
  (run in background; wait for exit 0). HA host = `homeassistant.local`, container =
  `addon_local_pipewire_audio_router`, daemon API on `:8099`.
- **Set up the AP2-only test route (unambiguous node names):**
  ```
  API=http://homeassistant.local:8099/api/routing
  curl -sX POST $API/link   -H 'Content-Type: application/json' -d '{"source":"airplay-in","output":"ap2-dev-dusche"}'
  curl -sX POST $API/link   -H 'Content-Type: application/json' -d '{"source":"airplay-in","output":"ap2-dev-pioneer_vsx_934_f11b89"}'
  curl -s $API   # inspect "links"
  ```
  The AirPlay input's mDNS name is currently **"Music Now"**; play to it to generate
  signal. A sendspin Voice satellite on the same source is a useful discriminator
  (if it plays but AP2 doesn't, the source is fine and the fault is AP2-side).
- **Watch the streamer:** logs are visible because `main.rs`'s EnvFilter default was
  widened to `bridge_daemon=info,shairplay=info,airplay_client=info,airplay_audio=info`
  (revert the last two for production once tuned).
  ```
  ssh root@homeassistant.local 'CID=addon_local_pipewire_audio_router; docker logs --since 90s "$CID" 2>&1 \
    | grep -iE "DIAG PCM frame|fell behind|TIMING STATS|AP2: streaming|Render delay|could not start"'
  ```
  `DIAG PCM frame … rms>0` = real audio reaching the sender; `fell behind` /
  `TIMING STATS max_jitter` = the jitter you're fixing.
- **gPTP sanity (host has tcpdump):**
  `ssh root@homeassistant.local 'timeout 6 tcpdump -ni any -c 40 "udp port 319 or udp port 320"'`
  Expect Sync/Follow_Up out + Delay_Req in from the receiver IPs.
- **A/B oracle — the tone spike (KNOWN-GOOD file path):**
  ```
  # unroute the live AP2 outputs first (each receiver accepts one session):
  curl -sX POST http://homeassistant.local:8099/api/routing/unlink -H 'Content-Type: application/json' -d '{"source":"airplay-in","output":"ap2-dev-dusche"}'
  curl -sX POST http://homeassistant.local:8099/api/routing/unlink -H 'Content-Type: application/json' -d '{"source":"airplay-in","output":"ap2-dev-pioneer_vsx_934_f11b89"}'
  # play a 60s tone to both (empty "ips" = all present discovered receivers):
  curl -sX POST http://homeassistant.local:8099/api/spike/ap2 -H 'Content-Type: application/json' -d '{"ips":["192.168.178.165","192.168.178.35"],"freq":440,"seconds":60}'
  curl -sX DELETE http://homeassistant.local:8099/api/spike/ap2   # stop
  ```
  This path is **known audible + in sync**. Diff its streamer behaviour (buffer/
  underrun) against the live path to find what the live path does wrong.

## Key files
- `bridge-daemon/src/ap2_server.rs` — LIVE AP2 audio engine; `const AP2_RENDER_DELAY_MS`,
  `AP2_RATE`, `connect_one` (uses `start_streaming_live`), `start`, capture-forward loop.
  `build_device` + `ALAC_MAGIC_COOKIE` are `pub(crate)` (shared with the spike).
- `bridge-daemon/src/ap2_spike.rs` — the KNOWN-GOOD file/tone path (`start_streaming`)
  behind `/api/spike/ap2`. Your A/B reference; diff it against `ap2_server.rs`.
- `bridge-daemon/src/sync_group.rs` — reconciler; `DesiredGroup.ap2_members`, AP2
  (re)start block, teardown-on-drop.
- `bridge-daemon/src/api.rs` — `OutputInfo.latency_ms`, `list_outputs`,
  `set_output_latency` (RAOP-only guard to extend).
- `bridge-daemon/src/sync_settings.rs` — per-node latency persistence
  (`raop_latencies` / `set_raop_latency`).
- `bridge-daemon/src/sendspin_capture.rs` — `spawn` / `spawn_with_rate` (AP2 uses
  44100).
- `bridge-daemon/src/main.rs` — EnvFilter default (revert extra targets for prod).
- `bridge-daemon/vendor/airplay2-sender/crates/airplay-audio/src/streamer.rs` — the RT
  producer + `sender_thread_main` + bounded(8) handoff (Workstream A lives here).
- `bridge-daemon/vendor/airplay2-sender/crates/airplay-client/src/connection.rs` —
  `set_render_delay_ms`, `start_streaming_live` (applies render delay + PTP sync mode).

## Background docs
- `docs/airplay2-sender-multiroom-spike.md` — how the sender + libairptp path was
  proven on real hardware.
- `docs/airplay2-roadmap.md` — the AP2 roadmap & architecture (status, phases,
  drop-RAOP plan); the authoritative source for where this work fits.
