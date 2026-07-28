#!/usr/bin/env python3
"""PipeWire introspection + the A2DP codec levers, for the Bluetooth testing app.

Everything here is split into a **pure** parser that takes an already-fetched
`pw-dump` list and a thin shell wrapper that fetches it. That split exists so the
parsing is unit-testable off-device (see tests/), which matters because this tool
is aimed at a Pi Zero 2 W that is not always reachable.

### On "switching the codec" when the Pi is the A2DP *sink*

Worth stating plainly, because it is the single most confusing thing about this
tool: **the sender picks the codec.** The Pi is an A2DP *sink*; the phone is the
source. In A2DP the source chooses from the capabilities the sink advertises, so
the Pi cannot simply command "use SBC now". Two levers exist, and which one is
available depends on the PipeWire/WirePlumber build on the box:

1. **Per-codec device profiles.** Some PipeWire builds expose each negotiated
   codec as its own `EnumProfile` entry on the BlueZ device (names/descriptions
   containing `sbc`, `aac`, `aptx`, ...). Where that exists, selecting one with
   `wpctl set-profile` renegotiates in place — the cheap, no-restart path.
2. **Restrict the advertised codec set, then force renegotiation.** Write a
   PipeWire drop-in narrowing `bluez5.codecs` — an **allow-list** — and reconnect
   the device so it re-negotiates against the narrowed set. This always works.
   Because it is an allow-list, excluding a codec means writing every *other*
   codec, which is why the API takes a set rather than one name: "everything
   except aptX/aptX-HD" is the natural first experiment when a decoder is
   suspect, and it must not force a drop all the way to a single codec.

[`available_levers`][] reports which of the two this box actually offers, so the
UI can show the truth instead of guessing. **Measured on the bridge itself
(2026-07-28): lever 1 does not exist there.** The phone's device advertises only
`off` and `audio-gateway` — no per-codec profiles — so lever 2 is the only way to
change the codec on this Pi.

That makes lever 2's mechanics load-bearing, and they have one sharp edge:
WirePlumber reads config only at startup, so the drop-in needs a WirePlumber
restart — and restarting WirePlumber *while a phone is connected* orphans the
bridge's loopback and wedges the audio path (../README.md, "Don't restart
PipeWire/WirePlumber while a phone is connected"). [`apply_codecs`][] therefore
runs the one ordering that is safe: **disconnect the phone, restart WirePlumber,
reconnect**. The restart happens with nothing connected, so there is no loopback
to orphan, and the reconnect renegotiates against the narrowed set.
"""

from __future__ import annotations

import json
import os
import subprocess
from dataclasses import dataclass, field

#: Codecs the bluez5 SPA plugin may support, best-first for a "highest quality"
#: default. Only ones the box actually advertises are ever offered in the UI.
KNOWN_CODECS = ("ldac", "aptx_hd", "aptx", "aac", "sbc_xq", "sbc")

#: The RTP sender sink created by setup_pi_bridge.py's module-rtp-sink.
SENDER_NODE = "rtp-bridge"
#: The loopback that bridges the phone's audio into the sender.
LOOPBACK_CAPTURE = "bt-bridge-capture"
LOOPBACK_PLAYBACK = "bt-bridge-playback"


def _env() -> dict[str, str]:
    """PipeWire client env. `XDG_RUNTIME_DIR` is usually already right when run
    as the bridge user, but a systemd unit or an ssh command may lack it."""
    env = dict(os.environ)
    env.setdefault("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    return env


def run(cmd: list[str], *, timeout: float = 10.0) -> tuple[int, str, str]:
    """Run `cmd`, never raise. Returns (returncode, stdout, stderr).

    Deliberately non-raising: this is a diagnostic tool and a missing binary or a
    momentarily-unavailable PipeWire must degrade to "unknown" in the UI, not
    take the web server down.
    """
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, env=_env())
        return p.returncode, p.stdout, p.stderr
    except FileNotFoundError:
        return 127, "", f"{cmd[0]}: not found"
    except subprocess.TimeoutExpired:
        return 124, "", f"{cmd[0]}: timed out after {timeout}s"
    except OSError as e:  # pragma: no cover - defensive
        return 1, "", f"{cmd[0]}: {e}"


def pw_dump() -> list[dict]:
    """The full `pw-dump` object list, or `[]` if PipeWire is unreachable."""
    rc, out, _ = run(["pw-dump"], timeout=15.0)
    if rc != 0 or not out.strip():
        return []
    try:
        data = json.loads(out)
    except json.JSONDecodeError:
        return []
    return data if isinstance(data, list) else []


