#!/usr/bin/env python3
"""Long-lived yt-dlp resolver, spoken to over a unix socket.

WHY THIS EXISTS
---------------
`resolver.js` used to spawn one `yt-dlp` per track, and everything expensive about
an authenticated resolve is **process-local state** that a fresh process cannot
inherit:

  - the Python interpreter start plus the `yt_dlp` import (measured ~2.3 s on a Pi
    Zero 2 W for `yt-dlp --version` alone, i.e. before any work happens),
  - the downloaded player JS, ~2-3 MB, held in the extractor's `_code_cache`,
  - derived player data in `_player_cache` (`sts`, sig functions, solved `n`
    values),
  - PO tokens, whose default cache provider is *memory*,
  - the InnerTube client-version probes.

Keeping one process alive across tracks amortises all of that over the session, so
only the parts that are genuinely per-video remain: the `/player` request and the
`n` challenge itself.

THE OTHER HALF: THE JS CHALLENGE
-------------------------------
Process warmth does nothing for YouTube's `n`/`sig` challenge, which yt-dlp solves by
spawning a JS runtime per resolve and re-parsing the 2.8 MB player JS inside it — 15 s
on the add-on host, ~0 ms of which is the actual solving. That is handled separately by
`jsc_resident.py`, which keeps the solver closures in a resident `node`. The two
compose: this file makes the *process* cheap, that one makes the *challenge* cheap.

AND THE PRICE OF STAYING ALIVE
------------------------------
A long-lived yt-dlp also keeps **session-scoped** state, and some of it expires. Proven
on the add-on: after ~45 min this daemon produced URLs that resolved fine and then
answered 403 on fetch — for two videos in a row — while a pristine process with the
same cookie jar, disk cache and solver produced working URLs for both. The per-track
spawn refreshed that state by construction; here it takes three explicit mechanisms:
`MAX_WARM_AGE_S`, the `invalidate` op (the client is the only party that can see a
verify failure), and an exit when a rebuilt session fails again.

Every reply carries `elapsed_ms` and a `cold` flag so the split between the two is
measurable on the hardware instead of guessed at:

    journalctl --user -u ytmusic-receiver -f | grep '\\[resolver\\]'

PROTOCOL
--------
Newline-delimited JSON in both directions, one object per line, correlated by `id`
exactly like mpv's own IPC (`mpv.js`) so the client side is the same shape:

    -> {"id": 1, "op": "resolve", "video_id": "...", "mode": "local"}
    <- {"id": 1, "url": "https://...", "elapsed_ms": 2140, "cold": false}
    <- {"id": 1, "error": "..."}                       (never both)
    -> {"id": 2, "op": "ping"}
    <- {"id": 2, "version": "2026.06.09", "modes": ["remote", "local"]}
    -> {"id": 3, "op": "invalidate", "mode": "local", "reason": "HTTP 403"}
    <- {"id": 3, "ok": true}

The socket is trusted: it is mode 0600 in a private runtime directory and the only
client is the receiver that spawned this process. No request can name a URL — only
a video id, which is substituted into a fixed template, so this cannot be turned
into a general-purpose fetcher.

CONFIGURATION
-------------
The resolve modes are passed as yt-dlp *command lines*, one `--mode LABEL=ARGV`
per mode, and turned into API options by yt-dlp's own `parse_options`. That is
deliberate: the previous implementation spawned these exact arguments, so parity
with what mpv's `ytdl_hook` would have resolved is guaranteed by construction
rather than by a hand-maintained translation table.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import signal
import socket
import sys
import urllib.parse
import threading
import time

# Running this file directly already puts its directory on sys.path; said explicitly so
# that importing it from elsewhere (a test harness) does not fail on a sibling module.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import jsc_resident  # noqa: E402

#: Written to a cookie jar at most this often. `--cookies FILE` makes the jar a
#: live, rotating credential that yt-dlp normally dumps back when the process
#: exits — and this process does not exit. Without this, cookies rotated in memory
#: would be lost on every restart and the on-disk jar would slowly go stale.
COOKIE_SAVE_INTERVAL_S = 300
#: Cookie file reads/writes are cheap enough to hash on every resolve (a jar is a
#: few kB), and hashing is what makes "did somebody push a new jar?" answerable —
#: see SharedCookieJar.
COOKIE_HASH_CHUNK = 1 << 16
#: Throw the warm `YoutubeDL` away after this many consecutive failures in a mode.
#:
#: Warm state cuts both ways: yt-dlp caches *negative* results too, and a JS
#: challenge provider that failed once is marked unavailable for the life of the
#: process. Observed while testing this daemon — after one failed solve, later
#: resolves stopped reporting `[jsc:node] Solving JS challenges using node` and
#: warned "No supported JavaScript runtime could be found" instead, on the same
#: instance. A fresh process per track used to paper over that by construction.
#:
#: Two, not one: a single failure is usually the *video* (region-blocked, taken
#: down), and discarding a warm session for that would make every such track cost a
#: full cold resolve for the next one as well.
RECYCLE_AFTER_FAILURES = 2
#: Rebuild a mode's `YoutubeDL` — and clear the process-wide PO-token cache — this often.
#:
#: NOT because age is the trigger: a session *seconds* old still produced 403 URLs on the
#: add-on, so that theory is dead. It is kept because a rebuild is the cheapest moment to
#: also run `clear_pot_cache()`, and periodic clearing is insurance against the per-video
#: poisoning described there. A rebuild costs about a second (the player is re-downloaded,
#: the client re-probed) while the resident worker's closures and both disk caches survive.
MAX_WARM_AGE_S = 300
#: Window in which repeated invalidations are worth remarking on.
#:
#: This used to exit the process ("a rebuilt session failed again"), which was wrong: the
#: rejections that trigger it arrive in bursts that hit *every* path — a fresh process
#: included — so exiting bought nothing and cost a restart plus a prewarm resolve, i.e.
#: more requests aimed at whatever was already rejecting them. Now it only logs, and
#: `MAX_WARM_AGE_S` remains the mechanism for genuinely stale state.
INVALIDATE_ESCALATION_S = 300
#: Prewarm target: yt-dlp's own former test video ("Me at the zoo"), matching
#: `setup_pi_ytmusic.py`'s PROBE_URL. Resolved once at startup — while nothing is
#: playing — so the *first* real track finds the caches populated. This is where
#: the largest single win sits: without it the first track of a session still pays
#: the full cold price.
PREWARM_VIDEO_ID = 'jNQXAC9IVRw'
#: Clients the diagnostic sweeps. Every valid `player_client` this yt-dlp knows, so a
#: report says which ones could have played the track and which could not.
DIAGNOSE_CLIENTS = ['(default)', 'web', 'web_safari', 'web_music', 'web_embedded', 'mweb',
                    'tv', 'tv_simply', 'ios', 'android_vr']
#: At most one diagnostic per this many seconds. Each one costs a resolve per client, so it
#: must never turn a bad window into a self-inflicted request storm.
DIAGNOSE_INTERVAL_S = 900
#: Where reports go. In /data on the add-on, so they survive a restart and can be handed
#: over verbatim.
DIAGNOSE_DIR = os.environ.get('YTCR_DIAGNOSE_DIR') or '/data/403-diagnostics'


#: Cap on one logged message. yt-dlp messages can carry a whole HTTP response — a
#: public cipher server behind nginx answered 504 once and its error page, padding
#: comments and all, arrived in the receiver's journal as a dozen unattributable
#: lines. Long enough to keep a real error's substance, short enough that a body
#: cannot bury the log.
LOG_MAX_CHARS = 500


def log(message: str) -> None:
    """Diagnostics go to stderr, which the client forwards into the receiver's log.

    EVERY line is prefixed, not just the first. A multi-line message used to emit its
    continuation lines bare, and a bare line in that journal cannot be traced back to
    the process that wrote it — which is exactly the position the 504 page above left
    us in.
    """
    for line in str(message).splitlines() or ['']:
        print(f'[ytdlpd] {line}', file=sys.stderr, flush=True)


def clear_pot_cache() -> int:
    """Empty yt-dlp's PO-token cache. Returns how many entries went, or -1.

    This one is **module-level** (`pot/_registry.py: _pot_memory_cache = Indirect({})`),
    so unlike everything else a rebuilt `YoutubeDL` inherits it, and only a new *process*
    ever gets a clean one. That matters because entries are keyed per video under
    YouTube's "bind GVS PO Token to video ID" experiment — which is exactly the failure
    shape observed on the add-on: from one long-lived daemon, `UnAHtKGgkQ8` and
    `jNQXAC9IVRw` verified 206 while `dQw4w9WgXcQ` and `kJQP7kiw5Fk` answered 403, in the
    same second, and a fresh process resolved all four fine.
    """
    try:
        from yt_dlp.extractor.youtube.pot._registry import _pot_memory_cache
        cache = _pot_memory_cache.value.get('cache')
        if cache is None:
            return 0
        count = len(cache)
        cache.clear()
        return count
    except Exception as e:  # noqa: BLE001 — an internal cache; never fatal
        log(f'could not clear the PO-token cache: {type(e).__name__}: {e}')
        return -1


def flatten(msg: str) -> str:
    """One line, bounded length — for text that came from somewhere else."""
    collapsed = ' '.join(str(msg).split())
    if len(collapsed) <= LOG_MAX_CHARS:
        return collapsed
    return f'{collapsed[:LOG_MAX_CHARS]}… (+{len(collapsed) - LOG_MAX_CHARS} chars)'


class YtdlpLogger:
    """Routes yt-dlp's own output into ours, tagged, flattened and bounded."""

    def debug(self, msg: str) -> None:
        # yt-dlp prefixes real debug lines with '[debug] '; everything else on this
        # channel is ordinary stdout-ish chatter.
        log(flatten(msg) if msg.startswith('[debug] ') else f'[info] {flatten(msg)}')

    def info(self, msg: str) -> None:
        log(f'[info] {flatten(msg)}')

    def warning(self, msg: str) -> None:
        log(f'[warn] {flatten(msg)}')

    def error(self, msg: str) -> None:
        log(f'[error] {flatten(msg)}')


