# AirPlay 2 **sender** / multi-room — research & spike results

**Goal.** Make this add-on an AirPlay 2 *sender* (source) that drives a
**synchronized multi-room group** across third-party (non-Apple) AirPlay 2
receivers — concretely a **Yamaha WX-021 (MusicCast, `Dusche.local`, 192.168.178.165)**
and a **Pioneer VSX-934 AV receiver (192.168.178.35)**.

This is the *send* direction and is unrelated to the receiver work. Enabling the
`ap2` feature on the vendored `metaneutrons/shairplay-rust` crate only improves us
as an AirPlay 2 *speaker*; it does nothing for sending. See
`architecture-limitations-and-options.md` for the receiver/RAOP context.

> **✅ WORKING (2026-07-25):** A pure-Rust sender (`lmcgartland/airplay2-rs` for
> pairing/SETUP/RTP/streaming) + **OwnTone's MIT `libairptp` via FFI** for PTP makes
> the Yamaha **render audio** (confirmed by listening) — the thing no Rust crate did
> alone. libairptp is vendored + compiled via `build.rs` (cc), wrapped in
> `airptp_ffi.rs` (`AirptpMaster`), and the streamer's PT=87 anchors were switched to
> `CLOCK_MONOTONIC` to match libairptp's grandmaster timeline. Single-device is done;
> multi-room (one libairptp, multiple `airptp_peer_add`) is the remaining wiring.
>
> **GROUND TRUTH (decisive):** **OwnTone — a non-Apple Linux sender — plays to
> BOTH the Yamaha and Pioneer in synchronized multi-room (confirmed by listening).**
> So an AP2 multi-room sender to these receivers is definitively achievable from
> Linux. OwnTone uses the *same* realtime-ALAC (type 96) path as the `lmcgartland`
> Rust crate and also sends **no** `SETRATEANCHORTIME` — the only thing it does that
> the Rust crate doesn't is make the device **lock PTP** (via its MIT-licensed
> `libairptp` grandmaster). Therefore **PTP lock (blocker A) is the sole remaining
> blocker; the timing anchor (blocker B) is NOT needed for realtime.** Path forward:
> either fix the Rust crate's PTP wire-format to match OwnTone (diff via `tcpdump`),
> or FFI OwnTone's MIT `libairptp`+`pair_ap` into the daemon. See "Ground truth" below.
>
> **TL;DR (Rust-crate spikes).** The *reachable* parts are empirically proven against the real devices:
> a Linux sender pairs with both receivers with **no MFi/FairPlay**, and a full
> two-device session (SETUP + RECORD + SETPEERS + 30 s of RTP + volume) runs
> end-to-end. **But no audio comes out of either speaker.** Root cause: the
> receivers never lock to our PTP clock (they send no `Delay_Req`) and the crate
> never sends a `SETRATEANCHORTIME` timing anchor — so the receivers have no
> timeline/anchor to render against. This is the genuinely hard, under-implemented
> frontier (a third-party multi-room *sender* is believed feasible but undemonstrated
> in public code). The current RAOP sender path cannot do this either (AirPlay 1
> only; multiple sinks drift).

---

## Why the current path is insufficient

| | Current add-on sender | Required for AP2 multi-room |
|---|---|---|
| Protocol | AirPlay 1 / RAOP (`libpipewire-module-raop-sink`) | AirPlay 2 |
| Timing | per-sink NTP-style, independent | **one shared PTP grandmaster** |
| Multi-device | N independent sinks → **drift/comb-filtering** | one RTP timeline + shared clock |
| Pairing | RSA/`auth_setup`/`fp_sap25` (RAOP fallback) | HomeKit transient (SRP + ChaCha20-Poly1305) |

PipeWire has **no** AirPlay 2 sender (buffered audio + PTP) and no roadmap item for
one; recent RAOP changes (e.g. `fp_sap25` in 1.4.8) are only compatibility shims so
the *legacy RAOP fallback* keeps connecting to newer devices. Streaming to two
`raop-sink`s does not share a clock and audibly drifts.

## Landscape (open-source AP2 senders)

- **OwnTone** (C, GPLv2) — the *only* mature open-source AP2 multi-room sender. It
  runs its **own PTP grandmaster** via `libairptp` (unlike shairport-sync's `nqptp`,
  which is a receiver-side *slave*). Its two hard parts — `pair_ap` (pairing) and
  `libairptp`/`airptpd` (PTP master) — are **MIT-licensed and reusable standalone**;
  only the ~4.4k-line RTSP sequencer glue is GPLv2 and entangled with its event loop.