# --------------------------------------------------------------------------
# pure parsers -- these take a dump and never shell out, so tests can drive them
# --------------------------------------------------------------------------


@dataclass
class Profile:
    index: int
    name: str
    description: str
    available: str = "unknown"
    #: The codec this profile implies, if its name/description names one.
    codec: str | None = None


@dataclass
class BluezDevice:
    id: int
    name: str
    alias: str
    address: str
    current_profile: str | None = None
    profiles: list[Profile] = field(default_factory=list)

    @property
    def codec_profiles(self) -> list[Profile]:
        """Profiles that name a codec — i.e. lever 1 above, if non-empty."""
        return [p for p in self.profiles if p.codec]


def _props(obj: dict) -> dict:
    return (obj.get("info") or {}).get("props") or {}


def _params(obj: dict) -> dict:
    return (obj.get("info") or {}).get("params") or {}


def codec_of(text: str) -> str | None:
    """Which known codec a profile name/description refers to, if any.

    Longest-match-first so `aptx_hd` isn't shadowed by `aptx`, and `sbc_xq` isn't
    shadowed by `sbc`.
    """
    t = text.lower().replace("-", "_").replace(" ", "_")
    for c in sorted(KNOWN_CODECS, key=len, reverse=True):
        if c in t:
            return c
    return None


def parse_bluez_devices(dump: list[dict]) -> list[BluezDevice]:
    """Every `device.api == bluez5` Device in the dump, with its profiles."""
    out: list[BluezDevice] = []
    for obj in dump:
        if obj.get("type") != "PipeWire:Interface:Device":
            continue
        props = _props(obj)
        if props.get("device.api") != "bluez5":
            continue
        params = _params(obj)
        profiles: list[Profile] = []
        for p in params.get("EnumProfile") or []:
            if not isinstance(p, dict):
                continue
            name = str(p.get("name", ""))
            desc = str(p.get("description", ""))
            profiles.append(
                Profile(
                    index=int(p.get("index", -1)),
                    name=name,
                    description=desc,
                    available=str(p.get("available", "unknown")),
                    codec=codec_of(f"{name} {desc}"),
                )
            )
        cur = None
        for p in params.get("Profile") or []:
            if isinstance(p, dict) and p.get("name"):
                cur = str(p["name"])
                break
        out.append(
            BluezDevice(
                id=int(obj.get("id", -1)),
                name=str(props.get("device.name", "")),
                alias=str(props.get("device.alias") or props.get("device.description") or ""),
                address=str(props.get("api.bluez5.address", "")),
                current_profile=cur,
                profiles=profiles,
            )
        )
    return out


@dataclass
class NodeInfo:
    id: int
    name: str
    description: str
    media_class: str
    state: str
    #: bluez5-only extras; None for ordinary nodes.
    bt_codec: str | None = None
    bt_profile: str | None = None
    bt_address: str | None = None


def parse_nodes(dump: list[dict]) -> list[NodeInfo]:
    out: list[NodeInfo] = []
    for obj in dump:
        if obj.get("type") != "PipeWire:Interface:Node":
            continue
        props = _props(obj)
        name = str(props.get("node.name", ""))
        if not name:
            continue
        out.append(
            NodeInfo(
                id=int(obj.get("id", -1)),
                name=name,
                description=str(props.get("node.description", "")),
                media_class=str(props.get("media.class", "")),
                state=str((obj.get("info") or {}).get("state", "unknown")),
                bt_codec=props.get("api.bluez5.codec"),
                bt_profile=props.get("api.bluez5.profile"),
                bt_address=props.get("api.bluez5.address"),
            )
        )
    return out


def parse_links(dump: list[dict]) -> list[dict]:
    """Links as `{output_node, output_port, input_node, input_port, state}`.

    **`link.output.node` / `link.input.node` in `pw-dump` are integer node *ids*,
    not names** — verified against the live bridge. An earlier version compared
    them straight to node names, so no link ever matched and the chain panel
    always claimed nothing was connected; the unit fixture repeated the same
    mistake, so the tests agreed with the bug. Hence the id -> name map built
    here from the same dump, and the ids kept alongside for debugging.
    """
    names = {
        int(o.get("id", -1)): str(_props(o).get("node.name", ""))
        for o in dump
        if o.get("type") == "PipeWire:Interface:Node"
    }
    out = []
    for obj in dump:
        if obj.get("type") != "PipeWire:Interface:Link":
            continue
        p = _props(obj)
        try:
            out_id = int(p.get("link.output.node", -1))
            in_id = int(p.get("link.input.node", -1))
        except (TypeError, ValueError):
            continue
        out.append(
            {
                "output_node": names.get(out_id, ""),
                "input_node": names.get(in_id, ""),
                "output_node_id": out_id,
                "input_node_id": in_id,
                "output_port": p.get("link.output.port"),
                "input_port": p.get("link.input.port"),
                "state": str((obj.get("info") or {}).get("state", "unknown")),
            }
        )
    return out


