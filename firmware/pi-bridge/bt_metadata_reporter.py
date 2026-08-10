#!/usr/bin/env python3
"""Report the connected phone's AVRCP track metadata to the audio-router add-on.

The Bluetooth bridge already carries the *audio* to the add-on over RTP. The
phone also sends **AVRCP metadata** — title, artist, album, duration, play state
— which BlueZ exposes on the system D-Bus and which nothing read until now. This
service forwards it, so Home Assistant can show what is playing on whatever
outputs the bridge's source is routed to.

See `docs/source-metadata-plan.md` (WP3). Two facts shape this file:

* **The metadata is on D-Bus, not in PipeWire.** `org.bluez.MediaPlayer1` carries
  `Track` (`Title`/`Artist`/`Album`/`Duration`/`TrackNumber`), `Status` and
  `Position`. PipeWire sees PCM frames on an A2DP transport and nothing else, so
  no amount of RTP plumbing could have carried this.
* **The player object is transient.** `/org/bluez/hciX/dev_.../playerN` exists
  only while a phone with an AVRCP target is connected. So this watches
  `InterfacesAdded`/`InterfacesRemoved` on the object manager rather than assuming
  a path, and re-scans on start (a phone may already be connected).

Deliberately *not* here: cover art. AVRCP cover art is the 1.6 BIP/OBEX feature
and BlueZ does not expose it, so Bluetooth tracks simply have no artwork.

The add-on is told "the sender on RTP port N is playing X" — this host never needs
to know the source ids the add-on assigned (`POST /api/now_playing/report`).

Runs as a systemd system service installed by `setup_pi_bridge.py`; run it by hand
with `--once` to see one snapshot and exit, which is the quickest way to tell
whether BlueZ is exposing a player at all.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request

BLUEZ = "org.bluez"
PLAYER_IFACE = "org.bluez.MediaPlayer1"
OBJECT_MANAGER = "org.freedesktop.DBus.ObjectManager"
PROPERTIES_IFACE = "org.freedesktop.DBus.Properties"

#: How long an HTTP report may take before we give up on it. Short: a wedged
#: add-on must not stall the D-Bus main loop, and a missed metadata update is
#: worth nothing — the next one is seconds away.
HTTP_TIMEOUT_S = 4

#: BlueZ `Status` values mapped onto the add-on's playback states. Anything else
#: (`forward-seek`, `reverse-seek`, `error`) is still *playing* as far as a
#: listener is concerned.
STATUS_MAP = {"playing": "playing", "paused": "paused", "stopped": "stopped"}


def log(message: str) -> None:
    """One-line stderr log. systemd journals it; no logging config needed."""
    print(message, file=sys.stderr, flush=True)


class Reporter:
    """Posts metadata to the add-on, skipping reports that say nothing new.

    The dedupe is not just politeness: BlueZ re-emits `PropertiesChanged` for
    `Position` while a track plays, and every identical POST would be a wasted
    round trip on a Pi Zero's shared radio.
    """

    def __init__(self, base_url: str, rtp_port: int) -> None:
        self._url = f"{base_url.rstrip('/')}/api/now_playing/report"
        self._rtp_port = rtp_port
        self._last: dict | None = None

    def send(self, metadata: dict) -> None:
        """Report `metadata`, or clear the entry when it is empty."""
        if metadata == self._last:
            return
        self._last = metadata
        body = {"rtp_port": self._rtp_port, **metadata}
        req = urllib.request.Request(
            self._url,
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S) as resp:
                resp.read()
        except urllib.error.HTTPError as e:
            # A 404 is the expected, benign case: no RTP source is configured on
            # this port (yet). Say so once per change rather than retrying — the
            # add-on side is a human's one-time setup step.
            log(f"[report] {e.code} from add-on: {e.reason}"
                + (" — is an RTP source configured on this port?" if e.code == 404 else ""))
            self._last = None  # so the next identical update is retried
        except (urllib.error.URLError, OSError) as e:
            log(f"[report] could not reach add-on: {e}")
            self._last = None

    def clear(self) -> None:
        """Tell the add-on nothing is playing (an empty report means "clear")."""
        self.send({})


def metadata_from_player(props: dict) -> dict:
    """Translate one `MediaPlayer1` property dict into a report body.

    Only fields that are actually present are included: the add-on merges rather
    than replaces, and a `None` there would mean "no change", not "clear".
    """
    track = props.get("Track") or {}
    out: dict = {}
    if title := _text(track.get("Title")):
        out["title"] = title
    if artist := _text(track.get("Artist")):
        out["artist"] = artist
    if album := _text(track.get("Album")):
        out["album"] = album
    if (duration := track.get("Duration")) is not None:
        out["duration_ms"] = int(duration)
    if (position := props.get("Position")) is not None:
        out["position_ms"] = int(position)
    # A play state with no track behind it is not worth sending: it would create an
    # entry in the add-on that describes nothing, which every consumer then has to
    # filter out. "Nothing is playing" is said by clearing instead.
    if not any(k in out for k in ("title", "artist", "album")):
        return {}
    status = str(props.get("Status") or "").lower()
    if status:
        out["state"] = STATUS_MAP.get(status, "playing")
    return out


def _text(value) -> str | None:
    """A trimmed string, or None for absent/blank. BlueZ hands out empty strings
    for fields the phone did not send."""
    if value is None:
        return None
    text = str(value).strip()
    return text or None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", required=True, help="Add-on host (IP or name) to report to.")
    ap.add_argument("--api-port", type=int, default=8099, help="Add-on API port (default 8099).")
    ap.add_argument("--rtp-port", type=int, required=True,
                    help="The UDP port this bridge's RTP stream is sent to — how the add-on "
                         "knows which source these reports belong to.")
    ap.add_argument("--once", action="store_true",
                    help="Report the current state once and exit (no D-Bus main loop). The "
                         "quickest check that BlueZ is exposing a player at all.")
    args = ap.parse_args()

    try:
        import dbus
        import dbus.mainloop.glib
        from gi.repository import GLib
    except ImportError as e:
        log(f"missing D-Bus bindings ({e}). Install: sudo apt-get install -y python3-dbus python3-gi")
        return 2

    reporter = Reporter(f"http://{args.host}:{args.api_port}", args.rtp_port)
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SystemBus()

    def players() -> dict[str, dict]:
        """Every `MediaPlayer1` object BlueZ currently exposes, by path."""
        manager = dbus.Interface(bus.get_object(BLUEZ, "/"), OBJECT_MANAGER)
        found = {}
        for path, interfaces in manager.GetManagedObjects().items():
            if PLAYER_IFACE in interfaces:
                found[str(path)] = dict(interfaces[PLAYER_IFACE])
        return found

    def report_path(path: str) -> None:
        """Read one player's properties fresh and report them.

        Re-reading rather than trusting the signal's payload is deliberate: a
        `PropertiesChanged` carries only what changed, so a `Status`-only signal
        would otherwise drop the title.
        """
        try:
            props = dbus.Interface(bus.get_object(BLUEZ, path), PROPERTIES_IFACE).GetAll(PLAYER_IFACE)
        except dbus.DBusException as e:
            log(f"[dbus] could not read {path}: {e.get_dbus_name()}")
            return
        metadata = metadata_from_player(dict(props))
        if not metadata:
            return
        reporter.send(metadata)

    def on_properties_changed(interface, changed, invalidated, path=None):
        if str(interface) == PLAYER_IFACE and path:
            report_path(str(path))

    def on_interfaces_added(path, interfaces):
        if PLAYER_IFACE in interfaces:
            log(f"[dbus] player appeared: {path}")
            report_path(str(path))

    def on_interfaces_removed(path, interfaces):
        if PLAYER_IFACE in interfaces:
            log(f"[dbus] player gone: {path}")
            # The phone disconnected (or stopped its AVRCP session). Clear rather
            # than leave the add-on's TTL to collect it, so the Home Assistant
            # media card collapses instead of freezing on the last track.
            if not players():
                reporter.clear()

    current = players()
    if args.once:
        if not current:
            log("no org.bluez.MediaPlayer1 object — is a phone connected and playing?")
            return 1
        for path in current:
            log(f"[dbus] player: {path}")
            report_path(path)
        return 0

    bus.add_signal_receiver(
        on_properties_changed,
        dbus_interface=PROPERTIES_IFACE,
        signal_name="PropertiesChanged",
        path_keyword="path",
    )
    bus.add_signal_receiver(on_interfaces_added, dbus_interface=OBJECT_MANAGER, signal_name="InterfacesAdded")
    bus.add_signal_receiver(on_interfaces_removed, dbus_interface=OBJECT_MANAGER, signal_name="InterfacesRemoved")

    # A phone may already be connected when this starts (a service restart, a
    # reboot with the phone in range), so report what is there before waiting for
    # a signal that may not come for hours.
    for path in current:
        report_path(path)
    log(f"watching BlueZ for AVRCP metadata; reporting to {args.host}:{args.api_port} for RTP port {args.rtp_port}")
    GLib.MainLoop().run()
    return 0


if __name__ == "__main__":
    sys.exit(main())
