"""A yt-dlp JS-challenge provider backed by a resident `node` worker.

WHAT IT REPLACES
----------------
yt-dlp's builtin `node` provider spawns a process per resolve and feeds it the solver
bundle plus the player JS on stdin. Measured on the add-on host (aarch64, nice 19):
**15.15 s**, of which 11.76 s is preprocessing the 2.8 MB player, 2.48 s is parsing
the stdin payload, 0.90 s is node itself — and **~0 ms is the actual solving**. Three
challenges in one call cost the same as one.

This provider keeps one `node` alive (`jsc_worker.js`) holding the `{n, sig}` solver
closures per player id, so a steady-state solve is a function call.

TWO CACHES, ONE FILE
--------------------
The worker's memory is lost on restart, so the *preprocessed* player is also written
to yt-dlp's own disk cache — under exactly the key the builtin ejs provider reads
(`player:{player_url}` in its `_CACHE_SECTION`). That is deliberate:

  * a worker restart costs a `Function()` compile (~13 ms) instead of another 11.8 s;
  * `enable_preprocessed_cache()` flips the builtin provider's
    `_ENABLE_PREPROCESSED_PLAYER_CACHE`, which upstream leaves off — so the *fallback*
    path reads what this provider wrote, and vice versa. One cached artefact serves
    both, and the slow path stops being catastrophic.

The section is suffixed with the solver version, because the upstream key is the
player URL alone: without scoping, a solver upgrade (which `run.sh` performs at every
boot) could pair a new solver with a player preprocessed by the old one. That is the
most plausible reason the flag is off upstream, and it is cheap to rule out.

WHY IN-PROCESS REGISTRATION, NOT A PLUGIN PACKAGE
-------------------------------------------------
The worker has to be owned by something long-lived, and that is this daemon. Providers
are instantiated per `YoutubeDL` (one director each), so the worker is a module-level
singleton and the provider instances borrow it; `close()` is therefore a no-op —
letting yt-dlp's downloader-close hook kill the worker would throw away every cached
player.

The provider is **opt-in by extractor-arg** (`youtubejsc-residentnode:enable=1`),
mirroring `yt-dlp-remote-cipher`. That is not decoration: `resolver.js` offers exactly
one challenge provider per attempt, so a provider that registered itself
unconditionally would outrank the remote cipher in the attempt meant to *use* it, and
the log would claim the server was in play while everything was solved locally — the
failure documented in `resolver.js#attempts`.
"""

from __future__ import annotations

import json
import os
import subprocess
import threading
import time

#: How long to wait for the worker to answer. Generous only for the first solve of a
#: player version, which pays the 11.8 s preprocess inside the worker.
SOLVE_TIMEOUT_S = 90
#: `node --permission` became stable in 23.5.0; before that it is `--experimental-`.
PERMISSION_STABLE_VERSION = (23, 5, 0)

_worker = None
_log = print


class WorkerError(Exception):
    """The worker could not answer. Always falls back to another provider."""


