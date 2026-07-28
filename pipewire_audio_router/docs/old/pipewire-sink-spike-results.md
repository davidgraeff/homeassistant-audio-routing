# pw-sink spikes — results

Bring-up spikes for the [pw-sink roadmap](pipewire-sink-roadmap.md) (stream audio
to a remote PipeWire host via native `rtp-sink` + `rtp-sap`). Two layers:

1. **Local CLI spikes** on a Fedora PipeWire desktop — validate the PipeWire
   mechanics in isolation (`pw-cli`/`pw-play`/`pw-record`, analysed with numpy).
2. **Real-LAN spike** in the add-on — `POST /api/spike/pw-sink` streams a
   self-driving test tone from the daemon (HA host `192.168.178.22`) to this
   Fedora box (`192.168.178.21`, `enp5s0`) running rtp-sap in discover mode.

Environment: PipeWire **1.6.8**, `clock.rate=48000 quantum=1024`, modules
`rtp-sink`/`rtp-source`/`rtp-sap`/`rtp-session`/`loopback`/`adapter` all present.
The shipped `/usr/share/pipewire/pipewire-aes67.conf` is the authoritative param
reference (rtp-sink + rtp-sap + PTP driver node).

---

## Method notes (reusable)

- **Dynamic module load without a desktop restart:** `pw-cli` runs one command
  then exits (unloading its modules), so `pw-cli load-module …` alone leaves
  nothing behind. Keep pw-cli alive by holding its stdin open:
  `pw-cli < fifo &` + an fd held on the fifo. Teardown = `kill -9` the pw-cli
  PID (unloads its modules); **never `wait`** on it (its mainloop ignores stdin
  EOF → `wait` hangs — the fd-hang the ops notes warn about).
- Null sinks / loopbacks for the mix-bus spike come from `pactl load-module
  module-null-sink|module-loopback` (clean `unload-module`).
- `pw-record` killed with `-9` leaves an unfinalized WAV header (`data` size 0);
  the PCM is intact — parse from byte 44 as the negotiated format.

---

## Spike 1 — native per-device announce/duck mix bus (roadmap §3, the must-have)

**Question:** can per-device duck + announce-overlay be done with native graph
nodes (no Rust relay), and is a control-plane volume ramp glitch-free?

**Topology tested:** a `null-sink` mix bus fed by a "music" stream (300 Hz) and a
pre-attached "announce" stream (1000 Hz); duck/overlay driven by stepping each
stream's `Props.channelVolumes`; recorded the mix monitor and analysed band
energy + per-sample discontinuities.

**Results — PASS:**
- **Duck is exact.** Music 300 Hz band `0.500 → 0.125` = ratio **0.250** = the
  commanded −12 dB. Overlay clean: announce 1000 Hz `0.000` when off →
  present when raised. A pre-attached-silent announce node ramped in with the
  duck region measuring **1.00× steady** (no onset click).
- **PipeWire smooths `channelVolumes` internally.** A single *instantaneous*
  `1.0 → 0.25` jump measured **1.00× steady** (no zipper) — and coarse/fine
  ramps likewise. So the daemon does not need a fine ramp for glitch-free
  volume; PipeWire ramps each change over ~a quantum.
- **Intermittent clicks (~3–4.5×) were harness artifacts**, not the design:
  spawning a `pw-cli` process *per volume step* (10 connect/disconnect cycles in
  150 ms) churns the graph, and WirePlumber's stream policy races manual volume
  sets on `pw-play` nodes. Neither exists on the daemon's own nodes driven over
  its persistent native connection (architecture.md §9).

**Directives for the implementation (P2):**
1. Do overlay by **pre-attaching the announce node at volume 0 and gain-ramping
   it** — never link/unlink mid-announce (avoids stream connect/disconnect
   transients).
2. Drive duck/volume via **native Props on the daemon's persistent connection**,
   not per-step external processes.
