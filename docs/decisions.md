# Design decisions

Reference material for *why* the system is built the way it is. Each
entry was an actual investigation with a concrete finding, not a
preference — most link to a `spikes/*.md` write-up with the full
evidence trail (packet captures, signal measurements, exact error
messages). If you're about to second-guess one of these, read the
linked spike first; there's a good chance the "obvious" alternative was
already tried and failed for a specific, documented reason.

## Why replace Music Assistant at all

MA's audio engine is pure Python (asyncio + soundfile/ffmpeg
subprocesses), which on a Raspberry Pi 4 caused audible stutter,
multi-second stream startup, and noticeable output delay. PipeWire's
graph is a compiled C daemon with realtime scheduling (rtkit) and mature
routing tooling — the goal was to keep MA's *idea* (unified sources →
mixed room outputs → HA entities) while reimplementing the engine on top
of PipeWire instead of Python audio processing.

## Container base: Ubuntu 26.04 LTS, not Debian

Debian bookworm ships PipeWire 0.3.65, which creates RAOP sink nodes
fine but **never performs the actual RTSP handshake with a real
receiver** — a silent, version-specific bug found via packet capture,
not a config problem. Ubuntu 26.04 LTS (PipeWire 1.6.2 / WirePlumber
0.5.13) works end-to-end against real hardware. Debian trixie (1.4.2)
also works but Ubuntu 26.04 was chosen for the better freshness/support
horizon. See `spikes/02-raop-static-sink.md`.

## Bridge daemon language: Rust, not Python

