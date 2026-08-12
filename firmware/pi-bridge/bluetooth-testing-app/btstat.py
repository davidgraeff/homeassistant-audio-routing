#!/usr/bin/env python3
"""BlueZ / kernel / RTP counters for the Bluetooth testing app.

These are the signals that actually resolved the 2026-07-28 "audio disappears
every few minutes" investigation, so they are first-class here rather than
afterthoughts (see ../../../pipewire_audio_router/docs/rtp-input-dropouts.md):

- **A2DP byte rate** (`hciconfig hci0 -a` RX counters). The decisive measurement:
  during a dropout the phone kept sending at *full* aptX rate (~46 kB/s) while
  the decoded audio was pure digital silence. Rate alone says "the link is
  alive"; only combining it with the waveform tells you whether the bytes carry
  anything. Both are on screen together for exactly that reason.
- **`MediaTransport1.State`** over D-Bus — `"active"` throughout the dropouts,
  which is how "the transport was never suspended" was established.
- **UDP OutDatagrams** from `/proc/net/snmp` — the Pi's own count of datagrams
  handed to the stack. This is authoritative for "did we transmit this second?"
  in a way tcpdump on a WiFi TX path is not (it lies; see ../README.md).

Every reader degrades to `None` rather than raising: this box may lack
`hciconfig` (deprecated on some distros) or `busctl`, and the web UI must still
come up and show the parts it can.

The parsers are pure functions over captured text so they are unit-testable
off-device; only the thin `read_*` wrappers shell out.
"""

from __future__ import annotations

import re
import time
from dataclasses import dataclass

from pwctl import run

# --------------------------------------------------------------------------
# pure parsers
# --------------------------------------------------------------------------


def parse_hci_rx_tx(text: str) -> tuple[int | None, int | None]:
    """(rx_bytes, tx_bytes) from `hciconfig hci0 -a` output.

    The lines look like::

        RX bytes:1156847603 acl:1764508 sco:0 events:3502 errors:0
        TX bytes:182816 acl:4431 sco:0 commands:490 errors:0

    Note the value is glued to the label with a colon and no space, which is why
    this is a regex and not a field split (an early version of this parser took
    `$1` and got the string "RX").
    """
    rx = tx = None
    m = re.search(r"RX bytes:(\d+)", text)
    if m:
        rx = int(m.group(1))
    m = re.search(r"TX bytes:(\d+)", text)
    if m:
        tx = int(m.group(1))
    return rx, tx


def parse_udp_out_datagrams(snmp: str) -> int | None:
    """`Udp: ... OutDatagrams ...` value from /proc/net/snmp.

    The file gives a header line of names then a values line, both prefixed
    `Udp:`, so the column index has to be looked up rather than hard-coded.
    """
    header = values = None
    for line in snmp.splitlines():
        if not line.startswith("Udp:"):
            continue
        if header is None:
            header = line.split()
        else:
            values = line.split()
            break
    if not header or not values or "OutDatagrams" not in header:
        return None
    i = header.index("OutDatagrams")
    try:
        return int(values[i])
    except (IndexError, ValueError):
        return None


#: A MediaTransport path. BlueZ puts the `fdN` object in **either** of two places
#: depending on version/state — both observed on this bridge (BlueZ 5.82):
#:
#:     /org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/fd13        (seen 2026-07-28, earlier)
#:     /org/bluez/hci0/dev_64_B5_F2_F9_A9_4A/sep3/fd1    (seen 2026-07-28, later)
#:
#: so the `/sepN` segment is optional. Matching only the first shape made the
#: transport row silently read "none" on a perfectly healthy link.
_TRANSPORT_RE = re.compile(
    r"/org/bluez/hci\d+/dev(?:_[0-9A-Fa-f]{2}){6}(?:/sep\d+)?/fd\d+"
)


def parse_transport_paths(tree: str) -> list[str]:
    """MediaTransport object paths (`.../fdN`) from a `busctl tree org.bluez` dump."""
    return sorted(set(_TRANSPORT_RE.findall(tree)))


def parse_busctl_scalar(text: str) -> str | None:
    """Value out of `busctl get-property` output, e.g. `s "active"` -> `active`.

    Handles the string (`s "x"`), byte (`y 255`) and boolean (`b true`) forms
    this module asks for.
    """
    text = text.strip()
    if not text:
        return None
    parts = text.split(None, 1)
    if len(parts) != 2:
        return None
    val = parts[1].strip()
    if val.startswith('"') and val.endswith('"'):
        return val[1:-1]
    return val


# --------------------------------------------------------------------------
# readers
# --------------------------------------------------------------------------


