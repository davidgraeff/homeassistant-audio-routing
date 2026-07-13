"""Switch to enable/disable the Bluetooth-bridge RTP source on the add-on.

A single switch per daemon. Turning it on loads `libpipewire-module-rtp-source`
into the add-on (via `PUT /api/source/rtp`) on the port held by the companion
`number` entity; turning it off unloads it (`DELETE /api/source/rtp`). Once on,
the source's `bt-bridge-rtp` node shows up in every output's `source_list`
automatically (the routing matrix is pushed live over the WebSocket), so there
is nothing else to wire — pick it as an output's source like any other.
"""

from __future__ import annotations

from homeassistant.components.switch import SwitchEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import PipewireRouterCoordinator
from .const import DOMAIN


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    coordinator: PipewireRouterCoordinator = hass.data[DOMAIN][entry.entry_id]
    async_add_entities([PipewireRtpSourceSwitch(coordinator, entry)])


class PipewireRtpSourceSwitch(CoordinatorEntity[PipewireRouterCoordinator], SwitchEntity):
    """Enable/disable the RTP source that receives the ESP32 Bluetooth bridge."""

    _attr_has_entity_name = True
    _attr_name = "Bluetooth bridge RTP source"
    _attr_entity_category = EntityCategory.CONFIG
    _attr_icon = "mdi:bluetooth-audio"

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_rtp_source"

    @property
    def available(self) -> bool:
        # Unavailable if the daemon is unreachable, or too old to expose
        # `/api/source/rtp` (coordinator leaves `rtp` None in that case).
        return super().available and self.coordinator.rtp is not None

    @property
    def is_on(self) -> bool | None:
        rtp = self.coordinator.rtp
        return rtp.enabled if rtp is not None else None

    async def async_turn_on(self, **kwargs) -> None:
        await self.coordinator.client.async_set_rtp_source(self.coordinator.rtp_desired_port)
        await self.coordinator.async_request_refresh()

    async def async_turn_off(self, **kwargs) -> None:
        await self.coordinator.client.async_disable_rtp_source()
        await self.coordinator.async_request_refresh()
