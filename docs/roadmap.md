# Roadmap / status

Short version: **Phases 0–5 are done, and Phase 6 (cutover from Music
Assistant) hasn't started.** Every "done" claim below has a real
end-to-end test behind it, not just code that compiles — see the
`tests/` script named alongside each item.

## Phase 0 — Spikes

All the foundational risk questions from before any real building
started. Every spike passed; see [decisions.md](decisions.md) for the
findings and `spikes/*.md` for the full evidence trail per spike.

## Phase 1 — Minimal add-on skeleton — done

`pipewire_audio_router/`, `tests/test_addon_phase1_e2e.sh`. Real HA
add-on skeleton: multi-stage Dockerfile + `run.sh` startup sequence and
the daemon's `serve` command. Verified end-to-end via the API:
`POST /api/outputs` → the daemon loads the `raop-sink` module into its own
PipeWire context → its registry listener discovers the node → the REST API
reports it correctly. Cross-builds for `linux/arm64`. (The original
skeleton instead loaded RAOP sinks from a generated `pipewire.conf.d`
seeded by `options.json`; that static path — and the `generate-config`
subcommand — was superseded by runtime module loading, see Phase 6.)

## Phase 2 — Single source → single sink, end to end — done

`tests/test_addon_phase2_e2e.sh`. A real AirPlay-receive source (then
`shairport-sync`; since replaced by a native in-process receiver — see the
refinements section below) plus a real sendspin output (an embedded native
sendspin server, `sendspin_server.rs`), linked via a real `POST /api/links`
call, verified with independent signal analysis (peak/RMS via `ffmpeg
astats`), not just byte counts.

## Phase 3 — Multi-output mixing + HA entities — done

- **Multi-output fan-out**: one source linked into two outputs
  simultaneously (`tests/test_addon_phase3_multi_output.sh`) — the
  actual "Brave→Dusche+Pioneer" scenario that motivated this project.
  Required no new mixing logic; PipeWire's graph already supports one
  output port linking to many inputs.
- **`media_player` entities**: `custom_components/pipewire_audio_router/`,
  one entity per routing-matrix output (RAOP + discovered sendspin devices;
  see refinements below). Verified with real tests against actual HA
  internals (`pytest-homeassistant-custom-component`) — only the
  network layer is mocked.
- **TTS/announce ducking**: `POST /api/media_players/:id/announce`,
  verified end-to-end against the real add-on binary
  (`tests/test_addon_announce_ducking_e2e.sh`): baseline RMS -16.7dB,
  ducked+announce -22.3dB, restored -17.3dB (matches baseline, clean
  restore confirmed).

## Phase 3.5 — Streaming TTS via Wyoming — done, additive

`pipewire_audio_router/bridge-daemon/src/wyoming.rs`,
`tests/test_addon_announce_wyoming_e2e.sh`. Caller picks `url` (v1,
unchanged) or `wyoming` (v2) per call via HA's standard `play_media`
`extra` dict — no daemon-wide mode switch. Verified end-to-end with a
protocol-conformant mock Wyoming server (real synthesized tone, proving
wire-level parsing, not TTS quality). **Not done**: standing up a real
local Piper instance against this now-verified client — should need no
further bridge-daemon code, just a `host`/`port` pointed at Piper's own
Wyoming server.

## Phase 4 — Manual routing web UI — done

`pipewire_audio_router/bridge-daemon/src/routing.rs`,
`pipewire_audio_router/frontend/` (Vite + Svelte),
`tests/test_addon_routing_ui_e2e.sh`. Source × output matrix,
live-updated via WebSocket, driven by real PipeWire registry events
(not polling). Verified end-to-end: real `pw-link -l` output cross-
checked against the API's own claims, WebSocket confirmed pushing a
fresh snapshot within milliseconds of a real link change. (Originally a
single static `routing_ui.html`; since rebuilt as a themed Vite + Svelte
admin console — matrix plus outputs/sources/announce — served by the
daemon and shown in the HA sidebar via ingress.)

## Phase 5 — Bluetooth bridge box — done