class SharedCookieJar:
    """One cookie jar object for every mode, reloaded in place when it changes on disk.

    THE BUG THIS REPLACES
    ---------------------
    The first version gave each mode its own `YoutubeDL`, hence its own jar, and
    treated any change to the file's mtime as "the operator pushed a new jar" —
    discarding that mode's warm caches. Observed on the add-on: *both* modes reported
    "cookie jar changed on disk" during a single track change, and the fallback resolve
    that followed a failed remote-cipher attempt cost **12.2 s cold** instead of
    running warm.

    Two things were wrong with it.

    1. **Almost every write is ours.** `--cookies FILE` makes yt-dlp dump the jar back
       on `close()`, and this daemon saves it periodically — so with two modes sharing
       one file, each mode's rotation looked like an external push to the other, and
       the two took turns going cold. mpv's `ytdl_hook` (the fallback resolver) writes
       it too. Tracking the file's *content* against what we last read or wrote makes
       our own writes invisible, which is the only way to spot a real push.
    2. **A new jar is no reason to throw away the player caches.** Cookies are not in
       `_code_cache`/`_player_cache`; they are in the jar object. So reload the jar in
       place and keep the expensive state — a push costs nothing but a re-read.

    Sharing one jar has a second, independent benefit: two `YoutubeDL`s rotating the
    same Google session against each other is precisely the hazard `push_cookies.py`
    warns about at length. Now there is one in-memory truth and one writer.
    """

    def __init__(self, path: str) -> None:
        self.path = path
        self.lock = threading.Lock()
        self._hash = None
        self._saved_at = 0.0
        from yt_dlp.cookies import YoutubeDLCookieJar
        self.jar = YoutubeDLCookieJar(path)
        if os.access(path, os.R_OK):
            self.jar.load()
            self._hash = self._digest()
            log(f'cookie jar loaded: {path} ({len(self.jar)} cookies)')
        else:
            log(f'cookie jar {path} is not readable — resolving anonymously')
        self._saved_at = time.monotonic()

    def _digest(self):
        """Content hash of the jar on disk, or None if it is not there."""
        import hashlib
        try:
            h = hashlib.sha256()
            with open(self.path, 'rb') as f:
                while chunk := f.read(COOKIE_HASH_CHUNK):
                    h.update(chunk)
            return h.digest()
        except OSError:
            return None

    def attach(self, ydl) -> None:
        """Point a `YoutubeDL` at the shared jar.

        `YoutubeDL.cookiejar` is a `functools.cached_property`, so assigning it fills
        the instance's cache before anything reads it — the per-instance jar is never
        constructed. Done right after building the instance, before the first request.
        """
        ydl.cookiejar = self.jar

    def refresh_if_pushed(self) -> None:
        """Re-read the jar iff its content differs from what we last read or wrote.

        A reload while another mode is mid-resolve can briefly present an incomplete
        jar, so that resolve may fail and escalate to the next attempt. That is a fair
        trade for a case that only happens when someone actually runs
        `push_cookies.py`.
        """
        with self.lock:
            current = self._digest()
            if current is None or current == self._hash:
                return
            try:
                self.jar.clear()
                self.jar.load()
            except Exception as e:  # noqa: BLE001 — a half-written push must not be fatal
                log(f'cookie jar {self.path} could not be reloaded ({e}) — keeping the '
                    f'one in memory')
                return
            self._hash = current
            log(f'cookie jar was replaced on disk — reloaded {len(self.jar)} cookies, '
                f'keeping warm resolver state')

    def maybe_save(self, *, force: bool = False) -> None:
        """Flush rotated cookies back, at most every COOKIE_SAVE_INTERVAL_S."""
        with self.lock:
            if not force and time.monotonic() - self._saved_at < COOKIE_SAVE_INTERVAL_S:
                return
            try:
                self.jar.save()
            except Exception as e:  # noqa: BLE001 — a failed save must not fail a resolve
                log(f'could not save cookies to {self.path}: {e}')
                return
            self._saved_at = time.monotonic()
            # Record what we just wrote, or the next resolve reads our own write as a
            # push — the exact loop this class exists to break.
            self._hash = self._digest()


