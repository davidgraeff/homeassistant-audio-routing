# Design decisions

Reference material for *why* the system is built the way it is. Each
entry was an actual investigation with a concrete finding, not a
preference — most link to a `spikes/*.md` write-up with the full
evidence trail (packet captures, signal measurements, exact error
messages). If you're about to second-guess one of these, read the
linked spike first; there's a good chance the "obvious" alternative was
already tried and failed for a specific, documented reason.

This document covers **project-wide and cross-component** decisions.
Decisions about the **bridge daemon / add-on internals** (Rust vs Python,
PipeWire module loading, RAOP/AP2 output, Sendspin, container packaging,
announce ducking, and more) live with the add-on in
[`../pipewire_audio_router/docs/decisions.md`](../pipewire_audio_router/docs/decisions.md).

## Why replace Music Assistant at all

MA's audio engine is pure Python (asyncio + soundfile/ffmpeg
subprocesses), which on a Raspberry Pi 4 caused audible stutter,
multi-second stream startup, and noticeable output delay. PipeWire's
graph is a compiled C daemon with realtime scheduling (rtkit) and mature
routing tooling — the goal was to keep MA's *idea* (unified sources →
mixed room outputs → HA entities) while reimplementing the engine on top
of PipeWire instead of Python audio processing.

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
     except `journalctl --user -u pipewire`. Originally worked around on
     the receiving side with `sess.ignore-ssrc = true`. That has since
     been superseded at the source: the firmware now derives its SSRC
     from the factory MAC (`esp_efuse_mac_get_default` in
     `a2dp_bridge.cpp`), so it is **stable across reboots**. With a stable
     SSRC the receiver can instead run `sess.ignore-ssrc = false` and get
     a useful property — it latches onto that one sender and rejects any
     *other* box hitting the same port, so a stray or second sender can't
     interleave into and corrupt the stream. Both modes are exposed
     (`ignore_ssrc` on `PUT /api/source/rtp`, the "Source" radio in the
     web UI); `true` (accept any sender) stays the default so an install
     with a not-yet-reflashed bridge can't go silent.
- **If BLE is ever added to this same device later**, note that
  ESPHome's `esp32_ble` component — even though it defaults to
  Bluedroid, which supports classic+BLE simultaneously in principle —
  explicitly sets `CONFIG_BT_CLASSIC_ENABLED = False` and reclaims
  classic-BT memory via `esp_bt_controller_mem_release(...)`. That
  default would actively kill the A2DP sink unless both the sdkconfig
  override and the mem-release call are explicitly prevented first.

## Raspberry Pi Bluetooth → RTP bridge

A second, independent Bluetooth-bridge implementation living beside the
ESP32 one: a Raspberry Pi (developed on a Zero 2 W) as an A2DP sink that
forwards to the *same* `bt-bridge-rtp` add-on source. It exists because a
Linux SBC needs **no custom firmware** (BlueZ + PipeWire do A2DP-sink and
RTP-send as pure configuration), gets **real codecs** (aptX/AAC/SBC vs the
ESP32 sink's effectively-SBC-only), and directly replaced a user's prior
high-latency PulseAudio → `darkice` → Icecast setup on the same box. The
deliverable is `firmware/pi-bridge/setup_pi_bridge.py`, an idempotent
configurator — see `firmware/pi-bridge/README.md`. Everything below was a
wall hit during real bring-up (verified end-to-end streaming aptX from a
Samsung S23 → RTP → add-on → speakers), not a preference.

- **WirePlumber 0.5's "seat monitoring" silently disables Bluetooth on a
  headless box — this was the single biggest blocker.** WP 0.5 only
  manages Bluetooth for the user on the *active login seat*
  (`monitor.bluez.seat-monitoring`, backed by `support.logind`). A
  lingering headless session has **no seat**, so the bluez monitor loads
  but registers **no A2DP media endpoint**: the adapter advertises AVRCP
  (`0000110c`/`110e`) but not **Audio Sink (`0000110b`)**, and phones pair
  fine yet show a device with *no audio service*. Fixed by disabling
  `monitor.bluez.seat-monitoring` + `support.logind` for the `main`
  profile. This is also exactly why **WirePlumber 0.4 (Debian bookworm)
  "just worked"** with no such setting — the seat gate didn't exist yet.
  Note bookworm's own Pi archive ships PipeWire **1.2.7** (not stock
  bookworm's broken 0.3.65), so the RAOP-era version concern (see
  "Container base" in the daemon decisions) does not apply to the sender
  side.
