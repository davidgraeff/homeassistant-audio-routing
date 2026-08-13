#!/usr/bin/env python3
"""What is in a YouTube cookie jar, and whether it can authenticate.

The *rules* only — parsing, which cookie families matter, how an expiry is read,
and the verdict. No transport, no printing, no yt-dlp.

Shared on purpose, because two very different callers ask the same question:

  - `../push_cookies.py`, on a workstation, about a jar it just extracted from a
    browser. It keeps all of its own printing (including the browser-profile
    hints, which only make sense there) and builds it from `analyse()`.
  - `admin.js`, inside the receiver, about a jar someone uploaded through the web
    UI. It shells out to `--json`, because that answer has to reach a browser.

A jar the web UI accepts and the workstation tool refuses — or the reverse — would
be a bug nobody could explain from either end, so the vocabulary lives here once.

Never returns cookie *values*: names, domains and expiry only. The file is a live
credential for a Google account (see push_cookies.py's header).
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import sys

#: Only these domains are kept. A browser jar contains every site you have ever
#: visited; none of that has any business reaching a receiver.
KEEP_DOMAIN_SUFFIXES = (".youtube.com", "youtube.com", ".google.com", "google.com")

#: The cookies that actually carry the login. If none are present, the export came
#: from a session that was not signed in.
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
#: Cookies a *signed-out* visit leaves behind. Recognised only so a report can say
#: "this is an anonymous session" instead of listing absences.
ANONYMOUS = ("VISITOR_INFO1_LIVE", "VISITOR_PRIVACY_METADATA", "YSC", "PREF", "GPS",
             "CONSENT", "SOCS", "NID", "DEVICE_INFO", "__Secure-YEC", "wide")


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


def write_jar(rows: list[list[str]], *, installed_by: str) -> str:
    """Serialise rows back to a Netscape jar, naming whoever installed it.

    The "stay writable" note is not decoration: `yt-dlp --cookies FILE` dumps the
    rotated jar back into FILE, so a read-only copy loses every refresh.
    """
    header = (
        "# Netscape HTTP Cookie File\n"
        f"# Installed by {installed_by} — yt-dlp rewrites this\n"
        "# file as cookies rotate, so it must stay writable by the receiver service.\n"
    )
    return header + "".join("\t".join(r) + "\n" for r in rows)


def domain_of(row: list[str]) -> str:
    return row[0].removeprefix("#HttpOnly_")


def filter_google(rows: list[list[str]]) -> list[list[str]]:
    return [r for r in rows if domain_of(r).endswith(KEEP_DOMAIN_SUFFIXES)]


# --- expiry ------------------------------------------------------------------


def expiry_of(value: str) -> dict:
    """Structured reading of a jar's expiry column.

    `text` is what a human should be shown; the rest is for a UI that wants to
    colour or sort by it. A `0` is a session cookie, which is a real answer and
    not a missing one.
    """
    try:
        ts = int(value)
    except ValueError:
        return {"kind": "unparseable", "epoch": None, "iso": None, "days": None,
                "text": "unparseable"}
    if ts == 0:
        return {"kind": "session", "epoch": 0, "iso": None, "days": None,
                "text": "session cookie (no expiry)"}
    when = dt.datetime.fromtimestamp(ts, dt.timezone.utc).astimezone()
    days = (when - dt.datetime.now(dt.timezone.utc).astimezone()).days
    return {"kind": "absolute", "epoch": ts, "iso": when.isoformat(), "days": days,
            "text": f"{when:%Y-%m-%d %H:%M %Z} ({days:+d} days)"}


def fmt_expiry(value: str) -> str:
    """One-line expiry, as the CLI has always printed it."""
    return expiry_of(value)["text"]


# --- the verdict -------------------------------------------------------------


def analyse(rows: list[list[str]]) -> dict:
    """Decide whether `rows` is a jar yt-dlp can authenticate with.

    Returns a `code` (stable, for callers to branch on), a one-line `summary` (for
    people), and the supporting detail. `ok` is the single question both callers
    actually gate on: pushing, or accepting an upload.
    """
    by_name = {r[5]: r for r in rows}
    names = sorted(by_name)
    login = [{"name": n, "expiry": expiry_of(by_name[n][4])} for n in CRITICAL if n in by_name]
    rotating = [{"name": n, "expiry": expiry_of(by_name[n][4])} for n in ROTATING if n in by_name]
    anonymous_only = bool(names) and all(n in ANONYMOUS for n in names)

    base = {
        "count": len(rows),
        "names": names,
        "login": login,
        "rotating": rotating,
        "anonymous_only": anonymous_only,
        "expiry": None,
    }

    if not rows:
        return {**base, "ok": False, "code": "empty",
                "summary": "No Google or YouTube cookies in this file."}
    if not login:
        # Say what IS there rather than listing what is absent: an anonymous jar is
        # a *recognisable* thing, and naming it points straight at the cause.
        return {**base, "ok": False, "code": "anonymous" if anonymous_only else "signed_out",
                "summary": (
                    "Every cookie here is one a signed-out visit leaves behind — that browser "
                    "profile has browsed YouTube, but is not logged in."
                    if anonymous_only else
                    "No login cookies: this export is from a signed-out session.")}
    # Both halves of the SAPISIDHASH pair must be present, or requests go out
    # unauthenticated despite the jar "having login cookies".
    if not any(n in by_name for n in SID_FAMILY):
        return {**base, "ok": False, "code": "no_sid_family",
                "summary": "No SID-family cookie (__Secure-1PSID/__Secure-3PSID/SID) — "
                           "yt-dlp cannot build an auth header from this jar."}
    if not any(n in by_name for n in APISID_FAMILY):
        return {**base, "ok": False, "code": "no_apisid_family",
                "summary": "No APISID-family cookie (SAPISID/__Secure-3PAPISID/…) — "
                           "yt-dlp cannot build an auth header from this jar."}

    # The useful headline: the soonest expiry among the *durable* login cookies.
    # A lower bound only — Google invalidates server-side at will, and logging this
    # session out anywhere kills the jar immediately.
    durable = [e["expiry"] for e in login if e["expiry"]["kind"] == "absolute" and e["expiry"]["epoch"] > 0]
    soonest = min(durable, key=lambda e: e["epoch"], default=None)
    return {**base, "ok": True, "code": "ok", "expiry": soonest,
            "summary": f"Signed-in session, {len(rows)} Google/YouTube cookies"
                       + (f", good until at least {soonest['text']}." if soonest else ".")}


# --- reading a failed liveness probe -----------------------------------------
#
# Whoever installs a jar then proves it resolves a video: push_cookies.py over ssh,
# the web UI with the receiver's own yt-dlp. Both then face the same question — was
# that the *cookies*? — so the answer is one rule set.

#: A video used only to prove that resolution works. "Me at the zoo" — the oldest
#: video on the platform, so about as unlikely to disappear as anything on YouTube.
#: (The obvious choice, yt-dlp's own `BaW_jenozKc` test video, is *gone*: it now
#: returns "Video unavailable", which made every probe fail for a reason that had
#: nothing to do with cookies. Hence `probe_dead` below: when this one eventually
#: dies too, that is a flag, not a code change.)
DEFAULT_PROBE_URL = "https://www.youtube.com/watch?v=jNQXAC9IVRw"

#: Substrings that mean "the JS challenge could not be solved" — a missing runtime
#: or missing yt-dlp-ejs scripts, NOT a cookie problem.
JS_CHALLENGE_MARKERS = ("No video formats found", "n challenge", "JavaScript runtime",
                        "challenge solver")

#: Substrings that mean "the probe video is the problem, not the credentials".
PROBE_DEAD_MARKERS = ("Video unavailable", "Private video", "has been removed",
                      "This video is not available", "video is unavailable")


def classify_probe_error(text: str) -> dict:
    """Why a liveness probe failed, from yt-dlp's stderr.

    The distinction that matters: two of the three outcomes are *not* the jar, and
    replacing a perfectly good jar is the wrong response to either.
    """
    if any(m in text for m in PROBE_DEAD_MARKERS):
        return {"code": "probe_dead",
                "message": "The probe video is unavailable — this says nothing about the cookies. "
                           "Pick another public video to probe with."}
    if any(m in text for m in JS_CHALLENGE_MARKERS):
        return {"code": "js_challenge",
                "message": "The cookies were read, but resolution failed on YouTube's JS challenge "
                           "— a JavaScript-runtime problem, not a credential one."}
    return {"code": "unknown",
            "message": "Resolution failed. The output below is yt-dlp's own."}


def analyse_text(text: str) -> dict:
    """`analyse()` straight from file contents, Google-filtered as it will be stored.

    This is the whole pipeline a caller wants: it is the *filtered* jar that gets
    written, so it must be the filtered jar that gets judged.
    """
    rows = filter_google(parse_jar(text))
    return analyse(rows)


def _cli() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("path", nargs="?", help="Netscape cookies.txt to inspect, or '-' for stdin.")
    ap.add_argument("--json", action="store_true",
                    help="Emit the verdict as JSON (what admin.js consumes).")
    ap.add_argument("--classify-probe-error", action="store_true",
                    help="Read yt-dlp stderr instead of a jar and say what failed (JSON).")
    ap.add_argument("--print-probe-url", action="store_true",
                    help="Print the liveness-probe video and exit, so callers that run "
                         "yt-dlp themselves (admin.js) need no copy of it.")
    args = ap.parse_args()

    if args.print_probe_url:
        print(DEFAULT_PROBE_URL)
        return
    if not args.path:
        ap.error("pass a jar path, or '-' to read one on stdin")

    text = sys.stdin.read() if args.path == "-" else open(args.path, encoding="utf-8", errors="replace").read()
    if args.classify_probe_error:
        json.dump(classify_probe_error(text), sys.stdout)
        sys.stdout.write("\n")
        return
    verdict = analyse_text(text)
    if args.json:
        json.dump(verdict, sys.stdout)
        sys.stdout.write("\n")
    else:
        print(verdict["summary"])
        for entry in verdict["login"]:
            print(f"  {entry['name']:<20} expires {entry['expiry']['text']}")
        for entry in verdict["rotating"]:
            print(f"  {entry['name']:<20} expires {entry['expiry']['text']}"
                  "   <- rotates hourly; not a session lifetime")
    # Exit status is the verdict, so a shell can gate on it.
    sys.exit(0 if verdict["ok"] else 1)


if __name__ == "__main__":
    _cli()