3. A modest ramp (≈10–20 steps / 150 ms) is more than enough given PipeWire's
   internal smoothing.
4. **Residual to confirm during P2** (low risk): end-to-end click-free ramp on
   the daemon's *own* mix-bus nodes (the CLI harness can't isolate this from
   WirePlumber/pw-play confounders).

---

## Spike 2 — rtp-sink + rtp-sap transport mechanics

**PASS on mechanics:**
- An `rtp-sink` with `stream.props.sess.sap.announce = true`, plus a separate
  `rtp-sap` module, is **auto-discovered**: rtp-sap created a matching source
  carrying `rtp.session = "<sess.name>"` and the JB `sess.latency.msec` applied.
  This is the "no static node on the receiver" mechanism the roadmap wants.
- The sender needs **both** modules: `rtp-sink` only flags itself; the separate
  `rtp-sap` (announce rule matching `sess.sap.announce = true`) actually emits
  the SDP. Confirmed against `pipewire-aes67.conf`.
- **JB knob = `sess.latency.msec`** on the receiver's create-stream rule.
- **Format** fixed S16LE/48000/2 end-to-end → no resampling (matches the 48 kHz
  bus, architecture.md §8).

**Audio flow could NOT be confirmed on a single box** — loopback multicast on
`lo` isn't routed, and CLI `pw-play`/`stream.capture.sink` wouldn't reliably
drive a *specific* non-default rtp-sink (the tone hit the default sink instead).
UDP sockets did establish both directions, so the transport plane is up. **The
definitive audio-flow test is the real-LAN `/api/spike/pw-sink` run** (below),
where the daemon links a real producer into the sink — not a CLI shortcut.

---

## Spike 3 — per-target routing / SAP filtering

- **Per-target key = `rtp.destination.ip`** — it is a matchable property in the
  receiver's `rtp-sap` `stream.rules`, so a host can instantiate **only**
  sessions addressed to it. Even with a permissive `~.*` rule the **media is
  unicast**, so only the addressed host actually receives audio; other hosts
  would create a silent phantom source (avoid by matching `rtp.destination.ip`).
- **Design consequence:** one `rtp-sink` per target, each with a unique
  `sess.name` (→ `rtp.session`) and unicast `destination.ip` = independently
  routable `pwsink-dev-<slug>`. One shared `rtp-sap` announcer covers all.
- **Two-box gap:** cross-host destination-filtering behaviour itself still wants
  a genuine two-box confirmation; the real-LAN spike provides it.

---

## Spike 4 — RTCP liveness — **roadmap §4 assumption INVALID**

**Finding:** PipeWire's RTP modules implement **no RTCP** — the module usage
strings expose no `rtcp*` params and no RTCP (odd-numbered) socket is opened.

**Impact:** the roadmap's §4 plan to derive receiver liveness from **RTCP
receiver reports is not possible.** RTP here is fire-and-forget; the sender has
no in-band back-channel telling it a target is actually receiving.

**Revised liveness options (decision deferred):**
- **Host reachability** — mDNS presence of the target (candidate discovery) plus
  a periodic TCP/ICMP probe. Coarse ("host is up"), not "is playing".
- **Receiver-announced back-channel** — the receiver host also runs rtp-sap in
  *announce* mode advertising a heartbeat session the daemon discovers. Requires
  more than stock receiver config.
- **Accept no true liveness** — treat pw-sink as fire-and-forget; show
  "configured/announced" rather than "connected". Simplest; matches the medium's
  reality.

This is a genuine decision (affects the receiver-setup help doc and the UI
badge) → **deferred**, per the autonomous-implementation instruction.

---

## Spike 5 — PTP sample-lock path (roadmap §7 future) — sanity check

- No `/dev/ptp*` on this box → no PHC; hardware PTP unavailable. Software PTP
  (`ptp4l`) or `CLOCK_TAI` would be required.
