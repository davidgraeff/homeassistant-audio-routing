"""Thin async client for the bridge daemon's REST API (bridge-daemon/src/api.rs)."""

from __future__ import annotations

import logging
from collections.abc import AsyncIterator
from dataclasses import dataclass

import aiohttp

_LOGGER = logging.getLogger(__name__)

# Ping cadence for the routing WebSocket — lets aiohttp notice a dead
# connection (and trigger the coordinator's reconnect) even when the daemon
# has no routing changes to push for a while.
WS_HEARTBEAT_SECONDS = 25

# Frame `type`s on /api/routing/ws this client consumes. The daemon multiplexes
# several onto that socket (routing.rs `Frame`); the rest are listings this
# integration re-fetches over REST instead, and are skipped.
ROUTING_WS_FRAME_MATRIX = "matrix"
ROUTING_WS_FRAME_NOW_PLAYING = "now_playing"
# Live input levels and xrun counts, on a 250 ms tick while any client watches.
# Named so it can be skipped *silently*: it is the only high-rate frame on the
# socket and there is nothing an HA entity could do with a peak meter, so logging
# it as an unrecognised frame would mean a debug line four times a second
# throughout playback.
ROUTING_WS_FRAME_METERS = "meters"


class PipewireRouterApiError(Exception):
    """Raised when the bridge daemon's API returns an error or is unreachable."""


@dataclass
class OutputMeta:
    """Supplementary per-output info from `/api/outputs` that the routing
    *matrix* (the entity source of truth) doesn't carry — the output `kind`
    (`"airplay"` | `"sendspin"` | `"airplay2"`) and the receiver's resolved
    `ip`. The IP is what lets an AirPlay-2 output correlate to its Home
    Assistant device for name/area adoption (there's no mDNS hostname trick for
    third-party receivers the way there is for ESPHome sendspin devices).

    For AirPlay-2 outputs (`kind == "airplay2"`) the daemon also carries the
    receiver's per-device volume/mute here: `ap2_volume` is the device-
    authoritative level (0.0–1.0) or `None` when it's genuinely unknown (the
    receiver hasn't reported it and the user hasn't set one) — reported to HA as
    `volume_level = None` rather than fabricating a value. `ap2_muted` is the
    last-known mute flag (defaulting to False). Both are `None` for non-AP2
    outputs (and for any daemon too old to send them).

    For pw-sink outputs (`kind == "pwsink"`) the same pair appears as
    `pwsink_volume`/`pwsink_muted` — the *host's own master out*, as reported by
    the pwrouter-agent running on it. The host owns that value (the user can turn
    the knob on their desktop), so it is likewise `None` when no agent is
    connected rather than a fabricated level."""

    node_name: str
    kind: str
    ip: str | None
    ap2_volume: float | None = None
    ap2_muted: bool | None = None
    pwsink_volume: float | None = None
    pwsink_muted: bool | None = None


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
class NowPlaying:
    """What one source is currently playing (`NowPlaying` in the daemon's
    now_playing.rs), as pushed on the routing socket's `now_playing` frame and
    served by `GET /api/now_playing`.

    Keyed elsewhere by source *node name* — the same identity the routing matrix
    and the persisted routing intent use — so an entity can look this up with the
    node name it already resolved for its `source` property.

    `position_updated_at` is Unix milliseconds: the position is only meaningful
    together with when it was true, and Home Assistant extrapolates from it (the
    daemon publishes a position at most every 5 s). `image_path` is a
    daemon-relative path for embedded cover art, already stamped with the
    revision that changes when the picture does; `image_url` is an absolute URL
    when a producer supplied one instead. At most one of the two is set."""

    state: str  # "playing" | "paused" | "stopped"
    title: str | None = None
    artist: str | None = None
    album: str | None = None
    duration_ms: int | None = None
    position_ms: int | None = None
    position_updated_at: int | None = None
    image_url: str | None = None
    image_path: str | None = None

    @property
    def has_metadata(self) -> bool:
        """Whether there is anything worth showing. A bare `stopped` with no
        fields is an entry mid-teardown, not a track."""
        return any((self.title, self.artist, self.album))


