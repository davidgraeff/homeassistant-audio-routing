# RTP / Bluetooth input dropouts — localised to the Bluetooth leg

**Symptom (reported 2026-07-28).** Music from the Bluetooth bridge disappears **every
5–10 minutes for up to ~2 minutes**, then returns on its own. The phone keeps playing
throughout — and since Bluetooth's own clock drives that leg, the A2DP link is not the
suspect. The fault is somewhere in **Pi bridge → LAN → `module-rtp-source` → graph**.

> **2026-07-28, localised by live measurement.** The premise above is wrong, and so is
> every hypothesis built on it. The audio is **already digital silence at the Bluetooth
> A2DP boundary on the Pi**, before it reaches the bridge's PipeWire graph — the phone
> transmits a *continuous, full-bitrate* aptX stream that decodes to zeros. Nothing in
> the bridge's graph, the LAN, the multicast group or `module-rtp-source` misbehaves
> during a dropout; they faithfully carry, and reproduce, the silence they are given.
>
> One question remains, and it decides whose bug this is: whether the **phone sends**
> silence or the **Pi's aptX decoder produces** it. The owner reports this phone works
> with every other Bluetooth receiver, which makes the Pi's decoder the leading
> candidate — see the end of §1a for the one test that settles it.
>
> §1a is the evidence; §4 records what each hypothesis turned out to be worth; §5 is
> the one real defect found on our side (`rtp_membership.rs`, since **deleted**).
>
> The original plan below is preserved because the *measurement design* (§3) is what
> produced the answer — but read §1a first.

> **2026-07-29 — §1b answers "does watching it change it?"** A 2 h session with the
> new [bluetooth-testing-app](../../firmware/pi-bridge/bluetooth-testing-app/)
> running *looked* dropout-free. It was not: one ~40 s silence is in the far-end
> record, the app's own log claims a 149 s episode that was largely its own reader
> stalling, and the app had **three defects that fabricate this exact symptom**. It
> also perturbs the graph in three measurable ways. §1b has the numbers and the one
> experiment that settles the observer question.

Current configuration of that source (`/api/sources`):

| Field | Value |
|---|---|
| `source_addr` | `239.255.42.42` (**multicast**) |
| `port` | 46000 |
| `rate` | 48000 |
| `latency_msec` | 100 |
| `ignore_ssrc` | `true` |
| sender | 192.168.178.78, RTP **PT=127**, 1252-byte UDP payloads |

---

## 1a. What actually happens (measured 2026-07-28, 18:21–18:35 CEST)

Two dropouts were caught with all four layers instrumented simultaneously, plus a
third-host observer on the wire and a probe on the bridge itself. The chain was walked
end to end, and the fault was found at the *first* link, not the last.

**On the wire (independent observer, 192.168.178.21, wired, same /24 — joins the group
and decodes the RTP headers *and payload*):**

```
18:28:57 pkts=155  peak=15801  dBFS=-6.3   zeropkts=0    seqgap=0 lost=0 ssrcchg=0 tsjump=0 tsd=[310] audio
18:28:58 pkts=155  peak=11759  dBFS=-8.9   zeropkts=140  seqgap=0 lost=0 ssrcchg=0 tsjump=0 tsd=[310] audio
18:28:59 pkts=154  peak=0      dBFS=-inf   zeropkts=154  seqgap=0 lost=0 ssrcchg=0 tsjump=0 tsd=[310] SILENCE
...                                        (115 s, 17 794 packets, all zero payload)
18:30:53 pkts=159  peak=0      dBFS=-inf   zeropkts=159  seqgap=0 lost=0 ssrcchg=0 tsjump=0 tsd=[310] SILENCE
18:30:54 pkts=153  peak=24511  dBFS=-2.5   zeropkts=101  seqgap=0 lost=0 ssrcchg=0 tsjump=0 tsd=[310] audio
18:30:55 pkts=154  peak=0      dBFS=-inf   zeropkts=154  seqgap=0 lost=0 ssrcchg=0 tsjump=0 tsd=[310] SILENCE
```

