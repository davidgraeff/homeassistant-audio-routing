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

**Using the Home Assistant add-on?** You don't need to hand-write any of
this. The [PipeWire audio router add-on](../../pipewire_audio_router/README.md)
now loads the RTP source for you. Three equivalent ways to turn it on:

- **From Home Assistant** — the
  [custom integration](../../custom_components/pipewire_audio_router/README.md)
  exposes a *"Bluetooth bridge RTP source"* **switch** and a *"...RTP port"*
  **number**. Set the port to match this firmware's target port and flip the
  switch.
- **The add-on's web UI** → **Sources** tab → **Bluetooth bridge (RTP)
  source**.
- **The REST API** directly: `PUT /api/source/rtp {"port":46000}` (see
  [api-reference.md](../../docs/api-reference.md#rtp-source-bluetooth-bridge--a-module-not-a-process)).

The add-on already hardcodes the exact format this firmware sends, so the
node (`bt-bridge-rtp`) appears as a routable source automatically.

The rest of this section is only needed if you're feeding a **standalone
PipeWire session** (e.g. testing on a dev machine, not the add-on). The
receiving end needs `libpipewire-module-rtp-source` configured to match
what this firmware actually sends: **native-endian `S16LE`**, not RFC
3551's big-endian `L16` convention. `sess.ignore-ssrc` may be either — the
firmware now sends a **stable SSRC** derived from its factory MAC (see
[decisions.md](../../docs/decisions.md#bluetooth-bridge-box-hardware-and-firmware-constraints)),
so `false` (accept only the first sender's SSRC — a corruption guard against
a stray/second sender) survives reboots. `true` (accept any sender) is the
safe default kept below. Example `pipewire.conf.d` snippet:

```
context.modules = [
    { name = libpipewire-module-rtp-source
        args = {
            source.ip = "0.0.0.0"
            source.port = 46000
            sess.media = "audio"
            sess.ignore-ssrc = true
            # The module default (100ms) is too tight for this sender: the
            # ESP32 shares one 2.4GHz radio between classic-BT A2DP and WiFi,
            # so RTP egress arrives in bursts, not paced. 200ms absorbs that
            # jitter and stops the underruns you'd otherwise hear as stutter.
            sess.latency.msec = 200
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

This snippet is the manual equivalent of what the add-on now does for you
at runtime (see above) — the add-on builds the same args in
`bridge-daemon/src/rtp_source.rs` and loads the module live, so there's no
checked-in `.conf` to maintain. See
[../../docs/roadmap.md](../../docs/roadmap.md#phase-5--bluetooth-bridge-box--done).

## Configuring the RTP target from Home Assistant

The RTP host/port aren't just a compile-time secret — they're exposed as
a `text` entity ("PipeWire RTP Host") and `number` entity ("PipeWire RTP
Port") with `restore_value: true`, so you can repoint this box at a
different PipeWire instance from Home Assistant without reflashing.
Whatever you set persists across reboots; the YAML `secrets.yaml` value
only matters for the very first boot before anything's been set.

### Sending to more than one receiver (multicast)

A plain host is **unicast** — the stream reaches exactly one PipeWire box.
To feed several receivers from one bridge (e.g. a dev box *and* the HA
add-on) set **"PipeWire RTP Host"** to an IPv4 **multicast group** such as
`239.255.42.42`. No firmware change is needed: `sendto()` to a multicast
address just works (default TTL 1 keeps it on the local subnet, which is
where whole-home audio lives).

On each receiver, set the rtp-source's **Source address** (Sources tab in
the add-on UI, or `source.ip`) to that same group so PipeWire joins it via
`IP_ADD_MEMBERSHIP`; leave it `0.0.0.0` for unicast. Note your switch must
forward multicast to those ports — most home switches do (flood, or IGMP
snooping with a querier present).

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