class Worker:
    """The resident `node` process, and the only thing that may talk to it."""

    def __init__(self, node_path: str, script: str, lib: str, core: str, logger) -> None:
        self.node_path = node_path
        self.script = script
        self.lib = lib
        self.core = core
        self.log = logger
        self.lock = threading.Lock()
        self.proc = None
        self._next_id = 1
        self._buffer = b''
        #: What the last solve cost: 'memory' (closures in hand), 'preprocessed' (a
        #: compile of the disk-cached player) or 'player' (the full preprocess). Read by
        #: the provider for its log line; only ever written under `lock`.
        self.last_source = None
        #: Milliseconds the worker spent rebuilding the solver closures, if it said.
        self.last_build_ms = None

    # --- process ------------------------------------------------------------

    def _node_version(self) -> tuple[int, ...]:
        out = subprocess.run([self.node_path, '--version'], capture_output=True,
                             text=True, timeout=30).stdout.strip().lstrip('v')
        return tuple(int(p) for p in out.split('.')[:3] if p.isdigit())

    def _spawn(self) -> None:
        version = self._node_version()
        # The player JS is untrusted and this process evaluates it, so keep the same
        # sandbox yt-dlp uses: no filesystem, no network, no child processes. The
        # script arrives via `-e`, which is why no read permission is needed for it.
        if version >= PERMISSION_STABLE_VERSION:
            flags = ['--permission']
        else:
            flags = ['--experimental-permission', '--no-warnings=ExperimentalWarning']
        self.log(f'starting resident worker: node {".".join(map(str, version))}')
        self.proc = subprocess.Popen(
            [self.node_path, *flags, '-e', self.script],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=None)
        self._buffer = b''
        reply = self._exchange({'op': 'init', 'lib': self.lib, 'core': self.core})
        if not reply.get('ok'):
            raise WorkerError(f'worker init failed: {reply}')
        self.log('resident worker ready')

    def _ensure(self) -> None:
        if self.proc is None or self.proc.poll() is not None:
            if self.proc is not None:
                self.log(f'resident worker exited (code={self.proc.poll()}) — restarting')
            self._spawn()

    def stop(self) -> None:
        with self.lock:
            if self.proc and self.proc.poll() is None:
                try:
                    self.proc.stdin.close()
                    self.proc.wait(timeout=5)
                except Exception:  # noqa: BLE001 — shutdown is best-effort
                    self.proc.kill()
            self.proc = None

    # --- messaging ----------------------------------------------------------

    def _exchange(self, payload: dict) -> dict:
        """Write one request, read one reply. Caller holds the lock."""
        try:
            self.proc.stdin.write((json.dumps(payload) + '\n').encode())
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError) as e:
            raise WorkerError(f'worker went away while writing: {e}') from e
        while b'\n' not in self._buffer:
            chunk = self.proc.stdout.read1(1 << 20)
            if not chunk:
                raise WorkerError('worker closed its output')
            self._buffer += chunk
        line, self._buffer = self._buffer.split(b'\n', 1)
        return json.loads(line)

    def solve(self, player_id: str, requests: list[dict], *, load_cached, store_cached,
              load_player) -> list[dict]:
        """Solve `requests` for one player, fetching code only if the worker asks.

        `load_cached`/`store_cached`/`load_player` are callbacks so this class never
        touches yt-dlp: the provider owns the disk cache and the player download.
        """
        with self.lock:
            self._ensure()
            request_id = self._next_id
            self._next_id += 1
            base = {'op': 'solve', 'id': request_id, 'player_id': player_id,
                    'requests': requests}
            reply = self._exchange(base)
            if reply.get('need') == 'code':
                # Prefer the preprocessed player from disk: it skips the 11.8 s.
                preprocessed = load_cached()
                if preprocessed:
                    reply = self._exchange({**base, 'preprocessed': preprocessed})
                else:
                    reply = self._exchange({**base, 'player': load_player()})
            if reply.get('error'):
                raise WorkerError(reply['error'])
            if fresh := reply.get('preprocessed'):
                store_cached(fresh)
            self.last_source = reply.get('source')
            self.last_build_ms = reply.get('build_ms')
            responses = reply.get('responses')
            if not isinstance(responses, list) or len(responses) != len(requests):
                raise WorkerError(f'malformed reply for {len(requests)} requests: '
                                  f'{str(reply)[:200]}')
            return responses


def enable_preprocessed_cache(logger=print) -> bool:
    """Let the *builtin* ejs providers cache the preprocessed player on disk.

    Upstream leaves `_ENABLE_PREPROCESSED_PLAYER_CACHE` off (`# TODO: Pass
    use_disk_cache=True when preprocessed player JS cache is solved`), so every
    invocation re-preprocesses. Turning it on is worth ~11.8 s of ~15 s on the add-on
    host, and the version-scoped section below removes the staleness that most likely
    motivated the switch. A wrong answer would still be caught downstream by
    `resolver.js#verify`'s two-byte ranged request.
    """
    from yt_dlp.extractor.youtube.jsc._builtin.ejs import EJSBaseJCP

    EJSBaseJCP._ENABLE_PREPROCESSED_PLAYER_CACHE = True
    EJSBaseJCP._CACHE_SECTION = cache_section()
    logger(f'preprocessed-player disk cache enabled (section {EJSBaseJCP._CACHE_SECTION})')
    return True


def cache_section() -> str:
    """The cache section, scoped to the solver version the scripts come from."""
    from yt_dlp.extractor.youtube.jsc._builtin.ejs import EJSBaseJCP

    version = EJSBaseJCP._SCRIPT_VERSION
    try:
        import yt_dlp_ejs
        version = yt_dlp_ejs.version
    except Exception:  # noqa: BLE001 — the vendored version is a fine fallback
        pass
    return f'challenge-solver-{version}'