def read_hci_bytes(dev: str = "hci0") -> tuple[int | None, int | None]:
    rc, out, _ = run(["hciconfig", dev, "-a"])
    if rc != 0:
        return None, None
    return parse_hci_rx_tx(out)


def read_udp_out_datagrams() -> int | None:
    try:
        with open("/proc/net/snmp") as fh:
            return parse_udp_out_datagrams(fh.read())
    except OSError:
        return None


def read_transports() -> list[dict]:
    """Every A2DP MediaTransport with its State and Codec."""
    rc, out, _ = run(["busctl", "--system", "tree", "org.bluez"], timeout=8.0)
    if rc != 0:
        return []
    result = []
    for path in parse_transport_paths(out):
        entry = {"path": path, "state": None, "codec": None, "device": None}
        m = re.search(r"dev_((?:[0-9A-Fa-f]{2}_){5}[0-9A-Fa-f]{2})", path)
        if m:
            entry["device"] = m.group(1).replace("_", ":")
        for prop, key in (("State", "state"), ("Codec", "codec")):
            rc2, out2, _ = run(
                ["busctl", "--system", "get-property", "org.bluez", path,
                 "org.bluez.MediaTransport1", prop],
                timeout=6.0,
            )
            if rc2 == 0:
                entry[key] = parse_busctl_scalar(out2)
        result.append(entry)
    return result


def read_connected_devices() -> list[dict]:
    """Connected BT devices, from `bluetoothctl devices Connected`."""
    rc, out, _ = run(["bluetoothctl", "devices", "Connected"], timeout=8.0)
    if rc != 0:
        return []
    devices = []
    for line in out.splitlines():
        m = re.match(r"Device\s+((?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2})\s+(.*)", line.strip())
        if m:
            devices.append({"address": m.group(1), "name": m.group(2).strip()})
    return devices


def read_link_quality(address: str) -> int | None:
    """`hcitool lq` — 0..255, lower means a worse radio link.

    Needs privileges on most setups; returns None rather than complaining if it
    is not permitted, since it is a nice-to-have next to the byte rate.
    """
    rc, out, _ = run(["hcitool", "lq", address], timeout=6.0)
    if rc != 0:
        return None
    m = re.search(r"Link quality:\s*(\d+)", out)
    return int(m.group(1)) if m else None


def disconnect(address: str) -> tuple[bool, str]:
    """`bluetoothctl disconnect` — one half of a renegotiation."""
    rc, out, err = run(["bluetoothctl", "disconnect", address], timeout=15.0)
    if rc != 0:
        return False, (err or out or "disconnect failed").strip()
    time.sleep(1.0)  # let BlueZ tear the transport down before anything else
    return True, f"disconnected {address}"


def connect(address: str) -> tuple[bool, str]:
    """`bluetoothctl connect` — the other half; renegotiates the A2DP stream."""
    rc, out, err = run(["bluetoothctl", "connect", address], timeout=25.0)
    if rc != 0:
        return False, (err or out or "connect failed").strip()
    return True, f"connected {address}"


def reconnect(address: str) -> tuple[bool, str]:
    """Disconnect + reconnect one device, to force A2DP renegotiation.

    This is the gentle way to apply a codec change: it renegotiates the stream
    without restarting PipeWire (which, with a phone connected, orphans the
    loopback and wedges the bridge).
    """
    rc, out, err = run(["bluetoothctl", "disconnect", address], timeout=15.0)
    if rc != 0:
        return False, (err or out or "disconnect failed").strip()
    time.sleep(2.0)
    rc, out, err = run(["bluetoothctl", "connect", address], timeout=20.0)
    if rc != 0:
        return False, (
            f"disconnected, but reconnect failed ({(err or out).strip()}). "
            "Reconnect from the phone."
        )
    return True, f"reconnected {address}; the codec is renegotiated on this new stream"


# --------------------------------------------------------------------------
# rate tracking
# --------------------------------------------------------------------------


@dataclass
class Rate:
    """Turns a monotonically-increasing counter into a per-second rate.

    Keeps the previous sample so the first call yields `None` instead of a
    meaningless spike from a zero baseline.
    """

    prev_value: int | None = None
    prev_time: float | None = None

    def update(self, value: int | None, now: float | None = None) -> float | None:
        now = now if now is not None else time.monotonic()
        if value is None:
            self.prev_value, self.prev_time = None, None
            return None
        rate = None
        if self.prev_value is not None and self.prev_time is not None:
            dt = now - self.prev_time
            dv = value - self.prev_value
            # A negative delta means the counter reset (adapter re-init); skip it.
            if dt > 0 and dv >= 0:
                rate = dv / dt
        self.prev_value, self.prev_time = value, now
        return rate
