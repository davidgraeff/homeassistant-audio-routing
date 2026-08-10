#!/usr/bin/env python3
"""Provision the receiver's YouTube cookie jar from this workstation.

Runs on the **desktop** (where a browser and a logged-in session exist), not on
the Pi. It extracts cookies, keeps only the Google/YouTube ones, tells you what
you are about to ship and when it expires, then installs it on the Pi as the
receiver's jar and proves it actually resolves a video there.

    ./push_cookies.py --from-browser firefox:ytm            # extract + push
    ./push_cookies.py --file ~/cookies.txt                  # push an existing jar
    ./push_cookies.py --file ~/cookies.txt --inspect        # look, don't push
    ./push_cookies.py --check                               # liveness only, on the Pi
    ./push_cookies.py --from-browser firefox --addon        # target the HA add-on
    ./push_cookies.py --addon --check                       # liveness only, in the add-on

WHY THIS IS "PROVISION", NOT "SYNC"
-----------------------------------
`yt-dlp --cookies FILE` *reads from and dumps the jar back into* FILE. The copy on
the Pi is therefore a **live, rotating credential**, not a static file:

  - It must stay writable by the receiver service, or refreshed cookies are lost.
  - Re-pushing an older export **over** a rotated jar rolls it back, and can
    invalidate the session. So this tool refuses to overwrite a jar that is newer
    than what you are pushing unless you pass --force.
  - Above all: if the same login session stays live in your everyday browser
    *and* on the Pi, the two rotate against each other and Google invalidates
    both. Export from a **dedicated or private browser session**, then close it
    **without logging out** and never use that session again. This is yt-dlp's own
    documented advice and it is the difference between "works for months" and
    "breaks by tomorrow".

The cookies grant full access to the Google account they came from. They are
installed mode 0600 and this tool never prints cookie *values*.
"""

from __future__ import annotations

import argparse
import datetime as dt
import os
import shlex
import subprocess
import sys
import tempfile

#: Default Pi target (user@host), matching the rest of this role.
DEFAULT_TARGET = "david@turnerstr-bluetooth.local"
#: Local yt-dlp used for extraction; overridable with --ytdlp.
YTDLP = "yt-dlp"
#: `--remote-components` value used by default, so a yt-dlp *without* the
#: yt-dlp-ejs package can still fetch the JS challenge-solver script it needs.
#:
#: **`ejs:github`, not `ejs:npm`.** Both are accepted values of the option, but for
#: the challenge solver yt-dlp only downloads from GitHub — with `ejs:npm` it logs
#: "Remote component challenge solver script (node) was skipped ... enable the
#: download with --remote-components ejs:github (recommended)" and then finds no
#: formats. Verified on this workstation: `ejs:npm` fails, `ejs:github` resolves.
#: See https://github.com/yt-dlp/yt-dlp/wiki/EJS
DEFAULT_REMOTE_COMPONENTS = "ejs:github"
REMOTE_COMPONENTS = DEFAULT_REMOTE_COMPONENTS
#: Where the receiver expects its jar (must match YTCR_COOKIES in the unit that
#: setup_pi_ytmusic.py writes).
REMOTE_JAR = ".local/state/pi-ytmusic-receiver/cookies.txt"

#: --- add-on target -------------------------------------------------------
#: The Home Assistant add-on keeps its jar in the add-on's persistent /data.
#: That directory is NOT reachable over the HA SSH add-on (which only mounts
#: /addons, /config, /share, /ssl, /backup), so the jar goes in through Docker
#: instead — the socket IS available there. Add-on containers are named
#: `app_<slug>` on this Supervisor version (checked with `docker ps`).
DEFAULT_HA_HOST = "root@homeassistant.local"
#: SSH target for the HA host; set from --ha-host in main().
HA_HOST = DEFAULT_HA_HOST
ADDON_CONTAINER = "app_local_ytmusic_receiver"
ADDON_JAR = "/data/cookies.txt"
ADDON_YTDLP = "/opt/ytdlp/bin/yt-dlp"
#: node, not quickjs: the add-on image has node 22, which yt-dlp accepts.
ADDON_JS_RUNTIME = "node"
#: The Pi's yt-dlp (the venv one setup_pi_ytmusic.py installs — deliberately not
#: the stale apt package).
REMOTE_YTDLP = ".local/share/pi-ytmusic-venv/bin/yt-dlp"
#: Only for printing a copy-pasteable diagnostic command.
REMOTE_TARGET_HINT = DEFAULT_TARGET

