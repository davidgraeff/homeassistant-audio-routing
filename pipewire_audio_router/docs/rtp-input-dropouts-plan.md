# RTP / Bluetooth input dropouts — analysis plan

**Symptom (reported 2026-07-28).** Music from the Bluetooth bridge disappears **every
5–10 minutes for up to ~2 minutes**, then returns on its own. The phone keeps playing
throughout — and since Bluetooth's own clock drives that leg, the A2DP link is not the
suspect. The fault is somewhere in **Pi bridge → LAN → `module-rtp-source` → graph**.

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

## 1. The one measurement we already have

Taken during a dropout on 2026-07-28 (~15:14), and it is the most useful fact in this
document because it eliminates two of the obvious causes:

- `tcpdump -i any 'udp port 46000'` on the HA host: **packets were arriving**, a steady
  stream from 192.168.178.78 → 239.255.42.42:46000.
- `/proc/net/udp`: the module's socket (`2A2AFFEF:B3B0`, bound to the group address) had
  **rx_queue = 0 and drops = 0** — it was *draining* those packets.
- `pw-record --target bt-bridge-rtp` at the same time: **peak 0**, digital silence.

So during that dropout the sender was fine, the host was receiving, the module's socket
was being read — and the module still produced silence. That points at the module's
**session/timestamp state**, not at the network or at IGMP.

**Treat this as one observation, not a conclusion.** It was a single sample and I did
not repeat it; §3 exists to confirm or overturn it. If it holds, hypotheses H1/H2 below
are dead on arrival and H3 is the whole story.

A second finding from the same inspection, independent of the cause: **our own
self-heal is broken** (§5).

---

## 2. Why "~2 minutes" is a strong clue

Two independent timers in this system sit suspiciously close to two minutes:

- **IGMP general query interval** on a typical switch/AP is **125 s**. If membership
  state expires somewhere in the path, the next general query restores it — which would
  make recovery look like "about two minutes, by itself".
- `module-rtp-source`'s own session timeout, after which it tears down and re-arms its
  session (worth reading the module source for the exact value).

Whichever it is, the recovery interval is the cheapest discriminator available: log the
*exact* silence→audio gap over several occurrences. A tight cluster around 125 s
implicates IGMP; a different, consistent value implicates the module; scattered values
implicate WiFi/airtime.

---

## 3. The measurement to run: one timeline, four layers

Everything above is guesswork until a single dropout can be attributed to one layer.
Run this for an hour and let it catch several events. Each line is one second, so a
dropout shows up as a contiguous block and the layer that changed *first* is the cause.

```bash
# on the HA host; writes /tmp/rtp-probe.log
ssh root@homeassistant.local 'cat > /tmp/rtp-probe.sh' <<'EOF'
#!/bin/bash
# 1 s cadence: wire packets, module socket counters, IGMP membership, node audio level.
GROUP_HEX=2A2AFFEF        # 239.255.42.42, little-endian as /proc/net/igmp prints it
PORT_HEX=B3B0             # 46000
while true; do
  ts=$(date +%H:%M:%S)
  # a) packets on the wire in the last second (sender still sending?)
  pkts=$(timeout 1 tcpdump -i any -n -q 'udp port 46000' 2>/dev/null | grep -c '46000:')
  # b) the module's socket: is it draining? is the kernel dropping?
  sock=$(grep -i "$GROUP_HEX:$PORT_HEX" /proc/net/udp | awk '{print "rx=" substr($5,10) " drops=" $NF}')
  # c) how many sockets hold the group membership
  users=$(grep -A0 -i "$GROUP_HEX" /proc/net/igmp | awk '{print $2}' | head -1)
  echo "$ts wire=$pkts $sock igmp_users=$users"
  sleep 1
done
EOF
ssh root@homeassistant.local 'chmod +x /tmp/rtp-probe.sh; (nohup /tmp/rtp-probe.sh > /tmp/rtp-probe.log 2>&1 &)'
```

Audio level is the fourth layer and needs a separate sampler, because `pw-record` is
flaky when run repeatedly (see
[live-instance-debugging.md](live-instance-debugging.md)); sample it every 10 s and
correlate by timestamp:

```bash
ssh root@homeassistant.local "docker exec addon_local_pipewire_audio_router bash -lc \
  'export XDG_RUNTIME_DIR=/run/pipewire; timeout 2 pw-record --target bt-bridge-rtp /tmp/p.wav'"
# then copy out and take the peak (see the daemon-side proxy below for the cheap version)
```

**Cheaper audio proxy, already in the log:** with a sendspin group on Opus, the relay
line prints `largest packet`. Digital silence encodes to ~3 B; real audio is 500–650 B.
So `docker logs … | grep 'sendspin relay'` is a free 10 s-resolution record of whether
audio reached the encoder — no capture needed:

```bash
ssh root@homeassistant.local "docker logs -f addon_local_pipewire_audio_router 2>&1 \
  | grep --line-buffered -a 'sendspin relay'"
```