`firmware/bt-bridge/`. Real hardware, real pairing, real AVRCP metadata,
all HA entities confirmed live, RTP audio confirmed reaching a real
`rtp-source` node with genuine signal (not silence) — see
[decisions.md](decisions.md#bluetooth-bridge-box-hardware-and-firmware-constraints)
for the three real bugs found and fixed along the way.

**RTP source now wired into the add-on, not just a dev machine.** The
previously-owed item — the receive side had only been verified against a
native PipeWire session on a dev machine
(`~/.config/pipewire/pipewire.conf.d/60-bt-bridge-rtp-source.conf`) — is
closed. The bridge daemon now loads `libpipewire-module-rtp-source` into
its own context at runtime, exactly like a RAOP sink (rather than the
originally-planned static `container/` template, which no longer matched
how the daemon manages everything else). It's enabled/disabled and
re-pointed live via `PUT/DELETE /api/source/rtp` and the web UI's Sources
tab; once loaded, the `bt-bridge-rtp` node shows up in the routing matrix
as a source automatically (routing.rs needed no change). Fixed audio
format (native-endian `S16LE`, 44100 Hz stereo, `sess.ignore-ssrc`) lives
in `bridge-daemon/src/rtp_source.rs`.

Bonus beyond original scope: RTP host/port are HA-configurable at
runtime on the *firmware* side too (`text`/`number` entities,
`restore_value: true`) — no reflash needed to repoint the box at a
different PipeWire host.

## RAOP output hot-reload — done

Supersedes the old "outputs need a restart" limitation, which was based
on a misreading of PipeWire's capabilities — see
[decisions.md](decisions.md#loading-pipewire-modules-at-runtime) for the
correction. The bridge daemon loads each `libpipewire-module-raop-sink`
into its *own* PipeWire context at runtime (FFI to
`pw_context_load_module` in `bridge-daemon/src/pw_module.rs`, driven over
a `pipewire::channel` onto the PipeWire thread), so an output can be
added or removed live without restarting PipeWire or interrupting the
other outputs.

**Control surface** (full reference:
[api-reference.md](api-reference.md#outputs-raop-hot-reloadable)):
`GET/POST /api/outputs` and `DELETE /api/outputs/:node_name`. Outputs are
persisted to a daemon-owned store (`/data/raop-outputs.json`) that loads
at startup and is populated at runtime via the API (and mDNS discovery —
Phase 6); there is no `options.json` seeding. `bridge-daemon
generate-config` and the static `pipewire.conf.d/10-outputs.conf` are
gone — the daemon owns RAOP sink lifecycle now.

Verified end-to-end against real PipeWire
(`tests/test_addon_hot_reload_e2e.sh`): empty start → `POST` adds an
output and its node appears live → duplicate add → 409 → `DELETE`
removes it and the node disappears live → unknown delete → 404 → a
persisted store loads its outputs at startup. Rust unit tests cover the
store (load/dedupe/persist round-trip) and the module-args rendering.

## Phase 6 — Fully native control plane + runtime-only config — done

Removed the last subprocess shell-outs from the daemon and moved **all**
user-facing configuration to runtime, so the daemon speaks one native
PipeWire API and owns every configurable object.

- **Native graph mutation**: link create/destroy via
  `Core::create_object` (`link-factory`) / `Registry::destroy_global`,
  driven over the PipeWire-thread command channel — no more `pw-link`
  (`pw_thread.rs`, `routing.rs`). Idempotency/unlink are decided against
  the observed registry, not subprocess stderr.
- **Native volume + announce**: per-node volume via the SPA `Props`
  `channelVolumes` param, cubic-scaled to match `wpctl` (`volume.rs`);
  announce playback via a `pw::stream` into the target sink (`player.rs`);
  clip decode via pure-Rust `symphonia` (`decode.rs`). Drops `wpctl`,
  `pw-cat`, and ~250–300MB of `ffmpeg` deps from the runtime image.
- **mDNS auto-discovery**: RAOP receivers are found over `_raop._tcp` and
  loaded automatically (`discovery.rs`), encryption picked from the mDNS
  `et` field; store-managed outputs act as overrides.
  `BRIDGE_DISCOVERY=off|log|on`.
- **Sources/outputs beyond RAOP**: the AirPlay-receive source and sendspin
  outputs were the two remaining subprocesses at this phase (a supervised
  `shairport-sync`, and embedded native sendspin servers). The RTP source is
  a native `rtp-source` module (like RAOP). All managed live via the API from
  a persisted store (`sources_store.rs`); `run.sh` shrank to infrastructure +
  the daemon; the `runtime-plan` subcommand is gone. *(Both the AirPlay
  source and sendspin were reworked afterward — AirPlay to a native
  in-process receiver, sendspin to auto-discovery + grouping — removing
  `supervisor.rs` entirely; see the refinements section below.)*
- **Runtime-only config**: the add-on's `options.json` seed fields and
  their `schema` were removed — user testing found seed-then-ignored
  options confusing. All stores start empty and are populated only at
  runtime; only container-level settings (`host_network`, image, arch)
  stay in `config.yaml`.
- **Hardening**: poison-safe mutex locking (`locks.rs`) so a panic holding
  a lock can't cascade the daemon into a fully-dead state.

Verified: full build + `clippy` clean, Rust unit tests, and live against
real PipeWire — native link create/idempotency/destroy, volume matching
`wpctl` on the cubic scale, announce to a null sink, mDNS discovery of the
real Pioneer/Dusche receivers, and AirPlay/sendspin CRUD persisting across
a daemon restart. Rationale in
[decisions.md](decisions.md#link-mutation-is-native-pipewire-rs-not-a-pw-link-subprocess).

## Post-Phase-6 refinements — done

Hardening and feature work after the native-control-plane cutover, driven by
real-hardware use:

- **Native AirPlay-receive source**: replaced the `shairport-sync` subprocess
  (no PipeWire backend on Ubuntu → its audio never reached the graph) with a
  native in-process RAOP receiver on a vendored+patched `shairplay` crate
  (`airplay_source.rs`). Removed `supervisor.rs` (no subprocesses left) and
  shairport's D-Bus/avahi requirement. Needed three PipeWire-sender interop
  fixes (Server header, unencrypted-ALAC advertisement) — see
  [decisions.md](decisions.md#native-airplay-receive-source-vendored-shairplay-not-shairport-sync).
  Configurable jitter buffer to ride out clock drift.
- **Sendspin auto-discovery + grouping + per-device volume + liveness**:
  devices are discovered over mDNS and grouped from routing intent; per-device
  volume is sent in-band (needing a `sendspin` crate patch to map a connection
  to its device); online/offline is connection- + TCP-probe-driven, so an mDNS
  flap no longer tears down a live group — see
  [decisions.md](decisions.md#sendspin-auto-discovery-grouping-per-device-volume-and-connection-driven-liveness).
- **Matrix-driven `media_player` entities**: one per routing-matrix output
  (RAOP + sendspin devices), removed when an output leaves the matrix, plus a
  `cleanup_entities` service for stale leftovers and a `sendspin_group_members`
  attribute for grouped devices.
- **RTP multicast + jitter buffer**: the RTP source can bind a multicast group
  (fan one bridge stream out to several PipeWire hosts) and its jitter buffer
  is tunable; both exposed on `PUT /api/source/rtp` and the firmware's HA
  entities.

## Phase 7 — Cutover — not started

Side-by-side run with Music Assistant disabled; validate all rooms, all
automations, voice responses; only then uninstall the MA add-on. This
is the one remaining phase with no code behind it yet — everything it
depends on (Phases 1–6) is done.

## Acceptance criteria for declaring the project successful

- Stream start latency (tap play → audible sound) under ~300ms on both
  sendspin and RAOP outputs, vs. the Music Assistant baseline —
  **not yet measured** on real hardware end-to-end (the individual
  mechanisms are proven fast in isolation, e.g. `pipewire-rs`'s 0.07ms
  link creation, but a full tap-to-audible measurement hasn't been run).
- No audible stutter/dropouts during a 30-minute continuous playback
  test on at least 2 simultaneous outputs — not yet run.
- All rooms controllable from HA (volume, play/pause, routing) and from
  voice-assistant TTS — **met** for volume/routing/TTS; play/pause was a
  deliberate non-goal (no clean mapping onto a passive routing sink with
  no queue of its own).
- CPU/memory headroom on the Pi 4 confirmed via `htop`/`docker stats`
  during simultaneous 3-output playback — not yet run; all testing so
  far has been on dev hardware (x86_64) plus arm64 cross-build smoke
  tests, not the real target Pi 4.

These four are the actual gate for Phase 6. Everything else in this
roadmap is infrastructure to make these measurable, not the goal itself.
