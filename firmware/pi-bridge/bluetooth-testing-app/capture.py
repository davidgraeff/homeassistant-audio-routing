#!/usr/bin/env python3
"""Audio capture -> waveform envelope + digital-silence detection.

One long-lived `pw-record` per target feeds a ring of fixed-duration blocks; each
block keeps only `peak`/`rms`, which is all a waveform envelope and a meter need.
At 20 ms blocks that is 50 points/s — cheap enough for a Pi Zero 2 W to also be
running the bridge, and fine enough that a single 20 ms silent gap is visible.

Two design points are load-bearing:

**One capture process, restarted only when the target changes.** Repeatedly
starting and stopping `pw-record` against the same node is known to hang on this
stack (see ../../pipewire_audio_router/tests/ notes and the project's test-script
gotchas), so this never polls by spawning: it holds the process open and reads it.

**Digital silence is tracked explicitly, not left to the eye.** `peak == 0` means
*every sample in the block is exactly zero*, which is categorically different from
"quiet" — it is what a stalled sender or a failing decoder produces, and it was
the whole answer to the dropout investigation. So the ring records it, the current
streak is measured, and completed episodes are logged with durations. That turns
this from a pretty meter into the instrument that actually diagnoses the fault.

### Silence must be *measured*, not inferred (2026-07-29)

The first version of this file reported silence it had never seen, and it was
caught red-handed: with the phone disconnected it showed an 8-minute and growing
"digital silence" streak, `error: None`, and a frozen block cursor. Three separate
defects, all fixed here, because each one on its own is enough to fabricate the
exact symptom this tool exists to detect:

1. **`--target` is not a pin.** When the target node disappears, PipeWire
   *reconnects* the stream to whatever the default source is — on this bridge that
   is `rtp-bridge:monitor`, which is silent. The app went on naming the phone's
   node while metering something else entirely. `node.dont-reconnect=true` fixes
   that; `node.name` is set too, so the stream is identifiable in the graph (see
   `STREAM_NODE`) and its links can be checked. **Both were verified on the bridge
   (WirePlumber 0.5.8 / libpipewire 1.4.2) by making a target vanish under a live
   capture**, and the caveats are in `record_cmd`: the stream is left *unlinked*
   rather than errored, and a target that is missing at *startup* still falls back.
   So the pin alone is not enough to trust a reading — hence 2, 3 and the binding
   check below.
2. **The streak grew on wall-clock time.** It was `now - silent_since`, which
   keeps counting when *no blocks arrive at all*. Silence is now counted in
   **blocks**: `silent_run * BLOCK_S`. A frozen capture freezes the streak, and
   `stalled()` says so out loud.
3. **An episode could be pure artifact.** One zero block set `silent_since`, then
   any stall — a starved reader, a dying child, a re-bind — and the next real
   block closed an "episode" whose duration was the stall. Block-derived
   durations make a reader stall harmless (buffered blocks still carry their
   20 ms each), and a wall-clock discontinuity larger than `MAX_GAP_S` beyond
   what the blocks account for is treated as **lost data**: it breaks the run and
   is logged as a stall, never billed as silence.

On top of that, blocks captured while the stream is **not linked to the intended
target** are recorded as `untrusted` and excluded from silence entirely — that is
what defect 1 produced, and no reading taken then means anything.
"""

from __future__ import annotations

import array
import subprocess
import sys
import threading
import time
from collections import deque
from dataclasses import dataclass, field

from pwctl import _env

#: Capture format. s16 stereo @48k matches what the bridge transmits, so the
#: waveform here is directly comparable to what lands on the RTP wire.
RATE = 48000
CHANNELS = 2
SAMPLE_BYTES = 2
#: Envelope resolution. 20 ms -> 50 points/s.
BLOCK_MS = 20
BLOCK_S = BLOCK_MS / 1000.0
BLOCK_FRAMES = RATE * BLOCK_MS // 1000
BLOCK_BYTES = BLOCK_FRAMES * CHANNELS * SAMPLE_BYTES
#: How much history the ring holds. 15 s comfortably covers "the last few
#: seconds" while still showing a whole short dropout on one screen.
HISTORY_S = 15
RING_POINTS = HISTORY_S * 1000 // BLOCK_MS