class Mode:
    """One resolve strategy — a label plus the yt-dlp options it implies.

    Holds its own `YoutubeDL`, because the caches that make this worth doing hang
    off the extractor instances inside it. The lock serialises extraction: a
    `YoutubeDL` is not thread-safe, and two concurrent resolves through one
    instance would race on exactly the shared state we are trying to reuse.
    """

    def __init__(self, label: str, argv: list[str], cookies: SharedCookieJar | None) -> None:
        self.label = label
        self.argv = argv
        self.cookies = cookies
        self.lock = threading.Lock()
        self._ydl = None
        self._failures = 0
        self._built_at = 0.0

    def _build(self):
        from yt_dlp import YoutubeDL, parse_options

        # Printing options are for the fallback *spawn* path, which has only stdout to
        # answer on. Here the URL and its headers come out of the info dict, so they are
        # dropped rather than left to write into this daemon's log.
        argv = []
        skip_next = False
        for arg in self.argv:
            if skip_next:
                skip_next = False
                continue
            if arg in ('-g', '--get-url'):
                continue
            if arg == '--print':
                skip_next = True
                continue
            argv.append(arg)
        opts = parse_options(argv).ydl_opts
        opts.update({
            'logger': YtdlpLogger(),
            'noprogress': True,
            # NOTE: deliberately NOT `no_color` — `parse_options` already resolved
            # colour policy into `color`, and setting both makes yt-dlp log
            # "Overwriting params from color with no_color" on every build. Output
            # goes to a pipe, which it detects on its own.
            # Not a download client: never touch the filesystem for media, and do
            # not let a playlist page turn one video id into a batch of resolves.
            'skip_download': True,
            'noplaylist': True,
        })
        log(f'mode {self.label}: {shlex.join(argv)}')
        self._ydl = YoutubeDL(opts, auto_init=True)
        self._built_at = time.monotonic()
        if self.cookies:
            self.cookies.attach(self._ydl)

    def recycle(self, reason: str) -> None:
        """Throw away this mode's session state; the next resolve rebuilds it.

        Called when the client reports that a URL we produced did not actually play
        (`resolver.js#verify`), and on age. The resident JS worker is NOT affected — the
        expensive part stays warm, only yt-dlp's session does not.
        """
        with self.lock:
            if self._ydl is None:
                return
            log(f'mode {self.label}: discarding session state ({reason})')
            self._close()
        # Outside the per-mode lock: this cache is process-wide, not per mode.
        cleared = clear_pot_cache()
        if cleared > 0:
            log(f'cleared {cleared} PO-token cache entr{"y" if cleared == 1 else "ies"}')

    def _close(self) -> None:
        if self._ydl:
            try:
                self._ydl.close()
            except Exception:  # noqa: BLE001
                pass
            self._ydl = None

    def resolve(self, video_id: str) -> dict:
        """Resolve one video id to a direct URL. Raises on failure."""
        with self.lock:
            # Timed from here, including a rebuild, so `elapsed_ms` is what the client
            # actually waited for rather than the flattering half of it.
            started = time.monotonic()
            if self.cookies:
                self.cookies.refresh_if_pushed()
            if self._ydl is not None and started - self._built_at > MAX_WARM_AGE_S:
                age = (started - self._built_at) / 60
                log(f'mode {self.label}: session state is {age:.0f} min old — rebuilding')
                self._close()
            cold = self._ydl is None
            if cold:
                self._build()
            try:
                info = self._ydl.extract_info(
                    f'https://www.youtube.com/watch?v={video_id}', download=False)
            except Exception:
                self._failures += 1
                if self._failures >= RECYCLE_AFTER_FAILURES:
                    log(f'mode {self.label}: {self._failures} consecutive failures — '
                        f'discarding warm state so the next resolve starts clean')
                    self._close()
                    self._failures = 0
                raise
            self._failures = 0
            elapsed_ms = int((time.monotonic() - started) * 1000)
            # `extract_info` returns None instead of raising when yt-dlp handled the
            # failure non-fatally — a video whose formats were all dropped, typically
            # because the JS challenge or a PO token did not come through. Seen in
            # testing as `AttributeError: 'NoneType' has no attribute 'get'`, which
            # told the caller nothing.
            if info is None:
                raise RuntimeError(
                    'yt-dlp found no usable formats (challenge or PO token failure?)')
            # `bestaudio/best` selects a single format, so the chosen URL is on the
            # top-level info dict. `requested_downloads` covers the merged case, and
            # is checked second so a format string that picks two streams degrades
            # to an error rather than a silently wrong URL.
            url = info.get('url')
            if not url:
                requested = info.get('requested_downloads') or []
                if len(requested) == 1:
                    url = requested[0].get('url')
            if not url:
                raise RuntimeError(
                    f'no direct URL in the extracted info (format={info.get("format_id")})')
            if self.cookies:
                self.cookies.maybe_save()
            # The headers are part of the answer, not decoration: some googlevideo URLs
            # answer **403 to a request without a matching User-Agent** and 206 with one
            # (measured on the add-on, same URL, three fetchers). Whoever fetches this
            # URL — the verify probe, or mpv — has to send these.
            return {'url': url, 'elapsed_ms': elapsed_ms, 'cold': cold,
                    'format_id': info.get('format_id'),
                    'headers': info.get('http_headers') or {}}