#: Only these domains are shipped. A browser jar contains every site you have
#: ever visited; there is no reason for any of that to reach the Pi.
KEEP_DOMAIN_SUFFIXES = (".youtube.com", "youtube.com", ".google.com", "google.com")

#: The cookies that actually carry the login. If none are present, the export came
#: from a session that was not signed in and pushing it is pointless.
#:
#: Split by role because they are not interchangeable: yt-dlp builds YouTube's
#: `SAPISIDHASH` authorization header from a SID-family *and* an APISID-family
#: cookie, so having only one family is a half-authenticated jar that fails in
#: confusing ways. `LOGIN_INFO` is YouTube's own and is a useful signal but not
#: sufficient on its own.
SID_FAMILY = ("__Secure-1PSID", "__Secure-3PSID", "SID")
APISID_FAMILY = ("SAPISID", "__Secure-1PAPISID", "__Secure-3PAPISID", "APISID")
OTHER_LOGIN = ("HSID", "SSID", "LOGIN_INFO")
CRITICAL = SID_FAMILY + APISID_FAMILY + OTHER_LOGIN
#: Short-lived companions that rotate constantly (hours). Their expiry says
#: nothing about the life of the session, so they are reported separately.
ROTATING = ("__Secure-1PSIDTS", "__Secure-3PSIDTS", "__Secure-1PSIDCC", "__Secure-3PSIDCC")
#: Cookies a *signed-out* visit leaves behind. Recognised only so the report can
#: say "this is an anonymous session" instead of listing absences.
ANONYMOUS = ("VISITOR_INFO1_LIVE", "VISITOR_PRIVACY_METADATA", "YSC", "PREF", "GPS",
             "CONSENT", "SOCS", "NID", "DEVICE_INFO", "__Secure-YEC", "wide")

#: A video used only to prove that resolution works. "Me at the zoo" — the oldest
#: video on the platform, so about as unlikely to disappear as anything on
#: YouTube. (The obvious choice, yt-dlp's own `BaW_jenozKc` test video, is *gone*:
#: it now returns "Video unavailable", which made every probe fail for a reason
#: that had nothing to do with cookies. Hence also `--probe-url`: when this one
#: eventually dies too, that is a flag, not a code change.)
DEFAULT_PROBE_URL = "https://www.youtube.com/watch?v=jNQXAC9IVRw"
PROBE_URL = DEFAULT_PROBE_URL

#: JS runtimes for YouTube's `n` signature challenge — **different on each side**,
#: which is not a quirk but the actual constraint:
#:
#:   - Workstation: `node`. A normal desktop has a recent enough one (verified with
#:     node 22).
#:   - Pi (armv7l): `quickjs`. `deno`/`bun` have no 32-bit ARM builds, and Raspbian
#:     trixie's node 20 is reported `(unsupported)` by yt-dlp's provider. Debian's
#:     `quickjs` package works — verified with cookies on the hardware.
#:
#: yt-dlp enables only `deno` by default, so both sides must be passed explicitly or
#: authenticated requests find NO formats at all.
LOCAL_JS_RUNTIME = "node"
REMOTE_JS_RUNTIME = "quickjs"

#: Substrings that mean "the JS challenge could not be solved" — a missing runtime
#: or missing yt-dlp-ejs scripts, NOT a cookie problem.
JS_CHALLENGE_MARKERS = ("No video formats found", "n challenge", "JavaScript runtime",
                        "challenge solver")

