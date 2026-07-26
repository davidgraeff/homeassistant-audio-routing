# AP2 Track P3 — fix the spike tone-feeder thread leak

**Parallel-safe.** Owns `bridge-daemon/src/ap2_spike.rs` **only**. Small, isolated.
(Do not touch `ap2_server.rs` / vendored crate — those are the audio-path owner's.)

## Problem
`ap2_spike.rs` live mode (`mode:"live"`, `connect_tone_live`) spawns a **detached**
`std::thread` per receiver ("ap2-spike-tone") that generates a sine and calls
`LiveFrameSender::send(...)` in a loop, exiting only when `send` returns `false`
(i.e. the decoder/connection was dropped). On `DELETE /api/spike/ap2`, `stop()` drops
the `Ap2ToneSpike` handle → the task disconnects the `Connection`s → the decoders drop
→ the feeders *should* exit. In practice, **leftover `ap2-spike-tone` threads were
observed still running after stop** (2 of them), and separately many `rt-sender`
threads accumulate across spike cycles — the stale streams then blast the receivers and
pollute later tests (this actually confounded a debugging session).

## Root cause
The feeder's only exit condition is `send()` failing. If the decoder isn't dropped
promptly (or the channel stays alive because a `LivePcmFrame` clone / the streamer task
lingers), the feeder blocks in `send()` (blocking on a bounded channel) and never
checks any stop flag. There's no deterministic stop signal and no join.

## Fix (all in `ap2_spike.rs`)
1. Give each feeder an explicit **stop signal** independent of the channel:
   `Arc<AtomicBool>` (or a `crossbeam`/`std` shutdown receiver). The feeder loop checks
   it each iteration and exits promptly; use `try_send` (or `send_timeout`) so it can't
   block forever past a stop.
2. **Track + join** the feeder `JoinHandle`s in the `Ap2ToneSpike` handle; on `stop()`
   set the flag and join (or at least detach-with-guaranteed-exit) so no thread
   outlives the spike.
3. Ensure `stop()` fully tears down: set stop flag → drop/disconnect connections →
   feeders observe the flag and exit. Verify with:
   `docker exec <addon> sh -c "ps -T -p \$(pgrep -f bridge-daemon|head -1) | grep -c ap2-spike-tone"`
   returns 0 after a DELETE.

## Note for the audio-path owner (not this track)
The related `rt-sender` accumulation is in the vendored `Connection`/`AudioStreamer`
teardown (disconnect doesn't stop the sender thread) — that's a separate fix in the
vendored crate, sequenced behind the audio-path work. This track only fixes the
spike-side feeder leak.

## Acceptance
- After `DELETE /api/spike/ap2`, zero `ap2-spike-tone` threads remain (verified via
  `ps -T`), for both `file` and `live` modes, repeated back-to-back.
- Starting a new spike (single slot) cleanly replaces the old one with no residue.

## Status — DONE (code), live `ps -T` check pending deploy
Implemented in `ap2_spike.rs` only (compiles clean, no new warnings; vendored crate
untouched):
1. **Stop signal independent of the channel** — `Ap2ToneSpike` holds a shared
   `Arc<AtomicBool> stop_flag`. Each live feeder checks it every iteration and exits
   promptly; the feed switched from blocking `send` to `try_send` + a 2ms pace-sleep,
   so it can never block past a stop (phase only advances on a successful send, so the
   tone stays continuous across retries).
2. **Track + join** — feeder `JoinHandle`s are collected into `Arc<Mutex<Vec<..>>>`
   by the task as it connects each receiver; `stop()` joins them (via `spawn_blocking`,
   off the async executor).
3. **Deterministic `stop()` teardown** — set flag → fire `shutdown` (task disconnects
   conns, drops decoders) → await the task (guarantees all feeders registered) → join
   the feeder threads. `Drop` is now a best-effort signal-only safety net.

Live acceptance (`ps -T ... | grep -c ap2-spike-tone == 0` after DELETE, both modes,
back-to-back) still needs a deploy to the Pi add-on + real receivers — not run here
(gated on hardware; a live spike blasts the speakers).