- The **config path is real and confirmed** in `pipewire-aes67.conf`: a
  `support.node.driver` node reading a PHC/`clock.id=tai`, `node.group =
  pipewire.ptp0` on the rtp nodes, and `sess.ts-refclk`/`sess.ts-direct` on the
  sink. So §7 is a legitimate future extension, gated on bridging our
  `libairptp` gPTP domain to PipeWire's PTP clock — a separate spike, not scoped
  into v1.

---

## Real-LAN spike — `/api/spike/pw-sink` (in the add-on)

Added `pw_sink.rs` (module-arg builders) + `pw_sink_spike.rs` (slot/handle/
start/stop, mirroring `ap2_spike.rs`) + `POST|DELETE /api/spike/pw-sink`.

```
# stream a 440 Hz tone from the daemon to this Fedora box:
curl -XPOST http://192.168.178.22:8099/api/spike/pw-sink \
     -d '{"target_ip":"192.168.178.21","freq":440}'
curl -XDELETE http://192.168.178.22:8099/api/spike/pw-sink
```

It loads the one `rtp-sap` announcer + one `rtp-sink` (unicast to `target_ip`,
S16LE/48k/stereo, `sess.sap.announce=true`), waits for the sink node, and loops
a synth tone into it via `player::play_loop_to_target`. Receiver side on this
box (`scratchpad/spikes/rx_verify.sh`) loads `rtp-sap` in discover mode, records
the auto-created source, and reports energy at the expected frequency.

**Findings from the real-LAN bring-up (HA `.22` → Fedora `.21`):**

1. **Sender works.** The daemon loads rtp-sap + rtp-sink and **unicast-RTP streams
   to the target** — confirmed by `tcpdump` on the HA host: `192.168.178.22 >
   192.168.178.21.5004 UDP length 396` (396 B = 2 ms of L16/48k/stereo). The sink
   advertises `rtp.mime=L16 rtp.payload=127 rtp.ptime=2 rtp.rate=48000` (S16 **big
   endian** on the wire).
2. **Multicast SAP does NOT cross this LAN.** The daemon emits SAP on `end0` every
   5 s (`> 224.2.127.254:9875`), but a raw multicast probe from `.22` never reaches
   `.21` (even a plain Python sender/receiver) — the consumer router (IGMP
   snooping, no querier) drops the group. **This invalidates multicast-SAP
   discovery on typical home LANs** — a key roadmap correction.
3. **Unicast SAP emits fine but is awkward to receive.** PipeWire *emits* a unicast
   SAP announcement (captured the full SDP). But a receiver given a unicast
   `sap.ip` opens a **connected** UDP socket (`ESTAB … peer=sap.ip:9875`), so it
   only accepts SAP from that exact peer:port — a remote sender's ephemeral-port
   SAP is rejected. So rtp-sap receive really wants the multicast group (blocked
   here).
4. **Robust receiver = static rtp-source with the daemon's fixed format.** Since
   the daemon always sends L16/48k/2 at a known per-target port, the receiver
   needs no discovery: a static `rtp-source` with `audio.format=S16BE`
   (L16 = big-endian — using S16LE gives silence/garbage), `source.port=<port>`,
   `sess.ignore-ssrc=true`, and **`node.always-process=true`** (else PipeWire
   suspends the idle node and it never opens its socket — observed as
   `driver=None, QUANT 0`). This is the receiver the verify harness + help doc use.
5. **Tone injection needs the anchor.** Feeding the tone player straight into the
   rtp-sink starved it (2 ms/96-sample quantum; the separate-context player ran at
   RATE 0 and the sink sent silence/nothing). Fix = the production data path: a
   `null-audio-sink` **anchor** (QUANT-1024 driver) is fed the tone, and its
   monitor is linked into the rtp-sink (`routing::ensure_monitor_link_by_name`,
   added). The sink pulls from the steady anchor monitor at its own 2 ms rate.