#: Substrings that mean "the probe video is the problem, not the credentials".
PROBE_DEAD_MARKERS = ("Video unavailable", "Private video", "has been removed",
                      "This video is not available", "video is unavailable")


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    print("  $", " ".join(cmd))
    return subprocess.run(cmd, text=True, **kw)


# --- Netscape cookies.txt ----------------------------------------------------


def parse_jar(text: str) -> list[list[str]]:
    """Parse a Netscape cookie file into its 7-column rows.

    `#HttpOnly_` is a *prefix on the domain field*, not a comment — dropping those
    lines silently discards the most important cookies, so it is preserved.
    """
    rows = []
    for line in text.splitlines():
        if not line.strip():
            continue
        if line.startswith("#") and not line.startswith("#HttpOnly_"):
            continue
        parts = line.split("\t")
        if len(parts) != 7:
            continue
        rows.append(parts)
    return rows


def write_jar(rows: list[list[str]]) -> str:
    header = (
        "# Netscape HTTP Cookie File\n"
        "# Installed by firmware/pi-ytmusic/push_cookies.py — yt-dlp rewrites this\n"
        "# file as cookies rotate, so it must stay writable by the receiver service.\n"
    )
    return header + "".join("\t".join(r) + "\n" for r in rows)


def domain_of(row: list[str]) -> str:
    return row[0].removeprefix("#HttpOnly_")


def filter_google(rows: list[list[str]]) -> list[list[str]]:
    return [r for r in rows if domain_of(r).endswith(KEEP_DOMAIN_SUFFIXES)]


def fmt_expiry(value: str) -> str:
    try:
        ts = int(value)
    except ValueError:
        return "unparseable"
    if ts == 0:
        return "session cookie (no expiry)"
    when = dt.datetime.fromtimestamp(ts, dt.timezone.utc).astimezone()
    days = (when - dt.datetime.now(dt.timezone.utc).astimezone()).days
    return f"{when:%Y-%m-%d %H:%M %Z} ({days:+d} days)"


def list_firefox_profiles() -> list[str]:
    """Profile names registered in profiles.ini, with the default marked.

    Only used to make a signed-out export actionable: the most common cause is
    naming a profile directory that exists on disk but is not the one the browser
    actually uses.
    """
    import configparser

    path = os.path.expanduser("~/.mozilla/firefox/profiles.ini")
    if not os.path.isfile(path):
        return []
    cp = configparser.ConfigParser()
    try:
        cp.read(path)
    except configparser.Error:
        return []
    out = []
    for section in cp.sections():
        if not section.startswith("Profile"):
            continue
        name = cp[section].get("Name")
        if not name:
            continue
        out.append(f"{name}{' (default)' if cp[section].get('Default') == '1' else ''}")
    return out


