# Bluetooth Audio Bridge firmware

ESPHome firmware that turns an ESP32 into a Bluetooth-speaker-like device
(classic-BT A2DP sink + AVRCP controller) and feeds whatever it receives
into the [PipeWire audio router](../../pipewire_audio_router/README.md)
as an RTP stream. Pair your phone with it like any other Bluetooth
speaker, and that audio becomes a source in the whole-home router.

![BT bridge firmware architecture](../../docs/diagrams/bt-bridge-firmware.svg)

## Hardware requirement — read this before buying a board

**Must be an original ESP32** (e.g. `ESP32-D0WDQ6`, WROOM/WROVER-class) —
**not** an S2/S3/C3/C6 board. Only the original ESP32 has classic
Bluetooth (BR/EDR) hardware at all; the newer variants are BLE-only and
cannot do A2DP, confirmed dead upstream
(`espressif/esp-idf#16232`, closed **Won't Do**). The USB-UART chip
visible in `lsusb` (commonly a CP210x/CP2102) tells you nothing about
which ESP32 variant is actually on the board — verify per-board with:

```
esptool chip-id --port /dev/ttyUSB0
```

Look for `Chip type: ESP32-D0WDQ6` (or similar plain "ESP32") in the
output, not `ESP32-S3`/`-C3`/etc.

## What it does

- Pairs as a classic-Bluetooth A2DP sink + AVRCP controller — a phone
  can connect and stream to it exactly like a Bluetooth speaker.
- Encodes the received PCM as RTP/UDP and sends it to a configurable
  `host:port` — normally the PipeWire add-on's `rtp-source` node.
- Exposes HA entities via ESPHome's normal native API: connection state,
  connected device name/MAC, and best-effort AVRCP track title/artist
  (polled every 5s).
- Otherwise a completely ordinary ESPHome device — WiFi (with fallback
  hotspot + captive portal), OTA, HA native API. None of this needed any
  compromise; see [../../docs/decisions.md](../../docs/decisions.md#bluetooth-bridge-box-hardware-and-firmware-constraints)
  for why owning the classic-BT radio doesn't conflict with any of it.

## Installing

```
cp secrets.yaml.example secrets.yaml
# edit secrets.yaml: wifi_ssid, wifi_password, fallback_ap_password,
# and pipewire_rtp_host (the IP of whatever machine/container runs the
# PipeWire rtp-source you're targeting)
esphome run bt-bridge.yaml           # first flash: via USB
```

After the first flash, `esphome run`/`esphome upload` also works over
WiFi (OTA) — no cable needed for subsequent updates.

### Setting up the PipeWire side

The receiving end needs `libpipewire-module-rtp-source` configured to
match what this firmware actually sends: **native-endian `S16LE`**, not
RFC 3551's big-endian `L16` convention, and `sess.ignore-ssrc = true`
(this firmware picks a new random SSRC every boot — see
[decisions.md](../../docs/decisions.md#bluetooth-bridge-box-hardware-and-firmware-constraints)
for why that setting matters, not just what it does). Example
`pipewire.conf.d` snippet:

```
context.modules = [
    { name = libpipewire-module-rtp-source
        args = {
            source.ip = "0.0.0.0"
            source.port = 46000
            sess.media = "audio"
            sess.ignore-ssrc = true
            audio.format = "S16LE"
            audio.rate = 44100
            audio.channels = 2
            stream.props = {
                media.class = "Audio/Source"
                node.name = "bt-bridge-rtp"
            }
        }
    }
]
```

A checked-in version of this for the actual add-on container is tracked
as a follow-up — see [../../docs/roadmap.md](../../docs/roadmap.md#phase-5--bluetooth-bridge-box--functionally-done-one-item-owed).

## Configuring the RTP target from Home Assistant

The RTP host/port aren't just a compile-time secret — they're exposed as
a `text` entity ("PipeWire RTP Host") and `number` entity ("PipeWire RTP
Port") with `restore_value: true`, so you can repoint this box at a
different PipeWire instance from Home Assistant without reflashing.
Whatever you set persists across reboots; the YAML `secrets.yaml` value
only matters for the very first boot before anything's been set.

## Known limitations

- **AVRCP metadata is polled every 5s**, not push-notified (would need
  an extra RN-capabilities negotiation round trip to arm) — fine for an
  HA state display, not instant.
- **Pairing is SSP "Just Works"** (`ESP_BT_IO_CAP_NONE`) — no PIN entry
  possible on this headless device. Works with essentially all modern
  phones; legacy PIN requests are still handled defensively (auto-replies
  "0000") for older sources.
- **No confirmed prior art** exists for A2DP sink specifically packaged
  as a modern ESPHome `external_components` package — this was genuinely
  new integration work. See
  [decisions.md](../../docs/decisions.md#bluetooth-bridge-box-hardware-and-firmware-constraints)
  for the real bugs real hardware surfaced that no amount of code review
  would have caught.

## Directory layout

```
bt-bridge.yaml               ESPHome device config
secrets.yaml.example          copy to secrets.yaml and fill in
components/a2dp_bridge/
  __init__.py                  ESPHome component config schema + codegen
  a2dp_bridge.h/.cpp            classic-BT A2DP sink + AVRCP + RTP-out
```