### Reading the result

| wire | module socket | igmp users | audio | ⇒ conclusion |
|---|---|---|---|---|
| 0 pkts | — | any | silent | **sender stopped** — move upstream to the Pi bridge (H4) |
| >0 | rx grows / drops grow | any | silent | module **not reading** its socket (stalled thread) |
| >0 | rx=0, drops=0 | ≥2 | silent | module reads and **discards** — session/timestamp state (H3) |
| 0 pkts | rx=0 | drops to 1 | silent | **membership lost** in the path (H1/H2) |

---

## 4. Hypotheses

**H3 — stale module session / timestamp base (currently the leading one).** The one
sample in §1 fits only this row: packets read, audio silent. Mechanism to look for: the
module latches a timestamp reference and, after a discontinuity (sender pause, RTP
timestamp jump, sequence reset, or the bridge restarting with a new SSRC), treats every
subsequent packet as outside its jitter window and drops it — recovering only when its
session times out and re-arms. **`ignore_ssrc = true` makes this more likely, not less:**
it stops the module from treating a new SSRC as a new session, so a sender restart keeps
the old reference. Read `module-rtp-source`'s session handling to confirm the mechanism,
and note what its timeout is (compare with §2).

*Cheap experiment:* set `ignore_ssrc = false` and see whether dropouts change character
(the trade-off is documented in
[api-reference.md](../../docs/api-reference.md#rtp-source-bluetooth-bridge--a-module-not-a-process)
— it then latches the first SSRC and rejects others).

**H1 — multicast membership expiry in the path (switch/AP IGMP snooping).** The ~2 min
recovery matches a 125 s general-query interval almost too well. §1's observation
argues against it (packets *were* reaching the host), but a single sample can't rule out
that some dropouts are this and others are H3.

**H2 — WiFi multicast handling.** The bridge is on WiFi; multicast is transmitted at a
low basic rate and is the first thing an AP drops under airtime pressure, and some APs
apply IGMP snooping per-client with their own timers. Distinguishing feature: the wire
column in §3 goes to 0 **at the host** while the bridge still believes it is sending —
so this needs a capture at *both* ends (see H4).

**H4 — the bridge's sender stalls.** Argued against by §1, but must be confirmed *during*
a dropout from the bridge side, because tcpdump on the sender's WiFi TX path is
unreliable (noted in the memory of the BT-bridge work: "tcpdump lies on WiFi TX"). Use
the bridge's own counters if it has them, or a third host on the LAN as an independent
observer of the group.

**H5 — the daemon's own keepalive is interfering.** `rtp_membership.rs` holds a second
socket bound to `0.0.0.0:46000` joined to the same group. It was observed with a
**211 KB backlog and 1.1 M drops**, i.e. it is not draining anywhere near the packet
rate. It should be harmless to the module's copy (separate socket, separate buffer), but
it is worth proving rather than assuming — the SO_REUSEPORT interaction between a
group-bound and a wildcard-bound socket on the same port deserves one deliberate test
(does removing the keepalive change the dropout rate?).

---

## 5. Deliverable regardless of the cause: fix the self-heal

`rtp_membership.rs` was written for exactly this symptom, and in this failure mode it
**cannot fire**. Its trigger is "audio is arriving *and* the group's joined-socket count
is only our keepalive (== 1)". Observed reality: `/proc/net/igmp` shows
`igmp_users = 2`, so the count test never passes — and the log is silent through every
dropout. Two concrete defects:

1. **The trigger is a proxy for the wrong thing.** It infers "the module lost its join"
   from a socket count. The state we actually care about is "packets are arriving and the
   module is not producing audio", which §3 shows is directly observable (wire count +
   the module socket's own rx/drops from `/proc/net/udp`, keyed by the group-bound
   socket rather than ours).
2. **The keepalive doesn't drain.** 1.1 M drops means its "audio is arriving now" signal
   is derived from a socket that spends most of its time full. Drain it properly (or
   size its buffer for the 5 s tick) before trusting anything computed from it.

Whatever §3 concludes, the watchdog should end up with a trigger that fires in the
observed broken state and demonstrably not otherwise — and, per its own doc comment,
never while audio is healthy.

---

## 6. The exit ramp

If H1/H2 turn out to be involved at all, the durable answer is the one the earlier
pw-sink work already reached: **run this one-to-one path as unicast.** Multicast buys
nothing here (a single receiver) and costs the whole IGMP/WiFi-multicast failure surface.
An A/B is cheap and decisive: switch `source_addr` to the host's address for a day and
count dropouts.

Do the A/B **before** investing in H3's module-internals work, because a clean unicast
result makes most of this document moot.

---

## 7. The RTP Source

The RTP source is accessible via "ssh david@192.168.178.78" and
is a "Raspberry Pi Zero 2 W" that receives audio via Bluetooth A2DP and can be investigated as well.
