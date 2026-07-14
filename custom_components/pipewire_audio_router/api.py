"""Thin async client for the bridge daemon's REST API (bridge-daemon/src/api.rs)."""

from __future__ import annotations

from collections.abc import AsyncIterator
from dataclasses import dataclass

import aiohttp

# Ping cadence for the routing WebSocket — lets aiohttp notice a dead
# connection (and trigger the coordinator's reconnect) even when the daemon
# has no routing changes to push for a while.
WS_HEARTBEAT_SECONDS = 25


class PipewireRouterApiError(Exception):
    """Raised when the bridge daemon's API returns an error or is unreachable."""


@dataclass
class MediaPlayerState:
    """Mirrors bridge-daemon's `MediaPlayerInfo` JSON shape exactly."""

    node_id: int
    node_name: str
    state: str  # "playing" | "idle"
    volume: float | None


@dataclass
class RtpSourceState:
    """Mirrors bridge-daemon's `RtpSourceInfo` JSON shape (api.rs). The
    Bluetooth-bridge RTP source is a native PipeWire module, not a subprocess,
    so its liveness is `loaded` (its node is in the live graph), not `running`."""

    enabled: bool
    port: int
    latency_msec: int
    loaded: bool


@dataclass
class RoutingNode:
    """One source or output in the bridge daemon's routing matrix
    (`RoutingNode` in routing.rs). `node_name` is the stable identity callers
    route on. `present` is False for a configured/previously-routed entity
    that isn't in the live graph right now (shown grayed; re-linked when it
    returns). `node_id` is the ephemeral live id, present only when `present`."""

    node_name: str
    display_name: str
    present: bool = True
    configured: bool = True
    node_id: int | None = None


@dataclass
class RoutingMatrix:
    """Mirrors bridge-daemon's `RoutingMatrix` (routing.rs). `links` are
    `(source_node_name, output_node_name)` pairs — the persisted routing
    intent (stable names), node-level (channels are paired daemon-side)."""

    sources: list[RoutingNode]
    outputs: list[RoutingNode]
    links: list[tuple[str, str]]