#: A block counts as digital silence only at exactly zero — see module docstring.
SILENCE_PEAK = 0
#: Don't log sub-second blips as "episodes"; they are usually block boundaries.
MIN_EPISODE_S = 0.5
#: Wall-clock time between consecutive blocks, *beyond* the 20 ms the block itself
#: accounts for, that is treated as lost data rather than a late reader. Generous
#: on purpose: a Zero 2 W running the bridge does get descheduled, and a burst
#: read after a short stall loses nothing (the blocks are still all there). What
#: it must catch is a real discontinuity, because that is the one case where a
#: silence run may not have been silent at all.
MAX_GAP_S = 0.5

#: Grace period after a target change before an *unbound* verdict is believed.
#: A new `pw-record` needs a moment to appear and be linked, and the graph poll is
#: itself up to `GRAPH_REFRESH_S` stale, so the first verdict after a switch is
#: routinely "not bound yet". Reporting that as untrusted discarded ~2.5 s of
#: perfectly good audio on every switch (measured on the bridge). A *positive*
#: verdict is always believed immediately — this only delays the negative one.
BIND_GRACE_S = 6.0

#: `node.name` given to our own capture stream. Distinct from the bridge's
#: `bt-bridge-capture` so the two are never confused in `pw-link -l`, and so the
#: binding check can find *our* stream unambiguously when several `pw-record`
#: processes are running.
STREAM_NODE = "bt-testing-capture"


def record_cmd(target: str) -> list[str]:
    """The `pw-record` argv for capturing `target`.

    Pure, so the two properties that matter are testable off-device:

    - **`node.dont-reconnect=true`** — without it `--target` is only a *preference*.
      When the target vanishes PipeWire silently rebinds the stream to the default
      source and the app meters the wrong node under the right name (measured on
      the bridge: rebound to `rtp-bridge:monitor` and reported as digital silence
      from the phone).
    - **`node.name`** — so the stream can be found in the graph and its links
      verified against `target`.

    Two limits of the pin, both measured on the bridge (WP 0.5.8 / PW 1.4.2) with
    a target destroyed under a live capture, and both the reason the app cannot
    stop at setting these properties:

    - The pinned stream is left **unlinked and alive**, not errored — `pw-record`
      does not exit, so nothing surfaces unless someone checks. That is what
      `SilenceTracker.stalled` and the binding watch in app.py are for, and why
      `App.refresh_binding` respawns the child once the target is back (an
      unlinked stream never re-links, by design).
    - A target that is **absent at startup** is still resolved to the default
      source, `dont-reconnect` notwithstanding. Only the link check catches that.
    """
    return [
        "pw-record",
        "--target", target,
        "--rate", str(RATE),
        "--channels", str(CHANNELS),
        "--format", "s16",
        "--latency", f"{BLOCK_MS}ms",
        "-P", f"{{ node.name={STREAM_NODE} node.dont-reconnect=true }}",
        "--raw",
        "-",
    ]


def block_peak_rms(buf: bytes) -> tuple[int, float]:
    """(peak absolute sample, RMS) of a little-endian s16 block.

    Uses `array` rather than a Python loop: on a Zero 2 W a per-sample loop over
    48 kHz stereo is a measurable fraction of a core, and this runs forever.
    """
    n = len(buf) // SAMPLE_BYTES
    if n == 0:
        return 0, 0.0
    a = array.array("h")
    a.frombytes(buf[: n * SAMPLE_BYTES])
    if sys.byteorder != "little":  # pragma: no cover - Pi and dev box are LE
        a.byteswap()
    lo, hi = min(a), max(a)
    peak = max(hi, -lo)
    # Mean square via sum of squares; ints stay exact, no float drift.
    total = 0
    for v in a:
        total += v * v
    return peak, (total / n) ** 0.5


@dataclass
class Episode:
    """A completed run of digital silence, or a gap in the capture itself.

    `start` is a monotonic stamp, used only to say how long ago it was. `duration`
    is **derived from the block count** for silence, so it measures audio actually
    inspected rather than time elapsed; for a stall it is the wall-clock gap,
    which is exactly the thing being reported.
    """

    start: float
    duration: float


