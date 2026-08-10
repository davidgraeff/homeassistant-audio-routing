#!/usr/bin/env python3
"""Configure a Raspberry Pi as a Bluetooth A2DP -> RTP audio bridge.

This is the "big brother" of the ESP32 firmware in ../bt-bridge/: instead of a
microcontroller, a Linux SBC (developed and validated on a **Raspberry Pi Zero
2 W**, Raspberry Pi OS / Debian 12 "bookworm") pairs as a Bluetooth speaker and
forwards whatever a phone streams to it into the PipeWire audio-router add-on as
an RTP stream -- landing on the same `bt-bridge-rtp` source node the ESP32 feeds.

Why a Pi instead of the ESP32:
  - No custom firmware to maintain: BlueZ + PipeWire do the A2DP sink and the
    RTP send with pure configuration.
  - BlueZ decodes **AAC** (not just SBC), so audio from iPhones arrives at
    higher quality before re-transmission.
  - Trivially upgradeable to Opus/other transports later (all config).
Trade-off honestly stated: the Pi Zero 2 W shares one 2.4 GHz radio between BT
and Wi-Fi just like the ESP32, so it does *not* magically escape the coexistence
airtime pressure -- compression (a future step) is the real lever there.

What this script does (idempotent -- safe to re-run):
  1. Installs pipewire, wireplumber, the BlueZ SPA plugin and bt-agent (apt).
  2. Enables lingering for the bridge user so its PipeWire user session runs
     headless at boot with no login.
  3. Writes a PipeWire drop-in that loads `module-rtp-sink` (the RTP *sender*,
     pointed at the add-on) plus a `module-loopback` that bridges the phone's
     audio into it.
  4. Writes a WirePlumber drop-in so the incoming Bluetooth audio is the
     preferred capture source and never suspends mid-stream. Emits the correct
     format for the installed WirePlumber (Lua for 0.4.x, SPA-JSON for 0.5+).
  5. Points BlueZ at the A2DP-sink role, an audio device class, and installs a
     "just works" pairing agent + discoverable/pairable-on-boot service so a
     phone can pair with no screen on the Pi.
  6. Publishes an mDNS advert (`_pwrouter-btbridge._tcp`) so the add-on can
     *discover* this bridge instead of being told about it -- see
     `avahi_service_xml`.
  7. (Re)starts everything and prints how to verify.

The audio path it builds:

    phone --A2DP--> BlueZ --> bluez_input.* (Audio/Source)
        --> [module-loopback] --> rtp-bridge (module-rtp-sink)
        --> UDP/RTP --> add-on's rtp-source (node `bt-bridge-rtp`)

Design notes proven on real hardware (see ../../docs/decisions.md, the
"Raspberry Pi Bluetooth bridge" entry, and firmware/pi-bridge/README.md):
  - The loopback capture is left to follow the *default* source rather than
    pinned to a per-phone node name. A headless Pi has no other audio source, so
    when a phone connects its (high-priority) BlueZ node wins and the loopback
    locks onto it. When no phone is connected the capture sits on the sink's own
    monitor, but that cycle has no driver -> **zero packets are sent while
    idle** (measured), so nothing is renamed and nothing leaks airtime.
  - `module-rtp-sink`'s S16LE/**48000**/stereo output matches what the add-on's
    `libpipewire-module-rtp-source` now defaults to (rtp_source.rs). Both ends at
    48 kHz keeps the whole path at the router graph's native rate — no resample
    on the Pi (phones stream 48 kHz over aptX/AAC) and none on the receiver. If
    you change the add-on's RTP source rate, pass a matching `--rate` here.

Run it AS the bridge user (not root), on the Pi, with passwordless sudo:

    ./setup_pi_bridge.py --host 192.168.178.22          # add-on / HA host IP
    ./setup_pi_bridge.py --host 239.255.42.42 --name "Bathroom Music"

Use --disable to tear the bridge config back out.
"""

from __future__ import annotations

import argparse
import os
import pwd
import re
import subprocess
import sys
import tempfile

# --- Constants ---------------------------------------------------------------

DEFAULT_PORT = 46000
DEFAULT_FORMAT = "S16LE"  # native-endian PCM; matches rtp_source.rs on the add-on
DEFAULT_RATE = 48000
DEFAULT_CHANNELS = 2
# Bluetooth Class of Device: major "Audio/Video", minor "Loudspeaker", with the
# Audio + Rendering service bits set, so phones show the Pi as a speaker.
DEFAULT_COD = "0x240414"

