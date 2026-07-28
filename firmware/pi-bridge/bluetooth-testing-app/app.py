#!/usr/bin/env python3
"""Bluetooth testing app — a web console for the Pi A2DP -> RTP bridge.

Serves one page that shows, live:

- a **waveform envelope of the last 15 s** of the incoming Bluetooth audio, with
  digital silence called out explicitly (peak == 0), a streak timer and an
  episode log;
- the **A2DP codec** in use, with controls to change it;
- the **RTP sender / PipeWire node state** and the phone -> loopback -> sender
  chain, plus the transmit rate from the Pi's own UDP counters.

### Why stdlib-only, and why SSE rather than WebSocket

The target is a Raspberry Pi Zero 2 W that is simultaneously running the bridge.
There is no venv, no pip install and no build step: `scp` the directory over and
run it. The live feed only ever flows server -> browser, which is exactly what
Server-Sent Events do, and SSE needs no framing library — so `http.server` plus a
long-lived `text/event-stream` response covers it with nothing outside the stdlib.

Run:  python3 app.py [--host 0.0.0.0] [--port 8080] [--target <pw-node>]
"""

from __future__ import annotations

import argparse
import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

import btstat
import pwctl
from capture import STREAM_NODE, Capture

HERE = os.path.dirname(os.path.abspath(__file__))
STATIC = os.path.join(HERE, "static")

#: How often the SSE loop pushes. 5 Hz is smooth for a scrolling waveform while
#: staying light: each frame carries only the envelope points added since the
#: browser's cursor (~10 points), not the whole ring.
STREAM_HZ = 5.0
#: The PipeWire graph changes far more slowly than audio; re-dumping it at stream
#: rate would waste a Zero 2 W's CPU on JSON parsing, so it gets its own cadence.
GRAPH_REFRESH_S = 2.0
#: How often the binding watch re-checks that the capture is on its target. It
#: must not depend on a browser being open — the whole point of the check is that
#: unattended measurements can be trusted — so it runs in its own thread and
#: reuses the graph cache, i.e. it costs no extra `pw-dump`.
BINDING_WATCH_S = 2.5
#: How long the capture may sit unbound *while its target exists* before the child
#: process is replaced. A pinned stream never re-links itself, so this is the only
#: way back after the phone reconnects; a few seconds of grace keeps a normal
#: reconnect (target appears, WirePlumber links it) from triggering a needless one.
UNBOUND_RESPAWN_S = 6.0


def auto_target(current: str | None, capture_targets: list[dict],
                is_auto: bool) -> str | None:
    """The A2DP source to adopt, or `None` to stay put.

    The app picks the connected phone at startup, but a phone that connects *later*
    used to be ignored — leaving the capture on the `rtp-bridge` fallback, silently
    measuring the wrong node for the rest of the session. That is the same class of
    error as the rebind this app was just fixed for, so it moves.

    Two rules keep the move from becoming its own trap: it only ever moves *to* an
    A2DP source (never away from one, so a live measurement is not interrupted),
    and it never overrides a target the user chose by hand — `is_auto` is cleared
    by `POST /api/capture`. Switching resets the ring and the statistics, so no
    reading is ever attributed to the wrong node.
    """
    if not is_auto:
        return None
    sources = [t["node"] for t in capture_targets if t.get("kind") == "a2dp-source"]
    if not sources or current in sources:
        return None
    return sources[0]


def binding_decision(*, pipewire_ok: bool, fed_by: list[str], target: str | None,
                     node_names: list[str], unbound_since: float | None, now: float,
                     grace_s: float = UNBOUND_RESPAWN_S) -> tuple[bool | None, float | None, bool]:
    """`(bound, unbound_since, respawn)` — pure, so the recovery rule is testable.

    `bound` is `None` when PipeWire could not be read: "unknown" must not be
    reported as "not attached", or a momentary `pw-dump` failure would throw away
    perfectly good blocks as untrusted.

    A respawn is proposed only when the capture is unbound *and* its target is in
    the graph, after `grace_s` of that state — see `UNBOUND_RESPAWN_S`.
    """
    if not pipewire_ok:
        return None, None, False
    bound = bool(target) and fed_by == [target]
    if bound or not target or target not in node_names:
        return bound, None, False
    if unbound_since is None:
        return bound, now, False
    if now - unbound_since >= grace_s:
        return bound, None, True
    return bound, unbound_since, False


