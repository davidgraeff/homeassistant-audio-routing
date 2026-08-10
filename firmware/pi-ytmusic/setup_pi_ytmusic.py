#!/usr/bin/env python3
"""Configure a Raspberry Pi as a YouTube Music cast receiver.

Sets up both halves of this signal path on the device (full documentation in
../../docs/ytmusic-receiver.md):

    YouTube Music app --DIAL/Lounge--> receiver/ (Node service)
        --JSON IPC--> mpv --> ytm-out (module-rtp-sink)
        --UDP/RTP--> add-on --> routing matrix --> speakers

The two halves stay separable on purpose, because they fail for entirely
different reasons: the audio path is testable with `--test-tone` and no YouTube
code in it at all, so once it is proven, anything that breaks later is in the
receiver or in yt-dlp. `--no-service` sets up the audio path alone.

The receiver service is the `receiver/` directory next to this script, so copy
the whole role directory to the Pi, not just this file:

    scp -r firmware/pi-ytmusic <user>@<pi>:~/

Designed to sit **alongside** ../pi-bridge/setup_pi_bridge.py on the same box.
It is additive: its own drop-in file, its own port, its own `--disable`. The
Bluetooth role's hard-won BlueZ/WirePlumber configuration is never touched.

Why a SECOND RTP port instead of feeding the Bluetooth bridge's stream:
  - The add-on supports any number of RTP sources, each with its own rate, so
    the two stay independently routable in the matrix.
  - YouTube audio is natively 48 kHz (Opus). A dedicated source at 48000 keeps
    the whole path at the router graph's rate — no resample on the Pi *and*
    none on the receiver — while the Bluetooth leg keeps whatever it needs.

Why NO null sink in front of the RTP sink:
  - `module-rtp-sink` already *is* an Audio/Sink node, and mpv is an ordinary
    playback client, so it can play straight into it. The Bluetooth role needs a
    `module-loopback` only because its input is a *capture* node (bluez_input.*).
  - A null sink + permanent loopback would also keep the graph driven forever,
    transmitting silence at ~1.5 Mbit/s (S16LE/48000/2) around the clock over
    the radio the Pi shares with Bluetooth. The Bluetooth role measured "zero
    packets while idle" and that property is worth keeping. Idle here means the
    sink has no client, so nothing is sent.
  - Consequence for the receiver: keep ONE long-lived mpv and rely on gapless
    playback to hold the device open *between tracks*; do not stream silence to
    hold it open forever.

What this script does (idempotent — safe to re-run):
  1. Installs pipewire/wireplumber (in case this role is set up on a Pi without
     the Bluetooth role), mpv, nodejs/npm and python3-venv (yt-dlp comes from
     pip, in its own venv — see ensure_ytdlp()).
  2. Enables user lingering so the PipeWire user session runs headless at boot.
  3. Writes a PipeWire drop-in loading `module-rtp-sink` as the node `ytm-out`,
     pointed at the add-on, S16LE/48000/stereo.
  4. Restarts the user services (refusing to, while a phone is connected over
     Bluetooth — that orphans the A2DP transport until a reboot).
  5. Installs the `receiver/` app + its `ytmusic-receiver.service` user unit.
  6. Verifies: the sink exists, no monitor cross-talk into the Bluetooth source,
     the service is up, and its DIAL surface answers on the LAN address.

Run it AS the appliance user (not root), on the Pi, with passwordless sudo:

    ./setup_pi_ytmusic.py --host 192.168.178.22            # add-on / HA host IP
    ./setup_pi_ytmusic.py --host 192.168.178.22 --test-tone

Use --disable to tear this role's configuration back out; it leaves the
Bluetooth bridge (and all installed packages) alone.
"""

from __future__ import annotations

import argparse
import os
import pwd
import re
import subprocess
import sys
import time

# --- Constants ---------------------------------------------------------------

#: Distinct from the Bluetooth bridge's 46000 so the two are separate sources in
#: the add-on. Must match the port of the add-on's RTP source for this role.
DEFAULT_PORT = 46001
DEFAULT_FORMAT = "S16LE"  # native-endian PCM; matches the add-on's rtp_source.rs
#: 48 kHz end to end: YouTube audio is natively 48 kHz and so is the router
#: graph, so nothing resamples anywhere on this path.
DEFAULT_RATE = 48000
DEFAULT_CHANNELS = 2

#: The PipeWire node the player targets, and the name to look for in `pw-cli ls
#: Node` / the add-on's matrix. This *is* the RTP sender (see the module
#: docstring on why there is no separate null sink).
SINK_NODE_NAME = "ytm-out"

#: NEGATIVE on purpose, and measured — not a guess. A sink also exposes a
#: *monitor* source, and the Bluetooth role's loopback capture deliberately
#: follows the **default source**, so a new monitor is a new candidate for it to
#: bind to. When it does, YouTube Music is forwarded into `bt-bridge-rtp` as
#: well: two supposedly independent sources carrying the same audio, audible on
#: everything the Bluetooth source is routed to.
#:
#: `rtp-bridge` (the Bluetooth role's sink) leaves `priority.session` **unset**,
#: which ranks as 0 — so any *positive* value here makes this monitor the
#: highest-ranked candidate and actively causes the cross-talk. Confirmed on the
#: hardware: with `100` the capture bound to `ytm-out:monitor_*`; with `-100` it
#: went back to `rtp-bridge:monitor_*` (that sink's own idle, driverless monitor,
#: which sends nothing). A tie at 0 would be non-deterministic, so stay negative.
#: verify() asserts this, and needs no audio to do it.
SINK_PRIORITY_SESSION = -100

APT_PACKAGES = [
    "pipewire",
    "pipewire-audio",
    "pipewire-bin",
    "wireplumber",
    "mpv",  # the player the receiver drives; also used by --test-tone
    # The cast receiver is a Node service. Raspberry Pi OS Trixie ships nodejs 20
    # for armhf, comfortably past yt-cast-receiver's Node >= 18.
    "nodejs",
    "npm",
    # yt-dlp comes from pip in a venv, NOT from apt. Debian trixie ships
    # 2025.04.30, and YouTube breaks extractors far faster than a stable release
    # can follow — a stale yt-dlp is the single most likely cause of "casting
    # connects but nothing plays". python3-venv gives us pip.
    "python3-venv",
    # The JS engine that solves YouTube's `n` signature challenge. See
    # JS_RUNTIME below for why it is quickjs on this box and not node or deno.
    "quickjs",
]

