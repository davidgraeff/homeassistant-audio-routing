# Live-instance debugging

How to poke a running bridge-daemon instance and the gotchas that cost
time during bring-up. Companion to [architecture.md](architecture.md)
(how it's built) and [airplay2-output.md](airplay2-output.md) (what's
done vs. planned).

**Open investigations** with their own plans (measurements, hypotheses,
decision points): [sendspin-open-items.md](sendspin-open-items.md) (open work — its two
headline faults, "adding a speaker silences the group" and "receives audio, renders
nothing", are both closed; what is left is a workaround for the latter) and the archived
[sendspin-group-churn-plan.md](old/sendspin-group-churn-plan.md) (the investigation) and
[rtp-input-dropouts.md](rtp-input-dropouts.md) (Bluetooth/RTP input
drops out every 5–10 min for up to 2 min).

## Environment / who's who (dev setup)

- **HA host (daemon)** = `192.168.178.173` (an x86 box since 2026-08-21; it was a
  Raspberry Pi 4 at `192.168.178.22` before that, and older recipes here still say
  `homeassistant.local`). Daemon REST API on `:8099`. SSH: `ssh root@192.168.178.173`
  (the SSH add-on container — it has `tcpdump`, `curl` and `python3`, which the
  *daemon's* container does not; `export XDG_RUNTIME_DIR=/run/user/0` for `pw-*`
  tools).
- **The daemon's container name is `app_local_pipewire_audio_router`.** The
  Supervisor used to name add-on containers `addon_*` and now uses `app_*`, so every
  older recipe fails with a bare `No such container` — which reads like the add-on is
  down rather than misnamed. Don't hardcode it; resolve it:
  ```bash
  ssh root@homeassistant.local 'docker ps --format "{{.Names}}" | grep pipewire'
  # or, in a script:  C=$(docker ps --format '{{.Names}}' | grep pipewire_audio_router)
  ```
- **Receivers (AP2):** "Dusche" = Yamaha WX-021 `192.168.178.165`;
  "Pioneer" = VSX-934 `192.168.178.35`.
- **AirPlay sender (input source)** = a Fedora box `192.168.178.21`.
- **Deploy:** `cd homeassistant-pipewire-audio-routing &&
  ./scripts/deploy-dev.sh addon` (~10–15 min: cross-build → GHCR push →
  Supervisor pull + container restart; run in background, it restarts the
  add-on).
- **Dev-box build/check:** `cd bridge-daemon && cargo build`; frontend
  `cd frontend && npm run check`.
- **`curl`/`python3` are NOT installed in the add-on container.** Drive the
  HTTP API from another host (`http://<ha-ip>:8099/…`, host-network) or use
  `pw-link`/`pw-dump` directly and parse on the client side.

## Reproducing / diagnosing the RAOP→sendspin graph stall

```
# who can drive: look for node.network / node.driver
pw-dump | jq '.[] | select(.type|endswith("Node")) | .info.props
  | {name:."node.name", network:."node.network", driver:."node.driver"}'

# reproduce the stall (WARNING: freezes the source's whole component):
pw-link bt-bridge-rtp:receive_FL raop-out-<name>:send_FL
pw-top -b -n 3        # stalled nodes show QUANT 0 / RATE 0 / ??? , music stops
pw-link -d bt-bridge-rtp:receive_FL raop-out-<name>:send_FL   # recover
```

A healthy (driver-anchored) cycle instead shows non-zero, advancing
`QUANT/RATE/BUSY` with `ERR 0`.

### Measurement gotchas

- **`pw-top` node state `running` does not mean audio is flowing.** A
  stalled, driverless cycle still reports its nodes as `R`; the tell is
  `QUANT 0 / RATE 0` and BUSY not advancing across samples.
- `curl`/`python3` are absent in the add-on container (see above).

## Device-side logs from the ESPHome speakers

**The highest-value diagnostic for any sendspin fault, and it was written off as
impossible for weeks** (the old item 8, "the device's API log subscription delivers
nothing"). It delivers fine. The subscription was never the problem — the *output was
being thrown away locally*, see the traps below.

Why it matters: the daemon cannot tell "this device is being sent audio" from "this
device is rendering audio" — that blind spot is what forced `tcpdump` and by-ear
bisection through the whole group-churn investigation. The device answers it directly.

### The recipe

```bash
DEV=192.168.178.82                  # the speaker
HA=192.168.178.173                  # the HA host

# 1. the device's API key, from HA's own config entry (the SSH add-on can read /config)
PSK=$(ssh root@$HA "python3 - <<PY
import json
d=json.load(open('/config/.storage/core.config_entries'))
print([e['data']['noise_psk'] for e in d['data']['entries']
       if e.get('domain')=='esphome' and e['data'].get('host')=='$DEV'][0])
PY")

# 2. subscribe. BOTH env var and -s INT matter — see the traps.
PYTHONUNBUFFERED=1 timeout -s INT 40 ~/.local/bin/aioesphomeapi-logs \
  "$DEV" --noise-psk "$PSK" --strip-ansi-escapes > /tmp/dev.log 2>/dev/null || true

# 3. the lines that matter
grep -E "sendspin|speaker_mixer|ring_buffer|Media Player" /tmp/dev.log
```

To watch a *transition*, start it in the background, sleep ~12 s for the config dump to
finish, then fire the API call — and only read the file after the tool has exited:

```bash
PYTHONUNBUFFERED=1 nohup timeout -s INT 55 ~/.local/bin/aioesphomeapi-logs \
  "$DEV" --noise-psk "$PSK" --strip-ansi-escapes > /tmp/dev.log 2>/dev/null &
sleep 12
curl -s -XPUT http://$HA:8099/api/outputs/sendspin-dev-<slug>/delay \
  -H 'Content-Type: application/json' -d '{"delay_ms":11}'
sleep 40          # must outlast the timeout, or you read a half-flushed file
```

### Five traps — the first two are the whole of the old item 8

1. **`timeout` sends SIGTERM, and Python dies without flushing stdout.** Redirected
   stdout is block-buffered and flushed only at interpreter exit, so SIGTERM discards the
   entire buffer. What survives is the three `logging` lines on *stderr* — "handshakes
   fine and then produces zero lines" is exactly that, and it is an artifact of the
   harness, not the device. Use **`timeout -s INT`** (SIGINT → `KeyboardInterrupt` → the
   CLI's `contextlib.suppress` → clean exit → flush) and/or `PYTHONUNBUFFERED=1`.
2. **`stdbuf` cannot fix it.** Python uses its own `io` layer rather than libc stdio, so
   `stdbuf -oL` has no effect whatsoever. `PYTHONUNBUFFERED=1` (or `python -u`) is the
   only knob.
3. **`timeout -s INT` makes a *pipeline* exit 130**, which aborts the rest of a
   `&&`-chain or a one-liner. Redirect to a file and grep it afterwards; don't pipe the
   tool straight into `grep`.
4. **Reading the file before the tool exits gets you nothing** (trap 1 again — the flush
   is at exit). If you background it, sleep past the `timeout` before grepping.
5. **~12 s of `[C]` config-dump comes first** on every connect, which buries whatever you
   were trying to see. Sleep it out before triggering anything, or filter `[C]` away.

Also: `--strip-ansi-escapes` exists only in the `~/.local/bin` install, not the venv's
(it errors out there) — which accounted for part of an earlier "empty log".

### The rendering oracle

A device that is really rendering logs this on every stream start:

```
sendspin.client: Stream Started
sendspin.sync_task: Processed new codec header: opus, 48000 Hz, 2 ch, 16-bit   <-- the decoder is built
ring_buffer: Created ring buffer with size 19200
speaker_mixer: Starting
```

A device that is **receiving audio and rendering nothing** logs `Stream Started`, then
goes straight to `PLAYING` with **no** `Processed new codec header`, no `ring_buffer`, no
`speaker_mixer: Starting` — and **no warning and no error**. That is the whole signature.

- `Processed new codec header` (and `ring_buffer: Created`) is the reliable tell.
- `speaker_mixer: Starting` only appears if the mixer had *stopped* first; it fires
  `Stopped` 330–450 ms after `Stream ended` (n=10), so a handover faster than that
  legitimately skips both the `Stopped` and the next `Starting` while still rendering —
  measured at a 60 ms group→group switch.
- **`media_player` state is NOT an oracle.** The firmware's own
  `media_player.<name>_media_player` entity reports `playing` in both states, so neither
  the HA entity nor its recorder history can tell you whether a speaker made a sound.

### Reproducing a device-side fault without a deploy

A 15-minute `deploy-dev.sh` per hypothesis makes protocol-level bisection impossible, so
there is a harness that drives a speaker directly:
[`bridge-daemon/examples/sendspin_handover.rs`](../bridge-daemon/examples/sendspin_handover.rs).

```bash
# take the speaker away from the daemon first, or the two compete for the connection
curl -XDELETE http://<ha-ip>:8099/api/outputs/sendspin-dev-<slug>

cd bridge-daemon
cargo run --example sendspin_handover -- ws://<device-ip>:8928/sendspin plain     # the fault
cargo run --example sendspin_handover -- ws://<device-ip>:8928/sendspin reissue   # the fix

curl -XPOST http://<ha-ip>:8099/api/outputs/sendspin-dev-<slug>/adopt            # give it back
```

Why it is fast enough to iterate with: **no audio is needed.** The oracle above fires at
`stream/start`, so a scenario is a dial plus one or two control frames — no encoder, no
PipeWire, no anchor. Its module docs carry the scenario table and the three things that
mislead (self-prime, or the first run after a quiet period passes and the next fails; scope
the verdict to the phase under test, or the priming phase's header makes it report the
opposite of the truth; and batch at least four runs per arm, because the base failure rate is
roughly half and several n=2/n=3 results reversed here at n=4+).

### Device inventory (dev setup)

| output node | speaker | IP | firmware |
|---|---|---|---|
| `sendspin-dev-home_assistant_voice_093ca8` | Badezimmer | 192.168.178.52 | stock Nabu Casa Voice PE **26.6.0** |
| `sendspin-dev-home_assistant_voice_096287` | Büro | 192.168.178.82 | stock Nabu Casa Voice PE **26.6.0** |
| `sendspin-dev-home_assistant_voice_0966f3` | Dusche | 192.168.178.84 | stock Nabu Casa Voice PE **26.6.0** |
| `sendspin-dev-satellite1_8f0c50` | satellite1 | 192.168.178.172 | own build, `v0.2.0` |
| `sendspin-dev-satellite1_c4150c_c4150c` | satellite1 | 192.168.178.99 | own build, `dev` |

**The three Voice PEs run stock Nabu Casa firmware (ESPHome 2026.6.0) with the
*upstream* `sendspin.hub` / `sendspin-cpp` component — not the stale March
`kahrendt/esphome` fork.** Only the satellite1 units run that fork. Confirmed from
`app:151`/`app:153` in the device log and from each device's `update.*` entity's
`installed_version`. Anything reasoning about "our pinned firmware" needs to say *which*
device it means.

## AirPlay-2 diagnostics

### Spike A/B oracle — the known-good vs live-path test

```
# file = known-good start_streaming; live = the real start_streaming_live path fed a tone
curl -XPOST :8099/api/spike/ap2 -d '{"ips":["192.168.178.165"],"freq":440,"mode":"file"|"live"}'
curl -XDELETE :8099/api/spike/ap2      # stop
```

`spike/ap2.rs`. Use it as the A/B oracle for **any** audio-path change
(e.g. the 48 kHz harmonization) — it touches the just-stabilized audio
path.

### PTP-lock health

```
curl -s :8099/api/outputs      # each ap2-dev-* shows ptp_locked + ptp_lock_age_s
```

`ptp_locked` / `ptp_lock_age_s` / `ptp_responsive` derive from libairptp
`airptp_peer_last_seen()` → `Ap2PtpService::peer_ptp()` and surface as a UI
badge in `OutputsTab.svelte`.

> **Two traps here, both cost a day (2026-08-21).**
>
> **1. `last_seen` is seeded at peer-add.** libairptp writes
> `peer->last_seen = time(NULL)` inside `daemon_peer_add`, so a raw read cannot
> tell "the receiver is answering" from "we just registered it". Our
> `ptp::heard_age()` discounts anything not newer than the add — but a `tcpdump`
> is still the only *proof*, and any future code reading the FFI directly will
> re-acquire the bug. What it caused: a receiver that has never sent one gPTP
> packet read as "locked 0 s ago" right after connect, then "lock lost" 30 s
> later, and the watchdog's own reconnect re-seeded the timestamp — a 2-minute
> reconnect loop, ~1.4 s of silence each time, on a receiver that was fine.
>
> **2. Receivers come in two classes, and only one has an observable lock.**
> Measured over 90 s with both streaming in one group:
>
> | receiver | `PTPInfo` | `Delay_Req` → us | Sync ← us |
> |---|---|---|---|
> | Pioneer VSX-934 | `SUE PTP 1.0` | **717** (~8/s) | 1434 |
> | Yamaha WX-021 "Dusche" | `AirPlay;2.0.4` | **0** | 1434 |
>
> Both render perfectly. The Yamaha listens one-way and never talks back, so for
> it `ptp_locked: false` is the permanent healthy state and there is nothing to
> recover; the watchdog is gated on `ptp_responsive` for exactly this reason.
> Judge gPTP health **only while actively streaming** — at idle even the Pioneer
> stops, and `tcpdump` on 319/320 showing ~0 packets is expected.

```
# while streaming, expect continuous Sync (~8/s) out; Delay_Req back only from
# a two-way receiver. src of every packet is what matters — decode the message
# type from the first payload byte's low nibble (0=SYNC, 1=DELAY_REQ, 8=FOLLOW_UP,
# 9=DELAY_RESP, 0xb=ANNOUNCE, 0xc=SIGNALING):
ssh root@<ha-ip> 'timeout 90 tcpdump -i any -nn -s 64 -w /tmp/ptp.pcap "udp portrange 319-320"'
# NB: `-i any` gives linktype 276 (LINUX_SLL2) = a 20-byte link header, not 16.
```

### First: "needs several attempts" is usually **our** leaked session, not the receiver

Before concluding anything about a receiver, separate the two failure modes — they look
similar in the UI and have opposite causes. **Ask whether RTSP made progress.**

| | wedged AirTunes (receiver's fault) | leaked session (ours) |
|---|---|---|
| log | fails at the *first* request, no `airplay_client` line in between | `Events port … connect timed out after 3s` → `RECORD timed out` |
| `GET /info` from another host | **zero bytes**, no reply ever | answers in ~4 ms |
| clears by | mains power cycle only | the next attempt, or on its own — "works after a few tries" |

If `GET /info` answers, the receiver is healthy and the fault is on our side. **The
mechanism, fixed 2026-08-12:** the vendored `Connection` has **no `Drop` impl**, `stop()`
sends only FLUSH, and only `disconnect()` sends TEARDOWN — so a *failed* connect used to
drop the value and tell the receiver nothing, leaving its single AirPlay session occupied
(and detaching, not aborting, the `events_task`/`timing_task`/`ptp_master_sync_task` that
`setup()` had already spawned). Retry N+1 then met a receiver still holding retry N's
session: its event port stops accepting, RECORD times out, and that failure abandons
another session — self-sustaining until the receiver's own session timeout expires. That
is the "I always need to reconnect it a few times" symptom, and it was self-inflicted.
`connect_one` now calls `abandon()` (bounded `disconnect()`) on every failure path.

So a receiver needing several attempts is **not** evidence of a bad receiver. Confirmed on
the Pioneer VSX-934 on 2026-08-12: it failed our connects repeatedly while answering
`GET /info` in ~4 ms throughout, with the liveness probe never once demoting it.

### A receiver that accepts TCP but answers nothing ("wedged AirTunes")

Symptom: one AP2 output never plays while others on the same source do. The log
shows only

```
AP2: connect attempt 1/2 to 'ap2-dev-<name>' (<ip>) failed: connect/pair failed: Operation timed out
```

repeated, with **no `airplay_client` line between "Connecting to AirPlay2 at …"
and the failure** — the very first RTSP request got no reply (that message is
`airplay_core::Error::Timeout` from the 10 s response read in
`airplay-rtsp/connection.rs`, *not* our 12 s `AP2_CONNECT_TIMEOUT`; the two are
easy to confuse). Reproduce off the daemon entirely, from any host on the LAN:

```
# one plaintext RTSP round-trip; a healthy receiver answers in ~4 ms
printf 'GET /info RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: AirPlay/745.83\r\n\r\n' \
  | nc <receiver-ip> 7000 | head -c 200
```

**`nc -z` / any TCP-connect check is worthless here** — and this is the trap.
`connect()` returns success because *our* kernel completed the handshake on
receiving the SYN-ACK; the receiver, with a full accept queue, dropped our final
ACK and never finished on its side. tcpdump is the tell:

```
ssh root@homeassistant.local 'timeout 12 tcpdump -ni any "host <ip> and tcp port 7000"'
# wedged: SYN → SYN-ACK → our ACK → our data/FIN, then the receiver
#   re-transmits the SAME SYN-ACK at 1.5/3.5/7.5 s and ACKs nothing.
# healthy: the handshake completes once and data is ACKed.
```

Confirmed 2026-08-10 on the Pioneer VSX-934 (`192.168.178.35`): `:7000` "open"
to every connect, **zero bytes** returned to `GET /info`, `OPTIONS`,
`/server-info` or plain HTTP, while the same unit's web UI (`:8080`) and eISCP
control (`:60128`) answered in milliseconds and its mDNS `_airplay._tcp` record
was live and byte-identical to the working Yamaha's (same `srcvers=366.0`,
`features=0x445F8A00,0x1C340`, `flags=0x4`, `acl=0`). So: not the network, not
the features, not our code path — the receiver's AirTunes process had stopped
calling `accept()`.

Soft remedies did **not** work, and how they fail is itself diagnostic — the
network module answers *queries* but refuses *state changes*:

```
# eISCP (Onkyo/Pioneer, TCP 60128). Frame: "ISCP" + hdr_size(16) + data_size + 01 00 00 00 + "!1<CMD>\r"
PWRQSTN -> PWR01     (on)          SLIQSTN -> SLI2B (NET input)
PWRSTBY -> PWRN/A    (refused!)    NLSI7   -> NLSN/A (refused!)
```

Only a **mains power cycle** clears it. Since `ap2_liveness` probed with a bare
`TcpStream::connect`, such a receiver read `present: true`/green in the UI
indefinitely — fixed by probing with a real `GET /info` round-trip
(`outputs/ap2/probe.rs`) and publishing the verdict to `outputs/ap2/health.rs`, which surfaces as
`last_error` on `/api/outputs` + the routing matrix. A `GET /info` probe is safe
mid-session: a Yamaha WX-021 streaming from this daemon answered six of them in
~4 ms each without a glitch.

### Real-time thread inventory

```
docker exec app_local_pipewire_audio_router sh -c \
  'for t in $(ls /proc/$(pidof bridge-daemon)/task); do chrt -p $t; done'
```

Elevated threads should be: PipeWire `data-loop.N` (FIFO 83, ×6),
`libairptp` (55), `rt-sender` (50), `ap2-producer` (48), capture/producer
mainloops (45), `ap2-relay`/`sendspin-relay` (40). (libairptp's worker may
show a sibling thread's `comm` because its `thread_name_set` callback path
can be null — identify it by the unique FIFO-55 priority, not the name.)

### Thread ages, not just priorities — finding leaked senders

`chrt` says what priority a thread runs at; it does not say whether the thread should
still exist. For that, read each thread's `starttime` (field 22 of
`/proc/<pid>/task/<tid>/stat`, in ticks since boot) and its CPU (fields 14+15), and take
**two samples a few minutes apart**:

```bash
C=$(docker ps --format '{{.Names}}' | grep pipewire_audio_router)
docker exec $C sh -c 'cat /proc/uptime; for t in /proc/$(pidof bridge-daemon)/task/*; do cat $t/stat; done'
# per thread: age = uptime - starttime/100 ;  cpu = (utime+stime)/100
# comm is between the first '(' and the LAST ')' — parsing on whitespace breaks
# on thread names containing spaces, and on the parens themselves.
```

The signal is a **generation pattern**: several threads with the same `comm` and clearly
different ages, where the count exceeds the number of things actually being served. On
2026-08-12 a 40-minute-old daemon had three `ap2-producer` and five `rt-sender` threads
against exactly **one** receiver streaming, spawned in three bursts that lined up with an
alignment session, a failed Pioneer connect and a group restart. Two of the orphaned
producers were still burning ~1.4 % CPU each with nothing to feed. Cross-check "how many
should there be" on the wire rather than from the log:

```bash
# distinct UDP flows to a receiver — one audio flow per live session, plus 319/320 gPTP
ssh root@homeassistant.local 'timeout 8 tcpdump -ni any -c 400 "udp and host <receiver-ip>" \
  | grep -oE "192\.168\.178\.22\.[0-9]+ > <receiver-ip>\.[0-9]+" | sort | uniq -c | sort -rn'
```

Root cause and fix: `outputs/ap2/server.rs` reached teardown only after its connect loop
had finished every member, so a group abandoned mid-connect never stopped the sender
threads of the members already up. Teardown is now a function called on every exit path,
a stop request is observed *between* members, and the post-connect volume read is bounded
(`AP2_VOLUME_READ_TIMEOUT`).

### The web UI is served but has no interactive parts

Symptom: pages load, and the graph/outputs/settings widgets never populate. This is
**not** the mDNS/CPU-starvation mode below — there the whole daemon is slow. Here most of
the API is instant and a few endpoints hang forever, so bisect by endpoint before
anything else:

```bash
for ep in /api/status /api/sources /api/settings /api/nodes /api/agents \
          /api/sync/settings /api/groups/music /api/align \
          /api/outputs /api/outputs/discovered /api/routing; do
  printf '%-28s ' "$ep"
  curl -s -o /dev/null -w 'http=%{http_code} t=%{time_total}s\n' --max-time 6 \
    "http://192.168.178.22:8099$ep"
done
```

A **split result is the diagnosis**: everything at 1–3 ms except a few at `http=000`
after the timeout. Then map the hanging set onto the shared state it locks — the
intersection names the wedged mutex. On 2026-08-12 exactly `/api/outputs`,
`/api/outputs/discovered` and `/api/routing` hung, which is precisely the set that locks
`state.ap2_control`; `/api/status` (PipeWire registry + `ap2_devices` + `sendspin_devices`
+ routing store) and `/api/agents` (the *other* async mutex) answered in 2 ms, which ruled
every other candidate out without touching the code.

Root cause: `Ap2Control`'s guard was held across `mpsc::Sender::send().await` on a
depth-32 channel, so any group task that stopped draining it blocked every reader of that
guard — including the reconciler, which is why AP2 also stopped retrying. The trigger was
the unbounded `get_volume()` after `register()`: a wedged receiver left the device
registered with nothing consuming its commands. Now every `Ap2Control` mutator is
**synchronous** and uses `try_send` (`outputs/ap2/volume.rs`; the module header documents
the rule and `a_task_that_stopped_draining_cannot_stall_a_writer` pins it).

**Check the FD cascade too — it outlives the cause.** Each hung request leaks a socket
and a blocked task, because the handler can't notice the client left:

```bash
docker exec $C sh -c 'PID=$(pidof bridge-daemon); ls /proc/$PID/fd | wc -l;
  grep "Max open files" /proc/$PID/limits;
  awk "NR>1{print \$4}" /proc/$PID/net/tcp | sort | uniq -c'   # 08 = CLOSE_WAIT
```

45 sockets sat in `CLOSE_WAIT` on `:8099` against a **1024** soft limit, climbing with
every UI retry — so a wedged lock eventually becomes EMFILE and takes the audio sockets
with it. A large `CLOSE_WAIT` count on 8099 is itself the fingerprint of a handler
blocked on shared state.

### USER ACTION logging

`grep "USER ACTION"` in the daemon log marks every routing-graph/source
mutation (link/unlink/forget/route/unroute/set-airplay-source/
add-remove-output); a `STACK:` marker on `sources::airplay::start`
distinguishes human actions from stack-driven churn.

## mDNS storm / high CPU

Symptom: sustained high CPU, stuttering audio, a device present-in-graph
but silent, dead web UI. **Check host CPU first** — this has been host
oversubscription (other add-ons) *and* the daemon's own mDNS daemons.

> Distinguish this from the deadlock above, because "dead web UI" is common to both and
> they need opposite investigations. **Starvation:** the daemon is uniformly slow and CPU
> is pegged (`docker stats`, per-thread CPU). **Deadlock:** CPU is unremarkable and the
> API is *bimodal* — most endpoints in single-digit ms, a specific few hanging forever.
> The endpoint sweep above costs seconds and tells you which one you have; run it before
> reaching for `tcpdump`.

```
# is the LAN flooding 5353? then which name is the culprit?
ssh root@homeassistant.local 'tcpdump -ni any "udp port 5353" | wc -l'
ssh root@homeassistant.local 'tcpdump -ni any "udp port 5353" | grep -i <name>'
```

Known root cause (2026-07-26): with `host_network: true`, `mdns-sd` had
joined the multicast group on all ~15 host interfaces (veths + bridges +
LAN NIC), so an address-less `_airplay._tcp` device (an old Xiaomi
projector with no AAAA record) triggered endless AAAA re-queries that
echoed out every veth and back — a self-amplifying storm (~11.5k pkt/s)
× multiple `ServiceDaemon`s. Fix (shipped): LAN-restrict + consolidate mDNS
daemons — see
[decisions.md](decisions.md#mdns-one-shared-lan-restricted-daemon-not-one-per-browseradvertiser).
Note HA **Core** is also `network_mode: host`, so any residual
`172.30.32.1` mDNS after the fix is HA Core's zeroconf, not the add-on's
(our daemons sit at 0% CPU).

Per-thread CPU is how the mDNS daemons were caught red-handed: after
stopping the other add-ons, `pipewire-router` alone drew 173% CPU and the
6 `mDNS_daemon` threads were ~94% of it (every audio + tokio thread
combined ≈6%). The UI dies because axum (SCHED_OTHER tokio) is starved by
the mDNS spin + host load.

## Data-file recovery (add-on `/data`)

The SSH add-on can't see the daemon container's `/data`. To repair a
corrupt store file (e.g. `airplay_clients.json`):

```
printf '{"clients":[]}' | ssh root@host 'docker run --rm -i --entrypoint sh \
  --volumes-from app_local_pipewire_audio_router \
  "$(docker inspect app_local_pipewire_audio_router --format {{.Config.Image}})" \
  -c "cat > /data/<file>"'
```

## Ruled out / key learnings (don't re-chase)

- **An empty ESPHome device log is almost certainly your own buffering, not the device.**
  This cost weeks as item 8 ("the subscription delivers nothing") and the devices were
  talking the whole time — `timeout`'s SIGTERM discards Python's unflushed stdout. See
  *Device-side logs* above. The corollary is bigger than the bug: the daemon has no way to
  tell "sent" from "rendered", and the device does, so reach for its log **before** reaching
  for `tcpdump` or your ears.
- **"Receives audio but renders nothing" is a dropped `stream/start`, and it is a race.**
  The device never processes the stream's codec header, so it builds no decoder and renders
  nothing while draining the socket at full rate and reporting `PLAYING`. Fixed by
  re-issuing the frame a second after the join (`STREAM_START_REISSUE`). `SharedTimeline`
  age, the redial gap, overlapping sockets, `stream/clear` and the group lead are all ruled
  out — see sendspin-open-items.md item 4 before re-deriving any of them. Note the *first*
  pass blamed the idle-sender handover on 3/3-vs-9/9 field evidence and the harness did not
  bear that out; the correlation is real, the causation is not established.

- **"Dropout every ~2 min" was source churn, mostly self-inflicted
  testing.** Each add-on restart needs a manual Sources-tab **Save**
  (→ `set_airplay_source` → restarts `airplay-in`) so the sender
  reconnects. `airplay-in` is stable when untouched (proven with the USER
  ACTION markers).
- **No shairplay cycling bug.** `Failed to bind additional listener …
  EADDRINUSE ::` is a benign dual-stack IPv6 warning (IPv4 `0.0.0.0`
  already covers the port).
- **Send-path jitter is not the cause of silence.** `Decode took …` /
  `Sender thread fell behind …` appear on both the file and live paths and
  are a separate (now-fixed) real-time-hygiene issue — don't over-index on
  them when chasing "no audio."
- **Validate new/edited vendored C with `gcc -std=c11` locally before the
  ~15-min cross-deploy.** Host gcc is C23 (so `bool`/`size_t` are builtin);
  the aarch64 cross-gcc is not — a missing `<stdbool.h>`/`<stddef.h>`
  compiles on the dev box and fails only in the cross-build.
- **A TCP connect proves nothing about an AirPlay receiver.** It succeeds against
  a receiver whose AirTunes has stopped accepting (see the wedged-AirTunes section
  above), so `nc -z`, `ss`, and the old `ap2_liveness` probe all called a dead
  receiver healthy. Only a request/response round-trip is evidence.
- **A repeated connect failure is not proof the receiver is wedged, and the inverse
  mistake is easy to make.** On 2026-08-12 the `SETUP/RECORD timed out` pattern was read
  as the wedged-AirTunes state and a mains power cycle was recommended — while the unit
  was answering `GET /info` in ~4 ms and had never been demoted by the liveness probe. The
  actual cause was our own un-torn-down session (see the table above). **Run the `GET
  /info` one-liner before blaming a receiver**; it is two seconds of work and it
  discriminates the two cases outright. Certified receivers do fail this way, but far less
  often than we leak a session.
- **A leaked `rt-sender` thread** persists to a receiver after a spike stop
  if teardown doesn't join the sender thread — shows as RTP still flowing
  (rms=0) with "no spike running." Confirm sender count with the chrt
  inventory above; two `Connection`s fighting one receiver is a candidate
  for "flaky, needs many reconnects." The same class bit again on 2026-08-12 via a
  different route (teardown unreachable when a group was abandoned mid-connect) — see
  *Thread ages* above, and prefer thread **age** over count when attributing it.
- **A stuttering symptom does not mean the daemon is at fault, and its own
  instrumentation will tell you.** On 2026-08-12 the sendspin relay logged a steady
  46.9 blocks/s with `received == replied` on every device and no drops or underruns,
  while the host ran frigate-beta at 113 % CPU on a 4-core Pi with 1.76 GB swapped —
  i.e. the same host-starvation mode as 2026-07-26, plus leaked producers on top. Read
  the relay's own line first (`sendspin relay '<group>' [codec]: N blocks in 10.0s`);
  if it is steady and drop-free, the stutter is downstream or host-side, not in the
  send path. Note also that a low `group_lead_*_ms` is **not** automatically the
  culprit: an opus lead of ~40 ms is stutter-free on an unloaded box, so a small lead
  only matters together with contention.