During a dropout the RTP stream is not merely present, it is **pristine**: a steady
155 packets/s, RTP timestamp advancing exactly 310 frames per packet with no
discontinuity, no SSRC change, and 10 lost packets across 282 silent seconds
(≈0.02 %, the same ambient WiFi-multicast loss seen while audio is healthy). The
payload is `s16le` stereo, and **every sample in every packet is zero**.

The single loud second at 18:30:54 (−2.5 dBFS) between two silent stretches, and the
loud onset at recovery (−2.2 dBFS), are the signature of a source buffer being flushed
— not of a receiver resynchronising.

**On the bridge (Pi Zero 2 W, during the dropout):**

| Measurement | Result | Meaning |
|---|---|---|
| `pw-record` on `bluez_input.64_B5_…` (the phone's A2DP source node) | **peak 0** | the phone's stream is *already* silence when it enters PipeWire |
| `pw-record` on `rtp-bridge` (the sender sink) | peak 0 | consequence |
| `pw-link -l` | links intact, 6 links | not a WirePlumber re-bind to another default source |
| `MediaTransport1.State` | **`"active"`** | A2DP was never suspended |
| `api.bluez5.codec` / node `state` | `aptx` / `running` | decoder alive, not idle |
| HCI ACL RX (`hciconfig hci0 -a`) | 224 385 B / 5 s = **44.9 kB/s** | the phone is transmitting at full aptX rate |
| HCI ACL RX **while healthy** | 45.9–46.6 kB/s | *the same rate* — only the content changes |
| `/proc/net/snmp` UDP OutDatagrams | ~160/s throughout | the Pi never stopped sending |

The last two rows are the crux: **the phone's A2DP bitrate is identical whether the
music is audible or gone.** Its Bluetooth stack streams ~370 kbit/s of aptX
continuously and simply fills it with zeros while its player is not producing audio.

**On the HA host, throughout both dropouts:**

```
18:29:21 wire=156 mod=0/0 ka=211904/425304 igmp=2
```

`wire` = packets/s arriving on `end0`; `mod` = the module's group-bound socket
`rx_queue/drops`; `igmp` = sockets holding the membership. The module's socket showed
**rx_queue 0 and drops 0 for the entire duration** — it read every packet — and
`rtp.receiving` stayed `"true"` with node `state: "running"`. It was handed silence and
it faithfully output silence.

### The causal chain

```
phone's player produces no audio (buffer stall)
  → Android keeps the A2DP transport "active" and streams full-rate aptX ZEROS
    → Pi bluez_input decodes zeros            (measured: peak 0)
      → loopback → rtp-bridge → multicast RTP (measured: perfect stream, zero payload)
        → module-rtp-source reads every packet (measured: rx=0 drops=0, receiving=true)
          → sendspin/AP2 encode silence        (measured: relay 'largest packet' 3 B)
            → speakers silent
```

Every component from the Pi's PipeWire graph rightwards is working exactly as
designed. **This is not a bug in this project.**

### Episode statistics

`module-rtp-source` never logs, but the sendspin relay line prints `largest packet`
every 10 s, and digital silence Opus-encodes to ~3 B against 400–700 B for real audio.
That makes the daemon's own log a free, retroactive, 10 s-resolution record of the
symptom. Over 64.5 min:

| # | start (UTC) | duration | gap since previous start |
|---|---|---|---|
| 1 | 15:33:08 | 10 s | — |
| 2 | 15:44:43 | 70 s | 11.6 min |
| 3 | 15:56:05 | 110 s | 11.4 min |
| 4 | 15:59:06 | 230 s | 3.0 min |
| 5 | 16:09:07 | 261 s | 10.0 min |
| 6 | 16:17:49 | 70 s | 8.7 min |
| 7 | 16:29:10 | 111 s | 11.3 min |
| 8 | 16:31:11 | 160 s | 2.0 min |

**26.4 % of the hour was silent.** Durations are scattered from 10 s to 261 s and
intervals from 2 to 11.6 min — matching no fixed timer in the system, which was the
first thing to rule the timer hypotheses out (§2). Episodes 7 and 8 are the two caught
at 1 s resolution above (16:29:10 UTC = 18:29:10 CEST), so the cheap proxy and the wire
observer agree.

### The one thing still open, and how to close it

The measurements above localise the fault precisely: **the Pi receives full-rate aptX
frames that decode to zeros.** That is as far as the receive-side evidence can take it.
Two candidates remain, and they differ only in which side of the aptX codec is at fault:

- **A — the phone transmits silence.** A stalled streaming player (the source was an
  88-minute festival mix, and buffer-flush transients bracket every episode) while
  Android keeps the A2DP transport up.
- **B — the Pi's aptX decode path emits zeros** while consuming frames at the correct
  rate.

**B is now the leading candidate.** The owner reports (2026-07-28) that **this phone
works with every other Bluetooth receiver** — so whatever it emits is fine everywhere
else, and the thing unique to the failing path is this Pi's decoder. That also fits the
otherwise-odd details: episode durations that grow steadily (10 → 261 s) rather than
tracking a player's buffer state, the phone's link quality reading 197 against 255 for
the other connected device, and clean *digital* zeros rather than the partial or
glitched output a truncated stream would give.

It also has precedent on this box: the 2026-07-27 "BT stutter" diagnosis found a fault
at the A2DP-source layer on this same bridge, and this Pi runs **libpipewire 1.4.2** —
older than the 1.6.2 on the HA host.

The decisive test needs a deliberate config change on live audio, so it was not run
unprompted: **force the A2DP codec to SBC** and watch several dropout intervals.

```bash
# on the Pi — switch the negotiated A2DP codec away from aptX
ssh david@192.168.178.78
export XDG_RUNTIME_DIR=/run/user/1000
wpctl status | grep -i bluez                    # find the device id
wpctl set-param <device-id> ...                 # or: bluetoothctl -> select codec
pw-cli info bluez_input.64_B5_F2_F9_A9_4A.2 | grep api.bluez5.codec   # confirm sbc
# then re-run observer.py and compare the silent-episode rate
```

Reading it: **silences vanish under SBC ⇒ B**, the aptX decode path on the Pi — a real
bug for the bridge's backlog, and a codec pin is the immediate workaround. **Silences
persist under SBC ⇒ A**, the phone, and this is closed as not-our-bug.

Two zero-risk cross-checks worth doing first, since they need no config change:

- Play a **local file** on the phone rather than a streamed one. If a local file also
  drops out, a rebuffering player (A) is excluded without touching the codec.
- Watch the observer while a **different phone** streams to this same bridge. A second
  phone failing the same way points hard at B.

Note that "it works with every other receiver" does not *by itself* exclude A: other
receivers may negotiate SBC rather than aptX, in which case they exercise a different
decode path and the phone's aptX output specifically could still be at fault. The SBC
test resolves that ambiguity too — which is why it is the one to run.

---

## 1b. Does monitoring change the fault? (2026-07-29)

The question after a 2 h session that felt clean: could running the testing app —
which holds a `pw-record` on the very node under suspicion — have suppressed the
dropouts, e.g. by keeping some timeout from firing?

**Timeouts: no.** Nothing in the causal chain of §1a is timeout-driven, both ~2 min
timers were already excluded (§2), and no client on the Pi can stop a phone or an
aptX decoder from emitting zeros.

**But the run was not clean, and the instrument was not trustworthy.**

### The two records of that session

| Witness | What it says |
|---|---|
| the app's episode log | one **148.9 s** episode at 22:31:01–22:33:29 CEST, plus nine 0.5–2.6 s blips; `silent_duty` 0.28 % over 108.9 min |
| the relay `largest packet` proxy on the HA host (zero footprint on the Pi) | audio 22:02–22:32:39, **SILENT 22:32:52–22:33:22** (~40 s), then 59 min clean, then end of stream 23:32:42 |

Clocks on Pi/HA/dev were checked and in sync, so those overlap only in their last
~30 s: the relay was still encoding 500–600 B packets through most of the app's
"episode". **Silent duty fell from 26.4 % to ~0.7 %, but one real dropout remains.**
Log retention on the add-on truncated at 22:02, so only the last 1.5 h has a
far-end record — raise it, or `tee` the relay lines, before the next run.

### The instrument was fabricating the symptom — caught in the act

Queried after the phone had disconnected, the app reported an **8-minute and
growing digital-silence streak** with `error: None` and a **frozen block cursor**,
while `pw-link -l` showed its stream connected to `rtp-bridge:monitor`. Three
defects, each independently sufficient to invent a dropout, all fixed
[in the app](../../firmware/pi-bridge/bluetooth-testing-app/) (2026-07-29):

1. **`pw-record --target` is a preference, not a pin.** When the target vanishes
   PipeWire rebinds the stream to the default source. Reproduced on demand by
   destroying a target under a live capture: the unpinned control moved to
   `rtp-bridge:monitor`; with `node.dont-reconnect=true` the stream is left
   unlinked instead. Caveats: `pw-record` does *not* exit, and a target missing at
   *startup* still falls back — so a link check is mandatory, not optional.
2. **The streak grew on wall-clock time** (`now - silent_since`), so it counted up
   while no blocks arrived at all. Silence is now counted in blocks.
3. **An episode could be entirely a stall.** One zero block set the run, any gap
   followed, and the next real block closed an "episode" whose duration was the
   gap. That is the most likely origin of the 148.9 s above. Gaps now end a run and
   are logged separately as lost data.

Which reframes the numbers from every earlier session: **`coverage` over that 2 h
was 0.92** — 326 622 blocks = 108.9 min of audio inspected across 117.8 min of
uptime. Nine minutes were never looked at, and the UI implied an unbroken record.

### It is also not a passive observer

Three real perturbations, worth knowing before treating a monitored run as
representative:

- **Quantum.** Its `pw-record` requests `node.latency = 960/48000`, and PipeWire
  takes the minimum in the driver's graph, so the bluez-driven graph runs at 960
  instead of the 1024 default. The bluez A2DP source's decode-buffer target and
  rate-matching derive from that duration — the exact buffer whose underrun emits
  zeros.
- **An always-active second consumer** on the suspect node: never idle, never
  suspended, links not re-evaluated.
- **CPU.** `block_peak_rms` runs a per-sample Python loop over 96 k samples/s on a
  Zero 2 W, and `pw-top` charged its stream **1621 xruns** against 887 for
  `bt-bridge-capture` and 117 for `rtp-bridge`. That pushes *toward* underruns, so
  it cannot explain an improvement — but it is why the app's own reader stalls.

### The likelier reason the rate collapsed

**The Pi had rebooted at 21:06:48 and PipeWire/WirePlumber started at 21:06:45** —
the entire clean run was on a fresh session, where the 26 %-duty hour was a
long-lived one with reconnect history. Episodes growing 10 → 261 s fit accumulating
state that a restart clears. No codec drop-in was written, so negotiation was still
free (presumably aptX again), and the content differed.

### The experiment that answers it

Stream ≥1 h with **no client on the Pi at all** and judge from the relay proxy
alone; compare with an identical run with the app attached. Until that A/B exists,
neither "monitoring suppresses it" nor "the reboot fixed it" is established — and
the SBC test at the end of §1a is still the one that decides whose bug this is.

---

## 2. Why "~2 minutes" was a red herring

Two timers do sit near two minutes, and both were checked:

- **IGMP general query interval** on a typical switch/AP is **125 s**.
- `module-rtp-source`'s own session timeout. **Now read from the source**
  (`module-rtp-source.c`, PipeWire 1.6.2): `DEFAULT_CLEANUP_SEC = 60`, and because
  `on_standby_timer_event` walks `STATE_RECEIVING → STATE_PROBE → STATE_STOPPING` one
  transition per tick, standby needs **60–120 s without packets** to engage. That is
  the "about two minutes" the symptom description suggested.

Both are dead ends here, for the same reason: the measured **durations are 10, 70, 110,
230, 261, 70, 111 and 160 s**. No fixed timer produces that spread. And the module's
60–120 s figure is the time to *enter* standby; resume is immediate on the next packet
(`on_rtp_io` → `do_start`), so it cannot hold audio down for minutes while packets flow.

The recovery interval was indeed the cheapest discriminator available — it just pointed
away from every candidate rather than at one.

---

## 3. The measurement that produced the answer

The design below was right and is worth keeping; the one thing it was missing is the
measurement that turned out to be decisive — **the RTP payload level**. Layers a–d tell
you *whether packets flow and who reads them*, but a stream of perfectly-delivered
zeros looks identical to a healthy stream in every one of those columns. Always measure
what is *in* the packets, not just that they arrived.

All four probes plus the two that mattered are saved and re-runnable; paths are noted at
the end of this section.

```bash
# on the HA host; writes /tmp/rtp-probe.log at 1 s cadence, a few bytes/s
# (disk is tight on this box -- do not let tcpdump write a pcap here)
GROUP_HEX=2A2AFFEF        # 239.255.42.42, little-endian as /proc prints it
PORT_HEX=B3B0             # 46000
# layer (a): ONE long-lived tcpdump, aggregated to one line per second by awk.
# Spawning `timeout 1 tcpdump` every second (as this plan first suggested) races
# its own startup and under-counts.
( tcpdump -i end0 -n -q -tt "udp port 46000 and host 239.255.42.42" 2>/dev/null \
  | awk '{s=int($1); if (s!=p) { if (p) {print p, c; fflush()} p=s; c=0 } c++}' \
  > /tmp/rtp-wire.log ) &
# layers (b)(c)(d): the module's socket, our keepalive's socket, group membership.
# NOTE busybox awk on HAOS has no strtonum(); convert the hex rx_queue in bash.
```

Two practical traps worth recording, both of which cost time here:

- **`/proc/net/udp` has two sockets on port 46000.** `2A2AFFEF:B3B0` is the module's
  (bound to the group address); `00000000:B3B0` is our keepalive (wildcard). Keying on
  the wrong one tells you nothing about the module.
- **`pkill -f <pattern>` matches the calling shell's own command line**, because the
  pattern is *in* it. It killed two ssh sessions before the probes were moved into
  scripts on the target. Use a pidfile, or the `rtp-[p]robe.sh` bracket trick. (It
  bit again on 2026-07-29: `pkill -f "python3 app.py"` over ssh killed its own shell
  before the restart it was supposed to precede.)
- **`pw-record --target X` does not guarantee you are recording X.** It is a
  preference: if `X` is absent — or disappears mid-capture — the stream is bound to
  the default source instead, which on this bridge is `rtp-bridge:monitor`, and it
  reports **digital silence** with no error. A probe that concludes "the A2DP source
  is silent" must therefore prove it was attached: add
  `-P '{ node.name=probe node.dont-reconnect=true }'` and check
  `pw-link -l | grep -B1 probe`. Verified on the bridge (WP 0.5.8 / PW 1.4.2) —
  §1b. The §1a readings survive this because the node was confirmed present and
  `running` with links intact at the time.

### The payload probe — the one that mattered

A third host on the LAN joins the group and reports, per second, packet count, RTP
seq/timestamp/SSRC integrity **and the peak PCM sample of the payload**:

```python
a = array.array("h"); a.frombytes(pkt[12:])      # s16le stereo @48k, 310 frames
peak = max(max(a), -min(a))                      # peak == 0  =>  sender sends silence
```

This doubles as the independent observer H4 asked for, and it costs nothing on the
constrained boxes because it runs on a dev machine.

**Cheaper audio proxy, already in the log:** with a sendspin group on Opus, the relay
line prints `largest packet`. Digital silence encodes to ~3 B; real audio is 500–650 B.
So `docker logs … | grep 'largest packet'` is a free 10 s-resolution record of whether
audio reached the encoder — **retroactively**, which is how the episode table in §1a was
built before any new instrumentation existed. Start here next time.

It has a second virtue that only became clear in §1b: it is the **only witness with
no footprint on the Pi**, so it is the one that can be trusted when the question is
whether the instrument is changing the outcome — and it is what caught the
testing-app's fabricated 149 s episode. Its weakness is retention: `docker logs`
had already truncated the first half of a 2 h run. Raise the add-on's log retention
or `tee` these lines to a file before a long session.

```bash
ssh root@homeassistant.local "docker logs addon_local_pipewire_audio_router 2>&1 \
  | grep -a 'largest packet'"
```

### Reading the result

| wire | payload peak | module socket | igmp users | ⇒ conclusion |
|---|---|---|---|---|
| >0 | **0** | rx=0, drops=0 | any | **sender is transmitting silence** — go upstream to the bridge and its Bluetooth source ← *this is what happened* |
| >0 | >0 | rx=0, drops=0 | ≥2 | receiver-side: module reads and discards (H3) |
| >0 | >0 | rx grows / drops grow | any | module **not reading** its socket (stalled thread) |
| 0 pkts | — | rx=0 | any | sender stopped or the path lost it (H1/H2/H4) |

### Saved probes

`observer.py` (payload/RTP integrity, run on a dev box), `host-probe.sh` (four-layer
receive side), `pi-probe.sh` (UDP OutDatagrams + link check), `mpris-probe.sh`
(AVRCP + HCI byte rate), `mine_relay.sh` + `episodes.py` (retroactive episode table
from the daemon log), `snapshot.sh` (one-shot receive-side state incl. `rtp.receiving`
and node peak).

---

## 4. Hypotheses — disposition

**H0 (not in the original list) — the sender transmits silence. ✅ CONFIRMED.** See §1a.
The plan had no entry for this, and that is the lesson: §1's observation (packets
arriving, socket draining, drops 0, `pw-record` peak 0) is explained *equally well* by
"the receiver discards good audio" and by "the receiver faithfully reproduces silence".
Nothing in that measurement set distinguishes them. One payload check would have.