APT_PACKAGES = [
    "pipewire",
    "pipewire-audio",
    "pipewire-bin",
    "wireplumber",
    "libspa-0.2-bluetooth",
    "bluez",
    "bluez-tools",  # provides bt-agent
    "iw",           # to disable WiFi power-save (BT coexistence)
    # For the AVRCP metadata reporter: BlueZ exposes track info on D-Bus, and
    # neither binding is installed on Raspberry Pi OS by default (verified on
    # Trixie: `import dbus` and `import gi` both fail). dbus-python needs the GLib
    # main loop from python3-gi to receive signals at all.
    "python3-dbus",
    "python3-gi",
]

#: mDNS service type the add-on browses to find Bluetooth->RTP bridges. Custom,
#: because there is no stock convention for "I am an RTP audio *sender* looking
#: for a receiver": PipeWire's `_pipewire-audio._udp` means the opposite (a
#: `module-rtp-session` host willing to *receive*), and the add-on already treats
#: those as routing *outputs* — advertising it here would make this Pi show up as
#: a speaker. `_tcp` because what the advert points at is the HTTP diagnostics
#: page; the audio itself stays plain RTP on `--port` and is not discovered.
AVAHI_SERVICE_TYPE = "_pwrouter-btbridge._tcp"
AVAHI_SERVICE_FILE = "/etc/avahi/services/pw-bt-bridge.service"
#: Port the bluetooth-testing-app listens on (its own default). The advert names
#: it so the add-on can offer a link to the diagnostics page; the add-on probes it
#: before showing the link, so advertising it while the app is *not* running is
#: harmless.
DEFAULT_DIAG_PORT = 8080
#: Bumped when the TXT record's meaning changes, so an old add-on can ignore an
#: advert it would misread rather than guess.
AVAHI_TXT_VERSION = 1
#: Version of the metadata-reporting contract this bridge speaks (the body shape
#: of `POST /api/now_playing/report`). Independent of AVAHI_TXT_VERSION so the
#: reporter can evolve without touching the discovery contract.
METADATA_CONTRACT_VERSION = 1

PW_DROPIN_NAME = "60-bt-rtp-bridge.conf"
WP_LUA_DROPIN_NAME = "51-bt-rtp-bridge.lua"        # WirePlumber 0.4.x
WP_CONF_DROPIN_NAME = "51-bt-rtp-bridge.conf"      # WirePlumber 0.5+
AGENT_UNIT = "bt-agent-a2dp.service"
#: Service forwarding the phone's AVRCP metadata to the add-on
#: (bt_metadata_reporter.py, docs/source-metadata-plan.md WP3). Separate from the
#: audio path on purpose: if it is not installed the bridge still carries sound.
METADATA_UNIT = "bt-metadata-reporter.service"
METADATA_SCRIPT = "/usr/local/bin/bt-metadata-reporter"
#: Port the add-on's HTTP API listens on. Only the *metadata* reporter needs it —
#: the audio path is plain RTP and talks to nobody.
DEFAULT_API_PORT = 8099
POWERSAVE_UNIT = "wifi-powersave-off.service"
NM_POWERSAVE_CONF = "/etc/NetworkManager/conf.d/wifi-powersave-off.conf"
BLUEZ_MAIN_CONF = "/etc/bluetooth/main.conf"
MANAGED_MARKER = "# Managed by firmware/pi-bridge/setup_pi_bridge.py"


# --- Small shell helpers -----------------------------------------------------


def run(cmd: list[str], *, check: bool = True, capture: bool = False) -> subprocess.CompletedProcess:
    """Run a command, echoing it. Raises on failure unless check=False."""
    print("  $", " ".join(cmd))
    return subprocess.run(cmd, check=check, text=True, capture_output=capture)