class Server:
    def __init__(self, socket_path: str, modes: dict[str, Mode]) -> None:
        self.socket_path = socket_path
        self.modes = modes
        self.sock = None
        self._stopping = threading.Event()
        #: When the last `invalidate` arrived, for the escalation in `_invalidate`.
        self._last_invalidate = 0.0
        #: When the last diagnostic ran, for DIAGNOSE_INTERVAL_S.
        self._last_diagnose = 0.0
        #: The shared cookie jar, so a diagnostic resolves with the same credentials.
        self.cookies = None

    # --- socket lifecycle ---------------------------------------------------

    def claim_socket(self) -> None:
        """Bind `socket_path`, removing it only if it is genuinely stale.

        Same reasoning as `MpvClient#claimSocketPath`: a bind is a kernel-enforced
        mutex, so an unconditional unlink is what would *allow* two daemons on one
        path. A socket nobody answers on is stale; one that accepts is a live daemon
        and a hard stop.
        """
        # A unix socket path is capped at ~108 bytes by `struct sockaddr_un`, and
        # bind() reports that as a bare `OSError: AF_UNIX path too long` with no clue
        # which path it meant.
        if len(self.socket_path.encode()) > 100:
            raise SystemExit(
                f'socket path is too long for AF_UNIX ({len(self.socket_path)} bytes): '
                f'{self.socket_path}')
        if os.path.exists(self.socket_path):
            probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            probe.settimeout(1.0)
            try:
                probe.connect(self.socket_path)
            except OSError:
                log(f'removing stale socket {self.socket_path}')
                os.unlink(self.socket_path)
            else:
                probe.close()
                raise SystemExit(
                    f'another resolver daemon is listening on {self.socket_path}')
            finally:
                probe.close()

        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.bind(self.socket_path)
        # The jar and the resolved URLs are session credentials; nobody else on the
        # box has any business talking to this.
        os.chmod(self.socket_path, 0o600)
        self.sock.listen(4)

    def stop(self, *_) -> None:
        self._stopping.set()
        try:
            if self.sock:
                self.sock.close()
        except OSError:
            pass

    # --- request handling ---------------------------------------------------

    def _reply(self, conn, write_lock: threading.Lock, payload: dict) -> None:
        line = (json.dumps(payload) + '\n').encode()
        with write_lock:
            try:
                conn.sendall(line)
            except OSError:
                pass  # client went away mid-resolve; nothing to do about it

    def _handle_request(self, conn, write_lock: threading.Lock, request: dict) -> None:
        rid = request.get('id')
        op = request.get('op', 'resolve')
        if op == 'ping':
            from yt_dlp.version import __version__ as ytdlp_version
            self._reply(conn, write_lock, {
                'id': rid, 'version': ytdlp_version, 'modes': list(self.modes)})
            return
        if op == 'diagnose':
            video_id = request.get('video_id') or ''
            if not video_id or not all(c.isalnum() or c in '-_' for c in video_id):
                self._reply(conn, write_lock, {'id': rid, 'error': 'bad video id'})
                return
            now = time.monotonic()
            if now - self._last_diagnose < DIAGNOSE_INTERVAL_S:
                self._reply(conn, write_lock, {'id': rid, 'skipped': 'throttled'})
                return
            self._last_diagnose = now
            mode = next(iter(self.modes.values()))
            log(f'diagnosing the 403 on {video_id} across '
                f'{len(DIAGNOSE_CLIENTS)} clients — this takes a minute')
            path = diagnose(video_id, request.get('reason') or '', mode.argv, self.cookies)
            self._reply(conn, write_lock, {'id': rid, 'report': path})
            return
        if op == 'invalidate':
            self._invalidate(request.get('mode'), request.get('reason') or 'client request',
                             request.get('video_id'), bool(request.get('proactive')),
                             bool(request.get('escalate')))
            self._reply(conn, write_lock, {'id': rid, 'ok': True})
            return
        if op != 'resolve':
            self._reply(conn, write_lock, {'id': rid, 'error': f'unknown op {op!r}'})
            return

        video_id = request.get('video_id') or ''
        # A video id, not a URL: the id is substituted into a fixed watch URL, and
        # anything that is not an id is refused before yt-dlp ever sees it.
        if not video_id or not all(c.isalnum() or c in '-_' for c in video_id):
            self._reply(conn, write_lock, {'id': rid, 'error': f'bad video id {video_id!r}'})
            return
        mode = self.modes.get(request.get('mode') or '')
        if not mode:
            self._reply(conn, write_lock, {
                'id': rid, 'error': f'unknown mode {request.get("mode")!r}'})
            return
        try:
            result = mode.resolve(video_id)
        except Exception as e:  # noqa: BLE001 — every failure is the client's to handle
            self._reply(conn, write_lock, {'id': rid, 'error': f'{type(e).__name__}: {e}'})
            return
        log(f'{video_id} via {mode.label} in {result["elapsed_ms"] / 1000:.1f}s'
            f'{" (cold)" if result["cold"] else ""} fmt={result.get("format_id")}')
        self._reply(conn, write_lock, {'id': rid, **result})

    def _invalidate(self, label: str | None, reason: str, video_id: str | None = None,
                    proactive: bool = False, request_escalate: bool = False) -> None:
        """A URL we produced did not work; drop the session state behind it.

        The client (`resolver.js#verify`) is the only thing that can know this: the
        resolve itself succeeded, and the rejection only appears when the URL is fetched.

        Deliberately does NOT escalate to exiting. Observed on the add-on: rejections
        arrive in bursts that hit the remote cipher, the resident worker, a freshly
        spawned process and mpv's own resolution alike, and clear on their own — so a
        restart adds load without changing the outcome.
        """
        now = time.monotonic()
        if request_escalate:
            targets = [self.modes[label]] if label in self.modes else list(self.modes.values())
            for mode in targets:
                mode.recycle(reason)
            log('a rebuilt session failed again — exiting so a fresh process takes over '
                '(process-level state a rebuild cannot reach; see clear_pot_cache)')
            self.stop()
            return
        if proactive:
            # A refresh asked for in advance (a sender connecting), not a failure report:
            # it must not look like a burst of rejections.
            targets = [self.modes[label]] if label in self.modes else list(self.modes.values())
            for mode in targets:
                mode.recycle(reason)
            return
        if now - self._last_invalidate < INVALIDATE_ESCALATION_S:
            log(f'second invalidation within {INVALIDATE_ESCALATION_S}s '
                f'({video_id or "?"}) — rejections may be arriving in a burst; '
                f'not restarting')
        self._last_invalidate = now
        targets = [self.modes[label]] if label in self.modes else list(self.modes.values())
        for mode in targets:
            mode.recycle(reason)

    def _handle_conn(self, conn) -> None:
        # One thread per request, so a slow prefetch cannot head-of-line block a
        # play arriving on the same connection. The real serialisation is per mode,
        # inside Mode.resolve.
        write_lock = threading.Lock()
        buffer = b''
        try:
            while not self._stopping.is_set():
                chunk = conn.recv(65536)
                if not chunk:
                    break
                buffer += chunk
                while b'\n' in buffer:
                    line, buffer = buffer.split(b'\n', 1)
                    if not line.strip():
                        continue
                    try:
                        request = json.loads(line)
                    except ValueError:
                        log(f'unparseable request: {line[:200]!r}')
                        continue
                    threading.Thread(
                        target=self._handle_request, args=(conn, write_lock, request),
                        daemon=True).start()
        except OSError:
            pass
        finally:
            conn.close()

    def serve(self) -> None:
        log(f'listening on {self.socket_path} (pid {os.getpid()})')
        # A timeout, because closing the listening socket from another thread does NOT
        # reliably wake a blocked `accept()` on Linux — measured: the escalation path
        # logged "exiting" and then sat here until the next connection arrived. This
        # affects SIGTERM the same way, so the loop has to come up for air instead.
        self.sock.settimeout(1.0)
        while not self._stopping.is_set():
            try:
                conn, _ = self.sock.accept()
            except TimeoutError:
                continue
            except OSError:
                break
            conn.settimeout(None)
            threading.Thread(target=self._handle_conn, args=(conn,), daemon=True).start()

    # --- prewarm -----------------------------------------------------------

    def prewarm(self, video_id: str) -> None:
        """Resolve one throwaway video per mode, in the background, at startup.

        Populates the caches while nothing is playing. Failures are logged and
        ignored: this is an optimisation, and at boot the network may not be up yet
        (the add-on's `run.sh` is also still `pip install`ing at this point).
        """
        def run() -> None:
            # The FIRST mode only. The others are failover rungs — a client ladder and the
            # remote cipher — and prewarming each would mean one full resolve per rung at
            # every start, for paths that may never be used.
            for mode in list(self.modes.values())[:1]:
                if self._stopping.is_set():
                    return
                try:
                    result = mode.resolve(video_id)
                    log(f'prewarmed {mode.label} in {result["elapsed_ms"] / 1000:.1f}s')
                except Exception as e:  # noqa: BLE001
                    log(f'prewarm of {mode.label} failed: {type(e).__name__}: {e}')

        threading.Thread(target=run, daemon=True).start()