**H1 — multicast membership expiry in the path. ❌ DEAD, twice over.** Membership was
`igmp_users = 2` continuously, through both dropouts. Beyond that it is *structurally*
excluded: our keepalive holds the **host** membership, and Linux delivers a multicast
datagram to any socket bound to the matching address/port whose `mc_list` has no
entry for the group (`ip_mc_sf_allow` returns true when no filter matches). So the
module's socket keeps being fed even if the module's own join were lost — which is
exactly what `mod=0/0` showed.

**H2 — WiFi multicast handling. ❌ DEAD as the cause.** Real but negligible: ~0.02 %
single-packet loss, at the *same* rate during silence and during healthy audio. Never
a gap, never a stall.

**H3 — stale module session / timestamp base. ❌ DEAD, and its stated mechanism does
not exist.** Read from `module-rtp/audio.c` (1.6.2):

- There is **no jitter-window discard**. `rtp_audio_receive` rejects a packet only for
  being short, having a bad version, or an unexpected SSRC. Everything else is written
  into the ring at `timestamp + target_buffer`, out-of-order packets included.
- **`ignore_ssrc = true` is the *safe* setting, the opposite of what this plan
  claimed.** The code is `impl->have_ssrc = !impl->ignore_ssrc;` guarding
  `if (impl->have_ssrc && impl->ssrc != hdr->ssrc) goto unexpected_ssrc;`. With
  `ignore_ssrc = true`, `have_ssrc` is permanently false and **no packet is ever
  rejected on SSRC grounds** — a sender restart with a fresh SSRC is accepted
  seamlessly. The proposed "cheap experiment" of setting it `false` would *add* the
  only SSRC rejection path there is. **Do not run that experiment**; leave it `true`.
