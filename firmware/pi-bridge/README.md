# Raspberry Pi Bluetooth → RTP bridge

A Linux-SBC alternative to the [ESP32 Bluetooth bridge](../bt-bridge/README.md).
A Raspberry Pi pairs as a Bluetooth speaker (classic-BT A2DP sink) and forwards
whatever a phone streams to it into the [PipeWire audio router
add-on](../../pipewire_audio_router/README.md) as an RTP stream — landing on the
**same `bt-bridge-rtp` source node** the ESP32 feeds, so the add-on needs no
change to accept either.

Unlike the ESP32, there is **no custom firmware** — BlueZ + PipeWire do the A2DP
sink and the RTP send with pure configuration. [`setup_pi_bridge.py`](setup_pi_bridge.py)
applies that configuration (idempotently) and is the whole deliverable.

Developed and verified end-to-end on a **Raspberry Pi Zero 2 W**, Raspberry Pi
OS **Trixie** (PipeWire 1.4.2 / WirePlumber 0.5.8 / BlueZ 5.82), streaming
**aptX** from a Samsung S23 → RTP → add-on → real speakers.

## Why a Pi instead of the ESP32

- **No firmware to maintain.** No vendored ESP-IDF, no A2DP/AVRCP hand-rolling.
- **Better codecs.** BlueZ negotiates **aptX / AAC / SBC** (the ESP32 sink is
  effectively SBC-only), so audio from modern phones arrives at higher quality
  before re-transmission.
- **Trivially extensible** (Opus RTP, unicast, etc. — all config).

Honest trade-offs: it's a full Linux box (boot time, SD-card wear, more power),
and the Pi Zero 2 W still shares **one 2.4 GHz radio** between BT and WiFi — the
same coexistence constraint as the ESP32 (mitigated here by disabling WiFi
power-save; see below).

## Requirements

- **Raspberry Pi** with classic-Bluetooth (a Zero 2 W, 3, 4, or 5 — anything
  BlueZ sees as an adapter). 512 MB RAM (Zero 2 W) is enough.
- **Raspberry Pi OS Trixie recommended** (WirePlumber ≥ 0.5). The script is
  version-aware and also supports bookworm (WirePlumber 0.4), but see the
  WirePlumber-0.5 note under *What the script configures* — the 0.5 path needed
  a specific fix that 0.4 didn't.
- Passwordless `sudo` for the user you run it as (the Raspberry Pi Imager
  default user gets this automatically).
- The add-on's **RTP source enabled** on the target port (default `46000`).

## Install

Run it **as the bridge user** (not root), on the Pi:

```
# copy the script over, then:
./setup_pi_bridge.py --host <add-on/HA host IP> [--port 46000] [--name "Bathroom Music"]
```

- `--host` (required): where the RTP is sent — the add-on/HA host IP for
  **unicast** (recommended for one Pi → one add-on), or an IPv4 **multicast
  group** like `239.255.42.42` to fan out to several receivers.
- `--port` (default `46000`): must match the add-on's RTP source port.
- `--name`: the Bluetooth name phones see (sets the system pretty-hostname,
  which BlueZ advertises; omit to keep the current one). It also becomes the
  bridge's mDNS name, i.e. how it appears on the add-on's Sources tab.
- `--diag-port` (default `8080`): the port the
  [testing app](bluetooth-testing-app/) serves on, published in the mDNS advert
  so the add-on can link to it.
- `--no-mdns`: don't advertise at all (the add-on then can't auto-discover this
  bridge, and shows no diagnostics link).
- `--disable`: remove the bridge config again.

Then pair your phone with the advertised name and play — audio appears as the
`bt-bridge-rtp` source in the add-on's routing matrix.

### Matching the add-on's RTP source

The Pi sends **S16LE / 44100 / stereo**, matching what the add-on's
`libpipewire-module-rtp-source` expects (`bridge-daemon/src/rtp_source.rs`), so
no receiver change is needed. If you use **unicast**, set the add-on's RTP
source to *Accept all senders* (`0.0.0.0`); for **multicast**, set the add-on's
source address to the same group.

## What the script configures