- **`lmcgartland/airplay2-rs`** (Rust, GPL-2.0) — the only Rust crate with *real*
  multi-room fan-out: a ~2.4k-line sender-side gPTP implementation, full
  SRP/HomeKit transient pairing, one shared RTP timeline across N receivers. Alpha,
  single-author, dormant since 2026-02, FairPlay left as a deliberate placeholder
  (sidestepped by transient pairing). **This is the crate used in the spike below.**
- **`jburnhams/airplay2-rs`** (Rust, MIT/Apache) — real single-device buffered
  AAC-ELD sending + `SETRATEANCHORTIME`, but multi-room is bookkeeping-only (no PTP
  wiring). Useful as a reference for the buffered path; not a group base.
- **Music Assistant / pyatv / libraop** — reach AP2 hardware only via legacy
  RAOP/NTP; they do sync in their own software, not native AP2 PTP.

**Key de-risking fact:** a *sender* does **not** need FairPlay or Apple secrets.
Confidentiality comes from HomeKit transient pairing; FairPlay keying is a
*receiver*-side obligation (the licensed Yamaha/Pioneer hold those keys).

---

## The spike

**Crate:** `lmcgartland/airplay2-rs` (commit `a7f019f`), cloned to
`~/Entwicklung/airplay2-rs-spike`. Builds clean on Fedora with Rust nightly + gcc
(`fdk-aac` compiles from vendored C).

**Host:** dev box `192.168.178.21`, same LAN as both devices.

**Privileged ports.** AP2 PTP uses UDP **319/320** (privileged). For autonomous
runs without `sudo` we used, once:
```bash
sudo sysctl -w net.ipv4.ip_unprivileged_port_start=53      # allow bind 319/320 as user
sudo firewall-cmd --zone=FedoraWorkstation --add-port=53-1024/udp   # inbound PTP
```
(Both runtime-only; revert on reboot. mDNS/5353 and high ports were already open.)
For production inside the add-on container the equivalent is
`CAP_NET_BIND_SERVICE` (+ `CAP_NET_RAW` not needed) and opening 319-320/udp, or a
shared PTP daemon.

### Device advertisement (both)

`features = 0x445F8A00,0x1C340`, `srcvers=366.0`, `protovers=1.1`, port 7000,
`flags=0x4`, `acl=0` ("Everyone can connect"), no password. Decoded feature bits:
`Audio(9)`, `Buffered Audio(40)`, `PTP(41)`, `HomeKit Pairing(46)`,
`Transient Pairing(48)` all **set**. Bit 26 is reported by the tool as
"MFi Auth Required" — see finding #2, it does **not** actually block us.

### Results

