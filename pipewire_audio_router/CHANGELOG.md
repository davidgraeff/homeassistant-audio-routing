# Changelog

<!--
  Supervisor serves this file at /store/addons/<slug>/changelog and Home
  Assistant renders it as the release notes of the `update.*` entity for this
  add-on. Two rules make that work, both enforced by scripts/release.py:

  1. A version heading is `## <version>` — hashes, ONE space, the exact string
     from config.yaml's `version:`, then end of line. Nothing else on the line.
     HA slices the notes out with `^#* <latest_version>\n(?:^(?!#* <installed>).*\n)*`
     (homeassistant/components/hassio/update.py), so `## v0.3.0` or
     `## 0.3.0 - 2026-07-28` silently fall back to dumping this whole file.
     Put the date on its own line underneath instead.
  2. Newest version first — the regex captures downwards from the new version's
     heading until it hits the installed one.

  Versions are `MAJOR.MINOR.REVISION`, where REVISION is the UTC build
  timestamp (`YYYYMMDDHHMMSS`) stamped at release time. `scripts/deploy-dev.sh
  addon` replaces that revision with a fresh timestamp for dev deploys, which
  is why unreleased dev builds never match a heading here (they show the full
  file — harmless).
-->

## 0.2.0

_2026-07-26_

Second iteration of the router: the audio path is now fully native (no
`shairport-sync`, no ffmpeg, no RAOP-via-PipeWire), everything is configured at
runtime through the web UI, and outputs can be grouped.

**Audio path**

- Replaced `shairport-sync` with a vendored in-process **shairplay** receiver,
  and the PipeWire RAOP sink with an in-process **AirPlay-2 sender** (vendored
  `airplay2-sender` + `libairptp`), so senders and receivers share one process
  and one mDNS daemon.
- **sendspin** outputs: mDNS discovery, wire-codec selection, per-device volume,
  writer lanes with bounded writes, and a routing-driven grouping reconciler.
- **pw-sink** outputs — stream to a remote PipeWire host over AppleMIDI-synced
  RTP.
- **Two-tier grouping** (music groups / announcement groups) on a shared
  timeline, with a per-output arbiter.
- Announcements can now target an output nothing is routed into, via an
  on-demand session with a stall watchdog and graceful teardown.
- Pure-Rust clip decoding via `symphonia`, dropping ~300 MB of video/GPU
  dependencies from the image.

**Inputs**

- Multiple input sources instead of one AirPlay + one RTP: add and remove
  AirPlay and RTP inputs at runtime.
- RTP source modes, `ignore-ssrc`, and self-healing for a multicast source that
  lost its IGMP join.
- Raspberry Pi Bluetooth→RTP bridge (`firmware/pi-bridge`) and the ESPHome
  `bt-bridge` firmware with a stable MAC-derived SSRC.

**Web UI & Home Assistant**

- Admin web UI (Vite + Svelte) served by the daemon and surfaced in HA's sidebar
  over ingress: routing matrix, Sources, Outputs, Settings, Diagnostics, Align.
- Per-node xrun counts and latency estimates in the routing matrix.
- HA integration: `media_player` entities driven by the routing matrix, AirPlay-2
  volume/announce, link/unlink services, and adoption of the matching HA device's
  name and area.

**Add-on & platform**

- All configuration moved to runtime (REST API + UI, persisted under `/data`);
  the static seed options were removed because they looked authoritative but
  were ignored after first run.
- `SYS_NICE` + `IPC_LOCK` instead of `full_access`, so PipeWire can run its data
  loop `SCHED_FIFO` and `mlockall()` the audio path under host CPU/memory load.
- Prebuilt multi-arch GHCR images (cross-compiled, not emulated) so an install
  never compiles Rust on the target.
- mDNS restricted to the LAN interface and consolidated onto a single
  `ServiceDaemon`, fixing a CPU storm caused by host-network veth amplification.
- CI: rustfmt, clippy, Rust tests, the Svelte UI, the HA integration, and a
  docker-based add-on end-to-end suite.

## 0.1.0

_2026-07-13_

Initial release — a PipeWire-based whole-home audio router as a Home Assistant
add-on, replacing Music Assistant's Python audio engine.

- Headless PipeWire graph in the add-on container, with graph control from a
  Rust bridge daemon.
- AirPlay receive (`shairport-sync`) and RAOP/sendspin outputs.
- Home Assistant integration exposing routing as entities and services.
- Multi-arch image builds in CI and a Pi dev-deploy script.