#: venv holding the *current* yt-dlp. The PyInstaller standalone build was
#: rejected: it unpacks itself to /tmp on every run, and on a Zero 2 W's SD card
#: that lands on top of an already-measured ~2.3 s Python cold start per track.
VENV_DIR_REL = ".local/share/pi-ytmusic-venv"
#: JavaScript runtime yt-dlp uses to solve YouTube's `n` signature challenge.
#: Without one, *authenticated* requests find no formats at all and nothing plays.
#:
#: **quickjs**, by elimination — all three alternatives are dead ends on armv7l:
#:   - `deno` (yt-dlp's only default-enabled runtime): no 32-bit ARM build at all.
#:   - `node`: present for the receiver, but Raspbian trixie ships **20.19.2** and
#:     yt-dlp reports `node-20.19.2 (unsupported)`, i.e. the provider refuses it.
#:     (A workstation with node 22 works fine — which is exactly how this was
#:     missed at first.)
#:   - `bun`: no armv7 build either.
#: `quickjs` is a Debian package (2025.04.26-1, `/usr/bin/qjs`) and works.
#: Verified on the hardware, *with cookies*: `251 opus 48000Hz`.
#:
#: Overridable per install via YTCR_JS_RUNTIME in the unit — worth revisiting if
#: node ever reaches >= 23.5 here, since yt-dlp prefers node over quickjs.
JS_RUNTIME = "quickjs"
#: Node >= 22 is what yt-dlp's provider actually requires
#: (`NodeJsRuntime.MIN_SUPPORTED_VERSION = (22, 0, 0)`), and Raspbian trixie ships
#: 20.19.2 — hence the quickjs fallback above. But quickjs has **no JIT**, and on
#: this hardware that is the difference between a usable receiver and not:
#:
#:     authenticated resolution, measured on the Pi Zero 2 W
#:       quickjs   ~90 s     <- unusable per track
#:       node 22   ~22 s
#:       (anonymous, no challenge at all: ~6 s)
#:
#: Node still publishes **official armv7l builds** for v22/v23 (v24+ dropped
#: 32-bit ARM), so a private copy is installed here and used only as yt-dlp's JS
#: runtime. Debian's node 20 continues to run the receiver itself, which does not
#: care about the version.
NODE22_VERSION = "v22.23.2"
NODE_DIR_REL = ".local/opt/node22"
#: Remote yt-cipher server that solves YouTube's `n`/`sig` challenges over HTTP,
#: via the yt-dlp-remote-cipher plugin (by a yt-dlp maintainer; yt-cipher is itself
#: "an http api wrapper for yt-dlp/ejs", i.e. the same solver, hosted).
#:
#: This is the single biggest latency win measured on this hardware, because the
#: challenge is CPU-bound JS and this box is slow:
#:
#:     authenticated resolve, Pi Zero 2 W
#:       local node 22        23-26 s
#:       remote cipher         8-9 s     <- close to the 6 s network floor
#:
#: The local runtime stays installed as the **fallback**: yt-dlp registers both
#: providers and falls back automatically — verified by pointing base_url at a dead
#: port, which still resolved (in 22 s) via node. So an outage costs latency, not
#: playback. Racing the two instead was rejected: the local solve is ~23 s of pegged
#: CPU, and running it concurrently every track would fight the audio path on a
#: 4-core 1 GHz box.
#:
#: The public instance is rate-limited (10 req/s, shared) and sends the challenge
#: strings plus this host's IP to a third party — never cookies. Point this at a
#: self-hosted yt-cipher (it ships a docker-compose) to avoid both.
DEFAULT_CIPHER_URL = "https://cipher.kikkia.dev"
#: Request timeout for the remote solver. Short on purpose: falling back to the
#: local runtime costs ~23 s, so waiting long on a wedged server is worse than
#: giving up early.
CIPHER_TIMEOUT_S = 8
#: Metadata-only probe used to prove resolution actually works. ("Me at the zoo" —
#: yt-dlp's own former test video BaW_jenozKc has been taken down.)
PROBE_URL = "https://www.youtube.com/watch?v=jNQXAC9IVRw"
#: Weekly `pip install -U yt-dlp yt-dlp-ejs`, because extractor breakage is the norm.
YTDLP_UPDATE_UNIT = "ytmusic-ytdlp-update.service"
YTDLP_UPDATE_TIMER = "ytmusic-ytdlp-update.timer"

#: Where the receiver app is installed on the Pi (copied from `receiver/` next to
#: this script). Separate from the source tree so the service does not depend on
#: a checkout being present.
APP_DIR_REL = ".local/share/pi-ytmusic-receiver"
#: The service's WorkingDirectory. yt-cast-receiver's DefaultDataStore persists
#: via node-persist *relative to the working directory*, and what it persists
#: includes the DIAL `pid` that makes the phone recognise this as the same device
#: across restarts — so this has to be a stable, writable directory, not wherever
#: systemd happened to start us.
STATE_DIR_REL = ".local/state/pi-ytmusic-receiver"
#: The receiver's cookie jar. In the state dir because it is **mutable**:
#: `yt-dlp --cookies FILE` reads from *and dumps the jar back into* FILE, so
#: rotated cookies must be able to persist. Provisioned by push_cookies.py from a
#: workstation — never generated here, never committed.
COOKIES_REL = f"{STATE_DIR_REL}/cookies.txt"
RECEIVER_UNIT = "ytmusic-receiver.service"
#: Port the DIAL server listens on (its device description + app endpoints).
DEFAULT_DIAL_PORT = 8099
#: The add-on's HTTP API port. Used only for *metadata* reporting (the daemon's
#: source-generic POST /api/now_playing/report) — the audio path is plain RTP and
#: talks to nobody, and the receiver still runs with reporting off.
DEFAULT_API_PORT = 8099

PW_DROPIN_NAME = "60-ytmusic-rtp.conf"
#: The Bluetooth role's drop-in (../pi-bridge/setup_pi_bridge.py). Its presence is
#: how we know that role is installed, so the cross-talk assertion below knows
#: whether to wait for its nodes or legitimately skip.
BT_DROPIN_NAME = "60-bt-rtp-bridge.conf"
MANAGED_MARKER = "# Managed by firmware/pi-ytmusic/setup_pi_ytmusic.py"


# --- Small shell helpers -----------------------------------------------------


def run(cmd: list[str], *, check: bool = True, capture: bool = False,
        input_text: str | None = None) -> subprocess.CompletedProcess:
    """Run a command, echoing it. Raises on failure unless check=False.

    `input_text` feeds stdin, which is how root-owned files get written here:
    `sudo(["tee", path], input_text=...)` needs no shell and no temp file.
    """
    print("  $", " ".join(cmd))
    return subprocess.run(cmd, check=check, text=True, capture_output=capture,
                          input=input_text)