class GraphCache:
    """Throttled, thread-safe `pw-dump` snapshot shared by all requests.

    `pw-dump` on a Zero 2 W is not free and every SSE client would otherwise
    trigger its own, so one cache serves them all.
    """

    def __init__(self, ttl: float = GRAPH_REFRESH_S) -> None:
        self._ttl = ttl
        self._lock = threading.Lock()
        self._at = 0.0
        self._value: dict = {}

    def get(self, force: bool = False) -> dict:
        with self._lock:
            fresh = time.monotonic() - self._at < self._ttl
            if fresh and not force and self._value:
                return self._value
        value = self._build()
        with self._lock:
            self._value, self._at = value, time.monotonic()
        return value

    @staticmethod
    def _build() -> dict:
        dump = pwctl.pw_dump()
        nodes = pwctl.parse_nodes(dump)
        links = pwctl.parse_links(dump)
        devices = pwctl.parse_bluez_devices(dump)
        return {
            "pipewire_ok": bool(dump),
            "chain": pwctl.sender_chain(nodes, links),
            # What our own capture stream is wired to. The target is not known
            # here, so the comparison happens in App.refresh_binding.
            "capture_fed_by": pwctl.feeding_nodes(links, STREAM_NODE),
            "capture_present": any(l["input_node"] == STREAM_NODE for l in links),
            # Whether a target still exists at all — the respawn decision needs it.
            "node_names": [n.name for n in nodes],
            "capture_targets": pwctl.capture_targets(nodes),
            "levers": pwctl.available_levers(devices),
            "devices": [
                {
                    "id": d.id,
                    "name": d.name,
                    "alias": d.alias,
                    "address": d.address,
                    "current_profile": d.current_profile,
                    "codec_profiles": [
                        {"index": p.index, "name": p.name, "codec": p.codec,
                         "description": p.description, "available": p.available}
                        for p in d.codec_profiles
                    ],
                }
                for d in devices
            ],
            "allowed_codecs": pwctl.read_codec_dropin(),
            "known_codecs": list(pwctl.KNOWN_CODECS),
        }


class Counters:
    """A2DP + RTP byte/packet rates, sampled on the stream cadence."""

    def __init__(self) -> None:
        self.a2dp = btstat.Rate()
        self.udp = btstat.Rate()
        self._lock = threading.Lock()
        self._bt_at = 0.0
        self._bt: list[dict] = []

    def sample(self) -> dict:
        rx, _ = btstat.read_hci_bytes()
        a2dp_rate = self.a2dp.update(rx)
        udp_rate = self.udp.update(btstat.read_udp_out_datagrams())
        # D-Bus round-trips are comparatively slow; refresh them rarely.
        with self._lock:
            stale = time.monotonic() - self._bt_at > 5.0
        if stale:
            transports = btstat.read_transports()
            connected = btstat.read_connected_devices()
            for d in connected:
                d["link_quality"] = btstat.read_link_quality(d["address"])
            with self._lock:
                self._bt = [{"transports": transports, "connected": connected}]
                self._bt_at = time.monotonic()
        with self._lock:
            bt = self._bt[0] if self._bt else {"transports": [], "connected": []}
        return {
            "a2dp_kb_s": round(a2dp_rate / 1024, 1) if a2dp_rate is not None else None,
            "udp_pkts_s": round(udp_rate, 1) if udp_rate is not None else None,
            "hci_available": rx is not None,
            **bt,
        }