def sudo(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    return run(["sudo", "-n", *cmd], **kw)


def sudo_write(path: str, content: str, *, mode: str = "0644") -> None:
    """Write `content` to a root-owned `path` atomically via a temp file."""
    with tempfile.NamedTemporaryFile("w", delete=False, suffix=".tmp") as tf:
        tf.write(content)
        tmp = tf.name
    sudo(["install", "-m", mode, tmp, path])
    os.unlink(tmp)


def user_write(path: str, content: str) -> None:
    """Write a file owned by the current (bridge) user, creating parent dirs."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(content)
    print("  wrote", path)


def user_remove(path: str) -> None:
    try:
        os.unlink(path)
        print("  removed", path)
    except FileNotFoundError:
        pass


def systemctl_user(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    """`systemctl --user ...` with XDG_RUNTIME_DIR set (works over headless SSH)."""
    env = dict(os.environ, XDG_RUNTIME_DIR=f"/run/user/{os.getuid()}")
    cmd = ["systemctl", "--user", *args]
    print("  $", " ".join(cmd))
    return subprocess.run(cmd, check=check, text=True, env=env)


# --- Preconditions -----------------------------------------------------------


def preflight() -> None:
    if os.geteuid() == 0:
        sys.exit(
            "Run this AS the bridge user (e.g. `david`), not root — it configures that\n"
            "user's PipeWire session. It uses `sudo` internally for system changes."
        )
    if subprocess.run(["sudo", "-n", "true"]).returncode != 0:
        sys.exit("Passwordless sudo is required (sudo -n true failed).")


def wireplumber_major_minor() -> tuple[int, int]:
    """Return (major, minor) of the installed WirePlumber, e.g. (0, 4)."""
    out = subprocess.run(["wireplumber", "--version"], text=True, capture_output=True).stdout
    m = re.search(r"(\d+)\.(\d+)", out)
    if not m:
        # Not installed yet at this point is fine; caller installs it first.
        return (0, 0)
    return (int(m.group(1)), int(m.group(2)))


# --- Steps -------------------------------------------------------------------


def ensure_packages() -> None:
    print("== Installing packages ==")
    missing = [
        p
        for p in APT_PACKAGES
        if subprocess.run(["dpkg", "-s", p], capture_output=True).returncode != 0
    ]
    if not missing:
        print("  all present")
        return
    sudo(["apt-get", "update"])
    sudo(["env", "DEBIAN_FRONTEND=noninteractive", "apt-get", "install", "-y", *missing])


def ensure_linger(user: str) -> None:
    print("== Enabling user lingering (headless session) ==")
    sudo(["loginctl", "enable-linger", user])


def configure_bluez(name: str | None) -> None:
    """Set the A2DP-sink-friendly bits in /etc/bluetooth/main.conf idempotently."""
    print("== Configuring BlueZ ==")
    try:
        with open(BLUEZ_MAIN_CONF) as f:
            text = f.read()
    except FileNotFoundError:
        text = "[General]\n"

    # NB: the BlueZ 'hostname' plugin overrides main.conf's `Name` with the
    # system pretty-hostname, so the advertised name is set via hostnamectl
    # below, not here. We still set the audio device class + pairable bits here.
    # `[Policy] AutoEnable` matters on a fresh image: without it the controller
    # comes up *unpowered* at boot, so WirePlumber never sees an adapter and no
    # A2DP sink is offered — observed on a clean Trixie install.
    # `JustWorksRepairing = always`: BlueZ defaults to `never`, which *refuses* a
    # new "just works" pairing from a device that's already bonded (an anti-MITM
    # default). On this headless speaker that means: once a phone has paired, if
    # it later unpairs (dropping its key) and tries to pair again, BlueZ rejects
    # it because it still holds the old bond — so re-pairing fails until the
    # bond is manually purged from /var/lib/bluetooth. `always` lets a known
    # device re-pair and overwrite the stale bond seamlessly.
    wanted = {
        "General": {
            "Class": DEFAULT_COD,
            "DiscoverableTimeout": "0",
            "AlwaysPairable": "true",
            "JustWorksRepairing": "always",
        },
        "Policy": {
            "AutoEnable": "true",
        },
    }

    lines = text.splitlines()

    def set_key(lines: list[str], section: str, key: str, value: str) -> list[str]:
        """Set key=value inside [section] (uncomment/replace, or insert; create
        the section if missing)."""
        header = f"[{section}]"
        if not any(l.strip() == header for l in lines):
            lines.append(header)
        in_section = False
        pat = re.compile(rf"^\s*#?\s*{re.escape(key)}\s*=", re.IGNORECASE)
        for i, l in enumerate(lines):
            s = l.strip()
            if s.startswith("[") and s.endswith("]"):
                in_section = s == header
                continue
            if in_section and pat.match(l):
                lines[i] = f"{key} = {value}"
                return lines
        # Not found in the section: insert right after its header.
        hi = next(i for i, l in enumerate(lines) if l.strip() == header)
        lines.insert(hi + 1, f"{key} = {value}")
        return lines

    for section, keys in wanted.items():
        for k, v in keys.items():
            lines = set_key(lines, section, k, v)

    new_text = "\n".join(lines) + "\n"
    changed = new_text != text
    if changed:
        sudo_write(BLUEZ_MAIN_CONF, new_text)

    # The advertised Bluetooth name = system pretty-hostname (the BlueZ hostname
    # plugin sources it there and overrides main.conf's Name). Set it reliably.
    if name:
        sudo(["hostnamectl", "set-hostname", "--pretty", name])
        changed = True

    if changed:
        sudo(["systemctl", "restart", "bluetooth"])
    else:
        print("  already configured")


def disable_wifi_powersave() -> None:
    """Keep WiFi power-save off. On the Pi Zero 2 W (and other BCM43xx SBCs) BT
    and WiFi share one 2.4 GHz radio; WiFi power-save parks the radio and
    starves/drops the Bluetooth link, so A2DP connections flap. This was the
    concrete cause of unstable pairing during bring-up. Persisted two ways: the
    NetworkManager setting (authoritative on Raspberry Pi OS bookworm) and a
    boot-time oneshot as a fallback for non-NM setups."""
    print("== Disabling WiFi power-save (BT coexistence) ==")
    sudo_write(
        NM_POWERSAVE_CONF,
        "# BT/WiFi share one 2.4GHz radio; power-save parks it and drops the BT\n"
        "# link. Keep it off. (firmware/pi-bridge/setup_pi_bridge.py)\n"
        "[connection]\nwifi.powersave = 2\n",
    )
    unit = """\
[Unit]
Description=Disable WiFi power save (Bluetooth coexistence)
After=sys-subsystem-net-devices-wlan0.device

[Service]
Type=oneshot
ExecStart=/usr/sbin/iw dev wlan0 set power_save off
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
"""
    sudo_write(f"/etc/systemd/system/{POWERSAVE_UNIT}", unit)
    sudo(["systemctl", "daemon-reload"])
    sudo(["systemctl", "enable", "--now", POWERSAVE_UNIT], check=False)
    # Apply immediately too (the unit only fires at boot / device appearance).
    sudo(["iw", "dev", "wlan0", "set", "power_save", "off"], check=False)


def install_pairing_agent() -> None:
    """A 'just works' pairing agent + discoverable/pairable on boot (headless)."""
    print("== Installing headless pairing agent ==")
    unit = f"""\
[Unit]
Description=Bluetooth just-works pairing agent for the A2DP -> RTP bridge
Documentation=https://github.com/ (firmware/pi-bridge/README.md)
After=bluetooth.service
Requires=bluetooth.service

[Service]
Type=simple
# Power the adapter and make it discoverable/pairable (DiscoverableTimeout=0 in
# main.conf keeps it up). The '-' prefixes tolerate a not-yet-ready controller.
ExecStartPre=-/usr/bin/bluetoothctl power on
ExecStartPre=-/usr/bin/bluetoothctl discoverable on
ExecStartPre=-/usr/bin/bluetoothctl pairable on
# NoInputNoOutput => accept SSP "just works" pairing with no PIN prompt.
ExecStart=/usr/bin/bt-agent --capability=NoInputNoOutput
Restart=on-failure
RestartSec=2

[Install]
WantedBy=bluetooth.target
"""
    sudo_write(f"/etc/systemd/system/{AGENT_UNIT}", unit)
    sudo(["systemctl", "daemon-reload"])
    sudo(["systemctl", "enable", "--now", AGENT_UNIT])


def install_metadata_reporter(*, host: str, api_port: int, rtp_port: int) -> None:
    """Install + start the AVRCP metadata reporter (bt_metadata_reporter.py).

    Additive and strictly optional: it forwards the phone's track info to the
    add-on and touches neither BlueZ nor PipeWire, so a failure here leaves the
    audio path untouched. The script is copied next to the unit rather than run
    from the invoking user's home, so a service start does not depend on where
    this setup script happened to be executed from.
    """
    print("== Installing AVRCP metadata reporter ==")
    source = os.path.join(os.path.dirname(os.path.abspath(__file__)), "bt_metadata_reporter.py")
    if not os.path.exists(source):
        print(f"  {source} not found — skipping the metadata reporter (audio is unaffected).")
        return
    with open(source, encoding="utf-8") as f:
        sudo_write(METADATA_SCRIPT, f.read(), mode="0755")
    unit = f"""\
{MANAGED_MARKER}
[Unit]
Description=Report the connected phone's AVRCP metadata to the PipeWire audio-router add-on
Documentation=https://github.com/ (docs/source-metadata-plan.md)
After=bluetooth.service network-online.target
Wants=network-online.target
Requires=bluetooth.service

[Service]
Type=simple
# Runs as a system service because org.bluez lives on the SYSTEM bus (the audio
# path's user session is irrelevant here). No audio, no realtime: plain default
# scheduling, so this can never compete with the bridge's capture/relay threads.
ExecStart={METADATA_SCRIPT} --host {host} --api-port {api_port} --rtp-port {rtp_port}
Restart=always
RestartSec=5
# It only reads D-Bus and makes outbound HTTP calls.
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
"""
    sudo_write(f"/etc/systemd/system/{METADATA_UNIT}", unit)
    sudo(["systemctl", "daemon-reload"])
    sudo(["systemctl", "enable", "--now", METADATA_UNIT])


def remove_metadata_reporter() -> None:
    """Stop and remove the reporter. Idempotent."""
    sudo(["systemctl", "disable", "--now", METADATA_UNIT], check=False)
    sudo(["rm", "-f", f"/etc/systemd/system/{METADATA_UNIT}", METADATA_SCRIPT], check=False)
    sudo(["systemctl", "daemon-reload"], check=False)


def avahi_service_xml(*, name: str | None, host: str, port: int, fmt: str, rate: int,
                      channels: int, diag_port: int) -> str:
    """A static Avahi service file advertising this bridge to the add-on.

    **Why the add-on cannot just work it out.** `module-rtp-source` exposes only
    the address it *listens on* (`rtp.source.ip` — `0.0.0.0` or a multicast
    group), never the sender's, and a second socket on the RTP port to sniff it
    would take datagrams away from the module (SO_REUSEPORT load-balances
    unicast) and cause real dropouts. So the bridge announces itself instead.

    **Why a static file and not `avahi-publish`.** `avahi-daemon` is already
    running and reads `/etc/avahi/services/` at startup and on change, so this
    needs no new package (`avahi-utils` is *not* installed on Raspberry Pi OS by
    default), no unit to supervise, and it survives reboots. It is also
    independent of the diagnostics app: the advert says "a bridge lives here and
    these are its stream parameters", and the add-on probes the HTTP port before
    offering a link, so the advert is never a lie about what is running.

    `<port>` is the **diagnostics** port, not the audio port: the advert points at
    something reachable over HTTP. The RTP parameters travel in TXT so the add-on
    can match this bridge to an existing RTP source (by `rtp_port`/`rtp_dest`) or
    prefill a new one without the user retyping them.

    `%h` is expanded by Avahi to this host's name; an explicit `--name` wins
    because it is what the owner already chose to see on their phone.
    """
    label = name.strip() if name and name.strip() else "Bluetooth bridge on %h"
    txt = [
        # NOTE: `ver` must stay 1. The add-on's bt_bridge_discovery.rs *skips* an
        # advert whose `ver` is newer than it understands, so bumping it would make
        # an updated Pi vanish from a not-yet-updated add-on — taking Bluetooth
        # discovery and adoption with it. New TXT keys are ignored by that same
        # parser, so additions like `role`/`meta_ver` below are safe both ways.
        f"ver={AVAHI_TXT_VERSION}",
        # Two roles now: the RTP audio sender, and the AVRCP metadata reporter
        # (docs/source-metadata-plan.md §3.5). `meta_ver` versions the metadata
        # contract independently of the TXT layout.
        "role=rtp-sender,metadata",
        f"meta_ver={METADATA_CONTRACT_VERSION}",
        f"rtp_port={port}",
        f"rtp_dest={host}",
        f"rate={rate}",
        f"fmt={fmt}",
        f"channels={channels}",
        f"diag_port={diag_port}",
        "diag_path=/",
    ]
    lines = "\n".join(f"    <txt-record>{_xml_escape(t)}</txt-record>" for t in txt)
    return f"""\
<?xml version="1.0" standalone='no'?><!--*-nxml-*-->
<!DOCTYPE service-group SYSTEM "avahi-service.dtd">
<!-- {MANAGED_MARKER.lstrip('# ')} -->
<!-- Advertises this Bluetooth->RTP bridge so the PipeWire audio-router add-on
     can discover it. <port> is the HTTP diagnostics port (the bluetooth-testing-
     app); the audio stream itself is plain RTP to rtp_dest:rtp_port and is not
     discovered. Delete this file (and reload avahi-daemon) to stop advertising. -->
<service-group>
  <name replace-wildcards="yes">{_xml_escape(label)}</name>
  <service>
    <type>{AVAHI_SERVICE_TYPE}</type>
    <port>{diag_port}</port>
{lines}
  </service>
</service-group>
"""


def _xml_escape(text: str) -> str:
    """Escape for XML text content. A Bluetooth name may contain `&` or `<`."""
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def publish_mdns_advert(*, name: str | None, host: str, port: int, fmt: str, rate: int,
                        channels: int, diag_port: int) -> None:
    """Install the Avahi service file and make the running daemon pick it up."""
    print("== Advertising this bridge over mDNS ==")
    if not os.path.isdir("/etc/avahi/services"):
        print("  /etc/avahi/services missing — avahi-daemon not installed; skipping advert.")
        print("  (The add-on can still receive audio; it just won't auto-discover this bridge.)")
        return
    sudo_write(
        AVAHI_SERVICE_FILE,
        avahi_service_xml(name=name, host=host, port=port, fmt=fmt, rate=rate,
                          channels=channels, diag_port=diag_port),
    )
    # avahi-daemon watches the directory, but a reload is instant and makes the
    # advert appear before `verify()` tells the user to go look for it.
    sudo(["systemctl", "reload-or-restart", "avahi-daemon"], check=False)


def remove_mdns_advert() -> None:
    """Stop advertising. Safe when the file was never written."""
    if not os.path.exists(AVAHI_SERVICE_FILE):
        return
    print("== Removing mDNS advert ==")
    sudo(["rm", "-f", AVAHI_SERVICE_FILE], check=False)
    sudo(["systemctl", "reload-or-restart", "avahi-daemon"], check=False)


def pw_dropin(host: str, port: int, fmt: str, rate: int, channels: int) -> str:
    return f"""\
{MANAGED_MARKER}
# Bluetooth -> RTP bridge. Edit via setup_pi_bridge.py, not here (re-running the
# script overwrites this file).
#
# module-rtp-sink is the RTP *sender*: it exposes an Audio/Sink named
# "rtp-bridge" and transmits whatever is played into it to the add-on's
# rtp-source. module-loopback bridges the phone's Bluetooth audio into that
# sink. The playback end is pinned to "rtp-bridge"; the capture end is left to
# follow the DEFAULT source. The WirePlumber rule gives the Bluetooth source a
# high priority.session, so when a phone is connected it always outranks the
# sink's own monitor and the capture binds to it — deterministically, and
# re-binds the same way on reconnect/re-pair. (An earlier attempt to pin the
# capture with target.object = the bluez node backfired on WirePlumber 0.5: it
# ADDED that link but also kept a fallback link to the sink monitor, creating a
# feedback loop that stalled the graph. Priority-based default selection avoids
# that — the capture only ever binds to the single highest-priority source.)
context.modules = [
  {{ name = libpipewire-module-rtp-sink
    args = {{
      destination.ip = "{host}"
      destination.port = {port}
      sess.media = "audio"
      audio.format = "{fmt}"
      audio.rate = {rate}
      audio.channels = {channels}
      stream.props = {{
        node.name = "rtp-bridge"
        node.description = "Bluetooth to RTP bridge (sender)"
        media.class = "Audio/Sink"
      }}
    }}
  }}
  {{ name = libpipewire-module-loopback
    args = {{
      node.description = "Bluetooth to RTP bridge (link)"
      capture.props = {{
        node.name = "bt-bridge-capture"
        stream.dont-remix = true
      }}
      playback.props = {{
        node.name = "bt-bridge-playback"
        target.object = "rtp-bridge"
        stream.dont-remix = true
      }}
    }}
  }}
]
"""


def wp_lua_dropin() -> str:
    return f"""\
-- {MANAGED_MARKER.lstrip('# ')}
-- WirePlumber 0.4.x (Lua). Make the incoming A2DP source the *preferred*
-- capture source (high priority.session, so it outranks the RTP sink's monitor
-- and the default-following loopback capture binds to the phone). Also expose
-- it as a capturable input and keep it from suspending mid-stream.
bluez_monitor = bluez_monitor or {{}}
bluez_monitor.rules = bluez_monitor.rules or {{}}
table.insert(bluez_monitor.rules, {{
  matches = {{
    {{ {{ "node.name", "matches", "bluez_input.*" }} }},
  }},
  apply_properties = {{
    ["bluez5.media-source-role"] = "input",
    ["priority.session"] = 3000,
    ["session.suspend-timeout-seconds"] = 0,
  }},
}})
"""


def wp_conf_dropin() -> str:
    return f"""\
{MANAGED_MARKER}
# WirePlumber 0.5+ (SPA-JSON).
#
# CRITICAL for headless: WirePlumber 0.5 added "seat monitoring" — it only
# manages Bluetooth for the user on the *active login seat*. A lingering,
# headless user session (this appliance) has no seat, so the bluez monitor
# loads but registers NO A2DP sink endpoint: the adapter never advertises Audio
# Sink (0000110b) and phones see a paired device with no audio service. (WP 0.4
# had no such gate, which is why bookworm "just worked".) Disabling seat
# monitoring + logind for the `main` profile makes WP manage Bluetooth for the
# lingering session. Verified on Raspberry Pi OS Trixie (WP 0.5.8, BlueZ 5.82).
wireplumber.profiles = {{
  main = {{
    monitor.bluez.seat-monitoring = disabled
    support.logind = disabled
  }}
}}

# Make the incoming A2DP source the *preferred* capture source: a high
# priority.session so it outranks the RTP sink's own monitor, so the loopback's
# (default-following) capture binds to the phone whenever one is connected and
# re-binds the same way on reconnect/re-pair. Also expose it as a capturable
# input and keep it from suspending mid-stream.
monitor.bluez.rules = [
  {{
    matches = [ {{ node.name = "~bluez_input.*" }} ]
    actions = {{
      update-props = {{
        bluez5.media-source-role = "input"
        priority.session = 3000
        session.suspend-timeout-seconds = 0
      }}
    }}
  }}
]
"""


def write_pipewire_config(home: str, host: str, port: int, fmt: str, rate: int, channels: int) -> None:
    print("== Writing PipeWire drop-in ==")
    path = os.path.join(home, ".config/pipewire/pipewire.conf.d", PW_DROPIN_NAME)
    user_write(path, pw_dropin(host, port, fmt, rate, channels))


def write_wireplumber_config(home: str) -> None:
    print("== Writing WirePlumber drop-in ==")
    major, minor = wireplumber_major_minor()
    if (major, minor) >= (0, 5):
        path = os.path.join(home, ".config/wireplumber/wireplumber.conf.d", WP_CONF_DROPIN_NAME)
        user_write(path, wp_conf_dropin())
    else:
        path = os.path.join(home, ".config/wireplumber/bluetooth.lua.d", WP_LUA_DROPIN_NAME)
        user_write(path, wp_lua_dropin())


def restart_services() -> None:
    print("== (Re)starting PipeWire user services ==")
    systemctl_user("daemon-reload", check=False)
    systemctl_user("enable", "--now", "pipewire", "wireplumber", check=False)
    systemctl_user("restart", "pipewire", "wireplumber")


def disable(home: str) -> None:
    print("== Removing bridge configuration ==")
    for rel in (
        f".config/pipewire/pipewire.conf.d/{PW_DROPIN_NAME}",
        f".config/wireplumber/bluetooth.lua.d/{WP_LUA_DROPIN_NAME}",
        f".config/wireplumber/wireplumber.conf.d/{WP_CONF_DROPIN_NAME}",
    ):
        user_remove(os.path.join(home, rel))
    remove_mdns_advert()
    remove_metadata_reporter()
    sudo(["systemctl", "disable", "--now", AGENT_UNIT], check=False)
    sudo(["systemctl", "disable", "--now", POWERSAVE_UNIT], check=False)
    sudo(["rm", "-f", f"/etc/systemd/system/{AGENT_UNIT}", f"/etc/systemd/system/{POWERSAVE_UNIT}", NM_POWERSAVE_CONF], check=False)
    sudo(["systemctl", "daemon-reload"], check=False)
    restart_services()
    print("Bridge config removed. Packages and BlueZ settings were left in place.")


def verify(host: str, port: int, *, diag_port: int | None = None) -> None:
    print("\n== Verification ==")
    env = dict(os.environ, XDG_RUNTIME_DIR=f"/run/user/{os.getuid()}")
    r = subprocess.run(["pw-cli", "ls", "Node"], text=True, capture_output=True, env=env)
    loaded = "rtp-bridge" in r.stdout
    print(f"  rtp-bridge sink present: {'YES' if loaded else 'NO'}")
    r = subprocess.run(["systemctl", "is-active", METADATA_UNIT], text=True, capture_output=True)
    active = r.stdout.strip() == "active"
    installed = os.path.exists(f"/etc/systemd/system/{METADATA_UNIT}")
    if installed:
        print(f"  metadata reporter:       {'RUNNING' if active else 'NOT RUNNING'} ({METADATA_UNIT})")
    if diag_port is not None:
        advertised = os.path.exists(AVAHI_SERVICE_FILE)
        print(f"  mDNS advert installed:   {'YES' if advertised else 'NO'} "
              f"({AVAHI_SERVICE_TYPE}, port {diag_port})")
    print(
        "\nNext:\n"
        f"  1. Pair a phone: it should see this Pi as a Bluetooth speaker and pair\n"
        f"     with no PIN. (Discoverable is on; `bluetoothctl devices` lists it.)\n"
        f"  2. Play music on the phone, then on the Pi check the live link:\n"
        f"       XDG_RUNTIME_DIR=/run/user/$(id -u) pw-link -l | grep bt-bridge\n"
        f"     You should see bluez_input.* -> bt-bridge-capture and\n"
        f"     bt-bridge-playback -> rtp-bridge.\n"
        f"  3. On the add-on, enable the RTP source on port {port} and confirm the\n"
        f"     `bt-bridge-rtp` node shows signal. RTP is being sent to {host}:{port}.\n"
    )
    if diag_port is not None:
        print(
            f"  4. The add-on's Sources tab should now *discover* this bridge and offer\n"
            f"     to add it as an RTP source. For the 'Show diagnostics' link there,\n"
            f"     run the testing app on this Pi (it serves port {diag_port}):\n"
            f"       firmware/pi-bridge/bluetooth-testing-app/  ->  python3 app.py\n"
        )


# --- Main --------------------------------------------------------------------


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", help="Add-on / HA host IP the RTP stream is sent to "
                                   "(or an IPv4 multicast group like 239.255.42.42).")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT, help=f"UDP port (default {DEFAULT_PORT}).")
    ap.add_argument("--name", help="Bluetooth device name shown to phones (optional; "
                                   "leaves the existing name if omitted).")
    ap.add_argument("--format", default=DEFAULT_FORMAT, help=f"RTP audio format (default {DEFAULT_FORMAT}).")
    ap.add_argument("--rate", type=int, default=DEFAULT_RATE, help=f"Sample rate (default {DEFAULT_RATE}).")
    ap.add_argument("--channels", type=int, default=DEFAULT_CHANNELS, help=f"Channels (default {DEFAULT_CHANNELS}).")
    ap.add_argument("--diag-port", type=int, default=DEFAULT_DIAG_PORT,
                    help=f"Port the bluetooth-testing-app serves on, advertised over mDNS "
                         f"(default {DEFAULT_DIAG_PORT}).")
    ap.add_argument("--api-port", type=int, default=DEFAULT_API_PORT,
                    help=f"Add-on HTTP API port, used only by the AVRCP metadata reporter "
                         f"(default {DEFAULT_API_PORT}).")
    ap.add_argument("--no-metadata", action="store_true",
                    help="Don't install the AVRCP metadata reporter (no track info in Home "
                         "Assistant; the audio path is unaffected).")
    ap.add_argument("--no-mdns", action="store_true",
                    help="Don't advertise this bridge over mDNS (the add-on then can't "
                         "auto-discover it, and shows no diagnostics link).")
    ap.add_argument("--disable", action="store_true", help="Remove the bridge configuration and exit.")
    args = ap.parse_args()

    preflight()
    home = pwd.getpwuid(os.getuid()).pw_dir
    user = pwd.getpwuid(os.getuid()).pw_name

    if args.disable:
        disable(home)
        return

    if not args.host:
        ap.error("--host is required (the add-on / HA host IP or a multicast group)")

    print(f"Configuring Bluetooth -> RTP bridge as user '{user}' -> {args.host}:{args.port}\n")
    ensure_packages()
    ensure_linger(user)
    disable_wifi_powersave()
    configure_bluez(args.name)
    install_pairing_agent()
    if args.no_metadata:
        remove_metadata_reporter()  # idempotent: also removes a previously-installed one
    else:
        install_metadata_reporter(host=args.host, api_port=args.api_port, rtp_port=args.port)
    write_pipewire_config(home, args.host, args.port, args.format, args.rate, args.channels)
    write_wireplumber_config(home)
    if args.no_mdns:
        remove_mdns_advert()  # idempotent: also un-advertises a previously-set-up bridge
    else:
        publish_mdns_advert(name=args.name, host=args.host, port=args.port, fmt=args.format,
                            rate=args.rate, channels=args.channels, diag_port=args.diag_port)
    restart_services()
    verify(args.host, args.port, diag_port=None if args.no_mdns else args.diag_port)


if __name__ == "__main__":
    main()