| Step | Yamaha WX-021 | Pioneer VSX-934 | Notes |
|---|:---:|:---:|---|
| mDNS discovery | ✅ | ✅ | correct AP2/PTP/buffered/transient flags |
| Transient SRP pairing (PIN 3939, RTSP, HKP=4, M1–M4) | ✅ | ✅ | session keys + encrypted RTSP established |
| Encrypted RTSP OPTIONS/GET /info | ✅ | ✅ | |
| SETUP over **NTP** | ❌ | — | phase-2 realtime (type 96) **times out** |
| SETUP + RECORD over **PTP** | ✅ | ✅ | realtime stream (type 96) accepted |
| Audio RTP delivered | ✅ (15 s, 1886 pkts) | ✅ | device sends periodic frames back |
| PTP grandmaster (after patch, finding #3) | ✅ | ✅ | real non-zero clock ID in sync anchors |
| **Two-device synchronized group stream** | ✅ 30 s | ✅ 30 s | shared clock + SETPEERS + `SET_PARAMETER` volume, clean teardown |
| **Audible output** | ❌ | ❌ | **silent** — see finding #6 (no PTP lock, no anchor); not a mute (volume was set) |

---

## Key findings

**1. These receivers require PTP, not NTP.** With `timingProtocol=NTP`, SETUP phase 2
(the realtime stream, type 96) times out. With `timingProtocol=PTP`, phase 1 + phase 2
+ RECORD all return 200 and the stream is accepted. Expected, given bits 40/41.

**2. Transient pairing works despite the `MfiRequired` advertisement — no MFi/FairPlay
needed.** Both devices advertise bit 26 (the tool labels it "MFi Auth Required"), yet
plain HomeKit **transient SRP** pairing (PIN `3939`, HKP=4, M1→M4 over RTSP) succeeds
on both, yielding session keys and an encrypted RTSP channel. This retires the single
biggest risk of the whole effort.

**3. Bug found and patched: the sender must stay PTP grandmaster.** The crate's
single-device path always ran the HomePod-oriented **BMCA *yield* flow**
(`run_bmca_yield_flow`, Priority1=250), which is designed to *lose* the master
election and become a slave. Third-party receivers never announce and expect the
**sender to remain grandmaster**. Symptom: `"Timeout waiting for remote Announce,
proceeding as slave anyway"` and the PT=87 audio-sync packets carrying an **all-zero
clock ID**.
The crate already contains the correct flow — `run_ptp_group_master_flow`
(`crates/airplay-timing/src/ptp.rs:1605`), which explicitly *"continues as master
anyway"* and enters a master loop feeding the clock to all peers. It simply was not
wired into the playback path. After the patch (below): the sender logs
`"Entering master loop"` and the PT=87 anchors carry a **real non-zero clock ID**
(e.g. `[18,c5,5c,…]`) — the sender's grandmaster identity the receivers slave to.

**4. Full multi-room group ran end-to-end.** `test_group` with the patch:
primary (Yamaha) becomes grandmaster with a shared clock ID; `SETPEERS` sent to both
with `[.165, .35, .21]`; Pioneer joins the group sharing the **same clock ID**
(offset 0); both stream the same RTP timeline **simultaneously for 30 s**; clean
teardown. No protocol errors.

**5. Non-blocking rough edges.**
- `Failed to set RT priority (need CAP_SYS_NICE)` — a warning; add
  `setcap cap_sys_nice+ep` on the binary (or run privileged) for low-jitter output.
- `play_audio` doesn't self-exit after a stream (its position loop never trips the
  stop condition on these devices); harmless for the spike (`timeout`-bounded).

**6. No audio is rendered (confirmed by listening, volumes up).** The receivers ACK
everything but stay silent. Diagnosis from the logs:
- **Not a mute:** `SET_PARAMETER` volume (=1.0) was sent and ACKed on both devices.
- **No PTP lock:** neither device ever sent a `Delay_Req` or any inbound PTP, even
  with the sender as grandmaster (real clock ID). The receivers are not slaving to
  our clock — our PTP `Sync`/`Announce`/`Follow_Up` are apparently not accepted as a
  valid master (wire-format / message-set / domain mismatch vs. what these devices
  expect). Without a locked clock, an AP2 receiver has no timeline to play against.
- **No anchor:** `SETRATEANCHORTIME` is never sent by the crate (grep-confirmed
  absent). That RTSP verb is how AP2 maps an RTP timestamp to a PTP instant
  ("play frame N at time T"); without it, even a locked receiver has no playout point.

These two are the real remaining blockers, and they are the classic hard parts of
AP2 that the research predicted. `lmcgartland/airplay2-rs` was validated against a
**HomePod** (which becomes master → the *yield* path, well-exercised) and a Samsung
TV; its **group-master path (device slaves to us) is essentially unvalidated**, which
is exactly the path these third-party receivers require.

---

## The patch

`crates/airplay-client/src/connection.rs`, in `setup()` under
`TimingProtocol::Ptp → PtpMode::Master`. Env-gated for minimal blast radius; the
clean upstream form is a real `PtpMode::GroupMaster` variant selected automatically
when the receiver is non-Apple/third-party.

```rust
let master_ip = addr;
// Third-party receivers (Yamaha/Pioneer/etc.) expect the SENDER to remain PTP
// grandmaster and provide the clock; they never announce/yield like a HomePod.
// AIRPLAY_PTP_STAY_MASTER selects the group-master flow (pri1=1 => always win
// BMCA, never yield) instead of the HomePod yield flow.
let stay_master = std::env::var("AIRPLAY_PTP_STAY_MASTER").is_ok();
self.ptp_master_sync_task = Some(tokio::spawn(async move {
    let res = if stay_master {
        run_ptp_group_master_flow(vec![master_ip], 1, clock_id_tx).await
    } else {
        run_bmca_yield_flow(master_ip, 250, offset_tx, clock_id_tx).await
    };
    if let Err(e) = res { tracing::error!("PTP flow error: {}", e); }
}));
```

`crates/airplay-client/examples/test_group.rs`: stream duration bumped 15 s → 30 s
for a longer listening window (spike convenience only).

## Reproduce