Idempotent — safe to re-run. Each step exists because bring-up hit a concrete
wall (see [../../docs/decisions.md](../../docs/decisions.md#raspberry-pi-bluetooth--rtp-bridge)):

1. **Installs** pipewire, wireplumber, `libspa-0.2-bluetooth`, `bluez-tools`, `iw`.
2. **Enables user lingering** so the PipeWire user session runs headless at boot.
3. **Disables WiFi power-save** (NetworkManager `wifi.powersave=2` + a boot
   oneshot). On the shared BCM43xx radio, WiFi power-save parks the radio and
   *drops the Bluetooth link* — this was the concrete cause of flaky pairing.
4. **BlueZ** (`/etc/bluetooth/main.conf`): audio device class, discoverable +
   `AlwaysPairable`, `[Policy] AutoEnable=true` (else the controller comes up
   *unpowered* on a fresh image — WirePlumber then sees no adapter and offers no
   A2DP sink), and **`JustWorksRepairing = always`** (else a device that
   unpaired can't re-pair without manually purging its bond from
   `/var/lib/bluetooth`).
5. **Headless pairing agent** (`bt-agent --capability=NoInputNoOutput`) +
   discoverable/pairable on boot, so phones pair with no screen / no PIN.
6. **PipeWire drop-in**: `module-rtp-sink` (the RTP *sender*, node `rtp-bridge`,
   pointed at the add-on) + `module-loopback` bridging the phone's audio into it.
7. **mDNS advert** (`/etc/avahi/services/pw-bt-bridge.service`): announces this
   bridge as `_pwrouter-btbridge._tcp` on the diagnostics port, carrying the
   stream parameters in TXT (`rtp_port`, `rtp_dest`, `rate`, `fmt`, `channels`)
   and its roles (`role=rtp-sender,metadata`).
   See [*Being discovered by the add-on*](#being-discovered-by-the-add-on).
8. **AVRCP metadata reporter** (`bt-metadata-reporter.service`, from
   [`bt_metadata_reporter.py`](bt_metadata_reporter.py)) — see
   [*Reporting what the phone is playing*](#reporting-what-the-phone-is-playing).
   `--no-metadata` skips it.
9. **WirePlumber drop-in** (SPA-JSON for 0.5, Lua for 0.4):
   - **`monitor.bluez.seat-monitoring = disabled`** (+ `support.logind`) —
     **critical on WirePlumber 0.5 headless.** WP 0.5 only manages Bluetooth for
     the user on the *active login seat*; a lingering headless session has no
     seat, so the bluez monitor loads but registers **no A2DP sink** — the
     adapter never advertises Audio Sink and phones see a paired device with no
     audio service. (WP 0.4 has no such gate, so bookworm "just worked".)
   - **`priority.session = 3000`** on the A2DP source so it outranks the RTP
     sink's own monitor: the loopback's default-following capture then binds to
     the phone (and re-binds the same way on reconnect/re-pair). `input` role +
     no idle suspend round it out.

### Being discovered by the add-on

The add-on cannot work out which host feeds an RTP source — `module-rtp-source`
only knows the address it *listens on*, and sniffing the RTP port would steal
datagrams from the module — so the bridge announces itself instead. With the
advert in place, the add-on's **Sources** tab:

- lists this bridge under **Discovered Bluetooth bridges** while no source is
  receiving it, with **Add as RTP source** copying the advertised port, rate and
  destination (so the two ends can't disagree by typo);
- once adopted, marks that source `via <bridge name>` and — if the
  [testing app](bluetooth-testing-app/) is running — offers **Show diagnostics**,
  a direct link to this Pi's live waveform/silence page.

The advert is written by this script and therefore outlives any particular run
of the testing app, so the add-on HTTP-probes the page before offering the link.
That means: **the bridge appears as soon as it is set up; the diagnostics link
appears only while the app is running.** The link opens the Pi directly, so the
browser has to be on the same network (a remote Home Assistant session won't
reach it).

Check the advert from any Linux box on the LAN:

```
avahi-browse -rt _pwrouter-btbridge._tcp
```

### Reporting what the phone is playing

The phone sends more than audio: **AVRCP** carries title, artist, album, duration
and play state, which BlueZ exposes on the system D-Bus as
`org.bluez.MediaPlayer1`. None of that is in the PipeWire graph, so the RTP path
could never have carried it — a small service forwards it instead, and Home
Assistant then shows the track on whatever outputs this bridge's source feeds.

- Unit: `bt-metadata-reporter.service` (system, not user — `org.bluez` is on the
  system bus). No audio, no realtime: it cannot compete with the capture/relay
  threads.
- It reports to the add-on's HTTP API (`--host`, `--api-port`, default 8099) as
  *"the sender on RTP port N is playing X"*, so this Pi never needs to know the
  source ids the add-on assigned.
- **No cover art.** AVRCP artwork is the 1.6 BIP/OBEX feature and BlueZ does not
  expose it, so Bluetooth tracks have title/artist/album and no picture.
- Needs `python3-dbus` + `python3-gi` (installed by the setup script; neither
  ships on Raspberry Pi OS by default).

Check it by hand — with a phone connected and playing:

```
systemctl status bt-metadata-reporter
journalctl -u bt-metadata-reporter -n 20
# One-shot: print/report the current state and exit (also the quickest way to see
# whether BlueZ is exposing a player at all).
sudo /usr/local/bin/bt-metadata-reporter --host <addon-ip> --rtp-port 46000 --once
```

The player object is **transient** — `/org/bluez/hciX/dev_.../playerN` exists only
while a phone with an AVRCP target is connected, so `busctl tree org.bluez` showing
only `dev_*` nodes means "nothing connected", not "broken".

See [../../docs/source-metadata-plan.md](../../docs/source-metadata-plan.md).

### The audio path

```
phone --A2DP(aptX/AAC/SBC)--> BlueZ --> bluez_input.* (Audio/Source, priority 3000)
   --> [module-loopback] --> rtp-bridge (module-rtp-sink)
   --> UDP/RTP --> add-on's rtp-source (node bt-bridge-rtp) --> routing matrix
```

## Verifying

On the Pi (`export XDG_RUNTIME_DIR=/run/user/$(id -u)` first):

```
bluetoothctl show | grep -E 'Powered|Audio Sink'   # want: Powered yes, UUID: Audio Sink
pw-cli ls Node | grep rtp-bridge                    # the RTP sender node
pw-link -l | grep -A2 bt-bridge-capture:input       # while playing: bound to bluez_input.*, NOT :monitor
cat /sys/class/net/wlan0/statistics/tx_packets      # rises ~150/s while streaming
```

For anything more than a one-off check, use the
[**Bluetooth testing app**](bluetooth-testing-app/README.md) instead: a
dependency-free web console that shows a live waveform of the incoming Bluetooth
audio, flags **digital silence** (which every counter above reports as healthy),
switches the A2DP codec, and shows the sender chain's PipeWire state.

**Do not use `tcpdump` to confirm RTP egress on the Pi** — this WiFi driver
offloads locally-generated multicast TX past tcpdump's capture hook, so it
misleadingly shows **0 packets even when audio is flowing**. Use the `wlan0`
`tx_packets` counter (above), the `pw-link` binding, or simply whether the
add-on plays.

## Operational notes / gotchas

- **Don't restart PipeWire/WirePlumber while a phone is connected.** It orphans
  the A2DP transport (bluetoothd still shows "connected", but the audio channel
  is dead and the source node disappears), and only a **clean boot** — or a full
  BT disconnect+reconnect — reliably re-establishes it. The appliance is
  designed to run from boot; that's its normal mode.
- **Re-pairing** works without intervention thanks to `JustWorksRepairing =
  always`. If a device ever still refuses (e.g. from a much older BlueZ), purge
  its bond: `bluetoothctl remove <MAC>` **and** `sudo rm -rf
  /var/lib/bluetooth/<adapter>/<MAC> /var/lib/bluetooth/<adapter>/cache/<MAC>`.
- **Multicast vs unicast.** Multicast (`239.255.42.42`) fans out to several
  receivers but depends on IGMP group membership, which a receiver must re-join
  after a restart (the add-on's **Enable** button does a full module reload =
  re-join). For one Pi → one add-on, **unicast is simpler and restart-proof** —
  no group to lose. Prefer `--host <add-on-IP>` unless you genuinely need fan-out.
- **`bluez5` codec plugins** (aptX/LDAC/AAC/Opus) come from `libspa-0.2-bluetooth`
  and are enabled by default; the phone picks the best both sides support.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Phone pairs, but no "media audio" / no sound; adapter has no `Audio Sink` UUID | WP 0.5 seat-monitoring gating on headless | ensure the `seat-monitoring = disabled` drop-in is present (script step 7); restart wireplumber |
| Re-pairing a previously-paired device fails | stale bond + `JustWorksRepairing = never` | script sets `always`; if needed, purge the bond (above) |
| Flaky pairing / dropped BT link | WiFi power-save on the shared radio | script disables it; confirm `iw dev wlan0 get power_save` = off |
| Adapter `Powered: no` after boot | `AutoEnable` not set | script sets `[Policy] AutoEnable=true` |
| Connected but no audio after you restarted a service | orphaned A2DP transport | reboot, or BT disconnect+reconnect the phone |
| `tcpdump` shows 0 RTP but you hear audio | WiFi TX offload hides it from tcpdump | not a problem — use the `tx_packets` counter |

## Directory layout

```
setup_pi_bridge.py       the idempotent configurator (run on the Pi)
bt_metadata_reporter.py  forwards the phone's AVRCP track info to the add-on
bluetooth-testing-app/   live web console: waveform, codec switching, sender state
README.md                this file
```
