"""Thin async client for the bridge daemon's REST API (bridge-daemon/src/api.rs)."""

from __future__ import annotations

from dataclasses import dataclass

import aiohttp


class PipewireRouterApiError(Exception):
    """Raised when the bridge daemon's API returns an error or is unreachable."""


@dataclass
class MediaPlayerState:
    """Mirrors bridge-daemon's `MediaPlayerInfo` JSON shape exactly."""

    node_id: int
    node_name: str
    state: str  # "playing" | "idle"
    volume: float | None


class PipewireRouterApiClient:
    """Talks to one bridge-daemon instance (one add-on / one Pi)."""

    def __init__(self, session: aiohttp.ClientSession, host: str, port: int) -> None:
        self._session = session
        self._base_url = f"http://{host}:{port}"

    async def async_get_media_players(self) -> list[MediaPlayerState]:
        try:
            async with self._session.get(f"{self._base_url}/api/media_players") as resp:
                resp.raise_for_status()
                data = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not reach bridge daemon: {err}") from err
        return [
            MediaPlayerState(
                node_id=item["node_id"],
                node_name=item["node_name"],
                state=item["state"],
                volume=item.get("volume"),
            )
            for item in data
        ]

    async def async_set_volume(self, node_id: int, volume: float) -> None:
        try:
            async with self._session.post(
                f"{self._base_url}/api/media_players/{node_id}/volume",
                json={"volume": volume},
            ) as resp:
                resp.raise_for_status()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not set volume: {err}") from err

    async def async_announce(self, node_id: int, url: str, duck_volume: float | None = None) -> None:
        """Ducks whatever is currently linked into this output and plays
        `url` into the same sink (bridge-daemon's `/announce` endpoint,
        PLAN.md Section 5.6 v1 — the file+URL path, unchanged by v2)."""
        await self._async_announce(node_id, {"url": url}, duck_volume)

    async def async_announce_wyoming(
        self,
        node_id: int,
        host: str,
        text: str,
        port: int = 10200,
        voice: str | None = None,
        duck_volume: float | None = None,
    ) -> None:
        """Same ducked-announce mechanism as `async_announce`, but
        synthesizes directly against a Wyoming TTS server (e.g. Piper)
        instead of fetching a rendered URL (Section 5.6 v2, Phase 3.5) —
        additive: callers pick this per call via `play_media`'s `extra`
        dict (see media_player.py), `async_announce`/v1 is unaffected."""
        wyoming: dict = {"host": host, "text": text, "port": port}
        if voice is not None:
            wyoming["voice"] = voice
        await self._async_announce(node_id, {"wyoming": wyoming}, duck_volume)

    async def _async_announce(self, node_id: int, source: dict, duck_volume: float | None) -> None:
        """Shared POST for both announce sources — blocks for the duration
        of playback, since the bridge daemon only responds once the clip
        has finished and ducked sources are restored, so the timeout here
        needs headroom beyond typical announce-clip length."""
        payload = dict(source)
        if duck_volume is not None:
            payload["duck_volume"] = duck_volume
        try:
            async with self._session.post(
                f"{self._base_url}/api/media_players/{node_id}/announce",
                json=payload,
                timeout=aiohttp.ClientTimeout(total=60),
            ) as resp:
                resp.raise_for_status()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not announce: {err}") from err
