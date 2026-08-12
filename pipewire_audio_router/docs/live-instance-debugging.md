# Live-instance debugging

How to poke a running bridge-daemon instance and the gotchas that cost
time during bring-up. Companion to [architecture.md](architecture.md)
(how it's built) and [airplay2-output.md](airplay2-output.md) (what's
done vs. planned).

**Open investigations** with their own plans (measurements, hypotheses,
decision points): [sendspin-open-items.md](sendspin-open-items.md) (open work) and the archived
[sendspin-group-churn-plan.md](old/sendspin-group-churn-plan.md) (the investigation)
(adding a speaker silences the group for >10 s) and
[rtp-input-dropouts.md](rtp-input-dropouts.md) (Bluetooth/RTP input
drops out every 5–10 min for up to 2 min).

## Environment / who's who (dev setup)

- **HA host (daemon)** = `192.168.178.22` (`homeassistant.local`), daemon REST API
  on `:8099`. SSH: `ssh root@homeassistant.local` (add-on container;
  `export XDG_RUNTIME_DIR=/run/user/0` for `pw-*` tools).
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

`ptp_locked` / `ptp_lock_age_s` derive from libairptp
`airptp_peer_last_seen()` → `Ap2PtpService::peer_lock_age()` and surface as
a UI badge in `OutputsTab.svelte` (red "no PTP lock" / green "PTP ✓").
`last_seen` is bumped on every gPTP packet a locked receiver sends, so it's
a true lock-liveness signal.

> **Caveat learned the hard way:** single-device AP2 rendering was observed
> to work **while gPTP was effectively silent** on the wire, so `ptp_locked`
> can read `false` while audio plays — it is a *sync-quality / multi-room*
> signal, not a "will it render" signal. gPTP health can only be judged
> **while actively streaming** to a receiver: receivers only send Delay_Req
> (which refreshes `last_seen`) during an active RTSP session; at idle
> `tcpdump` on 319/320 shows ~0 packets and that is **expected**.

```
# while streaming, expect continuous Sync (~8/s) out + Delay_Req back:
ssh root@homeassistant.local 'timeout 6 tcpdump -ni any "udp and (port 319 or port 320)"'
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