def enable(node_path: str, script_path: str, logger=print) -> bool:
    """Register the resident provider. Returns whether it is usable.

    Refuses rather than degrades when a piece is missing — the builtin `node` provider
    is still there, so a refusal costs latency, not playback.
    """
    global _worker, _log
    _log = logger

    try:
        from yt_dlp_ejs.yt import solver
    except ImportError:
        logger('resident JS worker not enabled: yt-dlp-ejs is not installed')
        return False
    if not os.path.isfile(script_path):
        logger(f'resident JS worker not enabled: {script_path} is missing')
        return False

    with open(script_path) as f:
        script = f.read()
    _worker = Worker(node_path, script, solver.lib(), solver.core(), logger)
    _register_provider()
    logger(f'resident JS worker registered (node: {node_path})')
    return True


def stop() -> None:
    if _worker:
        _worker.stop()


def _register_provider() -> None:
    """Define and register the provider class, once."""
    from yt_dlp.extractor.youtube.jsc.provider import (
        JsChallengeProvider,
        JsChallengeProviderError,
        JsChallengeProviderRejectedRequest,
        JsChallengeProviderResponse,
        JsChallengeResponse,
        JsChallengeType,
        NChallengeOutput,
        SigChallengeOutput,
        register_preference,
        register_provider,
    )
    from yt_dlp.extractor.youtube.jsc._registry import _jsc_providers

    if 'ResidentNode' in _jsc_providers.value:
        return

    @register_provider
    class ResidentNodeJCP(JsChallengeProvider):
        PROVIDER_NAME = 'resident node'
        PROVIDER_VERSION = '1.0.0'
        BUG_REPORT_LOCATION = 'https://github.com/davidgraeff/homeassistant-audio-routing'
        _SUPPORTED_TYPES = (JsChallengeType.N, JsChallengeType.SIG)

        def is_available(self) -> bool:
            # Opt-in, so that an attempt which means to use another provider is not
            # silently served by this one (see the module docstring).
            return bool(_worker) and bool(self._configuration_arg('enable', default=[]))

        def close(self):
            # Deliberately empty: the worker outlives every YoutubeDL.
            pass

        def _real_bulk_solve(self, requests):
            if not requests:
                return
            # Group by player, exactly as the ejs provider does: one player's solver
            # closures serve all of its challenges.
            by_player = {}
            for request in requests:
                by_player.setdefault(request.input.player_url, []).append(request)

            for player_url, grouped in by_player.items():
                try:
                    yield from self._solve_group(player_url, grouped)
                except Exception as e:  # noqa: BLE001
                    # One yield per request, as an error, so the director moves the
                    # whole group on to the next provider instead of hanging.
                    error = JsChallengeProviderError(
                        f'resident worker failed: {e}', expected=True)
                    for request in grouped:
                        yield JsChallengeProviderResponse(request=request, error=error)

        def _solve_group(self, player_url, grouped):
            player_id = self.ie._player_js_cache_key(player_url)
            payload = [{'type': r.type.value, 'challenges': list(r.input.challenges)}
                       for r in grouped]
            # Logged unconditionally, at our own level rather than yt-dlp's trace: "is
            # the resident worker actually being used, and was this player already
            # loaded?" is the question every latency report about this needs answered,
            # and yt-dlp's own log says nothing when a provider simply works.
            started = time.monotonic()

            responses = _worker.solve(
                player_id, payload,
                load_cached=lambda: self.ie.cache.load(cache_section(), f'player:{player_url}'),
                store_cached=lambda data: self.ie.cache.store(
                    cache_section(), f'player:{player_url}', data),
                load_player=lambda: self._get_player(grouped[0].video_id, player_url))
            challenges = sum(len(r['challenges']) for r in payload)
            _log(f'jsc: solved {challenges} challenge(s) for {player_id} in '
                 f'{(time.monotonic() - started) * 1000:.0f}ms '
                 f'[{_worker.last_source}'
                 f'{f" build {_worker.last_build_ms}ms" if _worker.last_build_ms else ""}]')

            for request, data in zip(grouped, responses, strict=True):
                if data.get('type') != 'result':
                    yield JsChallengeProviderResponse(
                        request=request,
                        error=JsChallengeProviderError(
                            f'{data.get("error")}', expected=True))
                    continue
                output = (NChallengeOutput if request.type is JsChallengeType.N
                          else SigChallengeOutput)(data['data'])
                yield JsChallengeProviderResponse(
                    request=request,
                    response=JsChallengeResponse(request.type, output))

    @register_preference(ResidentNodeJCP)
    def _preference(provider, requests):
        # Above every builtin runtime (node is 900): when this provider is enabled at
        # all, it is because it is the fast one. It is opt-in, so this cannot hijack
        # an attempt that did not ask for it.
        return 1100

    # Referenced so linters do not read these as unused.
    assert ResidentNodeJCP and _preference and JsChallengeProviderRejectedRequest