```bash
cd ~/Entwicklung/airplay2-rs-spike
# 1. discovery
cargo run -q -p airplay-discovery --example debug_devices -- --info
# 2. pairing (no privileged ports needed)
cargo run -q -p airplay-pairing --example pair_with_device -- \
    --match 192.168.178.165 --mode transient-rtsp --pin 3939
# 3. single device, sender-as-grandmaster (needs 319/320)
AIRPLAY_PTP_STAY_MASTER=1 cargo run -q -p airplay-client --example play_audio -- \
    192.168.178.165 7000 tone_30s.wav --airplay2 --ptp --ptp-master
# 4. synchronized two-device group
AIRPLAY_PTP_STAY_MASTER=1 cargo run -q -p airplay-client --example test_group -- \
    192.168.178.165 192.168.178.35 tone_30s.wav
```
(`tone_30s.wav` = a 30 s 440 Hz stereo tone generated in the spike dir.)

---

## Second crate spiked: `jburnhams/airplay2-rs`

Cloned to `~/Entwicklung/airplay2-jburnhams-spike` (commit `527884f`, MIT/Apache,
single crate, edition 2024). Custom example `examples/spike_play.rs` targets a device
by name/IP, forces us to PTP master (`PTP_PRIORITY=1`), streams a 30 s sine, and
reports `ptp_status()`. Run:
`PTP_PRIORITY=1 cargo run --example spike_play -- 192.168.178.165 30`.

Result against the Yamaha:
- **Pairing:** Auth-Setup + transient SRP (M4) succeed — consistent with crate #1.
- **Smarter AP2 handling:** it detects buffered audio, logs *"Skipping ANNOUNCE for
  PTP/Buffered Audio device"*, uses PTP, and **SETUP Step 1 → 200** with the device
  echoing `SupportsClockPortMatchingOverride` + our addresses.
- **SETUP Step 2 → 400 (fails, never streams).** Root cause: its *sender* Step-2 is
  built from a **RAOP-style `Transport:` header** (`RTP/AVP/UDP;unicast;mode=record;…`
  via `setup_stream_request`, `src/protocol/rtsp/session.rs:157`) instead of an AP2
  **binary-plist `streams` array** (type 103 buffered / 96 realtime). The plist-streams
  SETUP exists only in the crate's *receiver* code. jburnhams' CI passes because its
  Python reference receiver accepts the RAOP transport; real Yamaha/Pioneer reject it.

**The two Rust crates are complementary — neither is complete for these devices:**

| | `lmcgartland` | `jburnhams` |
|---|---|---|
| Pairing (transient, no MFi) | ✅ | ✅ |
| SETUP (AP2 plist `streams`) | ✅ 200 (type 96) | ❌ 400 (sends RAOP `Transport`) |
| RECORD + RTP transport | ✅ | never reached |
| PTP: stays grandmaster | ✅ (after our patch) | ✅ (`PTP_PRIORITY=1`) |
| PTP: device actually locks | ❌ (no `Delay_Req` back) | untested (dies at SETUP) |
| `SETRATEANCHORTIME` anchor | ❌ absent | ✅ present (but stranded behind SETUP 400) |
| Audio out | ❌ silent | ❌ never streams |

Net: `lmcgartland` has the **correct SETUP** but **no anchor** and **no PTP lock**;
`jburnhams` has the **anchor** but the **wrong SETUP**. A working Rust sender would
need `jburnhams`' anchor path behind `lmcgartland`'s (correct) plist SETUP, *plus*
solving PTP lock — i.e. real reverse-engineering on either base. OwnTone already does
all three correctly against third-party receivers.

## Ground truth: OwnTone works on both devices ✅

Ran the official `owntone/owntone` Docker image (Alpine/OpenRC; run `/usr/sbin/owntone`
directly, host networking, host D-Bus mounted so it uses the host Avahi):

```bash
docker run -d --name owntone-spike --network host \
  -v ~/Entwicklung/owntone-spike/config/owntone.conf:/etc/owntone.conf:ro \
  -v ~/Entwicklung/owntone-spike/music:/music:ro \
  -v /run/dbus:/run/dbus \
  --entrypoint sh owntone/owntone:latest -c \
  'adduser -D -g owntone -h /dev/null -s /sbin/nologin owntone 2>/dev/null; \
   mkdir -p /var/cache/owntone && chown -R owntone /var/cache/owntone; \
   exec /usr/sbin/owntone -f -c /etc/owntone.conf'
# then via JSON API on :3689 — select both outputs, queue a track, play:
curl -X PUT  localhost:3689/api/outputs/set -d '{"outputs":["<dusche-id>","<pioneer-id>"]}'
curl -X POST 'localhost:3689/api/queue/items/add?uris=library:track:1&clear=true&playback=start'
```