def inspect(rows: list[list[str]]) -> bool:
    """Report what is in the jar. Returns whether it looks like a signed-in session.

    Never prints cookie values — only names, domains and expiry.
    """
    by_name = {r[5]: r for r in rows}
    print(f"\n  cookies for Google/YouTube: {len(rows)}")

    found = [n for n in CRITICAL if n in by_name]
    if found:
        print("  login cookies:")
        for name in found:
            print(f"    {name:<20} expires {fmt_expiry(by_name[name][4])}")
        for name in ROTATING:
            if name in by_name:
                print(f"    {name:<20} expires {fmt_expiry(by_name[name][4])}"
                      f"   <- rotates hourly; not a session lifetime")
    else:
        # Say what IS there rather than listing what is absent: an anonymous jar
        # is a *recognisable* thing, and naming it points straight at the cause.
        names = sorted(by_name)
        anon = [n for n in names if n in ANONYMOUS]
        print("  login cookies:              NONE")
        print(f"  cookies present:            {', '.join(names) if names else '(none)'}")
        if anon and len(anon) == len(names):
            print("\n  Every cookie here is one a *signed-out* visit leaves behind. That browser")
            print("  profile has browsed YouTube, but is not logged in.")
        else:
            print("\n  No login cookies — this export is from a signed-out session.")
        profiles = list_firefox_profiles()
        if profiles:
            print(f"\n  Firefox profiles registered in profiles.ini: {', '.join(profiles)}")
            print("  (A profile *directory* under ~/.mozilla/firefox can exist without being")
            print("   listed here — extracting from one of those gets you a stale, and often")
            print("   signed-out, cookie store. Pass the profile NAME, not the directory.)")
        print(
            "\n  Set up a dedicated session and export from that:\n"
            "    firefox -CreateProfile ytm\n"
            "    firefox -no-remote -P ytm      # log in to music.youtube.com, then QUIT\n"
            "    ./push_cookies.py --from-browser firefox:ytm\n"
            "  ...then never open that profile again (see this file's header for why).\n"
            "  A Private Window does NOT work: Firefox keeps private cookies in memory\n"
            "  only, never in cookies.sqlite, so there is nothing for yt-dlp to read."
        )
        return False
    # Both halves of the SAPISIDHASH pair must be present or YouTube requests go
    # out unauthenticated despite the jar "having login cookies".
    if not any(n in by_name for n in SID_FAMILY):
        print("\n  No SID-family cookie (__Secure-1PSID/__Secure-3PSID/SID) — yt-dlp cannot")
        print("  build an auth header from this jar.")
        return False
    if not any(n in by_name for n in APISID_FAMILY):
        print("\n  No APISID-family cookie (SAPISID/__Secure-3PAPISID/...) — yt-dlp cannot")
        print("  build an auth header from this jar.")
        return False
    present = found
    # The useful headline number: the soonest expiry among the durable ones.
    soonest = min(
        (int(by_name[n][4]) for n in present if by_name[n][4].isdigit() and int(by_name[n][4]) > 0),
        default=0,
    )
    if soonest:
        print(f"\n  Session should last until {fmt_expiry(str(soonest))}")
    print(
        "  ...but treat that as a LOWER BOUND only: Google invalidates server-side at\n"
        "  will, and logging this session out anywhere kills the jar immediately."
    )
    return True


# --- extraction / transport --------------------------------------------------


def extract_from_browser(spec: str) -> str:
    """Dump cookies via yt-dlp's own browser support (handles Firefox's sqlite and
    Chromium's keyring-encrypted store, so we do not reimplement either).

    yt-dlp has no "just dump cookies" mode: it writes the jar as a side effect of
    a request, so this does one metadata-only fetch. That doubles as a check that
    the cookies work *here* before they are shipped anywhere.
    """
    tmp = tempfile.NamedTemporaryFile("w+", suffix=".txt", delete=False)
    # `--cookies FILE` LOADS before it dumps, and an empty file is not a valid
    # Netscape jar — yt-dlp then fails with "does not look like a Netscape format
    # cookies file" before it ever touches the browser. Seed the magic header so it
    # parses as an empty jar. (Creating the file ourselves, rather than passing a
    # bare path, is deliberate: it lets us set 0600 *before* every cookie in the
    # browser lands in it.)
    tmp.write("# Netscape HTTP Cookie File\n")
    tmp.close()
    os.chmod(tmp.name, 0o600)
    remote = ["--remote-components", REMOTE_COMPONENTS] if REMOTE_COMPONENTS else []
    r = run([
        YTDLP,
        "--js-runtimes", LOCAL_JS_RUNTIME,
        *remote,
        "--cookies-from-browser", spec,
        "--cookies", tmp.name,
        "--skip-download", "--simulate",
        "--quiet", "--no-warnings",
        "--print", "%(title)s",
        PROBE_URL,
    ], capture_output=True)
    if r.returncode != 0:
        os.unlink(tmp.name)
        err = (r.stderr or "").strip()
        if any(m in err for m in PROBE_DEAD_MARKERS):
            sys.exit(
                f"extraction failed, but on the PROBE VIDEO, not your cookies:\n{err}\n\n"
                f"Pass a different --probe-url (any public video)."
            )
        if any(m in err for m in JS_CHALLENGE_MARKERS):
            sys.exit(
                f"the cookies were read, but resolution failed on YouTube's JS challenge:\n{err}\n\n"
                f"This is NOT a cookie problem. `{YTDLP}` needs BOTH:\n"
                f"  - a JS runtime ({LOCAL_JS_RUNTIME} — check `{LOCAL_JS_RUNTIME} --version`; yt-dlp\n"
                f"    rejects versions it considers too old, logging '(unsupported)'), and\n"
                f"  - the yt-dlp-ejs solver scripts.\n"
                f"A distro yt-dlp usually lacks the latter. Easiest fix: point --ytdlp at a\n"
                f"venv that has both, the way the Pi does:\n"
                f"    python3 -m venv /tmp/ytdlp && /tmp/ytdlp/bin/pip install -U yt-dlp yt-dlp-ejs\n"
                f"    ./push_cookies.py --ytdlp /tmp/ytdlp/bin/yt-dlp --from-browser {spec}\n"
                f"(This run already passed --remote-components {REMOTE_COMPONENTS or 'none'}; note that\n"
                f" for the challenge solver yt-dlp downloads only from GitHub, so 'ejs:npm'\n"
                f" does NOT work — see https://github.com/yt-dlp/yt-dlp/wiki/EJS)"
            )
        sys.exit(
            f"extraction failed:\n{err}\n\n"
            f"Check the browser/profile spec (e.g. firefox, firefox:ytm, brave, "
            f"'chromium:Profile 1'). Firefox profile NAMES come from "
            f"~/.mozilla/firefox/profiles.ini."
        )
    print(f"  extracted, and cookies resolve locally: {(r.stdout or '').strip()!r}")
    with open(tmp.name) as f:
        text = f.read()
    # The intermediate file held EVERY site's cookies — do not leave it lying about.
    os.unlink(tmp.name)
    return text