def diagnose(video_id: str, reason: str, argv: list[str], jar) -> str | None:
    """Snapshot a 403 while it is still happening, and write a report.

    WHY THIS EXISTS
    ---------------
    These rejections come in **windows**: the same video, from the same code, 403s three
    times in a row through the daemon while an interleaved CLI gets 206 — and ten minutes
    later every configuration succeeds. Every hypothesis tested outside a window (and
    several were) measured nothing. So the only way to learn anything is to sweep *at the
    moment of failure*, which is what this does, unattended.

    For each client: a fresh `YoutubeDL`, resolve, then fetch two bytes of the resulting
    URL. Fresh instances on purpose — the report has to describe YouTube's behaviour, not
    this daemon's warm state.
    """
    import urllib.error
    import urllib.request

    from yt_dlp import YoutubeDL, parse_options

    started = time.monotonic()
    base = [a for a in argv if a not in ('-g', '--get-url')]
    base = [a for i, a in enumerate(base)
            if a != '--print' and (i == 0 or base[i - 1] != '--print')]
    report = {
        'video_id': video_id,
        'reason': reason,
        'utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
        'yt_dlp': None,
        'clients': [],
    }
    from yt_dlp.version import __version__ as ytdlp_version
    report['yt_dlp'] = ytdlp_version

    for client in DIAGNOSE_CLIENTS:
        entry = {'client': client}
        ads: list[str] = []

        class AdLogger(YtdlpLogger):
            def debug(self, msg: str) -> None:
                if ' ad ' in msg and 'Detected' in msg:
                    ads.append(' '.join(msg.split())[:120])
                super().debug(msg)

        argv_client = list(base)
        if client != '(default)':
            argv_client += ['--extractor-args', f'youtube:player_client={client}']
        try:
            opts = parse_options(argv_client).ydl_opts
            opts.update({'logger': AdLogger(), 'noprogress': True, 'skip_download': True,
                         'noplaylist': True, 'verbose': True})
            with YoutubeDL(opts, auto_init=True) as ydl:
                if jar:
                    jar.attach(ydl)
                info = ydl.extract_info(
                    f'https://www.youtube.com/watch?v={video_id}', download=False)
            if not info:
                entry['result'] = 'no formats'
            else:
                url = info.get('url') or ''
                entry['itag'] = info.get('format_id')
                entry['abr'] = info.get('abr')
                headers = info.get('http_headers') or {}
                req = urllib.request.Request(url, headers={**headers, 'Range': 'bytes=0-1'})
                try:
                    with urllib.request.urlopen(req, timeout=20) as resp:
                        entry['http'] = resp.status
                except urllib.error.HTTPError as e:
                    entry['http'] = e.code
                except Exception as e:  # noqa: BLE001
                    entry['http'] = f'{type(e).__name__}'
                # Server-side identifiers, so two reports can be compared. No signature
                # material and no cookies: `n`, `sig` and `pot` are deliberately omitted.
                params = urllib.parse.parse_qs(urllib.parse.urlparse(url).query)
                entry['url_params'] = {
                    k: params[k][0] for k in ('expire', 'ip', 'ei', 'itag', 'source',
                                              'requiressl', 'xpc', 'svpuc', 'cps')
                    if k in params}
                entry['has_pot_param'] = 'pot' in params
        except Exception as e:  # noqa: BLE001 — a report must survive any single failure
            entry['result'] = f'{type(e).__name__}: {str(e)[:160]}'
        entry['ads'] = ads[:6]
        report['clients'].append(entry)
        log(f'diagnose {video_id} {client}: {entry.get("http", entry.get("result"))} '
            f'(itag {entry.get("itag")}, {len(ads)} ad markers)')

    report['elapsed_s'] = round(time.monotonic() - started, 1)
    try:
        os.makedirs(DIAGNOSE_DIR, exist_ok=True)
        path = os.path.join(
            DIAGNOSE_DIR, f'{time.strftime("%Y%m%d-%H%M%S", time.gmtime())}-{video_id}.json')
        with open(path, 'w') as f:
            json.dump(report, f, indent=1)
        return path
    except OSError as e:
        log(f'could not write the diagnostic report: {e}')
        return None


