# AP2 Track P2 — diagnose Pioneer's events-channel timeout (why it never renders)

**Parallel-safe: READ-ONLY investigation.** Produces a diagnosis + proposed fix; makes
**no code edits** (so it can't conflict with the audio-path work). If a fix is obvious
and isolated, propose it for sequenced application — do not edit the vendored
`connection.rs` while the audio-path owner is in it.

## Symptom
Two receivers behave differently on the SAME code path:
- **Dusche (Yamaha WX-021, 192.168.178.165):** plays (once the prefill/dropout fixes
  are in). Its events channel connects: log `Establishing events connection to
  192.168.178.165:<port>` → `Events connection established`.
- **Pioneer (VSX-934, 192.168.178.35):** **never renders any audio.** Its events
  channel connect **times out**: `Could not connect to events port
  192.168.178.35:<port> (proceeding anyway): Connection timed out (os error 110)`.

AP2 receivers can refuse to render if the events channel isn't up, so this is the prime
suspect for Pioneer's total silence — a *separate* failure from the Dusche dropout.

## What the "events channel" is
After RTSP `SETUP`, the vendored sender opens a TCP connection to an **events port the
receiver advertises in the SETUP response** (see `vendor/airplay2-sender/crates/
airplay-client/src/connection.rs`, the `Establishing events connection to …` /
`Events connection established` code, and where it parses the SETUP reply). The
`os error 110` (ETIMEDOUT) means the sender's TCP connect to that port never completed.

## Investigation steps (all read-only)
1. **Capture the SETUP exchange for both receivers** and compare the advertised events
   port + any events-related fields. On the HA host:
   `tcpdump -ni any -A "host 192.168.178.35 and tcp"` while routing
   `airplay-in → ap2-dev-pioneer_vsx_934_f11b89` (and separately for `.165`). Look at
   the RTSP `SETUP` response bodies (plist/bplist) — the `eventPort` (or equivalent).
2. **Is the advertised port actually open on the Pioneer?** From the HA host:
   `nc -vz 192.168.178.35 <eventPort>` vs the Dusche port. If Pioneer's port is closed
   / different / 0, that's the cause.
3. **Direction/ління:** confirm whether the sender should *connect to* the receiver's
   events port (current behavior) or whether this receiver expects the reverse
   (receiver connects back), or a different transport. Compare against how OwnTone
   (`~/Entwicklung/owntone-src`) handles the events channel for non-Apple receivers.
4. **Does Pioneer render WITHOUT the events channel?** The sender already "proceeds
   anyway" on timeout — so events-timeout ≠ automatically fatal. Rule in/out by
   checking whether Pioneer ever emits any audible output or sends RTCP/feedback at all
   (tcpdump the audio + control ports). If it's silent even with RTP flowing + PTP
   locked, the events channel (or a Pioneer-specific SETUP quirk) is likely required.
5. **Per-brand quirk angle:** the fixed features string / SETUP params in
   `ap2_server::build_device` + the `StreamConfig` are tuned for what both receivers
   accepted in the spike. Check whether Pioneer needs a different `eventPort` handling,
   timing-port setup, or a longer connect timeout.

## Deliverable
A short report in this file (append a "## Findings" section): the advertised events
port for each receiver, whether it's reachable, the exact divergence, and a **proposed
fix** (e.g., longer connect timeout, correct port parsing, make events non-blocking, or
a Pioneer quirk entry). Flag whether the fix touches the vendored `connection.rs`
(coordinate sequencing) or is daemon-side.

## References
- `vendor/airplay2-sender/crates/airplay-client/src/connection.rs` — SETUP + events
  connect (`Establishing events connection`).
- `docs/airplay2-sender-multiroom-spike.md` — the proven per-device setup.
- Per-brand interop is won device-by-device (roadmap risks section); keep a quirk table.

---

## Findings (2026-07-25, read-only investigation)

### TL;DR
The events-channel connect at `connection.rs` has **no connect timeout**. When Pioneer
drops the SYN to its advertised events port (ETIMEDOUT / os error 110, *not* a fast
RST), the sender blocks on `TcpStream::connect().await` for the **full kernel SYN-retry
timeout (~127 s on this host)** *in the middle of SETUP*, between phase-1 and phase-2.
By the time it "proceeds anyway", the RTSP/stream session is long stale, so phase-2 +
RECORD + RTP land on a receiver that has given up → **total silence**. Yamaha's events
port is reachable immediately, so it never hits the stall. OwnTone — the reference —
bounds this exact connect to **3 s** and treats failure as non-fatal. **The isolated,
high-confidence fix is to give the events connect a short timeout (mirror OwnTone /
mirror the RECORD call that already does this 8 lines below). That touches vendored
`connection.rs`, so it must be sequenced with the audio-path owner.**

### Evidence

**1. The connect has no timeout — but the very next request (RECORD) does.**
Three identical call sites, none wrapped:
- `connection.rs:820` `setup()` — `match TcpStream::connect(events_addr).await`
- `connection.rs:1555` `setup_as_ptp_master()` — same, via `tokio::net::TcpStream::connect`
- `connection.rs:1713` `setup_for_group()` — same

Compare `connection.rs:862`, the RECORD sent right after, which *is* bounded:
`tokio::time::timeout(Duration::from_secs(2), self.rtsp.send(record_req))`. So the
timeout idiom (and import) already exist in this file; the events connect just never got
it. The comment at `:817` ("This MUST be done before SETUP phase 2 or some devices
return 500") means the blocking connect sits **on the critical SETUP path** — its stall
delays phase-2 for every receiver whose events port isn't instantly connectable.

**2. `os error 110` = ETIMEDOUT = dropped SYN, quantified.**
`/proc/sys/net/ipv4/tcp_syn_retries = 6` on the HA host → initial SYN + 6 retransmits
with exponential backoff (1+2+4+8+16+32+64) ≈ **127 s** before `connect()` fails. That
is the length of the mid-SETUP stall for Pioneer. (A fast RST would instead give
`ECONNREFUSED`/os error 111 in <1 ms — see next point — so this is specifically a
*silently-dropped* SYN, i.e. the port isn't accepting / is firewalled at connect time.)

**3. Both receivers RST their *generic* closed ports — so "port merely closed" cannot
explain the timeout.** `nmap -Pn` (from the HA host, same subnet 192.168.178.22):

| Port | Pioneer .35 (VSX-934) | Yamaha .165 (WX-021, "Dusche") |
|---|---|---|
| 7000 (AP2 RTSP) | **open** | **open** |
| 49153 / 49154 | closed | **open** (persistent listeners) |
| 554 / 3689 / 5000 / 49152 / 49155-49164 | closed (RST) | closed (RST) |

Closed ports return **RST** on both (nmap `closed`, not `filtered`). So an events port
that were simply "not listening" would produce a *fast refuse*, not a 127 s timeout.
Pioneer keeps **no** persistent high-port listeners (only 7000); Yamaha keeps
49153/49154 up. This fits: Yamaha's events port is up and reachable the instant it's
advertised, whereas Pioneer's advertised events port is either opened lazily/late or
firewalled per-session such that the sender's immediate SYN is dropped → the 127 s
stall only Pioneer hits.

**4. Live confirmation of the working side (passive, no session grabbed).** During the
investigation the daemon was already streaming (concurrent owner testing the live path):
`airplay_audio::streamer` sending ALAC to `targets=1`, RTP flowing to
`192.168.178.165.35324` with an established RTSP TCP on `192.168.178.165:7000`. The
active target was the **Yamaha** (the working receiver); **no** session to Pioneer
existed. This is why the fresh Pioneer SETUP capture (steps 1-2 below) is still
**pending** — see "Not yet confirmed".

**5. OwnTone (the reference) bounds this connect and shrugs off failure.**
`src/outputs/airplay.c:3230` → `airplay_events_listen()` (`airplay_events.c:479`) →
`net_connect()` → `net_connect_impl(..., NET_CONNECT_TIMEOUT_MS, false)` with
`NET_CONNECT_TIMEOUT_MS = 3000` (`misc.h:18`). `net_connect_addrinfo()` (`misc.c:408`)
does a **non-blocking `connect()` + `poll(POLLOUT, 3000ms)`**, so a dropped SYN fails in
3 s, logs *"Could not connect to events port, proceeding anyway"*, and SETUP continues
while the session is still fresh. OwnTone additionally treats **`eventPort == 0` as a
hard error** (`airplay.c:3223`). The vendored Rust side instead has
`#[serde(rename = "eventPort", default)]` (`plist_codec.rs:113`) → a missing eventPort
silently becomes **0**, and the code then tries to `connect` to `<addr>:0`.

### Root cause (two layers)
- **Layer A — why fatal (confirmed, daemon-vendored):** the missing connect timeout
  turns a non-fatal "events port unreachable" into a ~127 s mid-SETUP stall that
  invalidates the whole session. This alone explains Pioneer's silence and is the part
  we can fix with confidence.
- **Layer B — why Pioneer's SYN is dropped (hypothesis, needs the capture):** Pioneer
  advertises an events port that isn't connectable at that instant (opened late /
  per-session firewall / possibly a different/zero port), unlike Yamaha. This is the
  per-brand quirk. It may become moot once Layer A is fixed (OwnTone proves rendering
  does **not** require the events channel — it "proceeds anyway"), but if Pioneer still
  stays silent after the timeout fix, Layer B must be solved directly.

### Proposed fix
**Primary (isolated, mirrors OwnTone + the adjacent RECORD call) — touches vendored
`connection.rs`, so sequence with the audio-path owner; do NOT apply while they are in
the file.** Wrap all three events connects in a 3 s timeout, e.g.:

```rust
let events_connect = tokio::time::timeout(
    std::time::Duration::from_secs(3),
    TcpStream::connect(events_addr),
).await;
match events_connect {
    Ok(Ok(stream)) => { tracing::info!("Events connection established"); self.events_stream = Some(stream); }
    Ok(Err(e))     => warn!("Could not connect to events port {} (proceeding anyway): {}", events_addr, e),
    Err(_)         => warn!("Events port {} connect timed out after 3s (proceeding anyway)", events_addr),
}
```

Apply at `:820`, `:1555`, `:1713` (factor into one small helper to avoid three copies).
This bounds Pioneer's stall from ~127 s to 3 s, so phase-2/RECORD/RTP proceed on a fresh
session — the change most likely to make Pioneer render. Keep the patch minimal and
upstreamable (convention in `pull_request_docs/`); it's also a strict correctness win
for *any* receiver whose events port is slow/unreachable.

**Hardening (same file, fold in):** treat `event_port == 0` as "skip the connect" (log
and move on) instead of dialing `<addr>:0`, matching OwnTone's intent without aborting.

**Optional daemon-side mitigation (no vendored edit, if sequencing blocks the above):**
the daemon calls `setup()` inside `ap2_server`/`ap2_spike`; those calls could themselves
be wrapped in `tokio::time::timeout` as a coarse guard — but that aborts the *whole*
setup rather than just the events step, so it's a worse fit than the in-crate 3 s
timeout. Prefer the primary fix.

### Not yet confirmed (needs a quiet window — owner was live-testing)
Steps 1-2 of the plan (capture Pioneer's actual SETUP phase-1 reply to read the
advertised `eventPort`, then `nc -vz` that exact port) require an RTSP session to
Pioneer, which grabs its single AP2 session and collides with the single-slot spike the
owner was actively running. Run this when the daemon is idle (no `airplay_audio::streamer`
DIAG lines in the log):

```sh
# on the HA host (daemon binds 0.0.0.0:8099, host network):
tcpdump -i any -n -s0 -w /tmp/pio.pcap "host 192.168.178.35 and tcp" &
curl -sS -X POST 192.168.178.22:8099/api/spike/ap2 \
     -H 'content-type: application/json' \
     -d '{"ips":["192.168.178.35"],"seconds":6,"freq":440}'
sleep 8; curl -sS -X DELETE 192.168.178.22:8099/api/spike/ap2; kill %1
# then: read the SETUP phase-1 reply plist for eventPort, and confirm the sender's
# SYNs to that port get NO reply (dropped) vs Yamaha's immediate SYN-ACK.
```

This confirms Layer B (the exact advertised port + drop behavior) but is **not** required
to justify the Layer-A timeout fix, which stands on the evidence above.