**Result: audible tone on BOTH the Yamaha (Dusche) and Pioneer simultaneously.**
OwnTone discovered both as `type: "AirPlay 2"`, negotiated `format: alac` (realtime,
type 96), RECORD succeeded on both, and streamed with its own PTP grandmaster.

Implications (this reframes everything above):
- A Linux, non-Apple, multi-room AP2 **sender to these exact receivers is proven**.
- Since OwnTone uses the same realtime-ALAC path as `lmcgartland` and **no**
  `SETRATEANCHORTIME`, the delta that makes audio play is **PTP lock**. Getting the
  receivers to slave to our PTP clock is the one thing to solve; the anchor is not
  required for realtime.
- The proven ingredient is OwnTone's **MIT** `libairptp` (PTP grandmaster) + `pair_ap`.

## PTP wire diff — ROOT CAUSE FOUND ✅

Captured PTP (UDP 319/320) for both senders against the same Yamaha
(`capture_ptp_diff.sh` → `~/Entwicklung/airplay2-ptp-capture/*.pcap`) and diffed with
`tshark`. OwnTone: 275 pkts out, **101 back from the device (incl. 73 `Delay_Req`)** —
the device actively slaves. Rust: 108 pkts out, **0 back**. Field diff:

| PTP field | OwnTone (works) | Rust `lmcgartland` (ignored) |
|---|---|---|
| **majorSdoId** (transportSpecific nibble) | **gPTP (0x1)** | **0x0 (plain IEEE-1588)** ← decisive |
| **PTP_UNICAST flag** | set | **not set** |
| **PTP_TIMESCALE flag** | set | not set |
| flags value | `0x0408` / `0x0608` | `0x0000` / `0x0200` |
| grandmasterClockClass | 6 (locked-to-primary) | 248 (free-running) |
| priority1 | 128 | 1 |
| Sync/Follow_Up TLV | Organization-extension (proper gPTP) | malformed "Unknown 0x0016/0x0027" |

**Root cause:** AirPlay 2 timing is **gPTP (IEEE 802.1AS)**; the first header byte's high
nibble must be `0x1`. `crates/airplay-timing/src/ptp.rs:372` writes
`buf[0] = self.message_type as u8` (high nibble 0 = plain PTP), and the flags field
omits UNICAST+TIMESCALE. The Yamaha/Pioneer silently drop non-gPTP frames, so they never
lock, never `Delay_Req`, never render.

