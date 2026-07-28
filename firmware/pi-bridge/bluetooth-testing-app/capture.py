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
    """A completed run of digital silence."""

    start: float
    end: float

    @property
    def duration(self) -> float:
        return self.end - self.start


@dataclass
class SilenceTracker:
    """Turns a stream of per-block silence flags into a streak + episode log.

    Pure and time-injectable so the episode logic is unit-tested without sleeping.
    """

    min_episode_s: float = MIN_EPISODE_S
    silent_since: float | None = None
    episodes: deque[Episode] = field(default_factory=lambda: deque(maxlen=50))
    #: Total silent blocks seen, for a duty-cycle readout.
    silent_blocks: int = 0
    total_blocks: int = 0

    def update(self, silent: bool, now: float) -> None:
        self.total_blocks += 1
        if silent:
            self.silent_blocks += 1
            if self.silent_since is None:
                self.silent_since = now
        elif self.silent_since is not None:
            start, self.silent_since = self.silent_since, None
            if now - start >= self.min_episode_s:
                self.episodes.append(Episode(start, now))

    def streak(self, now: float) -> float:
        return 0.0 if self.silent_since is None else now - self.silent_since

    @property
    def duty(self) -> float:
        return self.silent_blocks / self.total_blocks if self.total_blocks else 0.0


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
        """Envelope points (optionally only those after `since`) + live stats."""
        now = time.monotonic()
        with self._lock:
            total = self._cursor
            points = list(self._ring)
            target, error = self._target, self._error
            started = self._started_at
        first = total - len(points)
        if since is not None and since > first:
            points = points[max(0, since - first):]
            first = max(first, since)
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
            "silence_streak_s": round(self.silence.streak(now), 2),
            "silent_duty": round(self.silence.duty, 4),
            "uptime_s": round(now - started, 1) if started else 0.0,
            "episodes": [
                {"start_ago_s": round(now - e.start, 1), "duration_s": round(e.duration, 1)}
                for e in list(self.silence.episodes)[-12:]
            ],
        }

    # -- worker ----------------------------------------------------------

    def _spawn(self) -> subprocess.Popen | None:
        if not self._target:
            return None
        cmd = [
            "pw-record",
            "--target", self._target,
            "--rate", str(RATE),
            "--channels", str(CHANNELS),
            "--format", "s16",
            "--latency", f"{BLOCK_MS}ms",
            "--raw",
            "-",
        ]
        try:
            return subprocess.Popen(
                cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
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
                self.silence.update(peak <= SILENCE_PEAK, now)