def feeding_nodes(links: list[dict], node_name: str) -> list[str]:
    """Names of the nodes whose output is linked into `node_name`."""
    return sorted({l["output_node"] for l in links if l["input_node"] == node_name})


def capture_binding(links: list[dict], stream_node: str, target: str | None) -> dict:
    """Is the app's own capture stream really connected to `target`?

    The reason this exists: `pw-record --target X` is a *preference*, and when X
    goes away PipeWire rebinds the stream to the default source. On this bridge
    that is `rtp-bridge:monitor`, which is silent — so the app reported the phone's
    node name while metering something else, and called it digital silence. The
    capture now sets `node.dont-reconnect`, which should make that impossible; this
    check is the independent confirmation, since a wrong reading here is
    indistinguishable from the fault under investigation.
    """
    fed_by = feeding_nodes(links, stream_node)
    return {
        "stream_node": stream_node,
        "fed_by": fed_by,
        "present": any(l["input_node"] == stream_node for l in links),
        "bound": bool(target) and fed_by == [target],
    }


def a2dp_source_nodes(nodes: list[NodeInfo]) -> list[NodeInfo]:
    """Connected phones/senders: bluez5 nodes that are an audio *source* to us."""
    return [n for n in nodes if n.bt_address and n.media_class.startswith("Audio/Source")]


def capture_targets(nodes: list[NodeInfo]) -> list[dict]:
    """Nodes worth pointing the waveform capture at, best-first.

    The A2DP sources come first: that is the measurement that matters for the
    "audio silently disappears" symptom, because it shows whether silence is
    already present *as it enters* PipeWire (i.e. upstream of this whole bridge).
    The sender sink is offered second so the two can be compared.
    """
    out: list[dict] = []
    for n in a2dp_source_nodes(nodes):
        label = n.description or n.name
        out.append({"node": n.name, "label": f"{label} (Bluetooth in)", "kind": "a2dp-source"})
    for n in nodes:
        if n.name == SENDER_NODE:
            out.append({"node": n.name, "label": "RTP sender (what we transmit)", "kind": "sender"})
    for n in nodes:
        if n.name in (LOOPBACK_CAPTURE, LOOPBACK_PLAYBACK):
            out.append({"node": n.name, "label": f"{n.name} (bridge internal)", "kind": "internal"})
    return out


def sender_chain(nodes: list[NodeInfo], links: list[dict]) -> dict:
    """State of the phone -> loopback -> rtp-bridge chain the bridge depends on.

    Reports each hop's presence/state plus whether the links exist, so a broken
    or re-bound graph (e.g. the loopback capture following the *wrong* default
    source — a real hazard, since the capture end is intentionally unpinned) is
    visible at a glance rather than needing `pw-link -l` by hand.
    """
    by_name = {n.name: n for n in nodes}
    sources = a2dp_source_nodes(nodes)

    def hop(name: str) -> dict:
        n = by_name.get(name)
        return {"node": name, "present": n is not None, "state": n.state if n else None}

    feeding = feeding_nodes(links, LOOPBACK_CAPTURE)
    return {
        "a2dp_sources": [
            {
                "node": n.name,
                "label": n.description or n.name,
                "address": n.bt_address,
                "codec": n.bt_codec,
                "profile": n.bt_profile,
                "state": n.state,
            }
            for n in sources
        ],
        "hops": [hop(LOOPBACK_CAPTURE), hop(LOOPBACK_PLAYBACK), hop(SENDER_NODE)],
        # What is actually wired into the capture end right now.
        "capture_fed_by": feeding,
        "capture_linked": bool(feeding),
        "sender_linked": any(l["input_node"] == SENDER_NODE for l in links),
        # The hazard: the loopback bound to something that isn't a phone.
        "capture_bound_to_a2dp": bool(feeding) and all(
            any(s.name == f for s in sources) for f in feeding
        ),
    }