- The actual silence generator, for the record, is the read side: when
  `avail < wanted` it does `memset(d[0].data, 0, wanted * stride)` and sets
  `SPA_CHUNK_FLAG_EMPTY`. But it also clears `have_sync`, and the next received packet
  re-syncs the ring in full (`readindex = timestamp`, `writeindex = write`). So a
  receiver-side underrun self-heals within one packet — ~6.5 ms here — and cannot
  produce a 261 s silence while 155 packets/s keep arriving.

**H4 — the bridge's sender stalls. ❌ DEAD.** `/proc/net/snmp` UDP OutDatagrams held
at ~160/s throughout (the counter the bridge itself maintains, which does not lie the
way tcpdump on a WiFi TX path does), and the independent LAN observer saw every packet.
The bridge sends continuously — it just has silence to send.

**H5 — the daemon's keepalive is interfering. ❌ DEAD as a cause, but it *is* broken.**
Its 208 KB backlog and million drops are real and are quantified in §5. They are also
present *while audio is perfectly healthy*, which is what rules them out as the cause.
The SO_REUSEPORT interaction is harmless: the module's own socket showed
`rx_queue = 0, drops = 0` at all times, healthy and silent alike.

---

## 5. The one real defect on our side: `rtp_membership.rs` — DELETED