@dataclass
class SilenceTracker:
    """Turns a stream of per-block silence flags into a streak + episode log.

    Pure and time-injectable so the episode logic is unit-tested without sleeping.

    Every number here answers "what did we actually look at?" rather than "how much
    time has passed?" — see the module docstring for the three ways the previous
    version got that wrong and reported silence it had never measured.
    """

    min_episode_s: float = MIN_EPISODE_S
    block_s: float = BLOCK_S
    max_gap_s: float = MAX_GAP_S
    #: Monotonic stamp of the current silent run's first block, for "ago".
    silent_since: float | None = None
    #: Length of the current silent run, in blocks. This — not the clock — is the
    #: streak: if blocks stop arriving, it stops growing.
    silent_run: int = 0
    episodes: deque[Episode] = field(default_factory=lambda: deque(maxlen=50))
    #: Gaps in the capture. Kept separately and never counted as silence, because
    #: "we saw nothing" and "we saw zeros" are different claims.
    stalls: deque[Episode] = field(default_factory=lambda: deque(maxlen=50))
    #: Total silent blocks seen, for a duty-cycle readout.
    silent_blocks: int = 0
    total_blocks: int = 0
    #: Blocks arriving while the stream was not linked to the intended target.
    untrusted_blocks: int = 0
    #: Wall-clock time the blocks do not account for, i.e. audio we never saw.
    lost_s: float = 0.0
    last_block_at: float | None = None

    def update(self, silent: bool, now: float, trusted: bool = True) -> None:
        """Fold one block in. `trusted=False` = the stream was not on the target."""
        self._check_continuity(now)
        self.last_block_at = now

        if not trusted:
            # Whatever this block contains, it is not the target's audio. Close any
            # run in progress rather than splicing foreign blocks into it.
            self.untrusted_blocks += 1
            self._close_run()
            return

        self.total_blocks += 1
        if silent:
            self.silent_blocks += 1
            if self.silent_run == 0:
                self.silent_since = now
            self.silent_run += 1
        else:
            self._close_run()

    def _check_continuity(self, now: float) -> None:
        """Detect a discontinuity: wall time the blocks cannot account for."""
        if self.last_block_at is None:
            return
        gap = now - self.last_block_at - self.block_s
        if gap <= self.max_gap_s:
            return
        self.lost_s += gap
        self.stalls.append(Episode(self.last_block_at, gap))
        # A run cannot be *extended* across a hole in the data — but what was
        # measured before the hole still counts, so close it rather than discard
        # it. Two 60 s episodes either side of a stall is the honest report; one
        # 130 s episode would be a claim about audio nobody looked at.
        self._close_run()

    def _close_run(self) -> None:
        """End the current silent run, logging it if it is long enough to matter."""
        duration = self.silent_run * self.block_s
        if self.silent_since is not None and duration >= self.min_episode_s:
            self.episodes.append(Episode(self.silent_since, duration))
        self.silent_run, self.silent_since = 0, None

    @property
    def streak(self) -> float:
        """Measured length of the current silent run. Frozen if blocks stop."""
        return self.silent_run * self.block_s

    def stalled(self, now: float) -> bool:
        """True when blocks are not currently arriving."""
        if self.last_block_at is None:
            return True
        return now - self.last_block_at > self.block_s + self.max_gap_s

    def since_last_block(self, now: float) -> float | None:
        return None if self.last_block_at is None else now - self.last_block_at

    @property
    def measured_s(self) -> float:
        """Seconds of audio actually inspected (trusted blocks only)."""
        return self.total_blocks * self.block_s

    @property
    def duty(self) -> float:
        return self.silent_blocks / self.total_blocks if self.total_blocks else 0.0

    def coverage(self, elapsed_s: float) -> float | None:
        """Fraction of `elapsed_s` covered by blocks — the integrity readout.

        On the bridge this sat at 0.92 over two hours: nine minutes of audio the
        app never saw, while the UI showed an unbroken record. A clean run should
        read ~1.0, and anything less caps how much a "no dropouts" claim is worth.
        """
        if elapsed_s <= 0:
            return None
        return (self.measured_s + self.untrusted_blocks * self.block_s) / elapsed_s


