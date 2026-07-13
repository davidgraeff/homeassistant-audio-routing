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
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .api import (
    MediaPlayerState,
    PipewireRouterApiClient,
    PipewireRouterApiError,
    RoutingMatrix,
    RtpSourceState,
)
from .const import (
    CONF_HOST,
    CONF_PORT,
    DEFAULT_RTP_PORT,
    DOMAIN,
    ROUTING_WS_RECONNECT_SECONDS,
    UPDATE_INTERVAL_SECONDS,
)

_LOGGER = logging.getLogger(__name__)
PLATFORMS = [Platform.MEDIA_PLAYER, Platform.NUMBER, Platform.SWITCH]

_EMPTY_ROUTING = RoutingMatrix(sources=[], outputs=[], links=[])


class PipewireRouterCoordinator(DataUpdateCoordinator[list[MediaPlayerState]]):
    """Two feeds from one daemon:

    - **Players** (volume + playing/idle state) are *polled* via
      `GET /api/media_players` — there's no push channel for volume — and
      live in `.data`.
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
        # Latest RTP-source state, refreshed each poll (best-effort — an older
        # daemon without `/api/source/rtp` leaves this `None`). `None` = the
        # switch/number entities show unavailable.
        self.rtp: RtpSourceState | None = None
        # The port the switch enables with. The daemon only remembers a port
        # while the source is *enabled*; when it's disabled the daemon reports
        # the default, so we track the user's intended port here (and the
        # `number` entity restores it across restarts) so a chosen-but-not-yet-
        # enabled port isn't lost.
        self.rtp_desired_port: int = DEFAULT_RTP_PORT

    async def _async_update_data(self) -> list[MediaPlayerState]:
        try:
            players = await self.client.async_get_media_players()
        except PipewireRouterApiError as err:
            raise UpdateFailed(str(err)) from err
        # RTP state is secondary — never fail the whole update (and take the
        # media_player entities down) if only this call fails.
        try:
            self.rtp = await self.client.async_get_rtp_source()
            if self.rtp.enabled:
                # An enabled source's stored port is authoritative.
                self.rtp_desired_port = self.rtp.port
        except PipewireRouterApiError as err:
            _LOGGER.debug("rtp source state unavailable: %s", err)
            self.rtp = None
        return players

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

    async def async_routing_ws_loop(self) -> None:
        """Hold a routing WebSocket open for the life of the config entry,
        applying each pushed matrix and reconnecting after a drop. Ends when
        the entry is unloaded (the background task is cancelled)."""
        while True:
            try:
                async for matrix in self.client.async_routing_ws_messages():
                    self._apply_routing(matrix)
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

    hass.data.setdefault(DOMAIN, {})[entry.entry_id] = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    unloaded = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if unloaded:
        hass.data[DOMAIN].pop(entry.entry_id, None)
    return unloaded