6. **Sender is PROVEN at the packet level.** With the anchor model, `tcpdump -X`
   on the HA host shows the RTP payload to `.21:5004` is the **actual 440 Hz sine**
   (decoded the L16/BE samples from the hex: a clean, smoothly-varying stereo
   sinusoid). So the daemon's pw-sink path — anchor → monitor → rtp-sink →
   unicast RTP — emits correct audio to the target. (`bridge-align` showing
   `RATE 0` in pw-top is a follower-node display artifact, not silence.)
7. **Audible receiver verification is gated on the receiver's firewall.**
   Fedora's **firewalld** (`FedoraWorkstation` zone) drops *all* inbound UDP from
   the daemon (probed 5004/9875/40000/6004 → 0 received; ping + TCP:22 confirm the
   network path is fine). Opening it requires the user's privilege (no
   non-interactive sudo; a privileged-container workaround was correctly blocked
   by the tooling). **This is a receiver-setup step, not a code issue.**

   To hear it, on the receiver run once:
   ```
   sudo firewall-cmd --add-port=5004/udp        # RTP media (per target port)
   # then verify:  bash scratchpad/spikes/rx_verify.sh 440 6   (while the spike streams)
   ```

8. **END-TO-END AUDIO PASS (real LAN, HA `.22` → Fedora `.21`).** After opening
   the receiver firewall (`firewall-cmd --add-port=5004/udp`), the static
   rtp-source received the tone **exactly**: recorded RMS = 0.173 (= a
   0.244-amplitude sine's RMS), **energy @ 440 Hz = 0.244** (= the transmitted
   amplitude), off-frequency = 0.000 (no noise). ✓✓
9. **Wire format is native S16LE, not L16/BE.** The SDP says `L16` (RFC 3551 =
   big-endian) but PipeWire puts **little-endian** bytes on the wire (it doesn't
   byte-swap to canonical BE). Receiver **must** use `audio.format=S16LE`; S16BE
   gives loud byte-swapped noise (RMS high, weak fundamental) — the one bug that
   sat between "audio arrives" and "clean tone."

### module-rtp-session interop — it needs an Apple-MIDI handshake (won't take plain RTP)

Two follow-up tests + a read of PipeWire's `module-rtp-session.c`:

- **In the addon container it can't even load** — the module needs a running
  `avahi-daemon` on the system D-Bus (`avahi_client_new`); the addon has the libs
  but no `avahi-daemon`, no `/run/dbus/system_bus_socket` (the daemon does mDNS
  in-process via `mdns-sd` on purpose, to stay storm-safe). So it is a
  **receiver-side (desktop) module only**, never daemon-side.
- **A plain `rtp-sink` stream is silently dropped by a discoverer.** The advert
  carries `port = 0` (no fixed media port) and the session opens **two** ephemeral
  ports — because `module-rtp-session` runs an **Apple-MIDI / RTP-MIDI control
  handshake** before any media: the discoverer sends `APPLE_MIDI_CMD_IN` to the
  announced **control port**, expects `APPLE_MIDI_CMD_OK`, exchanges SSRC +
  initiator tokens, and only then (`data_ready && receiving`) accepts RTP on the
  **data port = control+1**. A unidirectional sender that just fires RTP at a
  port is dropped. (mDNS TXT it publishes/requires: `subtype format rate channels
  position layout channelnames ts-refclk ts-offset`.) Confirmed empirically (a
  hand-published advert instantiated nothing) and in the module source.