class Capture(threading.Thread):
    """Holds one `pw-record` on `target` and fills the envelope ring.

    Thread-safe for a single reader (the HTTP/SSE side) via one lock around the
    ring. `cursor` counts blocks ever produced, so the browser can ask for "what
    is new since N" and the stream stays small.
    """

    daemon = True

    def __init__(self, target: str | None = None) -> None:
        super().__init__(name="capture")
        self._lock = threading.Lock()
        self._ring: deque[tuple[int, float]] = deque(maxlen=RING_POINTS)
        self._cursor = 0
        self._target = target
        self._proc: subprocess.Popen | None = None
        self._stop = threading.Event()
        self._restart = threading.Event()
        self._error: str | None = None
        self._started_at: float | None = None
        # Bumped on every target change. `_pump` stamps its blocks with the
        # generation it started under and drops them if it no longer matches, so
        # audio still in flight from the *previous* target can't land in the ring
        # after `set_target` has cleared it (which would show the old node's
        # audio under the new node's name for a fraction of a second).
        self._generation = 0
        # Whether our stream is really linked to `_target`, per the graph watch in
        # app.py. None = not checked yet; False = readings are worthless.
        self._bound: bool | None = None
        self._fed_by: list[str] = []
        #: When the target last changed, for BIND_GRACE_S. None once settled.
        self._changed_at: float | None = None
        self.silence = SilenceTracker()

    # -- control ---------------------------------------------------------

    @property
    def target(self) -> str | None:
        return self._target

    def set_target(self, target: str | None) -> None:
        """Point the capture at a different node; restarts the child process."""
        with self._lock:
            if target == self._target:
                return
            self._target = target
            self._ring.clear()
            self._generation += 1
            self.silence = SilenceTracker()
            self._error = None
            self._bound, self._fed_by = None, []
            self._changed_at = time.monotonic()
            self._started_at = None
        self._restart.set()
        self._kill_child()

    def set_binding(self, bound: bool | None, fed_by: list[str] | None = None) -> None:
        """Record what the graph says our stream is actually connected to.

        Called by the binding watch in app.py. While `bound` is False the pump
        marks its blocks untrusted, so a stream that ended up on the wrong node
        can no longer masquerade as the target going silent.

        A `False` arriving within `BIND_GRACE_S` of a target change is downgraded to
        "unknown": right after a switch it means "not linked *yet*", and treating
        that as a wrong binding threw away the first seconds of every measurement.
        """
        with self._lock:
            if bound is False and self._changed_at is not None:
                if time.monotonic() - self._changed_at < BIND_GRACE_S:
                    bound = None
            if bound is not None:
                self._changed_at = None  # verdict settled; grace no longer applies
            self._bound = bound
            self._fed_by = list(fed_by or [])

    def restart(self) -> None:
        """Replace the child process, keeping the target, ring and statistics.

        Needed because `node.dont-reconnect` cuts both ways: a stream whose target
        vanished is never re-linked, so when the target comes back the only way to
        capture it again is a new `pw-record`. Driven by `App.refresh_binding`.
        """
        self._restart.set()
        self._kill_child()

    def stop(self) -> None:
        self._stop.set()
        self._kill_child()

    def _kill_child(self) -> None:
        proc, self._proc = self._proc, None
        if proc and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:  # pragma: no cover - defensive
                proc.kill()

    # -- readout ---------------------------------------------------------

    def snapshot(self, since: int | None = None) -> dict:
        """Envelope points (optionally only those after `since`) + live stats.

        The integrity fields (`bound`, `stalled`, `coverage`, `lost_s`,
        `untrusted_blocks`, `stalls`) are not decoration: without them a clean
        `silent_duty` cannot be distinguished from an app that stopped looking.
        """
        now = time.monotonic()
        with self._lock:
            total = self._cursor
            points = list(self._ring)
            target, error = self._target, self._error
            started = self._started_at
            bound, fed_by = self._bound, list(self._fed_by)
        first = total - len(points)
        if since is not None and since > first:
            points = points[max(0, since - first):]
            first = max(first, since)
        s = self.silence
        elapsed = (now - started) if started else 0.0
        return {
            "target": target,
            "error": error,
            "cursor": total,
            "from": first,
            "block_ms": BLOCK_MS,
            "history_s": HISTORY_S,
            # Parallel arrays keep the SSE payload small.
            "peak": [p for p, _ in points],
            "rms": [round(r, 1) for _, r in points],
            "silence_streak_s": round(s.streak, 2),
            "silent_duty": round(s.duty, 4),
            "uptime_s": round(elapsed, 1),
            # -- integrity ---------------------------------------------------
            "stream_node": STREAM_NODE,
            "bound": bound,
            "fed_by": fed_by,
            "stalled": s.stalled(now),
            "last_block_ago_s": (
                None if s.since_last_block(now) is None else round(s.since_last_block(now), 2)
            ),
            "measured_s": round(s.measured_s, 1),
            "coverage": (
                None if s.coverage(elapsed) is None else round(min(1.0, s.coverage(elapsed)), 4)
            ),
            "lost_s": round(s.lost_s, 1),
            "untrusted_blocks": s.untrusted_blocks,
            "episodes": [
                {"start_ago_s": round(now - e.start, 1), "duration_s": round(e.duration, 1)}
                for e in list(s.episodes)[-12:]
            ],
            "stalls": [
                {"start_ago_s": round(now - e.start, 1), "duration_s": round(e.duration, 1)}
                for e in list(s.stalls)[-12:]
            ],
        }

    # -- worker ----------------------------------------------------------

    def _spawn(self) -> subprocess.Popen | None:
        if not self._target:
            return None
        try:
            return subprocess.Popen(
                record_cmd(self._target), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                bufsize=BLOCK_BYTES, env=_env(),
            )
        except OSError as e:
            with self._lock:
                self._error = f"could not start pw-record: {e}"
            return None

    def run(self) -> None:
        while not self._stop.is_set():
            self._restart.clear()
            if not self._target:
                time.sleep(0.3)
                continue
            proc = self._spawn()
            if proc is None:
                time.sleep(2.0)
                continue
            self._proc = proc
            with self._lock:
                # Session start, not process start: a respawn (phone reconnected,
                # child died) must count against `coverage`, not reset it. The gap
                # itself is caught as a stall by the tracker's continuity check.
                if self._started_at is None:
                    self._started_at = time.monotonic()
                generation = self._generation
            self._pump(proc, generation)
            # Surface why it died, unless we killed it deliberately.
            if not self._stop.is_set() and not self._restart.is_set():
                err = ""
                if proc.stderr:
                    try:
                        err = proc.stderr.read().decode(errors="replace").strip()
                    except (OSError, ValueError):
                        err = ""
                # Reap it. Without this the child stays a zombie until some later
                # Popen happens to clean it up, and `returncode` is still None —
                # so the error read "pw-record exited (None)", which says nothing.
                # This loop respawns every 2 s against a target that may never come
                # back, so "eventually" is not good enough.
                try:
                    proc.wait(timeout=2)
                except subprocess.TimeoutExpired:  # pragma: no cover - defensive
                    proc.kill()
                with self._lock:
                    self._error = err or f"pw-record exited ({proc.returncode})"
                time.sleep(2.0)  # don't spin on a target that never works

    def _pump(self, proc: subprocess.Popen, generation: int) -> None:
        """Read fixed-size blocks until the child dies or we're told to restart.

        Blocks are discarded if `generation` is stale — see `_generation`.
        """
        assert proc.stdout is not None
        buf = b""
        while not self._stop.is_set() and not self._restart.is_set():
            chunk = proc.stdout.read(BLOCK_BYTES)
            if not chunk:
                return
            buf += chunk
            while len(buf) >= BLOCK_BYTES:
                block, buf = buf[:BLOCK_BYTES], buf[BLOCK_BYTES:]
                peak, rms = block_peak_rms(block)
                now = time.monotonic()
                with self._lock:
                    if generation != self._generation:
                        return  # target changed under us; this audio is stale
                    self._ring.append((peak, rms))
                    self._cursor += 1
                    self._error = None
                    # `None` (not checked yet) counts as trusted: refusing to
                    # measure until the first graph poll lands would throw away
                    # the first seconds after every start.
                    trusted = self._bound is not False
                self.silence.update(peak <= SILENCE_PEAK, now, trusted=trusted)