def addon_sh(inner: str, *, stdin: str | None = None, capture: bool = False):
    """Run `inner` (a shell command) inside the add-on container over SSH.

    `inner` is quoted once for the remote shell; ssh joins its own argv with
    spaces, so without that the semicolons and redirects would be interpreted on
    the HA host instead of in the container.
    """
    argv = [
        "ssh", "-o", "BatchMode=yes", HA_HOST,
        "docker", "exec", "-i", ADDON_CONTAINER, "bash", "-c", shlex.quote(inner),
    ]
    print("  $", " ".join(argv[:6]), "…")
    return subprocess.run(argv, input=stdin, text=True,
                          capture_output=capture)


def addon_push(jar_text: str, *, force: bool) -> None:
    """Install the jar into the add-on's /data, atomically and 0600."""
    existing = addon_sh(f"stat -c %Y {ADDON_JAR} 2>/dev/null || true", capture=True)
    stamp = (existing.stdout or "").strip()
    if stamp.isdigit() and not force:
        when = dt.datetime.fromtimestamp(int(stamp)).astimezone()
        print(f"\n  A jar already exists in the add-on, last written {when:%Y-%m-%d %H:%M %Z}.")
        print("  yt-dlp rewrites it as cookies rotate, so it may be newer than what you are")
        print("  pushing — re-run with --force if you have just made a fresh export.")
        sys.exit(1)
    print(f"\n== Installing the jar into {ADDON_CONTAINER}:{ADDON_JAR} ==")
    r = addon_sh(
        f"cat > {ADDON_JAR}.tmp && chmod 600 {ADDON_JAR}.tmp && mv {ADDON_JAR}.tmp {ADDON_JAR}"
        f" && echo installed {ADDON_JAR}",
        stdin=jar_text)
    if r.returncode != 0:
        sys.exit(f"failed to install the jar in the add-on (is {ADDON_CONTAINER} running?)")