The original plan defaulted to Python ("fast iteration... performance
not critical, it's control-plane only"). Spike 4
(`spikes/04-graph-control.md`) changed that once actually measured:

- Python shelling out to `pw-link`/`pw-cli`/`pw-dump`: ~8ms state read,
  ~16ms per link create+destroy round trip. Fine for a human clicking a
  button, but no native Python PipeWire binding exists worth using.
- Rust via `pipewire-rs` (the official upstream binding): full
  node/port/link/registry control, **0.07ms** per link round trip via a
  persistent connection — roughly 230x faster — plus live registry
  state via event listeners instead of polling `pw-dump`.
- Go: no viable binding exists (checked two candidates — one
  early-stage/playback-only, one a read-only `pw-dump --monitor`
  wrapper).

Rust's web ecosystem (`axum`, `serde`) meant the *entire* daemon —
graph control, REST/WS API, static UI — could ship as one compiled
binary, not just the PipeWire layer. The sendspin sink adapter stays a
separate Python process per output regardless (see below) — rewriting
that in Rust wasn't justified.

## No MQTT `media_player` platform — a custom integration is required

Home Assistant core has **no MQTT `media_player` platform at all** —
confirmed against `homeassistant/components/mqtt/const.py`'s
`ENTITY_PLATFORMS` list (no `media_player` entry) and a 404 on the
would-be docs page. This is a 5+ year open, unresolved community feature
request, not a gap in this project's research — people who want an
"MQTT media player" today reach for third-party `custom_components`
that wrap MQTT themselves. So a `custom_components/` Python integration
was never optional.

**Consequence, not just a cost:** the custom integration gets real
`MediaPlayerEntityFeature.MEDIA_ANNOUNCE` support, which MQTT could
never have expressed regardless. Investigated directly against HA core:
the `announce` kwarg on `play_media` is forwarded straight into the
entity's `async_play_media(...)` with **no core-provided pause/duck/
resume logic** — every integration that supports it (Sonos, ESPHome's
`speaker`/`speaker_source`) implements its own overlay/duck/restore,
same as this project's announce-ducking design. Also note: HA core has
**no distinct "announcing" state** — even with `MEDIA_ANNOUNCE`
declared, nothing in HA's state machine reflects "this output is
mid-announcement" unless the integration adds its own attribute for it
(not currently done here).

## RAOP quirks found only by testing against real hardware

Two assumptions from PipeWire's own example configs turned out to be
wrong against real AV receivers (Pioneer VSX-934, a "Dusche" ESP
device), both in `spikes/02-raop-static-sink.md`:

- **`raop.port` is not reliably 5000.** Both real devices advertise RTSP
  on port **7000** via mDNS. A static config generator can't hardcode
  5000 — it needs the real port from mDNS discovery, or an explicit
  field. The failure mode is silent: writing audio into the sink
  reports success regardless, since the RTSP handshake fails
  independently with no error propagated back.
- **`raop.encryption.type` is not reliably `"none"`.** Both devices
  return `403 Forbidden` on `ANNOUNCE` with `"none"` or `"RSA"` — they
  require the Apple device-verification handshake (`"auth_setup"`, a
  real certificate exchange). The add-on defaults to `auth_setup` and
  treats `none`/`RSA` as fallbacks, not the other way around.

## PipeWire has no per-link volume

The original TTS-ducking design assumed a `volume` property could be
set on a **Link** (source→sink connection) to duck just that one path.
Spike 5 (`spikes/05-tts-ducking-mechanism.md`) found this doesn't exist:
PipeWire Links carry no Props/gain stage at all (Format param only) — a
`volume` property set via `pw-link -p` is stored but has zero audible
effect. The real mechanism is **per-source-**node** volume via `wpctl`**
— already used by the daemon's `/api/media_players/:id/volume`
endpoint. A real A/B/restore signal test confirmed this ducks only the
intended source while a second source mixed into the same sink is
unaffected, with a clean restore to the original level. A related real
bug caught during end-to-end testing: a stereo source contributes two
`Link` objects (FL+FR) sharing one output node — ducking/restoring per
*link* instead of per distinct *node* double-applied the duck/restore
and had to be fixed to dedupe by node id.

## PipeWire has no runtime "load module" RPC

Confirmed directly against `pipewire/core.h` — there is no `load_module`
entry in `pw_core_methods`. Modules (like `raop-sink`) can only be
loaded from the daemon's own startup config, never injected by a
connected client regardless of connection lifetime. This is *why*
output config is static-file-generated-before-start
(`bridge-daemon generate-config` → `pipewire.conf.d/10-outputs.conf`,
written before `pipewire` itself starts) rather than applied live over
the daemon's persistent connection — not a shortcut, a real protocol
limitation. Changing RAOP outputs requires regenerating the config and
restarting the add-on, matching standard HA add-on UX (most add-ons
already require a restart on config changes). Sendspin outputs don't
have this constraint — their sink node is created at runtime by
`sendspin-adapter.py` via `pw-cli create-node`, not via a config-time
module load.

## `pw-link` subprocess, not native `pipewire-rs` link mutation

Spike 4 measured native link creation at 0.07ms vs. ~16ms for shelling
out to `pw-link` — a 230x difference — but the daemon's actual link
endpoints (`POST /api/links`, `POST /api/routing/link`/`unlink`) still
shell out to `pw-link`. Deliberate: wiring a correctly thread-safe
command channel into the PipeWire event-loop thread (real `Send`/
lifetime work) wasn't justified for human-paced UI interactions. The
registry-*observation* half stays native (that's where the speed
mattered — polling `pw-dump` doesn't scale to live UI updates); only the
mutation path is "boring but proven" subprocess calls. Revisit only if
link-mutation frequency ever stops being human-paced.

## `host_network: true` is required, not a convenience

Both RAOP output and RTP-sourced input need to send/receive *unsolicited*
traffic on the LAN. Default Docker bridge networking isolates the
container from the host's other interfaces by design — confirmed
necessary empirically in `spikes/02-raop-static-sink.md` and
`spikes/03b-rtp-pc-transfer.md`, and consistent with every other
real-device test in this project. Multicast in particular has no "port"
to map, so port-mapping doesn't help either — only host networking (or
a macvlan/ipvlan network) removes the isolation.

## `shairport-sync` needs a real D-Bus system bus + avahi

Discovered in `spikes/shairport-sync-source.md`: shairport-sync
hard-requires a working `avahi-daemon` and a real D-Bus **system** bus
to even start (fatal exit otherwise) — the private session bus this
project's containers already set up for PipeWire's own portal/rtkit
probing isn't enough. PipeWire/WirePlumber themselves don't need the
system bus. Its PipeWire node also only exists while a session is
actively playing (lazy activation, same pattern as RAOP/RTP), which the
real bridge daemon handles fine (it holds a live registry listener) but
means "link ahead of time" isn't possible — the daemon must react to
the node appearing.

## TTS/announce ducking: URL-based (v1) and Wyoming-based (v2), additive

`POST /api/media_players/:node_id/announce` accepts either `url` (fetch
+ ffmpeg-decode to WAV, works with HA's standard `tts.speak` contract
unchanged) or `wyoming` (direct synthesis via the Wyoming protocol, no
ffmpeg needed) — mutually exclusive, chosen per call via HA's standard
`play_media` `extra` dict (the same mechanism other integrations use for
implementation-specific options), not a daemon-wide mode switch or
transparent interception. Either way, the daemon ducks every distinct
source node currently linked into the target sink, plays the clip, and
unconditionally restores original volumes even on failure.

The Wyoming client (`wyoming.rs`) buffers the full `audio-chunk` stream
into memory and builds one WAV before playback, rather than streaming
chunk-by-chunk into a live buffer — a deliberate simplification once it
came time to build it: for a short announce clip this buffering is
milliseconds, so the latency win over v1 (skipping the render-to-file-
then-HTTP-fetch round trip) is preserved without needing a push/callback
mixer interface. **Still unaddressed**: HA's built-in TTS cache (keyed
on engine+text+language+options) only exists in the `url`/file-based
code path — repeating the same `wyoming` text re-synthesizes it every
time, with no caching layer of our own. Low-priority as long as usage
stays occasional announcements rather than frequently-repeated phrases.

