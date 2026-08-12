# RTP / Bluetooth input dropouts — the investigation, and where the fault is

**Symptom (2026-07-28).** Music from the Bluetooth bridge disappeared **every
5–10 minutes for up to ~2 minutes**, then returned on its own, while the phone
kept playing.

**Answer.** The audio is **already digital silence at the Bluetooth A2DP boundary
on the Pi**, before it reaches the bridge's PipeWire graph. The phone transmits a
*continuous, full-bitrate* aptX stream that decodes to zeros. Nothing in the
bridge's graph, the LAN, the multicast group or `module-rtp-source` misbehaves
during a dropout: they faithfully carry, and faithfully reproduce, the silence
they are given. **This is not a bug in this project.**

One question is still open and it decides whose bug it *is*: whether the **phone
sends** silence or the **Pi's aptX decoder produces** it. The owner reports this
phone works with every other Bluetooth receiver, which makes the Pi's decoder the
leading candidate — §5 has the one test that settles it.

The investigation also found exactly one real defect on our side
(`rtp_membership.rs`, a watchdog that could never fire — deleted, §4), and it left
four lessons about measuring this class of fault that are worth more than the
conclusion (§3).

Configuration of the source throughout (`/api/sources`): `source_addr`
`239.255.42.42` (**multicast**), port 46000, rate 48000, `latency_msec` 100,
`ignore_ssrc` `true`; sender 192.168.178.78, RTP **PT=127**, 1252-byte UDP
payloads.

---

## 1. The evidence (measured 2026-07-28, 18:21–18:35 CEST)

Two dropouts were caught with four layers instrumented simultaneously, plus a
third-host observer on the wire and a probe on the bridge itself. The chain was
walked end to end and the fault was found at the *first* link, not the last.

**On the wire** (independent observer on 192.168.178.21, wired, same /24, joining
the group and decoding RTP headers **and payload**):

```
18:28:57 pkts=155  peak=15801  dBFS=-6.3   zeropkts=0    seqgap=0 lost=0 ssrcchg=0 tsjump=0 tsd=[310] audio
18:28:59 pkts=154  peak=0      dBFS=-inf   zeropkts=154  seqgap=0 lost=0 ssrcchg=0 tsjump=0 tsd=[310] SILENCE
...                                        (115 s, 17 794 packets, all zero payload)
18:30:54 pkts=153  peak=24511  dBFS=-2.5   zeropkts=101  seqgap=0 lost=0 ssrcchg=0 tsjump=0 tsd=[310] audio
18:30:55 pkts=154  peak=0      dBFS=-inf   zeropkts=154  seqgap=0 lost=0 ssrcchg=0 tsjump=0 tsd=[310] SILENCE
```

During a dropout the RTP stream is not merely present, it is **pristine**: a
steady 155 packets/s, RTP timestamp advancing exactly 310 frames per packet with
no discontinuity, no SSRC change, and 10 lost packets across 282 silent seconds
(≈0.02 %, the same ambient WiFi-multicast loss seen while audio is healthy). The
payload is `s16le` stereo and **every sample in every packet is zero**. The single
loud second at 18:30:54 (−2.5 dBFS) between two silent stretches, and the loud
onset at recovery, are the signature of a *source* buffer being flushed — not of a
receiver resynchronising.

**On the bridge** (Pi Zero 2 W, during the dropout):

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

The last two rows are the crux: **the phone's A2DP bitrate is identical whether
the music is audible or gone.** Its Bluetooth stack streams ~370 kbit/s of aptX
continuously and fills it with zeros while its player produces no audio.

**On the HA host, throughout both dropouts:** `wire=156 mod=0/0 ka=… igmp=2` —
`wire` = packets/s arriving on `end0`, `mod` = the module's group-bound socket
`rx_queue/drops`, `igmp` = sockets holding the membership. The module's socket
showed **rx_queue 0 and drops 0 for the entire duration** (it read every packet)
and `rtp.receiving` stayed `"true"` with node `state: "running"`.

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

### Episode statistics — and the free retroactive record