def _parse_now_playing(data: dict) -> dict[str, NowPlaying]:
    """Parse a `now_playing` frame / `GET /api/now_playing` body into entries by
    source node name. Unparseable entries are skipped rather than failing the
    whole frame — one odd source must not blank every other entity's metadata."""
    out: dict[str, NowPlaying] = {}
    for node_name, item in (data.get("sources") or {}).items():
        if not isinstance(item, dict):
            continue
        art = item.get("artwork") or {}
        out[str(node_name)] = NowPlaying(
            state=str(item.get("state") or "stopped"),
            title=item.get("title"),
            artist=item.get("artist"),
            album=item.get("album"),
            duration_ms=item.get("duration_ms"),
            position_ms=item.get("position_ms"),
            position_updated_at=item.get("position_updated_at"),
            image_url=art.get("url") if art.get("kind") == "url" else None,
            image_path=art.get("path") if art.get("kind") == "embedded" else None,
        )
    return out


@dataclass
class NowPlayingFrame:
    """A `now_playing` push: every source with metadata, by node name.

    A distinct type (rather than a bare dict) so the routing socket can yield two
    kinds of update and the coordinator can tell them apart without inspecting
    shapes."""

    sources: dict[str, NowPlaying]


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


@dataclass
class MusicGroup:
    """A named music group (groups_store.rs): exclusive set of member outputs."""

    id: str
    name: str
    members: list[str]


@dataclass
class AnnouncementGroup:
    """A named announcement group: target outputs + priority + duck level."""

    id: str
    name: str
    targets: list[str]
    priority: int
    duck: float


