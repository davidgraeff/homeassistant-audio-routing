"""PipeWire Audio Router — talks to one bridge-daemon instance's REST API.

See ../PLAN.md Section 6/9 for why this is a custom_components integration
rather than MQTT discovery (no media_player MQTT platform exists in HA
core at all).
"""

from __future__ import annotations

import asyncio
import logging
from datetime import timedelta

import aiohttp
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant, ServiceCall, callback
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .api import (
    AnnouncementGroup,
    MusicGroup,
    NowPlaying,
    NowPlayingFrame,
    OutputMeta,
    PipewireRouterApiClient,
    PipewireRouterApiError,
    RoutingMatrix,
    RtpSourceState,
)
from .const import (
    CONF_HOST,
    CONF_PORT,
    DEFAULT_RTP_LATENCY_MSEC,
    DEFAULT_RTP_PORT,
    DOMAIN,
    ROUTING_WS_RECONNECT_SECONDS,
    SERVICE_CLEANUP_ENTITIES,
    UPDATE_INTERVAL_SECONDS,
)
from .voice_duck import VoiceDucker

_LOGGER = logging.getLogger(__name__)
PLATFORMS = [Platform.MEDIA_PLAYER, Platform.NUMBER, Platform.SELECT, Platform.SWITCH]

_EMPTY_ROUTING = RoutingMatrix(sources=[], outputs=[], links=[])