class Handler(BaseHTTPRequestHandler):
    server_version = "bt-testing-app"
    protocol_version = "HTTP/1.1"

    # -- plumbing --------------------------------------------------------

    def log_message(self, fmt: str, *args) -> None:
        if self.server.verbose:  # type: ignore[attr-defined]
            super().log_message(fmt, *args)

    def _send_json(self, obj, code: int = 200) -> None:
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _send_file(self, name: str) -> None:
        path = os.path.join(STATIC, name)
        # Defensive: never serve outside static/, even though routes are fixed.
        if not os.path.abspath(path).startswith(STATIC) or not os.path.isfile(path):
            self._send_json({"error": "not found"}, 404)
            return
        with open(path, "rb") as fh:
            body = fh.read()
        ctype = "text/html; charset=utf-8" if name.endswith(".html") else "application/octet-stream"
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _body(self) -> dict:
        try:
            n = int(self.headers.get("Content-Length") or 0)
            return json.loads(self.rfile.read(n) or b"{}")
        except (ValueError, json.JSONDecodeError):
            return {}

    # -- routes ----------------------------------------------------------

    def do_GET(self) -> None:  # noqa: N802
        u = urlparse(self.path)
        if u.path in ("/", "/index.html"):
            return self._send_file("index.html")
        if u.path == "/api/state":
            return self._send_json(self._state())
        if u.path == "/api/stream":
            return self._stream()
        self._send_json({"error": "not found"}, 404)

    def do_POST(self) -> None:  # noqa: N802
        u = urlparse(self.path)
        app = self.server  # type: ignore[assignment]
        body = self._body()

        if u.path == "/api/capture":
            target = body.get("target") or None
            # A hand-picked target wins over auto-adoption from here on: the app
            # must not move the measurement out from under whoever set it.
            app.target_is_auto = target is None
            app.capture.set_target(target)
            return self._send_json({"ok": True, "target": target,
                                    "auto": app.target_is_auto})

        if u.path == "/api/codec/profile":
            try:
                dev, idx = int(body["device_id"]), int(body["profile_index"])
            except (KeyError, TypeError, ValueError):
                return self._send_json({"ok": False, "message": "device_id and profile_index required"}, 400)
            ok, msg = pwctl.set_profile(dev, idx)
            app.graph.get(force=True)
            return self._send_json({"ok": ok, "message": msg})

        if u.path == "/api/codec/pin":
            # An allow-*set*: `bluez5.codecs` is a list, and excluding one codec
            # means allowing all the others. A bare `codec` string is accepted
            # too, as a one-element set.
            codecs = body.get("codecs", body.get("codec"))
            if codecs is not None and not isinstance(codecs, (list, tuple, str)):
                return self._send_json(
                    {"ok": False, "message": "codecs must be a list (or omitted to allow all)"}, 400)
            ok, msg = pwctl.write_codec_dropin(codecs)
            app.graph.get(force=True)
            return self._send_json({"ok": ok, "message": msg}, 200 if ok else 400)

        if u.path == "/api/codec/apply":
            # Write the allow-list AND renegotiate: disconnect -> restart
            # WirePlumber -> reconnect. That ordering is the point (see
            # pwctl.apply_codecs); the drop-in is inert without a restart, and
            # restarting with a phone connected orphans the bridge's loopback.
            codecs = body.get("codecs", body.get("codec"))
            if codecs is not None and not isinstance(codecs, (list, tuple, str)):
                return self._send_json(
                    {"ok": False, "message": "codecs must be a list (or omitted to allow all)"}, 400)
            address = body.get("address") or self._first_bt_address()
            ok, steps = pwctl.apply_codecs(codecs, address,
                                           btstat.disconnect, btstat.connect)
            app.graph.get(force=True)
            return self._send_json({"ok": ok, "message": " · ".join(steps), "steps": steps},
                                   200 if ok else 400)

        if u.path == "/api/reconnect":
            addr = body.get("address")
            if not addr:
                return self._send_json({"ok": False, "message": "address required"}, 400)
            ok, msg = btstat.reconnect(addr)
            app.graph.get(force=True)
            return self._send_json({"ok": ok, "message": msg})

        self._send_json({"error": "not found"}, 404)

    def _first_bt_address(self) -> str | None:
        """The connected phone's address, so the UI needn't pass one."""
        graph = self.server.graph.get()  # type: ignore[attr-defined]
        for s in (graph.get("chain") or {}).get("a2dp_sources") or []:
            if s.get("address"):
                return s["address"]
        for d in graph.get("devices") or []:
            if d.get("address"):
                return d["address"]
        return None

    # -- payloads --------------------------------------------------------

    def _state(self, since: int | None = None) -> dict:
        app = self.server  # type: ignore[assignment]
        return {
            "audio": app.capture.snapshot(since),
            "graph": app.graph.get(),
            "counters": app.counters.sample(),
            "now": time.time(),
        }

    def _stream(self) -> None:
        """SSE: one `data:` frame per tick with only the new envelope points."""
        app = self.server  # type: ignore[assignment]
        q = parse_qs(urlparse(self.path).query)
        try:
            since = int(q.get("since", ["0"])[0])
        except ValueError:
            since = 0

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.end_headers()
        # HTTP/1.1 without Content-Length: close-delimited, so no chunking needed.
        self.close_connection = True

        interval = 1.0 / STREAM_HZ
        try:
            while not app.stopping.is_set():
                payload = self._state(since)
                since = payload["audio"]["cursor"]
                self.wfile.write(b"data: " + json.dumps(payload).encode() + b"\n\n")
                self.wfile.flush()
                time.sleep(interval)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass  # browser navigated away; nothing to clean up