@dataclass
class AppSettings:
    """Subset of the daemon's `/api/settings` the integration needs."""

    expose_outputs_as_media_players: bool


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

    @property
    def base_url(self) -> str:
        """This daemon's base URL. Needed to turn a daemon-relative artwork path
        into something Home Assistant can fetch."""
        return self._base_url

    async def async_health(self) -> None:
        """Reachability probe (`GET /health`) — the daemon's cheapest endpoint,
        used by the config flow and as the coordinator's authoritative call (the
        one whose failure takes the entities down). Raises on anything else."""
        try:
            async with self._session.get(f"{self._base_url}/health") as resp:
                resp.raise_for_status()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not reach bridge daemon: {err}") from err

    async def async_get_outputs(self) -> list[OutputMeta]:
        """Fetch the Outputs listing (`GET /api/outputs`) for the per-output
        `kind` and resolved `ip`. The routing matrix omits both, but the
        AirPlay-2 device-adoption path needs the IP; the `kind` is a convenience
        (the media_player platform still keys behaviour off the node-name
        prefix). Best-effort — an older daemon without this endpoint just yields
        an empty map coordinator-side."""
        try:
            async with self._session.get(f"{self._base_url}/api/outputs") as resp:
                resp.raise_for_status()
                data = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not reach bridge daemon: {err}") from err
        return [
            OutputMeta(
                node_name=str(item["node_name"]),
                kind=str(item.get("kind", "")),
                ip=item.get("ip"),
                # Absent/null => volume genuinely unknown; keep it None (do not
                # coerce to a number) so the entity can report volume_level=None.
                ap2_volume=item.get("ap2_volume"),
                ap2_muted=item.get("ap2_muted"),
                pwsink_volume=item.get("pwsink_volume"),
                pwsink_muted=item.get("pwsink_muted"),
            )
            for item in data
        ]

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

    async def async_set_ap2_volume(self, node_name: str, volume: float) -> None:
        """Set one AirPlay-2 receiver's volume (`PUT /api/ap2/volume`, 0.0–1.0).
        Pushed in-band to the receiver as an RTSP SET_PARAMETER and stored
        daemon-side (re-applied while streaming). Like the sendspin outputs
        these are virtual — there's no PipeWire node volume to set."""
        try:
            async with self._session.put(
                f"{self._base_url}/api/ap2/volume",
                json={"node_name": node_name, "volume": volume},
            ) as resp:
                resp.raise_for_status()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not set AirPlay-2 volume: {err}") from err

    async def async_set_pwsink_volume(self, node_name: str, volume: float) -> None:
        """Set a pw-sink host's *master* volume (`PUT /api/pwsink/volume`,
        0.0-1.0 cubic). Applied by that host's pwrouter-agent to the sink our
        stream actually plays into, via the device's Route param — the same lever
        the user's own volume applet uses, so the two agree.

        Unlike sendspin/AirPlay-2 volume there is no store-and-replay: an
        unreachable host is a failure, not a saved intent, because the host owns
        the value and reports it back."""
        try:
            async with self._session.put(
                f"{self._base_url}/api/pwsink/volume",
                json={"node_name": node_name, "volume": volume},
            ) as resp:
                resp.raise_for_status()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not set pw-sink volume: {err}") from err

    async def async_set_pwsink_mute(self, node_name: str, muted: bool) -> None:
        """Mute/unmute a pw-sink host's master out (`PUT /api/pwsink/mute`)."""
        try:
            async with self._session.put(
                f"{self._base_url}/api/pwsink/mute",
                json={"node_name": node_name, "muted": muted},
            ) as resp:
                resp.raise_for_status()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not set pw-sink mute: {err}") from err

    async def async_set_ap2_mute(self, node_name: str, muted: bool) -> None:
        """Mute/unmute one AirPlay-2 receiver (`PUT /api/ap2/mute`). Daemon-side
        this maps to volume 0; stored and re-applied like the volume."""
        try:
            async with self._session.put(
                f"{self._base_url}/api/ap2/mute",
                json={"node_name": node_name, "muted": muted},
            ) as resp:
                resp.raise_for_status()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not set AirPlay-2 mute: {err}") from err

    # The RTP source is now one entry in the daemon's source collection
    # (`/api/sources`), not the retired singular `/api/source/rtp`. This
    # integration models a single Bluetooth-bridge RTP input, so it operates on
    # THE rtp source: the well-known legacy id `bt-bridge-rtp` if present, else
    # the first source of kind `rtp`. Disabled defaults mirror the daemon's own
    # (rtp_source.rs): 46000 / 200 ms.
    _RTP_LEGACY_ID = "bt-bridge-rtp"
    _RTP_DEFAULT_PORT = 46000
    _RTP_DEFAULT_LATENCY_MSEC = 200

    async def _async_list_sources(self) -> list[dict]:
        """The daemon's source collection (`GET /api/sources`)."""
        try:
            async with self._session.get(f"{self._base_url}/api/sources") as resp:
                resp.raise_for_status()
                data = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not reach bridge daemon: {err}") from err
        return list(data.get("sources", []))

    def _pick_rtp(self, sources: list[dict]) -> dict | None:
        """THE rtp source this integration manages (legacy id, else first rtp)."""
        rtp = [s for s in sources if s.get("kind") == "rtp"]
        for src in rtp:
            if src.get("id") == self._RTP_LEGACY_ID:
                return src
        return rtp[0] if rtp else None

    async def async_get_rtp_source(self) -> RtpSourceState:
        """Fetch the Bluetooth-bridge RTP source state from the source collection
        (`GET /api/sources`). `enabled` = the source exists in the collection;
        `loaded` = its PipeWire node is live (`present`)."""
        src = self._pick_rtp(await self._async_list_sources())
        if src is None:
            return RtpSourceState(
                enabled=False,
                port=self._RTP_DEFAULT_PORT,
                latency_msec=self._RTP_DEFAULT_LATENCY_MSEC,
                loaded=False,
            )
        rtp = src.get("rtp") or {}
        return RtpSourceState(
            enabled=True,
            port=int(rtp.get("port", self._RTP_DEFAULT_PORT)),
            latency_msec=int(rtp.get("latency_msec", self._RTP_DEFAULT_LATENCY_MSEC)),
            loaded=bool(src.get("present", False)),
        )

    async def async_set_rtp_source(self, port: int, latency_msec: int) -> None:
        """Enable (or re-point) the RTP source. If it already exists, its stored
        config is read and only `port`/`latency_msec` are overridden — so
        `source_addr`/`ignore_ssrc`/`rate` (e.g. a multicast group) are PRESERVED
        (`PUT /api/sources/{id}`); if it doesn't exist yet it's created with the
        daemon's defaults for the rest (`POST /api/sources`). Logical failure is
        carried as `{ok: false}`, so that flag — not the HTTP status alone — is
        authoritative. (ids are slugs, URL-safe, so used unescaped.)"""
        src = self._pick_rtp(await self._async_list_sources())
        if src is not None:
            rtp = dict(src.get("rtp") or {})
            rtp["port"] = port
            rtp["latency_msec"] = latency_msec
            url = f"{self._base_url}/api/sources/{src['id']}"
            payload: dict = {"rtp": rtp}
            request = self._session.put
        else:
            url = f"{self._base_url}/api/sources"
            payload = {"label": "Bluetooth Bridge", "kind": "rtp", "rtp": {"port": port, "latency_msec": latency_msec}}
            request = self._session.post
        try:
            async with request(url, json=payload) as resp:
                body = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not enable RTP source: {err}") from err
        # POST returns the created SourceView (no `ok` field = success); PUT and
        # error responses carry `ok`. Only an explicit `ok: false` is a failure.
        if isinstance(body, dict) and body.get("ok") is False:
            raise PipewireRouterApiError(body.get("message") or "could not enable RTP source")

    async def async_disable_rtp_source(self) -> None:
        """Disable the RTP source by removing it from the collection
        (`DELETE /api/sources/{id}`). Idempotent: a no-op if none exists."""
        src = self._pick_rtp(await self._async_list_sources())
        if src is None:
            return
        try:
            async with self._session.delete(f"{self._base_url}/api/sources/{src['id']}") as resp:
                body = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not disable RTP source: {err}") from err
        if isinstance(body, dict) and body.get("ok") is False:
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

    async def async_routing_ws_messages(self) -> AsyncIterator[RoutingMatrix | NowPlayingFrame]:
        """Subscribe to the daemon's routing WebSocket (`/api/routing/ws`)
        and yield a fresh `RoutingMatrix` for every matrix push — the daemon
        sends one immediately on connect, then one on every registry change
        (and on its own meter tick), so this replaces polling `GET /api/routing`.

        Also yields a `NowPlayingFrame` for every `now_playing` push (per-source
        metadata, see the daemon's now_playing.rs). That frame is sent only when
        something actually changed, which is why it is a *separate* frame from the
        matrix and not a field on it.

        **That socket carries more than matrices.** `routing.rs`'s `Frame` enum is
        internally tagged, and alongside `{"type": "matrix", …}` it pushes
        `outputs`, `discovered` and `agents` listing frames — the first of them
        immediately after the initial matrix, on every connect. Frames we don't
        consume are skipped here rather than parsed: feeding a listing frame to
        `_parse_routing_matrix` raised `KeyError: 'display_name'` (an `OutputInfo`
        has `node_name`/`name`, no `display_name`), which the caller's catch-all
        turned into a reconnect — so the socket never survived its first second
        and the push path had quietly degraded to a 5-second reconnect poll with a
        traceback each time.

        A frame with no `type` at all is treated as a matrix: the matrix frame
        historically *was* the whole frame, and the daemon tags it internally
        precisely so that older readers keep working.

        Returns normally when the socket closes (the caller reconnects);
        raises `PipewireRouterApiError` on a connect/transport failure."""
        try:
            async with self._session.ws_connect(
                f"{self._base_url}/api/routing/ws", heartbeat=WS_HEARTBEAT_SECONDS
            ) as ws:
                async for msg in ws:
                    if msg.type is aiohttp.WSMsgType.TEXT:
                        data = msg.json()
                        if not isinstance(data, dict):
                            continue
                        frame = data.get("type")
                        if frame == ROUTING_WS_FRAME_METERS:
                            continue
                        if frame == ROUTING_WS_FRAME_NOW_PLAYING:
                            yield NowPlayingFrame(_parse_now_playing(data))
                            continue
                        if frame is not None and frame != ROUTING_WS_FRAME_MATRIX:
                            _LOGGER.debug("ignoring '%s' frame on the routing socket", frame)
                            continue
                        yield _parse_routing_matrix(data)
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

    async def async_get_music_groups(self) -> list[MusicGroup]:
        """Named music groups (`GET /api/groups/music`)."""
        try:
            async with self._session.get(f"{self._base_url}/api/groups/music") as resp:
                resp.raise_for_status()
                data = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not reach bridge daemon: {err}") from err
        return [
            MusicGroup(id=str(g["id"]), name=str(g["name"]), members=[str(m) for m in g.get("members", [])])
            for g in data
        ]

    async def async_get_announcement_groups(self) -> list[AnnouncementGroup]:
        """Named announcement groups (`GET /api/groups/announcement`)."""
        try:
            async with self._session.get(f"{self._base_url}/api/groups/announcement") as resp:
                resp.raise_for_status()
                data = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not reach bridge daemon: {err}") from err
        return [
            AnnouncementGroup(
                id=str(g["id"]),
                name=str(g["name"]),
                targets=[str(t) for t in g.get("targets", [])],
                priority=int(g.get("priority", 0)),
                duck=float(g.get("duck", 0.25)),
            )
            for g in data
        ]

    async def async_get_settings(self) -> AppSettings:
        """Daemon app settings (`GET /api/settings`) — the toggle for per-output entities."""
        try:
            async with self._session.get(f"{self._base_url}/api/settings") as resp:
                resp.raise_for_status()
                data = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not reach bridge daemon: {err}") from err
        return AppSettings(expose_outputs_as_media_players=bool(data.get("expose_outputs_as_media_players", False)))

    async def async_route_music_group(self, group_id: str, source: str) -> None:
        """Route a source to a whole music group (`POST /api/groups/music/{id}/route`)."""
        try:
            async with self._session.post(
                f"{self._base_url}/api/groups/music/{group_id}/route",
                json={"source": source},
            ) as resp:
                body = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not route group: {err}") from err
        if not body.get("ok", False):
            raise PipewireRouterApiError(body.get("message") or "route failed")

    async def async_unroute_music_group(self, group_id: str) -> None:
        """Un-route a whole music group (`DELETE /api/groups/music/{id}/route`)."""
        try:
            async with self._session.delete(f"{self._base_url}/api/groups/music/{group_id}/route") as resp:
                body = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not un-route group: {err}") from err
        if not body.get("ok", False):
            raise PipewireRouterApiError(body.get("message") or "un-route failed")

    async def async_announce_group(self, group_id: str, *, url: str | None = None, wyoming: dict | None = None) -> None:
        """Announce to a named announcement group (`POST /api/announce`), which
        resolves the group's targets/priority/duck. Returns once **admitted**
        (playing/queued), not after playback."""
        payload: dict = {"announcement_group": group_id}
        if url is not None:
            payload["url"] = url
        elif wyoming is not None:
            payload["wyoming"] = wyoming
        try:
            async with self._session.post(
                f"{self._base_url}/api/announce",
                json=payload,
                timeout=aiohttp.ClientTimeout(total=30),
            ) as resp:
                body = await resp.json()
        except aiohttp.ClientError as err:
            raise PipewireRouterApiError(f"could not announce to group: {err}") from err
        if not body.get("ok", False):
            raise PipewireRouterApiError(body.get("message") or body.get("reason") or "announce rejected")