def cookiefile_of(argv: list[str]) -> str | None:
    """The `--cookies` path in a mode's argv, if it has one.

    Read straight from the argv rather than from parsed options, because the jar has to
    exist before the first `YoutubeDL` is built — that instance is handed the shared
    jar instead of loading its own.
    """
    for i, arg in enumerate(argv):
        if arg == '--cookies' and i + 1 < len(argv):
            return argv[i + 1]
        if arg.startswith('--cookies='):
            return arg.split('=', 1)[1]
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--socket', required=True, help='unix socket to listen on')
    ap.add_argument('--mode', action='append', default=[], metavar='LABEL=ARGV',
                    help='a resolve strategy: a label and the yt-dlp command line it '
                         'stands for, e.g. local=--cookies /x -f bestaudio/best. '
                         'Repeatable; the client picks a mode by label.')
    ap.add_argument('--prewarm', metavar='VIDEO_ID', nargs='?', const=PREWARM_VIDEO_ID,
                    default=None, help=f'resolve this once at startup to warm the caches '
                                       f'(default target: {PREWARM_VIDEO_ID})')
    ap.add_argument('--jsc-resident', metavar='NODE', default=None,
                    help='keep one node alive to solve JS challenges, holding the '
                         'solver closures per player (jsc_resident.py). NODE is the '
                         'node binary. Modes that do not use the remote cipher get the '
                         "provider's extractor-arg injected.")
    ap.add_argument('--no-jsc-preprocessed-cache', action='store_true',
                    help="do not enable yt-dlp's preprocessed-player disk cache "
                         '(which upstream leaves off; worth ~11.8 s a resolve here)')
    args = ap.parse_args()

    parsed: dict[str, list[str]] = {}
    for spec in args.mode:
        label, _, argv = spec.partition('=')
        if not label or not argv:
            ap.error(f'--mode expects LABEL=ARGV, got {spec!r}')
        parsed[label] = shlex.split(argv)
    if not parsed:
        ap.error('at least one --mode is required')

    # Import here rather than on the first resolve. It is the single largest fixed
    # cost in yt-dlp (~2.3 s of it measured on a Pi Zero 2 W for `--version` alone),
    # and at startup nothing is playing and nobody is waiting.
    from yt_dlp import YoutubeDL  # noqa: F401 — imported for its side effect
    from yt_dlp.version import __version__ as ytdlp_version
    log(f'yt-dlp {ytdlp_version} loaded')

    # The JS challenge is the dominant cost of an authenticated resolve — 15.15 s
    # measured on the add-on host, against ~0 ms for the solving itself. Both levers
    # attack the same thing (re-parsing), and they share one cached artefact:
    #   * the disk cache makes the *fallback* provider's next process cheap;
    #   * the resident worker makes our own path free after the first player.
    if not args.no_jsc_preprocessed_cache:
        try:
            jsc_resident.enable_preprocessed_cache(log)
        except Exception as e:  # noqa: BLE001 — an internal flag; never fatal
            log(f'could not enable the preprocessed-player cache: {type(e).__name__}: {e}')
    if args.jsc_resident:
        script = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'jsc_worker.js')
        try:
            if jsc_resident.enable(args.jsc_resident, script, log):
                # Only modes that are not the remote-cipher attempt: offering two
                # providers in one attempt is what `resolver.js#attempts` exists to
                # avoid, and this one would win.
                for label, argv in parsed.items():
                    if any('remotecipher' in a for a in argv):
                        continue
                    argv += ['--extractor-args', 'youtubejsc-residentnode:enable=1']
                    log(f'mode {label}: solving JS challenges in the resident worker')
        except Exception as e:  # noqa: BLE001
            log(f'resident JS worker unavailable: {type(e).__name__}: {e}')

    # One jar for every mode, when they all name the same one — which is the case in
    # both deployments, since the path comes from a single environment variable. If
    # they ever disagree, each `YoutubeDL` loads its own and neither the reload nor the
    # periodic save applies; say so rather than silently sharing the wrong file.
    jars = {cookiefile_of(argv) for argv in parsed.values()}
    cookies = None
    if len(jars) == 1 and (path := next(iter(jars))):
        cookies = SharedCookieJar(path)
    elif len(jars) > 1:
        log(f'modes name different cookie jars ({jars}) — not sharing one, and neither '
            f'reloading nor saving them')

    modes = {label: Mode(label, argv, cookies) for label, argv in parsed.items()}
    server = Server(args.socket, modes)
    server.cookies = cookies
    signal.signal(signal.SIGTERM, server.stop)
    signal.signal(signal.SIGINT, server.stop)
    server.claim_socket()
    if args.prewarm:
        server.prewarm(args.prewarm)
    try:
        server.serve()
    finally:
        # Last chance to persist rotated cookies; the interval throttle does not apply
        # on the way out. One jar, so one save — `YoutubeDL.close()` would each write
        # the same file in turn.
        if cookies:
            cookies.maybe_save(force=True)
        for mode in modes.values():
            mode._close()  # noqa: SLF001 — same module, shutdown path
        jsc_resident.stop()
        try:
            os.unlink(args.socket)
        except OSError:
            pass
        log('stopped')
    return 0


if __name__ == '__main__':
    sys.exit(main())
