# Live-instance debugging

How to poke a running bridge-daemon instance and the gotchas that cost
time during bring-up. Companion to [architecture.md](architecture.md)
(how it's built) and [airplay2-roadmap.md](airplay2-roadmap.md) (what's
done vs. planned).

**Open investigations** with their own plans (measurements, hypotheses,
decision points): [sendspin-open-items.md](sendspin-open-items.md) (open work) and the archived
[sendspin-group-churn-plan.md](old/sendspin-group-churn-plan.md) (the investigation)
(adding a speaker silences the group for >10 s) and
[rtp-input-dropouts-plan.md](rtp-input-dropouts-plan.md) (Bluetooth/RTP input
drops out every 5–10 min for up to 2 min).

## Environment / who's who (dev setup)

- **HA host (daemon)** = `192.168.178.22` (`homeassistant.local`), add-on
  container `addon_local_pipewire_audio_router`, daemon REST API on
  `:8099`. SSH: `ssh root@homeassistant.local` (add-on container;
  `export XDG_RUNTIME_DIR=/run/user/0` for `pw-*` tools).
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
docker exec addon_local_pipewire_audio_router sh -c \
  'for t in $(ls /proc/$(pidof bridge-daemon)/task); do chrt -p $t; done'
```

Elevated threads should be: PipeWire `data-loop.N` (FIFO 83, ×6),
`libairptp` (55), `rt-sender` (50), `ap2-producer` (48), capture/producer
mainloops (45), `ap2-relay`/`sendspin-relay` (40). (libairptp's worker may
show a sibling thread's `comm` because its `thread_name_set` callback path
can be null — identify it by the unique FIFO-55 priority, not the name.)

### USER ACTION logging

`grep "USER ACTION"` in the daemon log marks every routing-graph/source
mutation (link/unlink/forget/route/unroute/set-airplay-source/
add-remove-output); a `STACK:` marker on `sources::airplay::start`
distinguishes human actions from stack-driven churn.

## mDNS storm / high CPU

Symptom: sustained high CPU, stuttering audio, a device present-in-graph
but silent, dead web UI. **Check host CPU first** — this has been host
oversubscription (other add-ons) *and* the daemon's own mDNS daemons, not a
daemon deadlock.

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
  --volumes-from addon_local_pipewire_audio_router \
  "$(docker inspect addon_local_pipewire_audio_router --format {{.Config.Image}})" \
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
- **A leaked `rt-sender` thread** persists to a receiver after a spike stop
  if teardown doesn't join the sender thread — shows as RTP still flowing
  (rms=0) with "no spike running." Confirm sender count with the chrt
  inventory above; two `Connection`s fighting one receiver is a candidate
  for "flaky, needs many reconnects."