**The fix (small, high-confidence), in `ptp.rs`:**
```rust
buf[0] = 0x10 | (self.message_type as u8);   // majorSdoId = gPTP (0x1)
// flags: OR in UNICAST (0x0400) + TIMESCALE (0x0008) alongside TWO_STEP (0x0200)
//   → two-step msgs 0x0608, one-step 0x0408
```
Secondary (match OwnTone, likely needed for BMCA/quality): `grandmasterClockClass = 6`,
`priority1 = 128`, and a correct gPTP Follow_Up **Organization-extension TLV**
(orgId 00:80:C2, subType 1) instead of the current malformed TLV bytes.
**Verifiable without listening:** after the fix, a re-capture should show the device
sending `Delay_Req` back to us (it's currently 0).

**Fix attempt 1 (applied in `~/Entwicklung/airplay2-rs-spike`): NECESSARY BUT NOT
SUFFICIENT.** Patched `serialize()` (gPTP nibble + UNICAST/TIMESCALE flags), set
Announce `clockClass=6`, and bumped the `Delay_Req` log to `info!`. The sender now
reaches the master loop, but the Yamaha still sends **0** packets back → still no lock,
still silent. So ≥1 more gPTP requirement remains — prime suspect the mandatory
**802.1AS Follow_Up Information TLV** (Organization-extension, orgId 00:80:C2,
subType 1): OwnTone sends 202 of these; lmcgartland emits malformed TLVs
("Unknown 0x0016/0x0027"). A conformant gPTP slave rejects Sync/Follow_Up without it.
Implication: full Apple-gPTP conformance is multi-step RE (the nibble was only step 1),
which **strengthens the case for FFI'ing OwnTone's proven MIT `libairptp`** over
hand-reimplementing gPTP in Rust. Efficient further iteration needs a wire capture per
attempt (`capture_ptp_diff.sh`).

## Recommended path (updated after ground truth)

1. **Capture the PTP wire reference (do this first).** With OwnTone streaming:
   `sudo tcpdump -i enp5s0 -w /tmp/owntone_ptp.pcap 'udp port 319 or udp port 320'`
   and the same for the `lmcgartland` run. Diff the `Sync`/`Follow_Up`/`Announce`
   (domain number, one- vs two-step, Follow_Up TLV, multicast `224.0.1.129` vs unicast,
   `clockClass`/`priority`). This reveals exactly why our PTP isn't accepted.
2. **Then pick the integration base:**
   - **Fix-the-Rust-crate:** patch `lmcgartland`'s `run_ptp_group_master_flow` to match
     OwnTone's PTP bytes. Cheapest if the delta is small; keeps the daemon pure-Rust
     (but GPL-2.0).
   - **FFI OwnTone's MIT libs** (`libairptp` + `pair_ap`) into the Rust daemon and
     reimplement the ~4.4k-line RTSP sequencer. Higher effort, but builds on the exact
     code proven to work here, and avoids GPL-2.0. This is the originally-planned route.
3. Integrate into the daemon: PipeWire capture node → ALAC encode → sender; expose as
   an "AirPlay 2 group output" in the routing matrix, replacing `raop-sink` for AP2 devices.

## Superseded / lower-priority next steps (pre-ground-truth)

The two blockers below decide whether this becomes real. The best reference for both
is **OwnTone**, which *does* drive third-party AP2 receivers as a grandmaster in
production — its `libairptp` (PTP wire format) and its `SETRATEANCHORTIME` handling in
`src/outputs/airplay.c` are the gold standard to compare our bytes against.

1. **Make the receivers slave to our PTP (blocker A).** Capture PTP on the wire
   (`tcpdump -i enp5s0 udp port 319 or udp port 320`) while streaming, and diff our
   `Sync`/`Follow_Up`/`Announce` against what OwnTone emits to the same device. Likely
   culprits: PTP domain number, two-step vs one-step, the Follow_Up TLV, multicast
   (`224.0.1.129`) vs unicast, or `clockClass`/`priority` values. Until a device sends
   `Delay_Req` back, it is not locking.
2. **Send a timing anchor (blocker B).** Implement `SETRATEANCHORTIME` (RTP-ts ↔
   PTP-time ↔ rate) — absent from `lmcgartland`. `jburnhams/airplay2-rs` has a working
   `SETRATEANCHORTIME`/buffered path to crib from; also try **buffered audio (type 103
   + AAC)** rather than realtime (type 96), since these devices advertise buffered.
3. **Reconsider the base.** Given blockers A+B are exactly OwnTone's proven strengths,
   seriously weigh building the sender on OwnTone's **MIT** `pair_ap` + `libairptp`
   (reimplementing only the RTSP sequencer) instead of finishing `lmcgartland`'s
   unvalidated master path. This also avoids GPL-2.0 (see Licensing).
4. **Upstream the master-flow fix** as `PtpMode::GroupMaster`, auto-selected for
   non-Apple receivers, instead of the env flag (only relevant if we stay on `lmcgartland`).
5. **RT priority / caps:** grant `CAP_NET_BIND_SERVICE` (ports 319/320) + `CAP_SYS_NICE`
   (jitter) in the add-on container; open `319-320/udp`.
6. **Integrate into the daemon (only after audio works):** feed PCM from a PipeWire
   capture node → ALAC/AAC encode → sender; expose the group as a new "AirPlay 2 group
   output" in the routing matrix, replacing the RAOP `raop-sink` path for these devices.

## Licensing

`lmcgartland/airplay2-rs` is **GPL-2.0** (vs. the LGPL-3.0 vendored shairplay-rust).
If we vendor it, the daemon becomes GPL-2.0. Alternative: reimplement the ~4.4k-line
RTSP sequencer against OwnTone's **MIT** `pair_ap` + `libairptp` to avoid GPL and lean
on battle-tested C timing/pairing.

## Sources

- OwnTone AP2 sender: `src/outputs/airplay.c`; PTP master `src/libairptp/README.md`;
  pairing `src/pair_ap/` — https://github.com/owntone/owntone-server
- Rust sender used here: https://github.com/lmcgartland/airplay2-rs
- Buffered-mode reference: https://github.com/jburnhams/airplay2-rs
- Protocol: https://emanuelecozzi.net/docs/airplay2/ ; https://openairplay.github.io/airplay-spec/