class PipewireRouterCoordinator(DataUpdateCoordinator[None]):
    """Two feeds from one daemon:

    - **Polled** state that has no push channel — RTP source, per-device
      volumes, output metadata, named groups — is refreshed on the interval and
      lives in the attributes set up below, not in `.data` (which is unused:
      every value here has a different shape and its own staleness rules).
    - **Routing** (the source×output matrix) is *pushed* over the daemon's
      `/api/routing/ws` WebSocket by a background task (`async_routing_ws_loop`)
      and lives in `.routing`, so a re-wire is reflected instantly instead of
      up to one poll interval later.
    """

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry, client: PipewireRouterApiClient) -> None:
        super().__init__(
            hass,
            _LOGGER,
            config_entry=entry,
            name=DOMAIN,
            update_interval=timedelta(seconds=UPDATE_INTERVAL_SECONDS),
        )
        self.client = client
        # Latest routing matrix, kept current by the WS task. Seeded by a
        # one-shot REST fetch at setup (`async_init_routing`) so entities have
        # it before the socket's first push arrives.
        self.routing: RoutingMatrix = _EMPTY_ROUTING
        # What each source is playing, by source node name — pushed on the same
        # socket as the matrix, but in its own low-rate frame (see the daemon's
        # now_playing.rs). Empty until the first push; a source with no entry is
        # simply not playing anything we know about.
        self.now_playing: dict[str, NowPlaying] = {}
        # Latest RTP-source state, refreshed each poll (best-effort — an older
        # daemon without an RTP source in `/api/sources` leaves this `None`). `None` = the
        # switch/number entities show unavailable.
        self.rtp: RtpSourceState | None = None
        # The port the switch enables with. The daemon only remembers a port
        # while the source is *enabled*; when it's disabled the daemon reports
        # the default, so we track the user's intended port here (and the
        # `number` entity restores it across restarts) so a chosen-but-not-yet-
        # enabled port isn't lost.
        self.rtp_desired_port: int = DEFAULT_RTP_PORT
        # Same story for the jitter-buffer latency: tracked here so a value
        # chosen while the source is disabled survives until the next enable
        # (and is restored across restarts by its `number` entity).
        self.rtp_desired_latency_msec: int = DEFAULT_RTP_LATENCY_MSEC
        # Desired per-device sendspin volumes (node_name -> 0-100), refreshed
        # each poll. Sendspin devices are virtual (no PipeWire node volume), so
        # their media_player volume comes from here, not from `.data`.
        self.sendspin_volumes: dict[str, int] = {}
        # Supplementary per-output metadata from `/api/outputs`, keyed by node
        # name, refreshed each poll. The routing matrix (which drives the
        # entities) doesn't carry the receiver IP, and the AirPlay-2 output's
        # device/area adoption needs it — so it's fetched here alongside it.
        self.outputs_meta: dict[str, OutputMeta] = {}
        # Named music/announcement groups (groups_store.rs) + the per-output
        # entity toggle, refreshed each poll. These drive which media_player
        # entities exist: one per music group + one per announcement group, plus
        # one per output when `expose_outputs` is on.
        self.music_groups: list[MusicGroup] = []
        self.announcement_groups: list[AnnouncementGroup] = []
        self.expose_outputs: bool = False
        # Voice-assistant ducking (voice_duck.py). Set in `async_setup_entry`
        # right after the coordinator exists, because the ducker reads the polled
        # routing/groups/outputs state this object holds. Its enabled/level/scope
        # live on it and are owned by the switch/number/select entities.
        self.voice_duck: VoiceDucker = None  # type: ignore[assignment]

    async def _async_update_data(self) -> None:
        # Reachability first, and it is the only fatal call: everything below is
        # best-effort, so one unavailable endpoint must not take the entities
        # down. (This used to be `GET /api/media_players`, which no longer
        # exists — every output is virtual, so it had nothing to report.)
        try:
            await self.client.async_health()
        except PipewireRouterApiError as err:
            raise UpdateFailed(str(err)) from err
        # RTP state is secondary — never fail the whole update (and take the
        # media_player entities down) if only this call fails.
        try:
            self.rtp = await self.client.async_get_rtp_source()
            if self.rtp.enabled:
                # An enabled source's stored config is authoritative.
                self.rtp_desired_port = self.rtp.port
                self.rtp_desired_latency_msec = self.rtp.latency_msec
        except PipewireRouterApiError as err:
            _LOGGER.debug("rtp source state unavailable: %s", err)
            self.rtp = None
        # Sendspin volumes are secondary too — never take entities down for it.
        try:
            self.sendspin_volumes = await self.client.async_get_sendspin_volumes()
        except PipewireRouterApiError as err:
            _LOGGER.debug("sendspin volumes unavailable: %s", err)
        # Output metadata (IP/kind) is secondary too — an AP2 output simply
        # doesn't adopt an HA device if this is unavailable.
        try:
            self.outputs_meta = {o.node_name: o for o in await self.client.async_get_outputs()}
        except PipewireRouterApiError as err:
            _LOGGER.debug("outputs listing unavailable: %s", err)
        # Groups + the per-output toggle drive entity creation; secondary, and an
        # older daemon without these endpoints simply yields no groups.
        try:
            self.music_groups = await self.client.async_get_music_groups()
            self.announcement_groups = await self.client.async_get_announcement_groups()
            self.expose_outputs = (await self.client.async_get_settings()).expose_outputs_as_media_players
        except PipewireRouterApiError as err:
            _LOGGER.debug("groups/settings unavailable: %s", err)

    async def async_init_routing(self) -> None:
        """One-shot routing fetch so `source`/`source_list` are populated the
        moment entities are added. Non-fatal on failure — the WebSocket
        delivers a full snapshot on connect regardless."""
        try:
            self.routing = await self.client.async_get_routing()
        except PipewireRouterApiError as err:
            _LOGGER.debug("initial routing fetch failed, waiting for websocket: %s", err)

    @callback
    def _apply_routing(self, matrix: RoutingMatrix) -> None:
        """Store a pushed matrix and re-render every entity immediately."""
        self.routing = matrix
        self.async_update_listeners()

    @callback
    def _apply_now_playing(self, sources: dict[str, NowPlaying]) -> None:
        """Store a pushed metadata frame and re-render.

        The frame is always the *complete* picture, so assigning it (rather than
        merging) is what makes a cleared source disappear — which is the whole
        point of the daemon clearing on session end instead of letting its TTL
        expire."""
        self.now_playing = sources
        self.async_update_listeners()

    @callback
    def now_playing_for(self, source_node_name: str | None) -> NowPlaying | None:
        """What the given source is playing, if anything worth showing.

        The single lookup both the output and the music-group `media_player` use,
        so their metadata can never disagree about the same source."""
        if not source_node_name:
            return None
        entry = self.now_playing.get(source_node_name)
        if entry is None or not entry.has_metadata:
            return None
        return entry

    async def async_routing_ws_loop(self) -> None:
        """Hold a routing WebSocket open for the life of the config entry,
        applying each pushed matrix or metadata frame and reconnecting after a
        drop. Ends when the entry is unloaded (the background task is
        cancelled)."""
        while True:
            try:
                async for frame in self.client.async_routing_ws_messages():
                    if isinstance(frame, NowPlayingFrame):
                        self._apply_now_playing(frame.sources)
                    else:
                        self._apply_routing(frame)
            except PipewireRouterApiError as err:
                _LOGGER.debug("routing websocket disconnected: %s", err)
            except Exception:  # noqa: BLE001 - never let the loop die on a bad frame
                _LOGGER.exception("unexpected error in routing websocket loop")
            # Socket closed/errored (or a bad frame) — back off, then reconnect.
            await asyncio.sleep(ROUTING_WS_RECONNECT_SECONDS)


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Set up one bridge-daemon connection from a config entry."""
    session: aiohttp.ClientSession = async_get_clientsession(hass)
    client = PipewireRouterApiClient(session, entry.data[CONF_HOST], entry.data[CONF_PORT])
    coordinator = PipewireRouterCoordinator(hass, entry, client)
    await coordinator.async_config_entry_first_refresh()
    await coordinator.async_init_routing()

    # Live routing over the WebSocket instead of polling /api/routing. Bound
    # to the entry, so it's cancelled automatically on unload.
    entry.async_create_background_task(
        hass, coordinator.async_routing_ws_loop(), f"{DOMAIN}_routing_ws"
    )

    # Voice-assistant ducking: watches every `assist_satellite` and asks the
    # daemon to duck the outputs in the talking satellite's area. Inert until the
    # switch entity turns it on (off by default — see PipewireVoiceDuckingSwitch).
    coordinator.voice_duck = VoiceDucker(hass, entry, coordinator)
    coordinator.voice_duck.async_start()

    hass.data.setdefault(DOMAIN, {})[entry.entry_id] = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    _async_register_cleanup_service(hass)
    return True


@callback
def _async_register_cleanup_service(hass: HomeAssistant) -> None:
    """Register the domain-wide `cleanup_entities` service once. It deletes
    media_player registry entries whose output the daemon no longer reports —
    the manual purge for stale/renamed devices left behind as `unavailable`.
    Run it while the daemon is reachable so the live matrix is authoritative."""
    if hass.services.has_service(DOMAIN, SERVICE_CLEANUP_ENTITIES):
        return

    async def _handle_cleanup(_call: ServiceCall) -> None:
        registry = er.async_get(hass)
        removed = 0
        for entry_id, coordinator in hass.data.get(DOMAIN, {}).items():
            # New scheme: one entity per music group + per announcement group,
            # plus per output when the toggle is on. Namespaced unique_ids so the
            # kinds never collide. Anything else (incl. pre-migration
            # f"{entry_id}_{node_name}" per-output ids) is stale → removed.
            valid = {f"{entry_id}_mg_{g.id}" for g in coordinator.music_groups}
            valid |= {f"{entry_id}_ag_{g.id}" for g in coordinator.announcement_groups}
            if coordinator.expose_outputs:
                valid |= {f"{entry_id}_out_{o.node_name}" for o in coordinator.routing.outputs}
            for entity in er.async_entries_for_config_entry(registry, entry_id):
                if entity.domain != "media_player":
                    continue
                if entity.unique_id not in valid:
                    registry.async_remove(entity.entity_id)
                    removed += 1
        _LOGGER.info("cleanup_entities removed %d stale media_player entit%s", removed, "y" if removed == 1 else "ies")

    hass.services.async_register(DOMAIN, SERVICE_CLEANUP_ENTITIES, _handle_cleanup)


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    unloaded = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if unloaded:
        coordinator = hass.data[DOMAIN].pop(entry.entry_id, None)
        # Stop watching satellites and hand back any duck we're holding, so a
        # reload mid-turn doesn't leave music quiet until the lease expires.
        if coordinator is not None and coordinator.voice_duck is not None:
            await coordinator.voice_duck.async_stop()
    return unloaded