def addon_check() -> bool:
    """Prove the jar works inside the add-on, with the add-on's own yt-dlp."""
    print(f"\n== Liveness check in {ADDON_CONTAINER} ==")
    inner = (
        f"test -f {ADDON_JAR} || {{ echo NO_JAR; exit 4; }}; "
        f"{ADDON_YTDLP} --js-runtimes {ADDON_JS_RUNTIME} --cookies {ADDON_JAR} "
        f"--skip-download --simulate --no-warnings --print '%(title)s' '{PROBE_URL}'"
    )
    r = addon_sh(inner, capture=True)
    out = (r.stdout or "").strip()
    if "NO_JAR" in out:
        print("  the add-on has no cookie jar yet — push one")
        return False
    if r.returncode != 0:
        print(f"  FAILED:\n{(r.stderr or out).strip()}")
        return False
    print(f"  OK — resolved {out!r} inside the add-on")
    return True


def remote_jar_mtime(target: str) -> int | None:
    r = run(["ssh", "-o", "BatchMode=yes", target, f"stat -c %Y {REMOTE_JAR} 2>/dev/null || true"],
            capture_output=True)
    out = (r.stdout or "").strip()
    return int(out) if out.isdigit() else None


def push(target: str, jar_text: str, *, force: bool) -> None:
    existing = remote_jar_mtime(target)
    if existing is not None and not force:
        age = dt.datetime.fromtimestamp(existing).astimezone()
        print(f"\n  A jar already exists on {target}, last written {age:%Y-%m-%d %H:%M %Z}.")
        print(
            "  yt-dlp rewrites that file as cookies rotate, so it may be NEWER than what\n"
            "  you are pushing — overwriting it with an older export can roll the session\n"
            "  back and invalidate it. Re-run with --force if you really mean to replace it\n"
            "  (which is right when you have just made a fresh export from a fresh login)."
        )
        sys.exit(1)

    print(f"\n== Installing the jar on {target} ==")
    # Write to a temp path, then move into place, so a half-written file is never
    # what the resolver picks up.
    script = (
        f"set -e; mkdir -p $(dirname ~/{REMOTE_JAR}); "
        f"cat > ~/{REMOTE_JAR}.tmp; chmod 600 ~/{REMOTE_JAR}.tmp; "
        f"mv ~/{REMOTE_JAR}.tmp ~/{REMOTE_JAR}; "
        f"echo installed ~/{REMOTE_JAR}"
    )
    r = subprocess.run(["ssh", "-o", "BatchMode=yes", target, script],
                       input=jar_text, text=True)
    if r.returncode != 0:
        sys.exit("failed to install the jar on the Pi")


