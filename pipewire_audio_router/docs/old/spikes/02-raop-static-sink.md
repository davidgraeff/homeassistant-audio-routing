# Spike 2 result: static RAOP-sink config — PASSED end-to-end, audible on real hardware

Per PLAN.md Section 7 spike #2 and Section 5.4a. This spike went through
several rounds of debugging before reaching a real, audible pass against
the user's Pioneer VSX-934 and Dusche (WX-021) receivers. Summary first,
full debugging trail below.

## Outcome

- **Base image switched from Debian bookworm to Ubuntu 26.04 LTS.**
  Bookworm ships PipeWire 0.3.65 (2022-era), whose `raop-sink` module
  creates a sink node fine and accepts audio locally, but **never
  performs the actual RTSP handshake** with the receiver — a silent,
  version-specific bug, not a config problem. Ubuntu 26.04 LTS ships
  PipeWire 1.6.2 (newer than Debian trixie's 1.4.2, comparable to Debian
  sid's 1.6.8 but on an actual LTS), confirmed working end-to-end. See
  "Version comparison" below for the full table considered.
- **`raop.encryption.type` must be `"auth_setup"`, not `"none"` or
  `"RSA"`**, for these receivers. Both alternatives get `403 Forbidden`
  on `ANNOUNCE`; `auth_setup` completes the full RTSP session lifecycle
  (`OPTIONS` → `POST /auth-setup` → `ANNOUNCE` → `SETUP` → `RECORD`,
  `Audio-Jack-Status: connected`) and the user confirmed **audible
  playback** on Dusche.
- **`raop.port` must be discovered, not assumed 5000** (see below) — 7000
  for both devices on this network.
- WirePlumber 0.5.13 (Ubuntu 26.04) boots headless with **zero config
  overrides** — the `bluetooth.lua`-drops-with-logind crash from Debian
  bookworm's WirePlumber 0.4.13 doesn't reproduce here, so
  `container/etc-wireplumber/` was removed entirely.
- RAOP sink port names are `send_FL`/`send_FR` on this PipeWire version
  (not the numbered `playback_1`/`playback_2` seen on the old bookworm
  build) — matches the naming visible in the user's original Fedora
  qpwgraph screenshot from the start of this project.

## Version comparison that led to the Ubuntu 26.04 decision

| Base | pipewire | wireplumber | Support horizon |
|---|---|---|---|
| Debian bookworm (original, broken) | 0.3.65 | 0.4.13 | oldstable |
| Debian trixie (current stable, works) | 1.4.2 | 0.5.8 | ~5yr combined |
| Debian sid (rolling/unstable) | 1.6.8 | 0.5.15 | no support guarantee |
| Ubuntu 24.04 LTS | 1.0.5 | 0.4.17 | LTS to 2029, older PipeWire than trixie |
| Ubuntu 25.04 (non-LTS) | 1.2.7 | 0.5.8 | EOL in ~9 months |
| **Ubuntu 26.04 LTS (chosen)** | **1.6.2** | **0.5.13** | 5yr standard, up to 10 w/ Pro |

Ubuntu 26.04 LTS wins on both freshness (newest PipeWire of any
supportable option) and support length (genuine LTS, unlike 25.04) —
confirmed empirically to work, not just picked on paper.

## Full debugging trail

1. **Config/plumbing check (no real device needed):** a static
   `libpipewire-module-raop-sink` block
   (`container/etc-pipewire/pipewire.conf.d/10-raop-static.conf`) pointed
   at an unroutable placeholder IP (`192.0.2.1`, RFC 5737 TEST-NET-1)
   loads without crashing and produces an ordinary linkable
   `Audio/Sink` node. Verified via `tests/test_spike02_raop_sink_boots.sh`.

2. **First real-hardware attempt looked like it passed, but wasn't
   actually testing anything.** The original real-device script used
   `pw-cli load-module ...` to add the sink dynamically. That loads the
   module into `pw-cli`'s own transient client connection — the instant
   `pw-cli` exits (immediately, being a one-shot CLI call), the module
   and its node vanish. Fixed by having the container write a
   `pipewire.conf.d/99-real-device.conf` snippet and load it at the
   daemon's own startup instead, matching how the baked-in placeholder
   config works.

3. **Wrong port, silently.** Hardcoded `raop.port = 5000` (the
   "traditional" AirPlay 1 port). Both real devices actually use **7000**
   — confirmed via `avahi-browse -r _raop._tcp` TXT records and via
   `pw-cli info` on the host's own already-working, mDNS-discovered sink
   nodes (`raop_sink.Pioneer-VSX-934-F11B89.local.192.168.178.35.7000`,
   `raop_sink.Dusche.local.192.168.178.165.7000`). This is a *silent*
   failure mode — `pw-cat` reports success regardless, since it only
   writes into the local PipeWire node; the RTSP handshake to the wrong
   port fails independently with no error propagated back.

4. **Even with the right port, nothing happened — on any encryption
   setting.** Packet capture (`tcpdump` on the host, once `tcpdump` was
   granted `cap_net_raw`/`cap_net_admin` via `setcap` so it needs no
   sudo) showed **zero outbound connection attempts** to the receiver at
   all, regardless of a 1.5s or an 8s test clip. This turned out to be
   the Debian bookworm PipeWire 0.3.65 bug described above — the module
   never even opens a socket. Confirmed by rebuilding against Debian
   trixie (PipeWire 1.4.2) and re-capturing: this time a real TCP
   handshake and RTSP `OPTIONS`/`ANNOUNCE` exchange appeared.

5. **With the current PipeWire, `none` got further but still failed:**
   `OPTIONS` → `200 OK`, then `ANNOUNCE` (SDP describing PCM wrapped as
   AppleLossless payload type 96, standard for classic AirPlay — this is
   *not* a bug, this is what "PCM" mode actually sends on the wire) →
   **`403 Forbidden`**. `RSA` was worse: `OPTIONS` succeeded but the
   client never even sent `ANNOUNCE`.

6. **`auth_setup` completed the full lifecycle.** `OPTIONS` → `200 OK`,
   `POST /auth-setup` (real Apple certificate exchange, ~1KB response) →
   `200 OK`, `ANNOUNCE` → `200 OK`, `SETUP` → `200 OK` (server allocates
   RTP/control/timing UDP ports), `RECORD` → `200 OK` with
   `Audio-Jack-Status: connected`, periodic `POST /feedback` keepalives,
   clean `TEARDOWN`. **User confirmed audible playback** on Dusche.

## Files changed as a result

- `container/Dockerfile`: base switched to `ubuntu:26.04`.
- `container/etc-wireplumber/`: removed (no longer needed on this base).
- `container/etc-pipewire/pipewire.conf.d/10-raop-static.conf`: default
  `raop.encryption.type` changed to `auth_setup`.
- `tests/test_spike02_raop_sink_boots.sh`: port names updated to
  `send_FL`/`send_FR`.
- `tests/test_spike02_raop_real_device.sh`: `RAOP_PORT` defaults to
  `7000`, `RAOP_ENCRYPTION` defaults to `auth_setup`, both still
  overridable; now auto-loops the test WAV to ~8s via `ffmpeg` (when
  available) since the full RTSP handshake needs more time than the
  ~1.5s original clip reliably allows; always dumps `raop`/`rtsp` log
  lines after playback since a clean `pw-cat` exit never implied session
  success.

## Net effect on PLAN.md Section 7 risk table

Spike #2 fully passes now, on real hardware, audibly. The bridge
daemon's real config generator (Section 5.4a) needs to know, per device:
`raop.port` (not reliably 5000) and `raop.encryption.type` (not reliably
`none`) — both need either a one-time mDNS discovery step or explicit
user-facing config fields, not hardcoded defaults.