`module-rtp-source` never logs, but the sendspin relay prints `largest packet`
every 10 s, and digital silence Opus-encodes to ~3 B against 400–700 B for real
audio. That makes the daemon's own log a **free, retroactive, 10 s-resolution
record of the symptom**, which is how the table below was built before any new
instrumentation existed. Over 64.5 min: eight episodes, 10 / 70 / 110 / 230 / 261
/ 70 / 111 / 160 s long, starting 2.0–11.6 min apart — **26.4 % of the hour was
silent**. Episodes 7 and 8 are the two caught at 1 s resolution above, so the
cheap proxy and the wire observer agree.

That spread is also what killed the timer hypotheses immediately: no fixed timer
produces 10 s to 261 s. Two timers do sit near the reported "about two minutes"
and both were checked and excluded — a switch/AP's **125 s** IGMP general-query
interval, and `module-rtp-source`'s own standby (`DEFAULT_CLEANUP_SEC = 60`, and
because `on_standby_timer_event` walks `STATE_RECEIVING → PROBE → STOPPING` one
transition per tick, standby needs 60–120 s *without packets* to engage — while
resume is immediate on the next packet, so it cannot hold audio down for minutes
while 155 packets/s keep arriving).

---

## 2. Does watching it change it? (2026-07-29) — the instrument was the problem

A 2 h session with the [bluetooth-testing-app](../../firmware/pi-bridge/bluetooth-testing-app/)
attached *looked* dropout-free, which raised the obvious question. **Timeouts: no**
— nothing in the causal chain is timeout-driven, and no client on the Pi can stop
a phone or an aptX decoder from emitting zeros. But the run was not clean and the
instrument was not trustworthy:

| Witness | What it says |
|---|---|
| the app's episode log | one **148.9 s** episode, plus nine 0.5–2.6 s blips; `silent_duty` 0.28 % over 108.9 min |
| the relay `largest packet` proxy on the HA host (zero footprint on the Pi) | audio through 22:32:39, **SILENT 22:32:52–22:33:22** (~40 s), then 59 min clean |

Clocks were checked and in sync, so those overlap only in their last ~30 s: the
relay was still encoding 500–600 B packets through most of the app's "episode".

**Three defects in the app, each independently sufficient to invent this exact
symptom** (all fixed 2026-07-29):

1. **`pw-record --target` is a preference, not a pin.** When the target vanishes,
   PipeWire rebinds the stream to the default source — here `rtp-bridge:monitor`,
   which reports digital silence with no error. Reproduced on demand;
   `node.dont-reconnect=true` leaves the stream unlinked instead. `pw-record` does
   *not* exit, and a target missing at *startup* falls back too, so a link check
   is mandatory.
2. **The silence streak grew on wall-clock time** (`now - silent_since`), so it
   counted up while no blocks arrived at all. Silence is now counted in blocks.
3. **An episode could be entirely a stall.** One zero block set the run, any gap
   followed, and the next real block closed an "episode" whose duration was the
   gap — the most likely origin of the 148.9 s above. Gaps now end a run and are
   logged separately as lost data.

Which reframes every earlier session's numbers: **`coverage` over that 2 h was
0.92**. Nine minutes were never looked at, and the UI implied an unbroken record.

**It is also not a passive observer.** Three real perturbations: its `pw-record`
requests `node.latency = 960/48000` and PipeWire takes the minimum in the
driver's graph, so the bluez-driven graph ran at 960 instead of 1024 — and the
bluez A2DP source's decode-buffer target derives from that duration, i.e. exactly
the buffer whose underrun emits zeros. It is an always-active second consumer on
the suspect node (never idle, never suspended). And `block_peak_rms` runs a
per-sample Python loop over 96 k samples/s on a Zero 2 W: `pw-top` charged its
stream **1621 xruns** against 887 for `bt-bridge-capture` and 117 for
`rtp-bridge`. That pushes *toward* underruns, so it cannot explain an improvement
— but it is why the app's own reader stalls.

**The likelier reason the rate collapsed:** the Pi had rebooted 20 minutes before
that run, so the whole clean session was on a fresh PipeWire session, where the
26 %-duty hour was a long-lived one with reconnect history. Episodes growing
10 → 261 s fit accumulating state that a restart clears.

Until an A/B exists — stream ≥1 h with **no client on the Pi at all**, judged from
the relay proxy alone, against an identical run with the app attached — neither
"monitoring suppresses it" nor "the reboot fixed it" is established.

---

## 3. What this taught about measuring it