## Bluetooth bridge box: hardware and firmware constraints

- **Chip variant is not negotiable.** Confirmed via `esptool chip-id`
  and Espressif's own SoC comparison page: only the original ESP32
  (`ESP32-D0WDQ6` here) has classic Bluetooth (BR/EDR) with
  A2DP(SNK)/AVRCP(CT) at all. ESP32-S2 has no Bluetooth; S3/C3/C6/H2 are
  BLE-5-only with no classic radio — confirmed dead by
  `espressif/esp-idf#16232`, closed **Won't Do**. The USB-UART chip
  visible in `lsusb` (e.g. CP210x) says nothing about which ESP32
  variant is on a board — verify per-board with `esptool chip-id`.
- **No conflict between ESPHome's normal features and owning the
  classic-BT radio.** ESPHome only initializes the Bluetooth
  controller/host stack when YAML includes `esp32_ble:`,
  `esp32_ble_tracker:`, `bluetooth_proxy:`, or `esp32_improv_ble:` —
  confirmed directly against `esphome/components/esp32_ble/ble.cpp`.
  Omit all four and the radio is free for a custom classic-BT component.
  WiFi provisioning (fallback AP + captive portal) is pure WiFi with no
  BLE involvement either, so "WiFi/OTA/HA API" and "own the BT radio"
  were never in tension.
- **`sendspin-cpp` was investigated and ruled out** as the audio-out
  transport from this box. It implements only the sendspin *player*
  role (receives a pushed stream), matching `aiosendspin`'s
  server→client direction — there is no way to use it to *upload*
  captured A2DP audio to a server. The sendspin protocol spec does
  define a `source@v1` role for exactly this, with one WIP reference
  client (`Sendspin/sendspin-jack-bridge`), but `aiosendspin` (the
  server library this project already depends on) has no server-side
  implementation of that role. RTP was the lower-risk choice, reusing
  the already-proven `rtp-source` receiving path instead of building
  both a new ESP32 client and new upstream-style server support.
- **Vendoring a self-contained ESP-IDF component is unsupported by
  ESPHome.** The firmware is a plain `external_components` package
  (own `.cpp`/`.h` + `__init__.py`) calling `esp_a2dp_api.h`/
  `esp_avrc_api.h` directly, following the same pattern ESPHome's own
  `esp32_ble` component uses internally — not a vendored
  `CMakeLists.txt`/`idf_component.yml` directory (tracked as an open,
  unimplemented ESPHome feature request, `esphome/feature-requests#1605`).
- **Real hardware surfaced three bugs no amount of code review would
  have caught:**
  1. `esp_bt_controller_enable` failed at first boot
     (`ESP_ERR_INVALID_ARG`) — the Kconfig `choice` controlling the
     controller's mode defaults to **BLE Only** since nothing in
     ESPHome ever needed classic BT before. Fixed by forcing
     `CONFIG_BTDM_CTRL_MODE_BR_EDR_ONLY=y`.
  2. The BT stack itself warned that AVRC must be initialized before
     A2DP. Fixed by reordering initialization.
  3. PipeWire's `rtp-source` module session-latches onto the first SSRC
     it sees and silently rejects every packet from a later reboot's
     different (randomly generated per-boot) SSRC — invisible anywhere
     except `journalctl --user -u pipewire`. Fixed on the receiving side
     with `sess.ignore-ssrc = true` (correct for a fixed point-to-point
     link with exactly one sender).
- **If BLE is ever added to this same device later**, note that
  ESPHome's `esp32_ble` component — even though it defaults to
  Bluedroid, which supports classic+BLE simultaneously in principle —
  explicitly sets `CONFIG_BT_CLASSIC_ENABLED = False` and reclaims
  classic-BT memory via `esp_bt_controller_mem_release(...)`. That
  default would actively kill the A2DP sink unless both the sdkconfig
  override and the mem-release call are explicitly prevented first.