# --------------------------------------------------------------------------
# levers
# --------------------------------------------------------------------------


def available_levers(devices: list[BluezDevice]) -> dict:
    """Which codec-switching mechanism this box actually offers.

    `profiles` lists per-device codec profiles when PipeWire exposes them
    (lever 1). `dropin` is always true — restricting `bluez5.codecs` and
    reconnecting works everywhere, it is just less pleasant.
    """
    per_device = {
        d.address or d.name: [
            {"index": p.index, "name": p.name, "description": p.description,
             "codec": p.codec, "available": p.available}
            for p in d.codec_profiles
        ]
        for d in devices
    }
    return {
        "profile_switching": any(v for v in per_device.values()),
        "profiles": per_device,
        "dropin": True,
    }


def set_profile(device_id: int, profile_index: int) -> tuple[bool, str]:
    """Lever 1: `wpctl set-profile`. Renegotiates without restarting anything."""
    rc, out, err = run(["wpctl", "set-profile", str(device_id), str(profile_index)])
    if rc == 0:
        return True, f"selected profile {profile_index} on device {device_id}"
    return False, (err or out or f"wpctl exited {rc}").strip()


#: Drop-in that narrows the advertised A2DP codec set.
#:
#: It goes in **WirePlumber's** config, not PipeWire's. Verified on the bridge
#: (WirePlumber 0.5.8): `scripts/monitors/bluez.lua` does
#: `config.properties = Conf.get_section_as_properties("monitor.bluez.properties")`,
#: so that section — and only that section — reaches the bluez5 monitor.
#: `bluez5.codecs` in `pipewire.conf.d` is silently ignored.
#:
#: Written into the *user's* config (no root needed), with a prefix above
#: setup_pi_bridge.py's `51-bt-rtp-bridge.conf` so it wins. It sets a different
#: section than that file does, so the two merge rather than clobber.
CODEC_DROPIN_NAME = "99-bt-testing-codec.conf"


def codec_dropin_path(home: str | None = None) -> str:
    home = home or os.path.expanduser("~")
    return os.path.join(home, ".config", "wireplumber", "wireplumber.conf.d", CODEC_DROPIN_NAME)


def normalize_codecs(codecs) -> tuple[list[str], list[str]]:
    """(accepted, rejected) from arbitrary input.

    `bluez5.codecs` is an **allow-list**, so this deals in sets rather than a
    single choice: de-duplicated, ordered best-first by [`KNOWN_CODECS`] so the
    rendered file reads predictably, with anything unrecognised reported back
    instead of silently dropped (a typo'd codec name in the allow-list would
    otherwise narrow the set differently than intended).
    """
    if not codecs:
        return [], []
    if isinstance(codecs, str):
        codecs = [codecs]
    wanted = {str(c).strip().lower().replace("-", "_") for c in codecs if str(c).strip()}
    accepted = [c for c in KNOWN_CODECS if c in wanted]
    rejected = sorted(wanted - set(KNOWN_CODECS))
    return accepted, rejected


def render_codec_dropin(codecs) -> str:
    """SPA-JSON limiting `bluez5.codecs` to `codecs` (or restoring the default).

    An empty/None selection renders a comment-only file, which is how "let
    PipeWire negotiate freely" is expressed without deleting the file and losing
    the audit trail.

    Note this is an allow-list, so "exclude aptX" is spelled as *everything
    except* aptX — the excluded names never appear in the file.
    """
    header = (
        "# Written by firmware/pi-bridge/bluetooth-testing-app -- safe to delete.\n"
        "# ALLOW-LIST of the A2DP codecs this Pi ADVERTISES. The phone (the A2DP\n"
        "# source) can then only choose from these, so a codec is excluded by\n"
        "# leaving it out.\n"
        "#\n"
        "# WirePlumber reads config only at startup, so this needs a WirePlumber\n"
        "# RESTART, and the phone must then RECONNECT to renegotiate. Do it in that\n"
        "# order with the phone DISCONNECTED first -- restarting WirePlumber while a\n"
        "# phone is connected orphans the bridge's loopback. The testing app's\n"
        "# 'Apply & renegotiate' does exactly that sequence.\n"
    )
    accepted, _ = normalize_codecs(codecs)
    if not accepted:
        return header + "# (no restriction active -- PipeWire negotiates freely)\n"
    excluded = [c for c in KNOWN_CODECS if c not in accepted]
    body = ""
    if excluded:
        body = f"# excluded by this file: {' '.join(excluded)}\n"
    return header + body + (
        "monitor.bluez.properties = {\n"
        f"  bluez5.codecs = [ {' '.join(accepted)} ]\n"
        "}\n"
    )