**1. Measure what is *in* the packets, not just that they arrived.** The original
measurement design had four layers — wire packet rate, the module's socket
counters, our keepalive's socket, group membership — and every one of them reads
identically for a healthy stream and for a stream of perfectly-delivered zeros.
The observation "packets arriving, socket draining, drops 0, `pw-record` peak 0"
is explained *equally well* by "the receiver discards good audio" and by "the
receiver faithfully reproduces silence". One payload check separates them:

```python
a = array.array("h"); a.frombytes(pkt[12:])      # s16le stereo @48k, 310 frames
peak = max(max(a), -min(a))                      # peak == 0  =>  sender sends silence
```

**2. Start with the witness that costs nothing.** The relay's `largest packet`
line gave a retroactive episode table for free, and it is the **only witness with
no footprint on the Pi** — which is what makes it the one to trust when the
question is whether the instrument is changing the outcome, and what caught the
testing app's fabricated 149 s episode. Its weakness is retention: `docker logs`
had already truncated the first half of a 2 h run, so raise the add-on's log
retention or `tee` these lines before a long session.

```sh
ssh root@homeassistant.local "docker logs addon_local_pipewire_audio_router 2>&1 \
  | grep -a 'largest packet'"
```

**3. Prove the probe was attached.** `pw-record --target X` does not guarantee you
are recording X (§2). A probe that concludes "the A2DP source is silent" must add
`-P '{ node.name=probe node.dont-reconnect=true }'` and check
`pw-link -l | grep -B1 probe`. The §1 readings survive this because the node was
confirmed present, `running`, and linked at the time.

**4. Two shell traps that each cost an hour.** `/proc/net/udp` had **two** sockets
on port 46000 — `2A2AFFEF:B3B0` the module's (bound to the group address) and
`00000000:B3B0` our keepalive's (wildcard) — and keying on the wrong one tells you
nothing about the module. And `pkill -f <pattern>` matches the calling shell's own
command line, because the pattern is *in* it; it killed two ssh sessions before
the probes were moved into scripts on the target, then bit again the next day.
Also: busybox awk on HAOS has no `strtonum()`, so convert the hex `rx_queue` in
bash; and one long-lived `tcpdump` aggregated by `awk` beats spawning `timeout 1
tcpdump` per second, which races its own startup and under-counts.

Saved probes: `observer.py` (payload + RTP integrity, run on a dev box),
`host-probe.sh` (four-layer receive side), `pi-probe.sh` (UDP OutDatagrams + link
check), `mpris-probe.sh` (AVRCP + HCI byte rate), `mine_relay.sh` + `episodes.py`
(the retroactive episode table), `snapshot.sh` (one-shot receive-side state).

### Reading the result

| wire | payload peak | module socket | ⇒ conclusion |
|---|---|---|---|
| >0 | **0** | rx=0, drops=0 | **sender is transmitting silence** — go upstream to the bridge and its Bluetooth source ← *this is what happened* |
| >0 | >0 | rx=0, drops=0 | receiver-side: the module reads and discards |
| >0 | >0 | rx / drops growing | the module is **not reading** its socket (stalled thread) |
| 0 pkts | — | rx=0 | the sender stopped, or the path lost it |

---

## 4. The one real defect on our side: `rtp_membership.rs`, deleted

Removed in `12415c7` along with its `mod` declaration and `spawn()` call
(replaced by a comment pointing at upstream's own IGMP recovery so the next
person does not re-add it). Three findings, and all three say delete rather than
fix:

1. **The trigger could not fire.** `should_reload` required `igmp_users < 2`. The
   measured value is **2**, constantly — healthy and throughout every dropout,
   because the module and our keepalive are both joined. The watchdog was silent
   through 8 dropouts in an hour, as designed-by-accident. Its doc comment's claim
   that it fires "**only** in the broken state" was true; the omission was that it
   fired in *no* state.
2. **The keepalive did not drain.** `rx_queue` sat pinned at its 208 KB `rcvbuf`
   ceiling with drops accumulating at **~140 of the ~155 packets/s arriving —
   about 90 % discarded**. The 5 s tick emptied it, it refilled in under a second,
   and it spent the rest of every tick full. The `audio_recent` signal derived
   from it happened to survive that (a full queue still means "packets arrived"),
   but nothing else computed from that socket should have been trusted, and it
   burned 208 KB of kernel memory plus per-packet receive work continuously.