def check_remote(target: str) -> bool:
    """Prove the jar works *on the Pi*, with the Pi's yt-dlp."""
    print(f"\n== Liveness check on {target} ==")
    script = (
        f"if [ ! -f ~/{REMOTE_YTDLP} ]; then echo 'NO_YTDLP'; exit 3; fi; "
        f"if [ ! -f ~/{REMOTE_JAR} ]; then echo 'NO_JAR'; exit 4; fi; "
        f"~/{REMOTE_YTDLP} --js-runtimes {REMOTE_JS_RUNTIME} --cookies ~/{REMOTE_JAR} "
        f"--skip-download --simulate "
        f"--no-warnings --print '%(title)s' '{PROBE_URL}'"
    )
    r = subprocess.run(["ssh", "-o", "BatchMode=yes", target, script],
                       text=True, capture_output=True)
    out = (r.stdout or "").strip()
    if r.returncode == 3:
        print("  the Pi has no venv yt-dlp — run setup_pi_ytmusic.py first")
        return False
    if r.returncode == 4:
        print("  the Pi has no cookie jar yet — push one")
        return False
    if r.returncode != 0:
        err = (r.stderr or "").strip()
        print(f"  FAILED:\n{err}")
        if any(m in err for m in PROBE_DEAD_MARKERS):
            print("\n  ...but that is the PROBE VIDEO, not the jar. Pass a different --probe-url.")
            return False
        print(
            "\n  Read the error before acting — the three causes need opposite responses:\n"
            "   - cookies invalid/expired: make a NEW export from a fresh dedicated login\n"
            "     session. Do NOT re-push the old jar; that can invalidate the session.\n"
            "   - bot check / 'confirm you are not a bot' / missing formats: the cookies are\n"
            "     probably fine and yt-dlp is stale — update it on the Pi with\n"
            "     `systemctl --user start ytmusic-ytdlp-update.service`.\n"
            "   - video unavailable: neither; the probe video died, pass --probe-url."
        )
        return False
    print(f"  OK — resolved {out!r} on the Pi")
    return True


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    src = ap.add_mutually_exclusive_group()
    src.add_argument("--from-browser", metavar="SPEC",
                     help="Extract via yt-dlp, e.g. 'firefox:ytm', 'brave', 'chromium:Profile 1'. "
                          "Use a DEDICATED session — see the header.")
    src.add_argument("--file", metavar="PATH", help="Use an existing Netscape cookies.txt.")
    ap.add_argument("--target", default=DEFAULT_TARGET, help=f"Pi ssh target (default {DEFAULT_TARGET}).")
    ap.add_argument("--addon", action="store_true",
                    help="Target the Home Assistant ADD-ON instead of the Pi. The jar goes into "
                         "the add-on's persistent /data via `docker exec` on the HA host — that "
                         "directory is not reachable over the HA SSH add-on, which mounts only "
                         "/addons, /config, /share, /ssl and /backup.")
    ap.add_argument("--ha-host", default=DEFAULT_HA_HOST,
                    help=f"SSH target for the HA host, used with --addon (default {DEFAULT_HA_HOST}).")
    ap.add_argument("--inspect", action="store_true", help="Report what is in the jar and exit.")
    ap.add_argument("--check", action="store_true", help="Only run the liveness check on the Pi.")
    ap.add_argument("--ytdlp", default="yt-dlp",
                    help="yt-dlp to use for LOCAL extraction. Override when the system one "
                         "lacks the yt-dlp-ejs challenge-solver scripts (default: yt-dlp).")
    ap.add_argument("--remote-components", metavar="COMPONENT", default=DEFAULT_REMOTE_COMPONENTS,
                    help="Let yt-dlp fetch the JS challenge-solver script at runtime, needed "
                         "when the local yt-dlp has no yt-dlp-ejs package (which most distro "
                         f"builds do not). Default: {DEFAULT_REMOTE_COMPONENTS}. Pass 'none' "
                         "to disable, e.g. when using --ytdlp with a venv that has the package.")
    ap.add_argument("--probe-url", default=DEFAULT_PROBE_URL,
                    help="Public video used to prove resolution works. Override if the "
                         f"default ever gets taken down (default: {DEFAULT_PROBE_URL}).")
    ap.add_argument("--force", action="store_true",
                    help="Replace an existing jar on the Pi even though it may be newer.")
    args = ap.parse_args()

    global PROBE_URL, YTDLP, REMOTE_COMPONENTS, HA_HOST
    PROBE_URL = args.probe_url
    YTDLP = args.ytdlp
    HA_HOST = args.ha_host
    REMOTE_COMPONENTS = None if args.remote_components in ("none", "") else args.remote_components

    if args.check and not (args.from_browser or args.file):
        sys.exit(0 if (addon_check() if args.addon else check_remote(args.target)) else 1)

    if not (args.from_browser or args.file):
        ap.error("pass --from-browser or --file (or --check on its own)")

    if args.from_browser:
        print(f"== Extracting cookies from {args.from_browser} ==")
        raw = extract_from_browser(args.from_browser)
    else:
        with open(args.file) as f:
            raw = f.read()
        print(f"== Read {args.file} ==")

    rows = filter_google(parse_jar(raw))
    if not rows:
        sys.exit("no Google/YouTube cookies found in that jar")
    if not inspect(rows):
        sys.exit("refusing to push: this jar cannot authenticate (see above)")

    if args.inspect:
        return

    if args.addon:
        addon_push(write_jar(rows), force=args.force)
        ok = addon_check()
    else:
        push(args.target, write_jar(rows), force=args.force)
        ok = check_remote(args.target)
    print(
        "\nNothing needs restarting: mpv passes --cookies to yt-dlp per track, so the new\n"
        "jar is picked up by the next song." if ok else
        "\nThe jar is installed but did not resolve — see above before casting.")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