def sudo(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    return run(["sudo", "-n", *cmd], **kw)


def user_write(path: str, content: str) -> None:
    """Write a file owned by the current user, creating parent dirs."""
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


def user_env() -> dict[str, str]:
    """Environment with XDG_RUNTIME_DIR set, so PipeWire CLI tools and
    `systemctl --user` work over a headless SSH session."""
    return dict(os.environ, XDG_RUNTIME_DIR=f"/run/user/{os.getuid()}")


def systemctl_user(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    cmd = ["systemctl", "--user", *args]
    print("  $", " ".join(cmd))
    return subprocess.run(cmd, check=check, text=True, env=user_env())


# --- Preconditions -----------------------------------------------------------


def preflight() -> None:
    if os.geteuid() == 0:
        sys.exit(
            "Run this AS the appliance user (e.g. `david`), not root — it configures\n"
            "that user's PipeWire session. It uses `sudo` internally for packages."
        )
    if subprocess.run(["sudo", "-n", "true"]).returncode != 0:
        sys.exit("Passwordless sudo is required (sudo -n true failed).")


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


def pw_dropin(host: str, port: int, fmt: str, rate: int, channels: int) -> str:
    return f"""\
{MANAGED_MARKER}
# YouTube Music -> RTP sender. Edit via setup_pi_ytmusic.py, not here (re-running
# the script overwrites this file).
#
# `module-rtp-sink` exposes an Audio/Sink node named "{SINK_NODE_NAME}" and
# transmits whatever is played into it to the add-on's rtp-source. The player
# (mpv) plays straight into this node — no null sink and no loopback: mpv
# is a playback client, and an always-on loopback would transmit silence at
# ~1.5 Mbit/s forever over the radio this Pi shares with Bluetooth. With no
# client connected the sink is idle and nothing is sent.
#
# priority.session is deliberately NEGATIVE ({SINK_PRIORITY_SESSION}): this sink's
# monitor must never outrank the Bluetooth role's own candidates, or that role's
# default-following loopback capture binds HERE and forwards YouTube Music into
# `bt-bridge-rtp` too. `rtp-bridge` leaves priority.session unset (= 0), so any
# positive value causes exactly that. Measured on the hardware; see the constant
# in setup_pi_ytmusic.py and the cross-talk assertion in its verify step.
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
        node.name = "{SINK_NODE_NAME}"
        node.description = "YouTube Music to RTP (sender)"
        media.class = "Audio/Sink"
        priority.session = {SINK_PRIORITY_SESSION}
      }}
    }}
  }}
]
"""


def ytdlp_path(home: str) -> str:
    return os.path.join(home, VENV_DIR_REL, "bin", "yt-dlp")


def cipher_extractor_args(url: str | None) -> str | None:
    """yt-dlp `--extractor-args` value enabling the remote solver, or None."""
    if not url:
        return None
    return f"youtubejsc-remotecipher:base_url={url};timeout={CIPHER_TIMEOUT_S}"


def node22_path(home: str) -> str:
    return os.path.join(home, NODE_DIR_REL, "bin", "node")


def js_runtime_spec(home: str) -> str:
    """The `--js-runtimes` value to use: `node:<path>` when the private Node 22 is
    installed, else plain [`JS_RUNTIME`] (quickjs).

    yt-dlp's option takes `RUNTIME[:PATH]`, which is what lets a private Node be
    used without touching the system one.
    """
    node = node22_path(home)
    return f"node:{node}" if os.path.isfile(node) else JS_RUNTIME


def ensure_node22(home: str) -> bool:
    """Install a private Node >= 22 for yt-dlp's JS challenge solver.

    Why not the system node: Raspbian trixie ships 20.19.2 and yt-dlp requires
    >= 22.0.0, so it is rejected outright. Why not just quickjs (which *is*
    accepted): no JIT — measured ~90 s per authenticated resolution here versus
    ~22 s with node 22. See NODE22_VERSION.

    Idempotent: skips the download when a suitable node is already unpacked.
    """
    print("== Installing a private Node >= 22 (yt-dlp JS runtime) ==")
    dest = os.path.join(home, NODE_DIR_REL)
    exe = node22_path(home)
    if os.path.isfile(exe):
        v = subprocess.run([exe, "--version"], text=True, capture_output=True)
        print(f"  already present: {(v.stdout or '?').strip()}")
        return True

    machine = os.uname().machine
    arch = {"armv7l": "armv7l", "armv6l": "armv6l", "aarch64": "arm64", "x86_64": "x64"}.get(machine)
    if not arch:
        print(f"  unknown machine {machine!r} — skipping; falling back to {JS_RUNTIME}")
        return False
    name = f"node-{NODE22_VERSION}-linux-{arch}"
    url = f"https://nodejs.org/dist/{NODE22_VERSION}/{name}.tar.xz"
    tmp = f"/tmp/{name}.tar.xz"
    if run(["curl", "-fsSLo", tmp, url], check=False).returncode != 0:
        print(f"  download failed ({url}) — falling back to {JS_RUNTIME}")
        return False
    if run(["tar", "xf", tmp, "-C", "/tmp"], check=False).returncode != 0:
        print(f"  extract failed — falling back to {JS_RUNTIME}")
        return False
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    run(["rm", "-rf", dest], check=False)
    run(["mv", f"/tmp/{name}", dest], check=False)
    run(["rm", "-f", tmp], check=False)
    if not os.path.isfile(exe):
        print(f"  install failed — falling back to {JS_RUNTIME}")
        return False
    v = subprocess.run([exe, "--version"], text=True, capture_output=True)
    print(f"  installed {(v.stdout or '?').strip()} at {exe}")
    return True


def ensure_ytdlp(home: str) -> bool:
    """Install/refresh yt-dlp in its own venv. Returns whether it is usable.

    Deliberately independent of the Node app and of apt: this one component must
    be updatable on its own, because it is the one that breaks.
    """
    print("== Installing yt-dlp (pip, in a venv) ==")
    venv = os.path.join(home, VENV_DIR_REL)
    if not os.path.isfile(os.path.join(venv, "bin", "pip")):
        r = run(["python3", "-m", "venv", venv], check=False)
        if r.returncode != 0:
            print("  venv creation failed — is python3-venv installed?")
            return False
    # yt-dlp-ejs carries the JavaScript challenge-solver scripts. Without it (and
    # a JS runtime) YouTube's `n` challenge cannot be solved and authenticated
    # requests return NO formats — the "connects but plays nothing" failure.
    r = run([os.path.join(venv, "bin", "pip"), "install", "--quiet", "--upgrade",
             "yt-dlp", "yt-dlp-ejs", "yt-dlp-remote-cipher"], check=False)
    if r.returncode != 0:
        print("  pip install failed (no network / registry trouble?) — keeping whatever is there")
    exe = ytdlp_path(home)
    if not os.path.isfile(exe):
        print("  no yt-dlp in the venv; playback will not resolve")
        return False
    v = subprocess.run([exe, "--version"], text=True, capture_output=True)
    print(f"  yt-dlp {(v.stdout or '?').strip()} at {exe}")
    return True


def install_ytdlp_update_timer(home: str) -> None:
    """A weekly `pip install -U yt-dlp`.

    This is the cheapest possible answer to "YouTube changed something again", and
    it is why yt-dlp is in a venv rather than pinned into the app: it updates
    without redeploying any of our code.
    """
    print("== Installing the yt-dlp update timer ==")
    venv = os.path.join(home, VENV_DIR_REL)
    unit = f"""\
{MANAGED_MARKER}
[Unit]
Description=Update yt-dlp + solver plugins (YouTube extractor breakage is routine)

[Service]
Type=oneshot
ExecStart={venv}/bin/pip install --quiet --upgrade yt-dlp yt-dlp-ejs yt-dlp-remote-cipher
"""
    timer = f"""\
{MANAGED_MARKER}
[Unit]
Description=Weekly yt-dlp update

[Timer]
OnCalendar=weekly
# Catch up after downtime: an appliance that was off on timer day still updates.
Persistent=true
RandomizedDelaySec=1h

[Install]
WantedBy=timers.target
"""
    user_write(os.path.join(home, ".config/systemd/user", YTDLP_UPDATE_UNIT), unit)
    user_write(os.path.join(home, ".config/systemd/user", YTDLP_UPDATE_TIMER), timer)
    systemctl_user("daemon-reload", check=False)
    systemctl_user("enable", "--now", YTDLP_UPDATE_TIMER, check=False)


def deploy_receiver(home: str) -> bool:
    """Copy the `receiver/` app next to this script into APP_DIR and install its
    dependencies. Returns whether the app is installed and runnable.

    Tolerates the script having been copied to the Pi on its own: without the app
    directory there is nothing to install, so say so plainly and let the audio half
    stand alone.
    """
    print("== Installing the cast receiver app ==")
    src = os.path.join(os.path.dirname(os.path.abspath(__file__)), "receiver")
    app_dir = os.path.join(home, APP_DIR_REL)
    if not os.path.isdir(src):
        print(f"  no `receiver/` directory next to this script ({src}) — skipping.")
        print("  Copy the whole role directory over to install the service, e.g.:")
        print("    scp -r firmware/pi-ytmusic <user>@<pi>:~/")
        return os.path.isfile(os.path.join(app_dir, "index.js"))

    os.makedirs(app_dir, exist_ok=True)
    # Copy EVERY app file, not a hardcoded list. An earlier version named the four
    # modules explicitly; adding a fifth (metadata.js) then produced an installed
    # app that crash-looped on `ERR_MODULE_NOT_FOUND` — the deploy step silently
    # shipped a partial app. Anything the app needs at runtime lives directly in
    # `receiver/`, so "all plain files here" is the correct rule, and node_modules
    # is npm's business below.
    copied = []
    for name in sorted(os.listdir(src)):
        if name.startswith(".") or name == "node_modules":
            continue
        s = os.path.join(src, name)
        if not os.path.isfile(s):
            continue
        with open(s) as f:
            content = f.read()
        with open(os.path.join(app_dir, name), "w") as f:
            f.write(content)
        copied.append(name)
    for required in ("package.json", "index.js"):
        if required not in copied:
            sys.exit(f"receiver/{required} is missing from {src} — refusing to install a partial app.")
    print(f"  copied app -> {app_dir} ({', '.join(copied)})")

    # Every local import in every installed module must have arrived, or the service
    # crash-loops at startup with a stack trace that says nothing about deployment.
    # Checking `index.js` alone is not enough — `mpv.js` imports `singleton.js`, so a
    # transitive module is just as fatal and just as invisible. The pattern matches
    # any relative specifier regardless of how the import is formatted.
    missing = sorted({
        f"{name} -> {target}"
        for name in copied if name.endswith(".js")
        for target in re.findall(r"from '\./([^']+)'", open(os.path.join(src, name)).read())
        if target not in copied
    })
    if missing:
        sys.exit(f"local imports not present in {src}: {missing} — refusing to install.")

    # `npm install` (not `ci`): no lockfile is committed, and on a Zero 2 W this
    # takes a while on first run. --omit=dev keeps the tree small.
    r = run(["npm", "install", "--omit=dev", "--prefix", app_dir], check=False)
    if r.returncode != 0:
        print("  npm install FAILED — the service will not start. Re-run after fixing "
              "connectivity/registry access.")
        return False
    return True


def receiver_unit(home: str, *, device_name: str | None, dial_port: int,
                  bind_address: str | None, addon_host: str | None,
                  api_port: int, rtp_port: int, cipher_url: str | None) -> str:
    app_dir = os.path.join(home, APP_DIR_REL)
    state_dir = os.path.join(home, STATE_DIR_REL)
    # Every value is QUOTED. systemd splits an unquoted `Environment=` line on
    # whitespace, so `Environment=YTCR_DEVICE_NAME=Turnerstr Musik` silently
    # delivers just "Turnerstr" — which then shows up as the DIAL name in the
    # phone's Cast menu.
    env = [
        f'Environment="YTCR_DIAL_PORT={dial_port}"',
        f'Environment="YTCR_AUDIO_DEVICE=pipewire/{SINK_NODE_NAME}"',
        f'Environment="YTCR_YTDL_PATH={ytdlp_path(home)}"',
        f'Environment="YTCR_COOKIES={os.path.join(home, COOKIES_REL)}"',
        f'Environment="YTCR_JS_RUNTIME={js_runtime_spec(home)}"',
        *([f'Environment="YTCR_CIPHER_URL={cipher_url}"'] if cipher_url else []),
        'Environment="YTCR_LOG_LEVEL=info"',
    ]
    # Now-playing reporting to the add-on. `YTCR_ADDON_HOST` is what switches it on
    # at all, and the RTP port is what tells the add-on which of its sources these
    # reports belong to (the same key the Bluetooth bridge's reporter uses).
    if addon_host:
        env += [
            f'Environment="YTCR_ADDON_HOST={addon_host}"',
            f'Environment="YTCR_ADDON_API_PORT={api_port}"',
            f'Environment="YTCR_RTP_PORT={rtp_port}"',
        ]
    if device_name:
        env.append(f'Environment="YTCR_DEVICE_NAME={device_name}"')
    if bind_address:
        env.append(f'Environment="YTCR_BIND_ADDRESS={bind_address}"')
    env_block = "\n".join(env)
    return f"""\
{MANAGED_MARKER}
[Unit]
Description=YouTube Music cast receiver (DIAL + Lounge) -> {SINK_NODE_NAME}
Documentation=file://{os.path.dirname(os.path.abspath(__file__))}/../../docs/ytmusic-receiver.md
# The receiver spawns mpv, which needs the PipeWire session to exist before it
# can open `{SINK_NODE_NAME}`. mpv is respawned by the app if it dies, so a
# transient PipeWire restart does not need a restart of this service.
After=pipewire.service wireplumber.service
Wants=pipewire.service wireplumber.service

[Service]
Type=simple
WorkingDirectory={state_dir}
ExecStart=/usr/bin/node {app_dir}/index.js
{env_block}
Restart=always
RestartSec=3
# NOT lowered. This was `CPUWeight=50`, meant to stop a yt-dlp resolution spike from
# starving the audio path — but a cgroup weight applies to the whole service, and
# **mpv is in this service**, so it throttled the player together with the offender
# while doing nothing about their relative priority. The resolve is what must yield:
# resolver.js exec's it through `nice`/`ionice` (which the JS runtime it spawns then
# inherits), and PipeWire's own loops are realtime — see ensure_realtime_scheduling().
CPUWeight=100

[Install]
WantedBy=default.target
"""


def install_receiver_service(home: str, *, device_name: str | None, dial_port: int,
                             bind_address: str | None, addon_host: str | None,
                             api_port: int, rtp_port: int, cipher_url: str | None) -> None:
    print("== Installing the receiver service ==")
    os.makedirs(os.path.join(home, STATE_DIR_REL), exist_ok=True)
    unit_path = os.path.join(home, ".config/systemd/user", RECEIVER_UNIT)
    user_write(unit_path, receiver_unit(home, device_name=device_name, dial_port=dial_port,
                                        bind_address=bind_address, addon_host=addon_host,
                                        api_port=api_port, rtp_port=rtp_port, cipher_url=cipher_url))
    systemctl_user("daemon-reload", check=False)
    systemctl_user("enable", "--now", RECEIVER_UNIT, check=False)
    systemctl_user("restart", RECEIVER_UNIT, check=False)


def remove_receiver_service(home: str) -> None:
    systemctl_user("disable", "--now", RECEIVER_UNIT, check=False)
    systemctl_user("disable", "--now", YTDLP_UPDATE_TIMER, check=False)
    user_remove(os.path.join(home, ".config/systemd/user", RECEIVER_UNIT))
    user_remove(os.path.join(home, ".config/systemd/user", YTDLP_UPDATE_UNIT))
    user_remove(os.path.join(home, ".config/systemd/user", YTDLP_UPDATE_TIMER))
    systemctl_user("daemon-reload", check=False)


def write_pipewire_config(home: str, host: str, port: int, fmt: str, rate: int, channels: int) -> None:
    print("== Writing PipeWire drop-in ==")
    path = os.path.join(home, ".config/pipewire/pipewire.conf.d", PW_DROPIN_NAME)
    user_write(path, pw_dropin(host, port, fmt, rate, channels))


def a2dp_client_connected() -> bool:
    """Whether a phone is currently connected as an A2DP source, i.e. whether the
    Bluetooth role is live right now. Asks the graph (`bluez_input.*` nodes)
    rather than BlueZ, because the graph is what a PipeWire restart destroys.
    `bluez_midi.*` does not count — only audio input nodes do."""
    r = subprocess.run(["pw-cli", "ls", "Node"], text=True, capture_output=True, env=user_env())
    return "bluez_input." in r.stdout


def restart_services(*, force: bool = False) -> None:
    """Restart the user PipeWire stack so the drop-in takes effect.

    **Refuses while a phone is connected.** Hot-restarting PipeWire/WirePlumber
    under a live A2DP transport orphans that transport on this hardware, and only
    a clean boot recovers it — a bring-up lesson from the Bluetooth role (see
    ../../docs/decisions.md, "Raspberry Pi Bluetooth → RTP bridge"). The drop-in
    on disk is the durable part; applying it can always wait for the next boot,
    so the safe default is to skip the restart rather than break a stream in
    progress. `--force-restart` overrides.
    """
    if not force and a2dp_client_connected():
        print("== SKIPPING PipeWire restart: a phone is connected over Bluetooth ==")
        print(
            "  Hot-restarting PipeWire under a live A2DP transport orphans it, and only a\n"
            "  clean boot recovers. The drop-in is written, so it will take effect on the\n"
            "  next boot. To apply now: disconnect the phone and re-run, or pass\n"
            "  --force-restart if you accept losing the Bluetooth path until a reboot."
        )
        return
    print("== (Re)starting PipeWire user services ==")
    systemctl_user("daemon-reload", check=False)
    systemctl_user("enable", "--now", "pipewire", "wireplumber", check=False)
    systemctl_user("restart", "pipewire", "wireplumber")


def disable(home: str, *, force_restart: bool = False) -> None:
    print("== Removing YouTube Music sender configuration ==")
    remove_receiver_service(home)
    user_remove(os.path.join(home, f".config/pipewire/pipewire.conf.d/{PW_DROPIN_NAME}"))
    print(f"  (left the app + its state in ~/{APP_DIR_REL} and ~/{STATE_DIR_REL}; "
          f"delete by hand to reset the DIAL identity)")
    restart_services(force=force_restart)
    print(
        f"Removed. The Bluetooth bridge role (if set up) is untouched; delete the\n"
        f"add-on's RTP source for this port by hand if you no longer want it."
    )


def node_present(name: str) -> bool:
    r = subprocess.run(["pw-cli", "ls", "Node"], text=True, capture_output=True, env=user_env())
    return name in r.stdout


def test_tone(seconds: int = 5) -> None:
    """Play a tone into the sink with mpv, pinned exactly the way the receiver
    pins it — so this also proves the pinning syntax on this box."""
    print(f"== Playing a {seconds}s test tone into {SINK_NODE_NAME} ==")
    cmd = [
        "mpv",
        "--no-video",
        f"--audio-device=pipewire/{SINK_NODE_NAME}",
        f"--length={seconds}",
        "--really-quiet",
        f"av://lavfi:sine=frequency=440:duration={seconds}",
    ]
    r = subprocess.run(cmd, text=True, env=user_env())
    if r.returncode != 0:
        print(
            f"  mpv exited {r.returncode}. If it could not open the device, list the\n"
            f"  real device strings with:  mpv --audio-device=help | grep -i pipewire\n"
            f"  and/or pin via the environment instead:\n"
            f"    PIPEWIRE_PROPS='{{ target.object = {SINK_NODE_NAME} }}' mpv ..."
        )


#: `LimitRTPRIO` for the user manager. 95 matches what PipeWire's own
#: `25-pw-rlimits.conf` grants the `@pipewire` group; module-rt asks for `rt.prio = 88`.
RTPRIO_LIMIT = 95


def rtprio_dropin_path(uid: int) -> str:
    return f"/etc/systemd/system/user@{uid}.service.d/50-rtprio.conf"


def realtime_status() -> tuple[bool, str]:
    """Whether PipeWire's data loops actually run with realtime scheduling.

    Measured, not inferred — the whole point is that every *configuration* on this
    box looked correct while the answer was no.
    """
    threads: list[tuple[str, str]] = []
    r = subprocess.run(["pgrep", "-x", "pipewire"], text=True, capture_output=True)
    for pid in r.stdout.split():
        t = subprocess.run(["ps", "-Lo", "cls,rtprio,comm", "--no-headers", "-p", pid],
                           text=True, capture_output=True)
        for line in t.stdout.splitlines():
            parts = line.split()
            if len(parts) >= 3 and parts[2].startswith("data-loop"):
                threads.append((parts[0], parts[1]))
    if not threads:
        return False, "no pipewire data-loop thread found (is PipeWire running?)"
    non_rt = [f"{cls}/{prio}" for cls, prio in threads if cls not in ("FF", "RR")]
    if non_rt:
        return False, (f"{len(non_rt)} of {len(threads)} data-loop thread(s) are not realtime "
                       f"({', '.join(non_rt)}) — audio can be preempted by any nice-0 work")
    return True, f"{len(threads)} data-loop thread(s) realtime ({threads[0][0]}/{threads[0][1]})"


def ensure_realtime_scheduling(uid: int) -> None:
    """Let PipeWire's data loops acquire SCHED_FIFO.

    WHY THIS IS HERE, AND WHY THE OBVIOUS FIX DOES NOT WORK
    ------------------------------------------------------
    Diagnosed 2026-08-10 while chasing YouTube Music playback stutter: every
    `data-loop.0` thread of `pipewire` and `wireplumber` was `SCHED_OTHER` at nice 0,
    so the RTP sender could be preempted by ordinary work — including this role's own
    `yt-dlp`/JS-runtime resolve, which pegs a core for ~20 s *while a track plays*.

    PipeWire ships `/etc/security/limits.d/25-pw-rlimits.conf` granting the
    `@pipewire` group `rtprio 95`, and its own comment says to add users to that
    group. **On this box that has no effect**: the group exists but limits.d is
    applied by `pam_limits`, and there is no `/etc/pam.d/systemd-user`, so
    `user@<uid>.service` — the manager that actually starts PipeWire — never passes
    through PAM at all. Its `LimitRTPRIO` comes from the *system* manager and was 0.
    (RTKit is running and is PipeWire's documented fallback; empirically it did not
    grant RT here either.) So the limit is raised where that manager is configured.

    This is an audio-session fix, not a YouTube Music one — the Bluetooth role's
    sender benefits identically. It is deliberately additive and lives here because
    `setup_pi_bridge.py` is not touched by this script; `--disable` leaves it in
    place for that reason.
    """
    print("== Realtime scheduling for the audio path ==")
    ok, detail = realtime_status()
    if ok:
        print(f"  already realtime: {detail}")
        return

    path = rtprio_dropin_path(uid)
    content = f"""# Managed by firmware/pi-ytmusic/setup_pi_ytmusic.py
#
# PipeWire's data loops ask for SCHED_FIFO ({RTPRIO_LIMIT} >= module-rt's rt.prio = 88).
# The @pipewire group + limits.d route does NOT work on this box: user@.service is
# started by the system manager and never goes through pam_limits (no
# /etc/pam.d/systemd-user), so its RTPRIO limit is whatever systemd was given — 0.
# Without this, audio threads run SCHED_OTHER at nice 0 and a yt-dlp resolve spike
# is enough to make playback stutter.
[Service]
LimitRTPRIO={RTPRIO_LIMIT}
"""
    sudo(["mkdir", "-p", os.path.dirname(path)])
    sudo(["tee", path], input_text=content, capture=True)  # capture: tee echoes stdin
    sudo(["systemctl", "daemon-reload"])
    print(f"  wrote {path} (LimitRTPRIO={RTPRIO_LIMIT})")
    # Deliberately NOT restarting user@<uid>.service: that tears down every user
    # service including the live PipeWire session, and this role's own guidance is
    # that a PipeWire restart under a connected phone orphans the A2DP transport.
    # The file on disk is the durable part.
    print("  REBOOT REQUIRED for this to take effect (the limit is inherited at "
          "user-manager start).")
    print(f"  current state: {detail}")


def crosstalk_check() -> tuple[bool, str]:
    """Assert that the Bluetooth role's loopback capture has NOT bound to this
    sink's monitor. Needs no audio: the link is assigned by policy as soon as the
    nodes exist, so this is testable silently — which matters, because the
    Bluetooth source is typically routed to real speakers and a test tone that
    leaks through this path is audible in the house.

    Returns (ok, detail). `ok` is True when the Bluetooth role is absent (nothing
    to cross-talk with) or its capture is bound elsewhere.
    """
    bt_installed = os.path.exists(
        os.path.join(pwd.getpwuid(os.getuid()).pw_dir,
                     f".config/pipewire/pipewire.conf.d/{BT_DROPIN_NAME}")
    )
    # After a PipeWire restart the loopback takes a moment to appear. Only wait
    # when the Bluetooth role is actually installed, so a box without it doesn't
    # pay the timeout — and so "not present" can never silently mask a real FAIL.
    deadline = time.monotonic() + (15.0 if bt_installed else 0.0)
    while True:
        r = subprocess.run(["pw-link", "-il"], text=True, capture_output=True, env=user_env())
        if "bt-bridge-capture" in r.stdout or time.monotonic() >= deadline:
            break
        time.sleep(1.0)
    if "bt-bridge-capture" not in r.stdout:
        if bt_installed:
            return False, (
                f"the Bluetooth role is installed ({BT_DROPIN_NAME}) but bt-bridge-capture "
                f"never appeared — check that role, it may be broken"
            )
        return True, "Bluetooth role not present — nothing to cross-talk with"
    bound_to: list[str] = []
    in_capture = False
    for line in r.stdout.splitlines():
        if not line.startswith((" ", "\t")):
            in_capture = line.startswith("bt-bridge-capture")
            continue
        if in_capture:
            bound_to.append(line.strip().lstrip("|<- ").strip())
    leaked = [b for b in bound_to if b.startswith(f"{SINK_NODE_NAME}:")]
    if leaked:
        return False, f"bt-bridge-capture is bound to {', '.join(leaked)}"
    return True, f"bt-bridge-capture is bound to {', '.join(bound_to) or '(nothing)'}"


def primary_lan_ip() -> str:
    """This host's LAN address, as chosen by the routing table. Opens a UDP socket
    to a discard address and reads back the local end — no packet is sent."""
    import socket
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 1))  # TEST-NET-1, guaranteed unrouted
        return s.getsockname()[0]
    except OSError:
        return "127.0.0.1"
    finally:
        s.close()