class App(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr, target: str | None, verbose: bool) -> None:
        super().__init__(addr, Handler)
        self.verbose = verbose
        self.stopping = threading.Event()
        self.graph = GraphCache()
        self.counters = Counters()
        self.capture = Capture(target)
        self.capture.start()
        # An explicit --target is the user's choice; auto-adoption must not fight it.
        self.target_is_auto = target is None
        self._unbound_since: float | None = None
        self._watch = threading.Thread(target=self._binding_watch, name="binding-watch",
                                       daemon=True)
        self._watch.start()

    def refresh_binding(self) -> None:
        """Tell the capture whether its stream is really on the target node.

        `pw-record --target` is only a preference: if the target disappears,
        PipeWire rebinds the stream to the default source — on this bridge
        `rtp-bridge:monitor`, which is silent — and every reading after that is
        about the wrong node. The capture sets `node.dont-reconnect` to prevent it;
        this is the independent check, and while it says `False` the capture marks
        its blocks untrusted instead of logging silence episodes.

        It is also the recovery path. `dont-reconnect` leaves a stream whose target
        vanished **unlinked and alive** — verified on the bridge — so once the
        target is back in the graph the child has to be replaced or the app stays
        blind for the rest of the session.
        """
        graph = self.graph.get()
        if graph.get("pipewire_ok"):
            adopt = auto_target(self.capture.target, graph.get("capture_targets") or [],
                                self.target_is_auto)
            if adopt:
                print(f"visualizing: {adopt} (A2DP source appeared)", flush=True)
                self.capture.set_target(adopt)
                self._unbound_since = None
                return  # let the next tick judge the new binding
        fed_by = graph.get("capture_fed_by") or []
        bound, self._unbound_since, respawn = binding_decision(
            pipewire_ok=bool(graph.get("pipewire_ok")),
            fed_by=fed_by,
            target=self.capture.target,
            node_names=graph.get("node_names") or [],
            unbound_since=self._unbound_since,
            now=time.monotonic(),
        )
        self.capture.set_binding(bound, fed_by)
        if respawn:
            self.capture.restart()

    def _binding_watch(self) -> None:
        while not self.stopping.wait(BINDING_WATCH_S):
            try:
                self.refresh_binding()
            except Exception:  # pragma: no cover - a diagnostic must not die
                pass

    def pick_default_target(self) -> str | None:
        """Default the waveform to the first connected phone, else the sender.

        Chosen deliberately: the A2DP source is where a "silent audio" fault is
        visible *before* anything in this bridge can be blamed for it.
        """
        targets = self.graph.get(force=True).get("capture_targets") or []
        for t in targets:
            if t["kind"] == "a2dp-source":
                return t["node"]
        return targets[0]["node"] if targets else None


def main() -> None:
    ap = argparse.ArgumentParser(description="Bluetooth/RTP bridge testing web app")
    ap.add_argument("--host", default="0.0.0.0", help="bind address (default: all)")
    ap.add_argument("--port", type=int, default=8080, help="TCP port (default: 8080)")
    ap.add_argument("--target", default=None,
                    help="PipeWire node to visualize (default: the connected phone)")
    ap.add_argument("--verbose", action="store_true", help="log every HTTP request")
    args = ap.parse_args()

    app = App((args.host, args.port), args.target, args.verbose)
    if not args.target:
        chosen = app.pick_default_target()
        if chosen:
            app.capture.set_target(chosen)
            print(f"visualizing: {chosen}")
        else:
            print("no capture target found yet — connect a phone, then pick one in the UI")

    print(f"http://{args.host}:{args.port}/  (Ctrl-C to stop)")
    try:
        app.serve_forever()
    except KeyboardInterrupt:
        print("\nstopping")
    finally:
        app.stopping.set()
        app.capture.stop()
        app.server_close()


if __name__ == "__main__":
    main()
