"""Talking to a source's producer directly: transport, play_media and browse.

The daemon's sources are inputs it *receives* — it can route them, not pause them. A
producer that can be driven (today the YouTube Music receiver, firmware/pi-ytmusic/
receiver/control.js) announces its own control API to the daemon, which lists it on
the source as `control`; this module is the client for that API, and the
`ControlledSourceMediaPlayer` entity is built on it.

The wire format is the producer's, versioned by `protocol` (this speaks 1):

    GET  /api/state?queue=N            transport state, current track, the next N items
    POST /api/command {command, ...}   play | pause | play_pause | stop | next | previous | seek | clear
    POST /api/play {media_id, media_type}
    GET  /api/browse?id=…&lang=…       one node and its children

Nothing here is YouTube-specific: media ids are opaque strings the producer hands out
in its browse tree and its queue, and its browse kinds map onto Home Assistant's media
classes. A producer that fails answers `{error: "<sentence>"}`, which is what the user
sees.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field

import aiohttp

_LOGGER = logging.getLogger(__name__)

#: The producer wire format this integration speaks.
CONTROL_PROTOCOL = 1
#: A state read is a local HTTP call that touches mpv's IPC; seconds mean it is wedged.
STATE_TIMEOUT_SECONDS = 4
#: Starting a playlist asks YouTube Music for its queue first (~1 s, a cold box more);
#: browsing a playlist is the same request.
ACTION_TIMEOUT_SECONDS = 45
#: How many upcoming queue entries a state read asks for — the `queue` attribute.
STATE_QUEUE_ITEMS = 10


class SourceControlError(Exception):
    """The producer refused or could not be reached; the message is its own sentence."""


@dataclass
class ControlTrack:
    """One track as the producer describes it — the current one, or a queue entry."""

    media_id: str | None
    title: str | None = None
    artist: str | None = None
    album: str | None = None
    duration: float | None = None
    image_url: str | None = None
    playable: bool = True
    index: int | None = None
    #: Only meaningful for the current track: where playback was, and when (Unix ms).
    position: float | None = None
    position_at: int | None = None

    def as_attribute(self) -> dict:
        """The shape of one entry in the entity's `queue` attribute."""
        return {
            "media_content_id": self.media_id,
            "title": self.title,
            "artist": self.artist,
            "album": self.album,
            "duration": round(self.duration) if self.duration else None,
            "image": self.image_url,
            "playable": self.playable,
        }


@dataclass
class ControlState:
    """`GET /api/state`, parsed."""

    state: str
    features: list[str] = field(default_factory=list)
    origin: str | None = None
    track: ControlTrack | None = None
    playlist_id: str | None = None
    playlist_media_id: str | None = None
    playlist_title: str | None = None
    queue: list[ControlTrack] = field(default_factory=list)
    queue_index: int | None = None
    queue_length: int = 0
    can_next: bool = False
    can_previous: bool = False
    cast_connected: bool = False
    cast_sender: str | None = None
    error: str | None = None


def _track(data: dict | None) -> ControlTrack | None:
    if not isinstance(data, dict):
        return None
    return ControlTrack(
        media_id=data.get("media_id"),
        title=data.get("title"),
        artist=data.get("artist"),
        album=data.get("album"),
        duration=data.get("duration"),
        image_url=data.get("artwork_url"),
        playable=bool(data.get("playable", True)),
        index=data.get("index"),
        position=data.get("position"),
        position_at=data.get("position_at"),
    )


def parse_state(data: dict) -> ControlState:
    """Parse a state read. Tolerant of absent fields — a newer producer may add some."""
    queue = data.get("queue") or {}
    playlist = data.get("playlist") or {}
    cast = data.get("cast") or {}
    return ControlState(
        state=str(data.get("state") or "idle"),
        features=[str(f) for f in data.get("features") or []],
        origin=data.get("origin"),
        track=_track(data.get("track")),
        playlist_id=playlist.get("playlist_id"),
        playlist_media_id=playlist.get("media_id"),
        playlist_title=playlist.get("title"),
        queue=[t for t in (_track(i) for i in queue.get("items") or []) if t is not None],
        queue_index=queue.get("index"),
        queue_length=int(queue.get("length") or 0),
        can_next=bool(queue.get("can_next")),
        can_previous=bool(queue.get("can_previous")),
        cast_connected=bool(cast.get("connected")),
        cast_sender=cast.get("sender"),
        error=data.get("error"),
    )


class SourceControlClient:
    """One producer's control API."""

    def __init__(self, session: aiohttp.ClientSession, url: str) -> None:
        self._session = session
        self.url = url.rstrip("/")

    async def _request(self, method: str, path: str, *, timeout: float, **kwargs) -> dict:
        try:
            async with self._session.request(
                method, f"{self.url}{path}", timeout=aiohttp.ClientTimeout(total=timeout), **kwargs
            ) as resp:
                try:
                    body = await resp.json(content_type=None)
                except (aiohttp.ClientError, ValueError):
                    body = None
                if resp.status >= 400:
                    message = body.get("error") if isinstance(body, dict) else None
                    raise SourceControlError(message or f"{method} {path} failed: HTTP {resp.status}")
                if not isinstance(body, dict):
                    raise SourceControlError(f"{method} {path} answered something that is not JSON")
                return body
        except (aiohttp.ClientError, TimeoutError) as err:
            raise SourceControlError(f"could not reach the source's control API at {self.url}: {err}") from err

    async def async_state(self, queue: int = STATE_QUEUE_ITEMS) -> ControlState:
        return parse_state(await self._request("GET", f"/api/state?queue={queue}", timeout=STATE_TIMEOUT_SECONDS))

    async def async_command(self, command: str, **extra) -> bool:
        body = await self._request("POST", "/api/command", json={"command": command, **extra}, timeout=ACTION_TIMEOUT_SECONDS)
        return bool(body.get("accepted", True))

    async def async_play(self, media_id: str, media_type: str | None = None) -> dict:
        return await self._request(
            "POST", "/api/play", json={"media_id": media_id, "media_type": media_type}, timeout=ACTION_TIMEOUT_SECONDS
        )

    async def async_browse(self, node_id: str | None, language: str | None = None) -> dict:
        params = {k: v for k, v in (("id", node_id), ("lang", language)) if v}
        return await self._request("GET", "/api/browse", params=params, timeout=ACTION_TIMEOUT_SECONDS)


def steady_position(previous: ControlState | None, current: ControlState, tolerance: float = 2.0) -> ControlState:
    """Keep the previous position stamp while playback is just running on.

    Home Assistant extrapolates `media_position` from `media_position_updated_at`, so a
    fresh stamp on every poll would only rewrite the state every few seconds for nothing
    (and fill the recorder). The new reading replaces the old one only when it says
    something extrapolation would not: another track, another state, or a jump (a seek,
    a stall) beyond `tolerance` seconds.
    """
    if previous is None or previous.track is None or current.track is None:
        return current
    old, new = previous.track, current.track
    if (
        old.media_id != new.media_id
        or previous.state != current.state
        or old.position is None
        or new.position is None
        or old.position_at is None
        or new.position_at is None
    ):
        return current
    elapsed = (new.position_at - old.position_at) / 1000 if current.state == "playing" else 0.0
    if abs((old.position + elapsed) - new.position) <= tolerance:
        new.position, new.position_at = old.position, old.position_at
    return current