def write_codec_dropin(codecs, home: str | None = None) -> tuple[bool, str]:
    """Lever 2. Writes the drop-in; does **not** restart PipeWire on purpose.

    Refuses a selection that resolves to nothing while the caller clearly meant
    to restrict something: an empty allow-list would leave the phone unable to
    negotiate any codec at all, which looks exactly like the fault being
    investigated. "Allow everything" is expressed by passing nothing.
    """
    accepted, rejected = normalize_codecs(codecs)
    if rejected:
        return False, f"unknown codec(s): {', '.join(rejected)}"
    if codecs and not accepted:
        return False, "that selection allows no codecs at all — audio would stop"
    path = codec_dropin_path(home)
    try:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as fh:
            fh.write(render_codec_dropin(accepted))
    except OSError as e:
        return False, f"could not write {path}: {e}"
    if not accepted:
        what = "no restriction — negotiates freely"
    else:
        excluded = [c for c in KNOWN_CODECS if c not in accepted]
        what = "allowing " + " ".join(accepted)
        if excluded:
            what += f" (excluding {' '.join(excluded)})"
    return True, (
        f"wrote {path}: {what}. WirePlumber must restart and the phone reconnect "
        "before this takes effect — use 'Apply & renegotiate'."
    )


def read_codec_dropin(home: str | None = None) -> list[str]:
    """The codecs the drop-in currently allows; `[]` if unrestricted or absent."""
    try:
        with open(codec_dropin_path(home)) as fh:
            text = fh.read()
    except OSError:
        return []
    for line in text.splitlines():
        line = line.split("#", 1)[0].strip()
        if line.startswith("bluez5.codecs"):
            inside = line.partition("[")[2].partition("]")[0]
            parts = [p.strip().strip('"\'') for p in inside.replace(",", " ").split()]
            accepted, _ = normalize_codecs(parts)
            return accepted
    return []


def restart_wireplumber() -> tuple[bool, str]:
    """`systemctl --user restart wireplumber`.

    Only safe with no phone connected — see [`apply_codecs`][], which is the
    entry point callers should use.
    """
    rc, out, err = run(["systemctl", "--user", "restart", "wireplumber"], timeout=30.0)
    if rc != 0:
        return False, (err or out or f"systemctl exited {rc}").strip()
    return True, "wireplumber restarted"


def apply_codecs(codecs, address: str | None, disconnect, connect,
                 restart=None, home: str | None = None) -> tuple[bool, list[str]]:
    """Write the allow-list and renegotiate, in the only safe order.

    disconnect -> restart WirePlumber -> reconnect. All three actions are injected
    (`disconnect`/`connect` live in btstat, and `restart` defaults to
    [`restart_wireplumber`][]) so the whole sequence is unit-testable without a
    Bluetooth stack *and without restarting a real service* — an earlier version
    took only the first two and a smoke test duly restarted the developer's
    WirePlumber. Returns (ok, step log) so the UI can show what happened even when
    a later step fails.

    If the phone cannot be reconnected the audio path is left down, so that case
    is reported as a failure with an explicit "reconnect from the phone" hint
    rather than a cheerful success.
    """
    steps: list[str] = []
    ok, msg = write_codec_dropin(codecs, home=home)
    steps.append(msg)
    if not ok:
        return False, steps

    if address:
        d_ok, d_msg = disconnect(address)
        steps.append(f"disconnect: {d_msg}")
        # A failed disconnect is not fatal, but restarting now risks the orphaned
        # loopback this ordering exists to avoid -- so stop instead.
        if not d_ok:
            steps.append("stopped before restarting WirePlumber: restarting it with a "
                         "phone still connected would orphan the bridge's loopback")
            return False, steps
    else:
        steps.append("no device address known — skipping disconnect")

    r_ok, r_msg = (restart or restart_wireplumber)()
    steps.append(f"wireplumber: {r_msg}")
    if not r_ok:
        return False, steps

    if address:
        c_ok, c_msg = connect(address)
        steps.append(f"reconnect: {c_msg}")
        if not c_ok:
            steps.append("audio is down until the phone reconnects — reconnect it from the phone")
            return False, steps
    return True, steps