def _parse_routing_matrix(data: dict) -> RoutingMatrix:
    """Parse the daemon's `RoutingMatrix` JSON (routing.rs). Shared by the
    REST fetch and the WebSocket push so both stay in lock-step."""

    def _node(item: dict) -> RoutingNode:
        return RoutingNode(
            node_name=item.get("node_name") or item["display_name"],
            display_name=item["display_name"],
            present=item.get("present", True),
            configured=item.get("configured", True),
            node_id=item.get("node_id"),
        )

    return RoutingMatrix(
        sources=[_node(s) for s in data.get("sources", [])],
        outputs=[_node(o) for o in data.get("outputs", [])],
        links=[(str(link["source"]), str(link["output"])) for link in data.get("links", [])],
    )


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

    async def async_get_sendspin_volumes(self) -> dict[str, int]:
        """Desired per-device sendspin volumes (`GET /api/sendspin/volumes`),
        keyed by virtual device node name (0–100). Sparse — a device with no
        entry is at full scale."""
        try:
            async with self._session.get(f"{self._base_url}/api/sendspin/volumes") as resp:
                resp.raise_for_status()
                data = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not reach bridge daemon: {err}") from err
        return {str(k): int(v) for k, v in data.items()}

    async def async_set_sendspin_volume(self, node_name: str, volume: int) -> None:
        """Set one sendspin device's volume (`PUT /api/sendspin/volume`, 0–100).
        Sent in-band to the device; stored daemon-side and re-applied on
        reconnect. There is no PipeWire node volume for these virtual outputs."""
        try:
            async with self._session.put(
                f"{self._base_url}/api/sendspin/volume",
                json={"node_name": node_name, "volume": volume},
            ) as resp:
                resp.raise_for_status()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not set sendspin volume: {err}") from err

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

    async def async_get_rtp_source(self) -> RtpSourceState:
        """Fetch the Bluetooth-bridge RTP source state (`GET /api/source/rtp`)."""
        try:
            async with self._session.get(f"{self._base_url}/api/source/rtp") as resp:
                resp.raise_for_status()
                data = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not reach bridge daemon: {err}") from err
        return RtpSourceState(
            enabled=bool(data.get("enabled", False)),
            port=int(data.get("port", 0)),
            latency_msec=int(data.get("latency_msec", 0)),
            loaded=bool(data.get("loaded", False)),
        )

    async def async_set_rtp_source(self, port: int, latency_msec: int) -> None:
        """Enable (or re-point) the RTP source (`PUT /api/source/rtp`). Sends both
        the listen `port` and the jitter-buffer `latency_msec` — the daemon
        replaces the whole config each call, so both are always supplied (the
        caller passes the current value for whichever knob it isn't changing).
        The daemon reports logical failure (e.g. the module refusing to load) as
        `{ok: false}` carried on a non-2xx status, so — like the routing ops —
        the `ok` flag, not the HTTP status alone, is authoritative."""
        try:
            async with self._session.put(
                f"{self._base_url}/api/source/rtp",
                json={"port": port, "latency_msec": latency_msec},
            ) as resp:
                body = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not enable RTP source: {err}") from err
        if not body.get("ok", False):
            raise PipewireRouterApiError(body.get("message") or "could not enable RTP source")

    async def async_disable_rtp_source(self) -> None:
        """Disable the RTP source (`DELETE /api/source/rtp`). Idempotent daemon-side."""
        try:
            async with self._session.delete(f"{self._base_url}/api/source/rtp") as resp:
                body = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not disable RTP source: {err}") from err
        if not body.get("ok", False):
            raise PipewireRouterApiError(body.get("message") or "could not disable RTP source")

    async def async_get_routing(self) -> RoutingMatrix:
        """Fetch the source×output routing matrix once (`GET /api/routing`).
        Used to seed state at setup; live updates come over the WebSocket
        (`async_routing_ws_messages`)."""
        try:
            async with self._session.get(f"{self._base_url}/api/routing") as resp:
                resp.raise_for_status()
                data = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not reach bridge daemon: {err}") from err
        return _parse_routing_matrix(data)

    async def async_routing_ws_messages(self) -> AsyncIterator[RoutingMatrix]:
        """Subscribe to the daemon's routing WebSocket (`/api/routing/ws`)
        and yield a fresh `RoutingMatrix` for every push — the daemon sends
        one immediately on connect, then one on every registry change, so
        this replaces polling `GET /api/routing`.

        Returns normally when the socket closes (the caller reconnects);
        raises `PipewireRouterApiError` on a connect/transport failure."""
        try:
            async with self._session.ws_connect(
                f"{self._base_url}/api/routing/ws", heartbeat=WS_HEARTBEAT_SECONDS
            ) as ws:
                async for msg in ws:
                    if msg.type is aiohttp.WSMsgType.TEXT:
                        yield _parse_routing_matrix(msg.json())
                    elif msg.type in (
                        aiohttp.WSMsgType.CLOSE,
                        aiohttp.WSMsgType.CLOSING,
                        aiohttp.WSMsgType.CLOSED,
                        aiohttp.WSMsgType.ERROR,
                    ):
                        break
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"routing websocket error: {err}") from err

    async def async_link(self, source: str, output: str) -> None:
        """Link a source into an output by stable node name
        (`POST /api/routing/link`). Persists intent daemon-side; applies live
        when both ends are present."""
        await self._async_routing_op("link", source, output)

    async def async_unlink(self, source: str, output: str) -> None:
        """Remove the `source`→`output` route by stable node name
        (`POST /api/routing/unlink`). Idempotent daemon-side."""
        await self._async_routing_op("unlink", source, output)

    async def _async_routing_op(self, op: str, source: str, output: str) -> None:
        # The routing endpoints answer 200 even for a logical failure (e.g.
        # "no matching channel ports"), carrying `{ok, message}` — so a 2xx
        # status alone isn't success; the `ok` flag is authoritative.
        try:
            async with self._session.post(
                f"{self._base_url}/api/routing/{op}",
                json={"source": source, "output": output},
            ) as resp:
                resp.raise_for_status()
                body = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not {op}: {err}") from err
        if not body.get("ok", False):
            raise PipewireRouterApiError(body.get("message") or f"{op} failed")

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