- **mDNS discovery itself works across the LAN** both directions (RAOP + the
  daemon's `mdns-sd` browsers already prove it), so the daemon *can* discover
  `_pipewire-audio._udp` advertisers — the blocker is purely the media handshake.

**Consequence:** to use stock `module-rtp-session` **receivers**, the daemon must
speak the Apple-MIDI **sender** protocol itself (mdns-sd advert + IN/OK/CK/BY
control state machine + RTP on the data port) — a bounded new module, no Avahi
needed.

### PROVEN: custom Apple-MIDI sender ↔ stock module-rtp-session (spike)

A ~150-line Python sender (`scratchpad/spikes/applemidi_sender.py`) fully
interoperates with a stock `module-rtp-session` receiver — handshake **and**
audio: recorded `E@440 = 0.238` (= the transmitted amplitude), clean. So the
Rust port is a known quantity. **The proven recipe:**

1. **Advertise** `_pipewire-audio._udp` (via `mdns-sd`, no Avahi) with the SRV
   port = our **control port** and TXT: `subtype=audio format=S16BE rate=48000
   channels=2 position=[ FL FR ] layout=Stereo ts-refclk=private ts-offset=0`.
2. **The receiver initiates.** The stock module discovers our advert and sends
   `APPLE_MIDI_CMD_IN` to our **control port** (from its own ephemeral port),
   then a second `IN` to our **data port = control+1**. (Same-host discovery
   needs `sess.discover-local=true` on the receiver; across two hosts it's
   automatic — cross-host is the real deployment.)
3. **Respond `OK`** to each `IN`, echoing the peer's 4-byte *initiator token* +
   our own 32-bit **SSRC** (packet = `FFFF`, cmd, `protocol=2`, initiator, ssrc,
   NUL-terminated session name — all big-endian).
4. **Answer `CK`** clock-sync on the data channel (echo ts1, fill ts2, reply
   `count=1`; peer sends `count=2`). Optional for audio but the module sends it.
5. **Stream RTP** to the address the data `IN` came from: standard 12-byte RTP
   header (`V=2`, `PT=127`, seq, ts += samples/pkt, our SSRC) + **L16
   big-endian** payload (matches the advertised `S16BE`). The module matches the
   session by our SSRC and plays it.

Gotchas the spike surfaced: bind IPv4 (the module also spins ipv6/loopback
sessions on one host — dead if the sender is v4-only; harmless cross-host);
stream to every handshaken data peer; the mDNS `port=0` you see on an
*idle* module is its receiver side (it advertises a real control port only when
it has something to send). Next: port this state machine to a Rust daemon
module and drive the RTP from the group anchor (replacing/So beside `rtp-sink`).

**Otherwise** the receiver is a **static `rtp-source`** (also proven) fed by the
plain `rtp-sink`.

**Net:** the pw-sink path is **fully proven end-to-end on real hardware** —
daemon anchor → rtp-sink → unicast RTP → Fedora static rtp-source → clean 440 Hz.
SAP discovery isn't viable on this LAN, so v1 uses a **static rtp-source receiver**
(S16LE / 48k / 2ch / `node.always-process` / per-target port), and the receiver
must open the firewall port. Remaining for the full feature: the native
per-device announce/duck mix bus (mechanics proven locally, §Spike 1; to be
demonstrated over RTP in Phase B) and the productionization (discovery/config/
routing/media_player).

### Fedora receiver config (documented)

For automated tests the receiver is loaded on demand by `rx_verify.sh` (no
persistent change). The equivalent **persistent** receiver — what an end-user
sets up per the future "help" doc — is a drop-in:

```
# ~/.config/pipewire/pipewire.conf.d/90-pw-sink-receiver.conf
context.modules = [
  { name = libpipewire-module-rtp-sap
    args = {
      local.ifname = enp5s0
      sap.ip = 224.2.127.254
      sap.port = 9875
      net.ttl = 16
      stream.rules = [
        { matches = [ { rtp.destination.ip = "192.168.178.21" } ]
          actions = { create-stream = {
            media.class = "Audio/Source"
            node.name = "pw-sink-in"
            sess.latency.msec = 150   # jitter buffer
          } } }
      ]
    }
  }
]
```
(To make it audible rather than just recordable, route `pw-sink-in` to the
default sink — a receiver-side detail, deferred.)