3. **The whole watchdog is redundant on the deployed PipeWire.** The container
   runs libpipewire 1.6.2, whose `module-rtp-source.c` implements IGMP recovery
   itself (`DEFAULT_IGMP_CHECK_INTERVAL_SEC 5`, `DEFAULT_IGMP_DEADLINE_SEC 30`):
   `on_igmp_recovery_timer_event` re-joins the group on its own socket in the data
   loop, and its comment names the exact failure this watchdog was written for —
   *"the receiver socket was silently kicked out of the IGMP group"*. So the
   module self-heals within ~35 s, with no module reload and no node churn.

**The trade-off accepted by deleting it.** The keepalive did have one genuine side
effect: holding the *host* membership kept the module's socket fed even if the
module's own join was lost, because Linux delivers a multicast datagram to any
socket bound to the matching address/port whose `mc_list` has no entry for the
group (`ip_mc_sf_allow` returns true when no filter matches). So it *masked*
module-side join loss completely. Without it that failure costs a **≤35 s outage**
instead of none — the right trade, since the masking also hid the problem. One
loose end: it is not established which PipeWire version the 2026-07-27 join-loss
event happened on. If that install predated 1.6.2's recovery, this is a clean
"upstream fixed it, drop the workaround"; if it was already 1.6.2, upstream's
recovery did not prevent that outage and the ≤35 s figure is optimistic. Worth
re-checking if multicast join loss is ever seen again.

**If anyone is tempted to re-add a watchdog:** rebuild the trigger on what is
actually observable — "packets are arriving **and** the node is emitting silence"
— reading the module's *own* socket counters plus `rtp.receiving` from the node's
properties. But it also needs the payload check, because on the §1 evidence such a
watchdog would have fired on all 8 dropouts and reloaded the module **eight times,
pointlessly**: the silence was genuine, and reloading cannot fix a sender that is
sending zeros. "Silence while packets arrive" is not a fault signal on its own.

### The other hypotheses, and what each was worth

- **Multicast membership expiry in the path. Dead, twice over.** Membership was
  `igmp_users = 2` continuously, and it is *structurally* excluded by the
  `ip_mc_sf_allow` behaviour above.
- **WiFi multicast handling. Dead as the cause.** Real but negligible: ~0.02 %
  single-packet loss, at the *same* rate during silence and during healthy audio.
  Never a gap, never a stall.
- **Stale module session / timestamp base. Dead, and its stated mechanism does not
  exist.** Read from `module-rtp/audio.c` (1.6.2): there is **no jitter-window
  discard** — `rtp_audio_receive` rejects a packet only for being short, having a
  bad version, or an unexpected SSRC; everything else is written into the ring at
  `timestamp + target_buffer`, out-of-order packets included. The actual silence
  generator is the read side (`memset` + `SPA_CHUNK_FLAG_EMPTY` when
  `avail < wanted`), but it also clears `have_sync` and the next packet re-syncs
  the ring in full, so a receiver-side underrun self-heals within one packet
  (~6.5 ms here).
- **`ignore_ssrc = true` is the *safe* setting**, the opposite of what was first
  assumed. The code is `impl->have_ssrc = !impl->ignore_ssrc;` guarding
  `if (impl->have_ssrc && impl->ssrc != hdr->ssrc) goto unexpected_ssrc;`, so with
  `ignore_ssrc = true` **no packet is ever rejected on SSRC grounds** and a sender
  restart with a fresh SSRC is accepted seamlessly. Setting it `false` as a "cheap
  experiment" would *add* the only SSRC rejection path there is. Leave it `true`
  (it is `DEFAULT_RTP_IGNORE_SSRC` in `sources/rtp.rs`).
- **The bridge's sender stalls. Dead.** `/proc/net/snmp` UDP OutDatagrams held at
  ~160/s throughout — the counter the bridge itself maintains, which does not lie
  the way `tcpdump` on a WiFi TX path does — and the LAN observer saw every packet.

**Unicast is still worth doing, but not as a fix for this.** The multicast path
was measured innocent: zero loss at both dropout onsets, membership stable, the
module's socket clean. What switching `source_addr` to the host's address buys is
the ~0.02 % ambient single-packet loss, the removal of the whole IGMP /
WiFi-multicast failure surface, and one less socket on port 46000. Multicast buys
nothing here — there is one receiver. Judge the change on the loss counter, not on
dropouts. (The original advice to run that A/B *before* reading module internals
was sound and would have saved effort: the internals reading found only that the
posited mechanism does not exist.)