def log_hint() -> str:
    """The command that actually reads this service's log on this box.

    `journalctl --user` fails here ("No journal files were found") because the
    appliance user is not in the `systemd-journal` group; user-unit records still
    land in the system journal, so read them by unit field with sudo.
    """
    return f"sudo journalctl _SYSTEMD_USER_UNIT={RECEIVER_UNIT} -n 40 --no-pager"


def verify_resolver(home: str, cipher_url: str | None = None) -> None:
    """Report the two things that decide whether *playback* works, as opposed to
    whether casting connects: the resolver's version and the cookie jar.

    Kept separate from the DIAL checks on purpose — a healthy receiver with a
    stale yt-dlp or a dead jar connects perfectly and then plays nothing, which is
    the confusing failure this section exists to make obvious.
    """
    exe = ytdlp_path(home)
    if os.path.isfile(exe):
        v = subprocess.run([exe, "--version"], text=True, capture_output=True)
        print(f"  yt-dlp (venv):               {(v.stdout or '?').strip()}")
        print(f"  JS challenge runtime:        {js_runtime_spec(home)}")
        # An installed yt-dlp is NOT the same as a working one: YouTube makes
        # requests solve an `n` challenge, which needs a JS runtime plus the
        # yt-dlp-ejs scripts. Missing either yields "No video formats found" and
        # nothing plays, while every other check here still passes.
        #
        # Crucially, probe **with the cookie jar when there is one**. Only
        # *authenticated* requests hit the challenge — an anonymous probe passes on
        # a box whose runtime is unusable, which is exactly how an unsupported node
        # went unnoticed here. So the honest check is the authenticated one, and
        # when no jar exists this says what it did and did not prove.
        jar = os.path.join(home, COOKIES_REL)
        authed = os.path.isfile(jar)
        cmd = [exe, "--js-runtimes", JS_RUNTIME, "-f", "bestaudio/best", "--skip-download",
               "--simulate", "--no-warnings", "--print", "%(format_id)s %(acodec)s %(asr)s"]
        if authed:
            cmd += ["--cookies", jar]
        cmd.append(PROBE_URL)
        probe = subprocess.run(cmd, text=True, capture_output=True, timeout=240)
        label = "authenticated" if authed else "anonymous"
        if probe.returncode == 0:
            print(f"  resolves a video ({label}):  YES ({(probe.stdout or '').strip()})")
            if not authed:
                print("    NB: anonymous requests skip YouTube's JS challenge, so this does")
                print("    NOT prove the runtime works. Re-run after pushing a cookie jar.")
        else:
            err = (probe.stderr or "").strip().splitlines()
            print(f"  resolves a video ({label}):  NO — {err[-1][:150] if err else 'unknown error'}")
            print(f"    'No video formats found' means the JS challenge failed. Check which\n"
                  f"    runtime yt-dlp actually accepts — `(unsupported)` in this output is the\n"
                  f"    tell, and is why the runtime here is {js_runtime_spec(home)!r}:\n"
                  f"      {exe} -v --js-runtimes {js_runtime_spec(home)} --simulate {PROBE_URL} 2>&1 | grep -i 'JS runtimes'")
    else:
        print(f"  yt-dlp (venv):               MISSING ({exe})")

    jar = os.path.join(home, COOKIES_REL)
    if not os.path.isfile(jar):
        print("  cookie jar:                  none — anonymous resolution only")
        print("    Provision from a workstation: firmware/pi-ytmusic/push_cookies.py")
        return
    writable = os.access(jar, os.W_OK)
    mode = oct(os.stat(jar).st_mode & 0o777)
    # The jar is a live credential that yt-dlp rewrites as cookies rotate, so a
    # read-only jar silently loses refreshes and eventually stops working.
    print(f"  cookie jar:                  present (mode {mode}, "
          f"{'writable' if writable else 'NOT WRITABLE — rotation will be lost'})")
    try:
        with open(jar) as f:
            names = {}
            for line in f:
                if line.startswith("#") and not line.startswith("#HttpOnly_"):
                    continue
                parts = line.rstrip("\n").split("\t")
                if len(parts) == 7:
                    names[parts[5]] = parts[4]
        critical = [n for n in ("__Secure-1PSID", "__Secure-3PSID", "SID", "SAPISID") if n in names]
        if not critical:
            print("    no login cookies in it — that jar is a signed-out session")
            return
        soonest = min((int(names[n]) for n in critical if names[n].isdigit() and int(names[n]) > 0),
                      default=0)
        if soonest:
            when = time.strftime("%Y-%m-%d", time.localtime(soonest))
            days = int((soonest - time.time()) // 86400)
            print(f"    login cookies: {', '.join(critical)} — earliest expiry {when} ({days:+d} days)")
            print("    (a lower bound only; Google can invalidate server-side at any time)")
    except OSError as e:
        print(f"    could not read the jar: {e}")


def verify_receiver_service(dial_port: int) -> None:
    """Report whether the receiver service is up and its DIAL surface answers.

    Fetches the DIAL device description and the `YouTube` app endpoint — the two
    things a sender fetches after SSDP — so the whole receiver is checked without
    a phone and without making a sound. A 404 on the app endpoint while the
    service is live would mean the app never registered.
    """
    r = systemctl_user("is-active", RECEIVER_UNIT, check=False)
    active = r.returncode == 0
    print(f"  receiver service active:      {'YES' if active else 'NO'}")
    if not active:
        print(f"    {log_hint()}")
        return
    import urllib.request

    # Probe the LAN address, NOT loopback: the DIAL server is deliberately bound
    # to one address (see YTCR_BIND_ADDRESS — a multi-homed responder answers
    # M-SEARCH twice and senders drop it), so 127.0.0.1 is refused even when
    # everything is healthy.
    base = f"http://{primary_lan_ip()}:{dial_port}/ytcr"
    # And retry: the service spawns mpv and waits for its IPC socket before
    # starting the DIAL server, which takes several seconds on a Pi Zero 2 W —
    # so an immediate probe after `systemctl restart` gets ECONNREFUSED from a
    # perfectly healthy install.
    deadline = time.monotonic() + 30.0
    last_err: Exception | None = None
    while True:
        try:
            with urllib.request.urlopen(f"{base}/ssdp/device-desc.xml", timeout=5) as resp:
                body = resp.read().decode("utf-8", "replace")
                app_url = resp.headers.get("Application-URL", "")
            name = ""
            if "<friendlyName>" in body:
                name = body.split("<friendlyName>")[1].split("</friendlyName>")[0]
            print(f"  DIAL device description:     YES (friendlyName={name!r})")
            print(f"    Application-URL: {app_url or '(missing!)'}")
            break
        except Exception as e:  # noqa: BLE001 — any failure here means "not serving yet"
            last_err = e
            if time.monotonic() >= deadline:
                print(f"  DIAL device description:     NO ({last_err})")
                print(f"    {log_hint()}")
                return
            time.sleep(2.0)
    try:
        with urllib.request.urlopen(f"{base}/apps/YouTube", timeout=5) as resp:
            print(f"  DIAL app 'YouTube':          {resp.status}")
    except Exception as e:  # noqa: BLE001
        print(f"  DIAL app 'YouTube':          NO ({e})")


def verify(host: str, port: int, *, dial_port: int = DEFAULT_DIAL_PORT, home: str | None = None,
           cipher_url: str | None = None) -> None:
    print("\n== Verification ==")
    present = node_present(SINK_NODE_NAME)
    print(f"  {SINK_NODE_NAME} sink present: {'YES' if present else 'NO'}")
    if not present:
        print(
            "  The module did not load. Check:\n"
            "    XDG_RUNTIME_DIR=/run/user/$(id -u) journalctl --user -u pipewire -n 50"
        )
    ok, detail = crosstalk_check()
    print(f"  no cross-talk into Bluetooth: {'PASS' if ok else 'FAIL'} — {detail}")
    # Asserted, not assumed: this was SCHED_OTHER on a box where every config file
    # said it should not be, and the symptom was intermittent playback stutter with
    # nothing logged anywhere. Needs no audio.
    rt_ok, rt_detail = realtime_status()
    print(f"  audio threads realtime: {'PASS' if rt_ok else 'FAIL'} — {rt_detail}")
    if not rt_ok:
        print(f"    Fix: re-run this script (it writes {rtprio_dropin_path(os.getuid())}) "
              f"and REBOOT.")
    verify_receiver_service(dial_port)
    if home:
        verify_resolver(home, cipher_url)
    if not ok:
        print(
            f"  The Bluetooth role's default-following capture bound to this sink's\n"
            f"  monitor, so anything played here is ALSO sent to `bt-bridge-rtp` and out\n"
            f"  of every speaker that source is routed to. Lower SINK_PRIORITY_SESSION\n"
            f"  (currently {SINK_PRIORITY_SESSION}) and re-run; it must stay below\n"
            f"  `rtp-bridge`'s (unset = 0), i.e. negative."
        )
    print(
        f"\nNext:\n"
        f"  1. On the add-on, add an RTP source with:\n"
        f"       port          {port}\n"
        f"       rate          {DEFAULT_RATE}\n"
        f"       source_addr   0.0.0.0        (unicast; this Pi sends to {host})\n"
        f"       ignore_ssrc   true\n"
        f"     Name it something like \"YouTube Music\". Leave latency at the default\n"
        f"     until there is a reason to change it.\n"
        f"  2. Route that source to a speaker in the matrix, then prove the path with\n"
        f"     no YouTube code involved — either:\n"
        f"       ./setup_pi_ytmusic.py --host {host} --test-tone\n"
        f"     or any file:\n"
        f"       XDG_RUNTIME_DIR=/run/user/$(id -u) \\\n"
        f"         mpv --audio-device=pipewire/{SINK_NODE_NAME} some.mp3\n"
        f"     You should hear it on the routed speaker.\n"
        f"     WARNING: the Bluetooth source is usually routed to real speakers. If the\n"
        f"     cross-talk check above says FAIL, a test tone will be AUDIBLE in the\n"
        f"     house — fix that first (it is asserted here precisely so you can check it\n"
        f"     without making a sound).\n"
        f"  3. Cast from the YouTube Music app: the receiver should appear in the Cast\n"
        f"     menu under its DIAL name (above). Watch it work with:\n"
        f"       sudo journalctl _SYSTEMD_USER_UNIT={RECEIVER_UNIT} -f\n"
        f"     A connect logs `sender connected: ... — YouTube Music`; each track logs\n"
        f"     `play <videoId>`. mpv's own errors (yt-dlp failures) appear there too.\n"
    )


# --- Main --------------------------------------------------------------------


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--host", help="Add-on / HA host IP the RTP stream is sent to "
                                   "(or an IPv4 multicast group).")
    ap.add_argument("--api-port", type=int, default=DEFAULT_API_PORT,
                    help=f"Add-on HTTP API port, used only for now-playing reporting "
                         f"(default {DEFAULT_API_PORT}).")
    ap.add_argument("--no-metadata", action="store_true",
                    help="Don't report now-playing metadata to the add-on (no track info in "
                         "Home Assistant; audio is unaffected).")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT,
                    help=f"UDP port; must match the add-on's RTP source (default {DEFAULT_PORT}).")
    ap.add_argument("--format", default=DEFAULT_FORMAT, help=f"RTP audio format (default {DEFAULT_FORMAT}).")
    ap.add_argument("--rate", type=int, default=DEFAULT_RATE,
                    help=f"Sample rate; keep 48000 unless the add-on's source differs "
                         f"(default {DEFAULT_RATE}).")
    ap.add_argument("--channels", type=int, default=DEFAULT_CHANNELS, help=f"Channels (default {DEFAULT_CHANNELS}).")
    ap.add_argument("--test-tone", action="store_true",
                    help="After setup, play a 5s 440 Hz tone into the sink (proves the "
                         "whole path and the mpv pinning syntax).")
    ap.add_argument("--name", help="DIAL device name shown in the app's Cast menu "
                                   "(default: 'Musik (<hostname>)').")
    ap.add_argument("--dial-port", type=int, default=DEFAULT_DIAL_PORT,
                    help=f"Port the DIAL server listens on (default {DEFAULT_DIAL_PORT}).")
    ap.add_argument("--bind-address",
                    help="Pin the DIAL server to this local IP. Omit to auto-detect the LAN "
                         "interface at runtime; set it on a multi-homed host, where answering "
                         "SSDP on the wrong address makes senders silently drop the device.")
    ap.add_argument("--cipher-url", default=DEFAULT_CIPHER_URL,
                    help=f"Remote yt-cipher server used to solve YouTube's JS challenges "
                         f"(~3x faster than solving locally on this hardware; the local "
                         f"runtime stays as the automatic fallback). Pass 'none' to solve "
                         f"only locally. Default: {DEFAULT_CIPHER_URL}")
    ap.add_argument("--no-service", action="store_true",
                    help="Set up only the audio path; skip installing/starting the "
                         "cast receiver service.")
    ap.add_argument("--force-restart", action="store_true",
                    help="Restart PipeWire even if a phone is connected over Bluetooth "
                         "(orphans that A2DP transport until a reboot).")
    ap.add_argument("--disable", action="store_true",
                    help="Remove this role's configuration and exit (leaves the Bluetooth bridge alone).")
    args = ap.parse_args()

    preflight()
    home = pwd.getpwuid(os.getuid()).pw_dir
    user = pwd.getpwuid(os.getuid()).pw_name

    cipher_url = None if args.cipher_url in ('none', '') else args.cipher_url

    if args.disable:
        disable(home, force_restart=args.force_restart)
        return

    if not args.host:
        ap.error("--host is required (the add-on / HA host IP)")

    print(f"Configuring YouTube Music -> RTP sender as user '{user}' -> {args.host}:{args.port}\n")
    ensure_packages()
    ensure_linger(user)
    # Before the audio config, because it is the precondition that makes any of it
    # reliable: without realtime data loops, playback stutters under this role's own
    # resolver spikes.
    ensure_realtime_scheduling(os.getuid())
    write_pipewire_config(home, args.host, args.port, args.format, args.rate, args.channels)
    restart_services(force=args.force_restart)
    if args.no_service:
        print("== Skipping the cast receiver service (--no-service) ==")
        remove_receiver_service(home)
    elif deploy_receiver(home):
        ensure_node22(home)
        ensure_ytdlp(home)
        install_ytdlp_update_timer(home)
        install_receiver_service(home, device_name=args.name, dial_port=args.dial_port,
                                 bind_address=args.bind_address,
                                 addon_host=None if args.no_metadata else args.host,
                                 api_port=args.api_port, rtp_port=args.port,
                                 cipher_url=cipher_url)
    verify(args.host, args.port, dial_port=args.dial_port, home=home, cipher_url=cipher_url)
    if args.test_tone:
        test_tone()


if __name__ == "__main__":
    main()