- **`JustWorksRepairing = never` (BlueZ default) blocks re-pairing.** Once
  a device is bonded, BlueZ *refuses* a new "just works" pairing from it
  (an anti-MITM default). So after a phone unpairs (dropping its key) and
  tries to pair again, the Pi rejects it because it still holds the old
  bond — re-pairing fails until the bond is manually purged from
  `/var/lib/bluetooth` (`bluetoothctl remove` alone leaves the `cache/`
  entry behind). Setting `JustWorksRepairing = always` lets a known device
  re-pair and overwrite the stale bond seamlessly.
- **WiFi power-save drops the Bluetooth link on the shared radio.** The
  BCM43xx combo chip shares one 2.4 GHz radio between BT and WiFi;
  NetworkManager's default WiFi power-save parks the radio and
  starves/drops the BT connection, producing intermittent pairing and mid-
  stream drops. Disabled via `wifi.powersave = 2` (+ a boot oneshot). The
  user's prior PulseAudio setup proving BT+WiFi coexist here is what ruled
  out a hardware/antenna limit and pointed at this.
- **A fresh image comes up with the controller unpowered.** Raspberry Pi
  OS defaults `[Policy] AutoEnable` off (commented) in
  `/etc/bluetooth/main.conf`; without it the adapter is `Powered: no` at
  boot, WirePlumber sees no adapter, and no A2DP sink is offered. The
  script sets `AutoEnable = true`.
- **Priority-based capture binding beats `target.object` on WP 0.5.** The
  loopback bridges the phone source → RTP sink; its capture must attach to
  the phone, not the sink's own monitor (a feedback loop). Pinning the
  capture with `target.object = <bluez node>` **backfired on WP 0.5**: it
  added that link *and* kept a fallback link to the sink monitor, mixing
  two clocks and stalling the graph. The robust approach is to give the
  bluez source a high `priority.session` so it always outranks the monitor,
  and let the (default-following) capture bind to the single highest-
  priority source — which also re-binds cleanly on reconnect/re-pair.
- **Restarting PipeWire/WirePlumber orphans the live A2DP transport.**
  After a session restart, bluetoothd still reports the device
  "connected", but the audio transport is dead and the source node is
  gone; neither replaying nor a Pi-side reconnect reliably rebuilds it —
  only a **clean boot** (or a full BT disconnect+reconnect) does. The
  appliance is meant to run from boot, so this is a debugging hazard, not a
  runtime one: don't hot-restart the audio stack under a live phone.
- **`tcpdump` cannot see the RTP egress on this WiFi driver.** Locally-
  generated multicast TX is offloaded past tcpdump's capture hook, so
  tcpdump reports **0 packets even while audio is audibly streaming**.
  Confirm egress with the `wlan0` `tx_packets` counter, the `pw-link`
  capture binding, or the add-on actually playing — not tcpdump. (Several
  bring-up mis-diagnoses came from trusting tcpdump's false zero.)
- **Unicast is more robust than multicast for one Pi → one add-on.**
  Multicast (`239.255.42.42`) fans out to several receivers but depends on
  IGMP group membership that a receiver must re-join after a restart (the
  add-on's **Enable** button does a full `module-rtp-source` reload =
  re-join, so no dedicated "rejoin" control is needed). A point-to-point
  link has no group to lose, so `destination.ip = <add-on IP>` + the add-on
  source set to *Accept all* just resumes after either side restarts.
