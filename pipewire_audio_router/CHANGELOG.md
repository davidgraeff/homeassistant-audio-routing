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

## 0.4.20260814092836

_2026-08-14_

- dbed27d readme: the receiver agent gets its own install step
- f42d256 ci: a tagged release carries the receiver-agent binaries
- d847824 test: the e2e suite asserts what the API answers now
- 87c3f27 ci: the formatter, two lints, and a matrix that names what broke
- 273c42f readme: music groups next to outputs in the screenshot grid
- 292025c docs: the front page's flow diagram, in the README too
- 89bc398 ui: one bar the panel shares with Home Assistant, and a front page behind the title
- 55ce60b groups: music-group presets — the whole grouping of the house, switchable in one click
- f8e3d44 ui: the routing graph spends its width on the names, not on the gutter
- ef831c3 ui: the tab bar opens with the setup path, drawn as a flow
- c5ee97e readme: show the UI — a 3-up thumbnail grid, click for full size
- 534571b readme: let the comparison table carry the comparison
- 39253b7 docs: fact-check the Music Assistant claims against MA's own documentation
- 5151a4d docs: Music Assistant is the layer above, not the thing being replaced
- 7adc3b2 ytmusic: the add-on sets itself up, so installing it is one click
- add50d0 spike: delete the harnesses, keep the recipe
- 6eec54e api: the status carries success, and a refusal says why in a word
- 9c2d121 readme: the Cast button was promised in the features and missing from the install
- 1378eba readme: lead with what a user gets, and let the buttons do the installing
- 99e115d api: one push socket, and every subject in the path
- 186eafc branding: one source, three outputs — a mark for the add-on and the integration
- 9c1c6f9 pw-sink: a suspending host says goodbye, and asleep is its own state
- 536548a ha: a pw-sink host device shows its agent build and its output sink
- e00dc02 ha: a pw-sink host gets a device, so its room can be assigned
- b8716fa align: measure one channel of a stereo pair, and refuse with the numbers
- e2f0abe agent: a build identity, and one path the unit can keep pointing at
- fee1823 fix(align): the pre-flight told a full capture it was still collecting
- a49d31f agent(tray): a Quit row, because the tray is sometimes the only interface
- b09e2d1 agent: the host's own volume changes never reached the add-on
- 2c0f752 fix(types): put the sync fields in the interfaces they describe
- b719300 ui(outputs): the level controls follow the daemon's capability, per knob
- 929bade fix(sync): the lead you can see, the lead in force, and a receiver that says it lost it
- 8db1126 pw-sink: a fresh session has to ask the receiver to re-handshake
- bf95ed4 agent(tray): the level is a control now, not a readout
- c320db1 chore(sendspin): bump the pin to the client/state parse fix
- 1007452 ui(outputs): pw-sink volume and mute went to the sendspin endpoint
- 61df833 test: run against the Home Assistant the add-on actually runs on
- 67964f4 ha: groups keep their own name, the service device keeps the settings
- 9b69b5b ha: the add-on becomes one Home Assistant device
- 7e968c5 refactor(outputs): one place decides an output's kind, and the daemon publishes what it can drive
- 4f55733 ui(align): the level slider follows the resolved capability, not the kind
- b978a7f duck: voice ducking ships on, and says what it does
- 3aa345f ha: adopt a sendspin output by MAC when its mDNS name doesn't match
- ca27474 ui: a page refresh comes back to the page you were on
- b745072 align: the hold registry stops panicking on a poisoned lock
- 4210209 ui(align): the alignment page stops behaving like the card it used to be
- 2cf84ac ui: a held output says so on its own row
- 069826f ui: shared section styles in one file, and OutputsTab/SourcesTab shed their logic
- 9ff1205 refactor(align): calibrate becomes a directory — session, audibility, status, click
- bda449b ui(outputs): the Outputs page stops mentioning alignment at all
- e97a85f ui(frontend): components by feature instead of one flat directory
- 1088c8d refactor(routing): peel four modules off the group reconciler (step 2)
- e3e21a2 refactor(align): the solver, chaining, the walk and the run driver leave measure
- 610f9c4 refactor(align): the reported shape, the seams and the feeder leave measure
- baae6a5 refactor(align): the pre-flight check and the W21 experiment move out of measure
- 96c4585 align(ui): show when the session closes, and reset from a pushed frame
- 159af72 refactor(align): measure becomes a directory; knobs and the gate move out
- f6e32a5 align: publish the session's remaining idle time, and push state changes
- 2405451 ap2: bound teardown per connection so one silent receiver can't strand a group
- cd6c191 ap2: a volume write could deadlock the daemon, and a failed connect leaked the session
- 08fbab7 Move investigation document to archive
- b3dc33c docs(align): what the first run on hardware changed
- e29e2ca align: band-split calibration, silence on park, and a run transcript
- 0f15c2b docs: drop "plan" from the names of the four docs that are records now
- fc84af5 align(ui): Alignment is its own page, gated by a microphone step
- 0888ba8 docs: the plans become records of what was built and learned
- 2b7b1b2 docs: record where tests live and why (plan §11)
- 32d99a5 test: the six remaining oversized test modules move to their own files
- a71ff23 test(align): measure's tests move into measure/tests/, one file per subject
- 86bfbb5 align(ui): diagnose worklet load failures, and fall back to a blob URL
- 9080580 align(ui): all alignment on the Outputs page; Manual and Near field are real modes
- 9339417 docs(daemon): comments state what is, not what was
- 909c6fb layout: the reference sweep, and the plan closes (plan §4 wave 7, §7)
- 7e1f170 layout: split api.rs, extract state.rs, and make api/ a leaf (plan §4-§6, wave 6)
- fede06c layout: routing/, announce/, spike/ and supervisor.rs (plan §4, wave 5)
- 150d6a5 layout: outputs/pwsink/ — one backend, one spelling (plan §4, wave 4c)
- 3776b61 layout: outputs/ap2/ (plan §4, wave 4b)
- a527653 layout: outputs/sendspin/ + the shared overlay mixer (plan §4, wave 4a)
- 697e6e0 layout: move the input side into sources/ (plan §4, wave 3)
- 3a69089 layout: move the persisted stores into store/ (plan §4, wave 2)
- 216208b docs(align): W6c done; W7 is the only unbuilt feature, W22 is the gate
- 0d07d8d align(ui): multi-position chaining, and the wizard moves to Outputs (W6c)
- 32e73d3 layout: move util/, audio/ and pw/ out of the flat root (plan §4, wave 1)
- 98cab67 docs: refresh the module-layout plan after the align wave
- d732781 fmt: clear the three pre-existing rustfmt findings
- 89cb035 docs(align): W9 waits for evidence, and will never be a toggle
- eae207b Move implemented plans to archive
- fc627e4 align: move the alignment cluster into align/ (module-layout-plan §4, wave 5)
- f4d9e99 align: multi-position chaining (W12)
- bc3cb42 align: microphone-assisted speaker alignment (W0–W21)
- b5787db docs: ytmusic — the resolver findings that were only in code comments
- 9f56025 audio: bound the pw-sink queue, and make every playout delay a knob
- 3dd1071 ytmusic: resolve in a warm yt-dlp with a resident JS-challenge solver
- 6c586d6 card: routing dashboard card, served and auto-loaded by the integration
- 40a5e36 ui: replace the browser confirm dialogs
- c7a9016 ap2: probe the AirPlay service, not just its TCP port
- 988563b docs: voice-duck plan — what landing on the current mainline took
- d648a9f docs: record VD5 done and the receiver-agent integration the plan did not foresee
- a584f59 ui: show which outputs are ducked right now
- 47c895b duck: mirror voice ducks to pw-sink agent hosts, via the mixer's aggregate
- 2fa3b3b docs: record VD1-VD4 as done, and why the UI badge waits
- 1be8d59 ha: automatic voice-assistant ducking, replacing the blueprint (VD4)
- 21fc0c9 duck: leased per-output duck holds, the mechanism voice ducking needs (VD1-VD3)
- 020dc23 docs: record L1-L5 as done in the voice-duck plan
- dadfa9b announce: remove the Wyoming TTS source — HA's own TTS replaces it (L5)
- f6fbee3 announce: drop the v1 node-based ducked announce, and delete volume.rs (L3+L4)
- 8dff7ba api: drop the node-backed media_player listing and per-node volume (L1+L2)
- 7a2b027 ci: the clippy job's apt list had drifted from the Dockerfile's
- ab40a4e pi-ytmusic: advance the queue when a track actually ends
- 73e5c4c docs: the YouTube Music plan becomes documentation of what exists
- 65d4eb5 tests: the add-on e2e suite still spoke the pre-adoption-gate API
- 5336bf7 pi-ytmusic: reach the add-on from push_cookies.py, and start it after deploy
- 0b9c3b5 Dockerfile: the native build stage has no C++ compiler
- 21e749e daemon: clear the clippy gate CI never reached
- c4b4a67 daemon: cargo fmt, which nothing had run in a while
- e09bd06 tests: the config-flow setup path had four unmocked fetches
- df4a2a0 ci: check out the submodule the daemon is built from
- 788a4c8 source metadata: what each input is playing, end to end
- c914729 ytmusic_receiver: the cast receiver as a Home Assistant add-on
- dcf7a38 pi-ytmusic: a YouTube Music cast receiver on the Pi bridge
- a25c18b pwrouter-agent: choose the output in the tray, and stay on it
- 4b5ef9d pwsink: take routing targets from paired agents, not from mDNS
- 8ef143c pwrouter-agent: show the pairing code on the desktop, and carry its own unit
- ecf1626 graph: scissors on a wire, and Ctrl-drag to move the wires you already have
- 281c2e5 outputs: a receiver host is a discovered output, and pairing is adding it
- 6d93558 ws: coalesce listing rebuilds onto the meter tick
- c3aa4da docs: point the alignment docs at the source card's panel, not an Align page
- abfbe43 pwrouter-agent: make the unit start on a fresh install, wire the agents up
- a3af654 ws: push the outputs, discovered and agents listings as typed frames
- 154673d outputs: tell "reachable" apart from "connected", in the daemon and the UI
- 6fbd99c outputs: gate the audio path on adoption, and wire the store up
- 8ee0aae outputs: forget a removed output's group membership
- 995e7bf outputs: rename an output, and line the card badges up
- 1a4ded9 addon build: stop BuildKit's GC from throwing away the cargo caches
- a46ebe3 ui: group cards two to a row, controls in the card's foot
- e458956 ui: fold the routing graph away, drop the side help card
- 424095d fix: install ca-certificates — no CA store broke every HTTP client
- 64242c5 pwsink: agent help behind a button, downloadable binaries, no polling
- 81d17d5 ui: put the graph's help in the left margin, one section at a time
- 857222f pwsink: pairing UI on the Outputs tab, and a shared pw-control crate
- 80e3d81 ui: rebuild the group pages as first-class cards, docs behind help buttons
- 4328368 pwsink: HA volume/mute for agent hosts, announcement ducking, packaging
- a82009d pwsink: implement the receiver agent — pairing, master volume, ducking
- 3138e5c pwsink: plan a receiver-side agent, and spike its two risky halves
- fec7772 docs: plan voice-assistant ducking in the add-on, and the legacy API it retires
- fd69e34 ap2: drop the 200 ms floor on the render delay
- 119ca72 vendor: silence upstream lint noise in the vendored trees
- 02042f1 daemon: delete the dead code behind 20 build warnings
- e6c9dbb sources: discover Bluetooth bridges, and link to their diagnostics page
- aff91ab capture: name each PipeWire capture after the subsystem that owns it
- 74293fd docs(sendspin): reproduction recipe for the silent devices, and withdraw a wrong claim
- a89de48 sendspin: expose stream/clear as POST /api/sendspin/clear
- 293c61f docs(sendspin): record the 2026-08-03 silent-devices fault, and rule the lead out
- 5d62e74 observability: log announcements, stop the AirPlay DIAG flood, correct media_players
- afa0ee5 docs(sendspin): archive the churn investigation, carry the open items forward
- 4c3dd46 bt-testing-app: only report silence that was actually observed
- dbc87c6 docs(sendspin-churn): the root cause was ours — record it, and the corrections
- b64b931 sendspin: a static-delay change reconnects one speaker, not the whole group

## 0.3.20260728212654

_2026-07-28_

- 048bf3b add-on(config): trim comments that point at the moved spikes/
- 370e784 add-on: a changelog, and a release script that stamps it
- 12f061c docs: drop the superseded PLAN.md
- f90c453 docs(sendspin churn): the daemon was never the 30 s; the firmware is
- a6d889c sendspin: stop reconnecting speakers, because a reconnect costs 30 s
- e1243b0 pi-bridge: a testing console for the Bluetooth leg
- 12415c7 RTP dropouts: the silence starts at the A2DP boundary; drop the IGMP watchdog
- 3a9e527 chore(doc): Move spikes into old/ documentation directory
- 05d6511 chore(sendspin): carry the fork as a submodule instead of a vendored copy

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
add-on, with the audio path on PipeWire's realtime graph rather than a Python
engine.

- Headless PipeWire graph in the add-on container, with graph control from a
  Rust bridge daemon.
- AirPlay receive (`shairport-sync`) and RAOP/sendspin outputs.
- Home Assistant integration exposing routing as entities and services.
- Multi-arch image builds in CI and a Pi dev-deploy script.
