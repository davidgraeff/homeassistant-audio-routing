"""PipeWire Audio Router — talks to one bridge-daemon instance's REST API.

See ../PLAN.md Section 6/9 for why this is a custom_components integration
rather than MQTT discovery (no media_player MQTT platform exists in HA
core at all).
"""

from __future__ import annotations

import logging
from datetime import timedelta

import aiohttp
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .api import MediaPlayerState, PipewireRouterApiClient, PipewireRouterApiError
from .const import CONF_HOST, CONF_PORT, DOMAIN, UPDATE_INTERVAL_SECONDS

_LOGGER = logging.getLogger(__name__)
PLATFORMS = [Platform.MEDIA_PLAYER]


class PipewireRouterCoordinator(DataUpdateCoordinator[list[MediaPlayerState]]):
    """Polls GET /api/media_players on an interval — no push/WebSocket
    subscription exists on the bridge daemon side yet (Section 8's
    routing UI would be the natural place to add one later)."""

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry, client: PipewireRouterApiClient) -> None:
        super().__init__(
            hass,
            _LOGGER,
            config_entry=entry,
            name=DOMAIN,
            update_interval=timedelta(seconds=UPDATE_INTERVAL_SECONDS),
        )
        self.client = client

    async def _async_update_data(self) -> list[MediaPlayerState]:
        try:
            return await self.client.async_get_media_players()
        except PipewireRouterApiError as err:
            raise UpdateFailed(str(err)) from err


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Set up one bridge-daemon connection from a config entry."""
    session: aiohttp.ClientSession = async_get_clientsession(hass)
    client = PipewireRouterApiClient(session, entry.data[CONF_HOST], entry.data[CONF_PORT])
    coordinator = PipewireRouterCoordinator(hass, entry, client)
    await coordinator.async_config_entry_first_refresh()

    hass.data.setdefault(DOMAIN, {})[entry.entry_id] = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    unloaded = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if unloaded:
        hass.data[DOMAIN].pop(entry.entry_id, None)
    return unloaded