---

## 5. The one open question, and the test that closes it

The measurements localise the fault precisely — **the Pi receives full-rate aptX
frames that decode to zeros** — and that is as far as receive-side evidence can
take it. Two candidates remain, differing only in which side of the aptX codec is
at fault:

- **A — the phone transmits silence.** A stalled streaming player (the source was
  an 88-minute festival mix, and buffer-flush transients bracket every episode)
  while Android keeps the A2DP transport up.
- **B — the Pi's aptX decode path emits zeros** while consuming frames at the
  correct rate.

**B leads.** The phone works with every other Bluetooth receiver, so whatever it
emits is fine everywhere else and the thing unique to the failing path is this
Pi's decoder. That also fits the otherwise-odd details: episode durations growing
steadily (10 → 261 s) rather than tracking a player's buffer state, the phone's
link quality reading 197 against 255 for the other connected device, and clean
*digital* zeros rather than the partial or glitched output a truncated stream
would give. There is precedent on this box — the 2026-07-27 "BT stutter"
diagnosis also found a fault at the A2DP-source layer here — and this Pi runs
**libpipewire 1.4.2**, older than the 1.6.2 on the HA host.

The decisive test needs a deliberate config change on live audio, so it has not
been run unprompted: **force the A2DP codec to SBC** and watch several dropout
intervals.

```bash
# on the Pi — switch the negotiated A2DP codec away from aptX
ssh david@192.168.178.78
export XDG_RUNTIME_DIR=/run/user/1000
wpctl status | grep -i bluez                    # find the device id
wpctl set-param <device-id> ...                 # or: bluetoothctl -> select codec
pw-cli info bluez_input.64_B5_F2_F9_A9_4A.2 | grep api.bluez5.codec   # confirm sbc
# then re-run observer.py and compare the silent-episode rate
```

**Silences vanish under SBC ⇒ B**, the Pi's aptX decode path — a real bug for the
bridge's backlog, with a codec pin as the immediate workaround. **Silences persist
under SBC ⇒ A**, the phone, and this closes as not-our-bug.

Two zero-risk cross-checks worth doing first, since neither needs a config
change: play a **local file** on the phone rather than a streamed one (if that
also drops out, a rebuffering player is excluded), and watch the observer while a
**different phone** streams to the same bridge (a second phone failing the same
way points hard at B). Note that "it works with every other receiver" does not by
itself exclude A: other receivers may negotiate SBC and so exercise a different
decode path, which is precisely the ambiguity the SBC test resolves.

---

## 6. The bridge, for next time

The RTP source is a **Raspberry Pi Zero 2 W** at `ssh david@192.168.178.78`,
receiving audio over Bluetooth A2DP.

```bash
export XDG_RUNTIME_DIR=/run/user/1000
pw-link -l                                   # phone -> bt-bridge-capture -> rtp-bridge
pw-cli info bluez_input.64_B5_F2_F9_A9_4A.2  # api.bluez5.codec, profile, node state
# NB the -P: without it a missing/vanished target silently records the default
# source (rtp-bridge:monitor) instead, i.e. guaranteed silence -- see §2.
timeout 4 pw-record --target bluez_input.64_B5_F2_F9_A9_4A.2 \
  -P '{ node.name=probe node.dont-reconnect=true }' /tmp/a2dp.wav   # peak!
pw-link -l | grep -B1 'probe:input_FL'       # ...and prove it was attached
hciconfig hci0 -a | grep 'RX bytes'          # ACL byte rate == is the phone sending?
busctl --system get-property org.bluez \
  /org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/fd13 org.bluez.MediaTransport1 State
```

Two standing notes on this box. Its
`~/.config/pipewire/pipewire.conf.d/60-bt-rtp-bridge.conf` leaves the loopback's
**capture end following the default source**, and a second device (the desktop) is
also BT-connected — so a re-bind to the wrong source is a plausible *future*
failure mode. It was checked and excluded here (links intact, bound to the phone
throughout). And the phone's AVRCP `Position` via `mpris-proxy` is a **cached**
value that does not track playback — it read frozen during healthy audio too, so
it is useless as a stall indicator. Use the HCI byte rate and the payload peak.