> **Done 2026-07-28.** `bridge-daemon/src/rtp_membership.rs` is removed, along with its
> `mod` declaration and `spawn()` call in `main.rs` (replaced by a comment pointing at
> upstream's own IGMP recovery, so the next person doesn't re-add it). `cargo check`
> introduces no new warnings — `rtp_source::reload` stays live, since `rtp_source::
> reconcile` calls it on the `PUT /api/source/rtp` path — and `cargo test` is
> **126 passed / 0 failed** (the module's 3 unit tests went with it). Not yet deployed.

Both defects the plan suspected are confirmed, and a third, larger finding supersedes
them.

1. **The trigger cannot fire.** `should_reload` requires `igmp_users < 2`. Measured
   value: **2**, constantly — healthy and throughout every dropout, because the module
   and our keepalive are both joined. The watchdog was silent through 8 dropouts in an
   hour, as designed-by-accident. Its doc comment's claim that it "fires **only** in
   the broken state" is true; the omission is that it fires in *no* state.

2. **The keepalive does not drain.** Quantified live: `rx_queue` sits pinned at its
   208 KB `rcvbuf` ceiling (`211904`), and drops accumulate at **~140 of the ~155
   packets/s that arrive — about 90 % discarded**. The 5 s `TICK` empties it, it refills
   in well under a second, and it spends the remaining ~4 s full. The `audio_recent`
   signal derived from it happens to survive this (a full queue still returns "packets
   arrived"), but nothing else computed from that socket should be trusted, and it
   burns 208 KB of kernel memory plus per-packet receive work continuously for no
   benefit.

3. **New: the whole watchdog is redundant on the deployed PipeWire.** The container runs
   **libpipewire 1.6.2**, and upstream `module-rtp-source.c` now implements IGMP
   recovery itself:

   ```c
   #define DEFAULT_IGMP_CHECK_INTERVAL_SEC  5
   #define DEFAULT_IGMP_DEADLINE_SEC        30
   ```

   `on_igmp_recovery_timer_event` checks every 5 s whether ≥30 s have passed since the
   last packet and, if so, runs `rejoin_igmp_group` — `IP_DROP_MEMBERSHIP` followed by
   `IP_ADD_MEMBERSHIP` on its own socket, in the data loop. Its comment states the
   exact failure this watchdog was written for: *"the receiver socket was silently
   kicked out of the IGMP group"*. So the module self-heals within ~35 s, with no
   module reload and no audio interruption.

**Deleted rather than fixed.** Its trigger had never fired, its keepalive was a
permanent 90 %-drop socket, and the failure it targets is handled upstream on the
version in production. Removing it also removes the confusing second socket on port
46000 that §3 warns about — `/proc/net/udp` now shows one socket there, the module's.

### The trade-off accepted by deleting it

The keepalive did have one genuine side effect, and it is worth stating plainly because
it is what the deletion gives up. Holding the **host** membership kept the module's
socket fed even if the module's own join was lost: Linux delivers a multicast datagram
to any socket bound to the matching address/port whose `mc_list` has no entry for the
group (`ip_mc_sf_allow` returns true when no filter matches). So the keepalive *masked*
module-side join loss completely.

Without it, that failure mode is handled by the module's own recovery instead — a **≤35 s
outage** (30 s deadline + the 5 s check interval) rather than none. That is the right
trade: the masking also hid the problem, the 30 s recovery needs no module reload and no
node churn, and the failure has not been observed since 2026-07-27.

One loose end for the commit message: **it is not established which PipeWire version the
2026-07-27 failure occurred on.** If that install predated 1.6.2's IGMP recovery, the
watchdog was correct for its time and this is a clean "upstream fixed it, drop the
workaround". If it was already 1.6.2, then upstream's recovery did *not* prevent that
outage and the ≤35 s figure above is optimistic — worth re-checking if multicast join
loss is ever seen again.

### If anyone is tempted to re-add a watchdog

Rebuild the trigger on what is actually observable — "packets are arriving **and** the
node is emitting silence" — reading the module's *own* socket counters
(`2A2AFFEF:B3B0`) rather than a socket count, plus `rtp.receiving` from the node's
properties. **But it also needs the payload check**, because on the evidence in §1a such
a watchdog would have fired on all 8 of these dropouts and reloaded the module **eight
times, pointlessly**: the silence was genuine, and reloading cannot fix a sender that
is sending zeros. "Silence while packets arrive" is not a fault signal on its own.

---

## 6. The exit ramp: unicast

Still worth doing, but **not** as a fix for this symptom and no longer urgent. The
multicast path was measured innocent: zero loss at both dropout onsets, membership
stable at 2, the module's socket clean throughout.

What the A/B *would* buy is the ~0.02 % ambient single-packet loss the observer sees
continuously (visible as `seq …->… (d=2)` a few times a minute), plus the removal of the
whole IGMP/WiFi-multicast failure surface and the second socket on port 46000.
Multicast buys nothing here — there is one receiver. Switch `source_addr` to the host's
address when convenient; judge it on the loss counter, not on dropouts.

The plan's advice to do the A/B *before* investigating module internals was sound, and
would have saved effort: the internals work (§4/H3) found only that the posited
mechanism does not exist.

---

## 7. The RTP Source

The RTP source is accessible via "ssh david@192.168.178.78" and
is a "Raspberry Pi Zero 2 W" that receives audio via Bluetooth A2DP and can be
investigated as well.

Useful state on it, for next time:

```bash
ssh david@192.168.178.78
export XDG_RUNTIME_DIR=/run/user/1000
pw-link -l                                   # phone -> bt-bridge-capture -> rtp-bridge
pw-cli info bluez_input.64_B5_F2_F9_A9_4A.2  # api.bluez5.codec, profile, node state
# NB the -P: without it a missing/vanished target silently records the default
# source (rtp-bridge:monitor) instead, i.e. guaranteed silence -- see §1b.
timeout 4 pw-record --target bluez_input.64_B5_F2_F9_A9_4A.2 \
  -P '{ node.name=probe node.dont-reconnect=true }' /tmp/a2dp.wav   # peak!
pw-link -l | grep -B1 'probe:input_FL'       # ...and prove it was attached
hciconfig hci0 -a | grep 'RX bytes'          # ACL byte rate == is the phone sending?
busctl --system get-property org.bluez \
  /org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/fd13 org.bluez.MediaTransport1 State
```

Two notes on this box. Its `~/.config/pipewire/pipewire.conf.d/60-bt-rtp-bridge.conf`
leaves the loopback's **capture end following the default source**, and a second device
(the desktop, `00:1A:7D:DA:71:15`) is also BT-connected — so a re-bind to the wrong
source is a plausible *future* failure mode. It was checked and excluded here (links
intact, 6 links, bound to the phone throughout). Also: the phone's AVRCP `Position`
via `mpris-proxy` is a **cached** value that does not track playback — it read frozen
during healthy audio too, so it is useless as a stall indicator. Use the HCI byte rate
and the payload peak instead.
