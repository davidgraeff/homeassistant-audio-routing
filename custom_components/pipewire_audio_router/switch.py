"""Switch to enable/disable the Bluetooth-bridge RTP source on the add-on.

A single switch per daemon. Turning it on loads `libpipewire-module-rtp-source`
into the add-on (via the `/api/sources` collection) on the port held by the companion
`number` entity; turning it off removes it (`DELETE /api/sources/{id}`). Once on,
the source's `bt-bridge-rtp` node shows up in every output's `source_list`
automatically (the routing matrix is pushed live over the WebSocket), so there
is nothing else to wire — pick it as an output's source like any other.
"""

from __future__ import annotations

from homeassistant.components.switch import SwitchEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import STATE_OFF, EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.restore_state import RestoreEntity
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import PipewireRouterCoordinator
from .const import DOMAIN


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    coordinator: PipewireRouterCoordinator = hass.data[DOMAIN][entry.entry_id]
    async_add_entities(
        [
            PipewireRtpSourceSwitch(coordinator, entry),
            PipewireVoiceDuckingSwitch(coordinator, entry),
        ]
    )


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
        # `/api/sources` (coordinator leaves `rtp` None in that case).
        return super().available and self.coordinator.rtp is not None

    @property
    def is_on(self) -> bool | None:
        rtp = self.coordinator.rtp
        return rtp.enabled if rtp is not None else None

    async def async_turn_on(self, **kwargs) -> None:
        await self.coordinator.client.async_set_rtp_source(
            self.coordinator.rtp_desired_port,
            self.coordinator.rtp_desired_latency_msec,
        )
        await self.coordinator.async_request_refresh()

    async def async_turn_off(self, **kwargs) -> None:
        await self.coordinator.client.async_disable_rtp_source()
        await self.coordinator.async_request_refresh()


class PipewireVoiceDuckingSwitch(CoordinatorEntity[PipewireRouterCoordinator], SwitchEntity, RestoreEntity):
    """Duck the router's speakers in a room while a voice assistant there talks.

    **On by default** (`DEFAULT_VOICE_DUCK_ENABLED`): it needs no configuration —
    satellites, areas and outputs all come from registries HA already has — so a
    default of off is a feature nobody finds. Only an explicit off is remembered:
    the state is ours to keep, since the daemon knows nothing about rooms or
    satellites.

    Note this switch is an *enable flag*, not a duck: turning it on arms the
    listener, and music only goes quiet once a satellite in a room with router
    outputs actually starts a turn.
    """

    _attr_has_entity_name = True
    _attr_name = "Voice assistant ducking"
    _attr_entity_category = EntityCategory.CONFIG
    _attr_icon = "mdi:account-voice"

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_voice_ducking"

    async def async_added_to_hass(self) -> None:
        await super().async_added_to_hass()
        last = await self.async_get_last_state()
        # Only a remembered *off* changes anything — the default is on, and a
        # restored `unknown`/`unavailable` (the entity was down at shutdown) must
        # not be read as a user's choice to disable it.
        if last is not None and last.state == STATE_OFF:
            self.coordinator.voice_duck.enabled = False
        self.async_write_ha_state()

    @property
    def is_on(self) -> bool:
        return self.coordinator.voice_duck.enabled

    async def async_turn_on(self, **kwargs) -> None:
        self.coordinator.voice_duck.enabled = True
        self.async_write_ha_state()

    async def async_turn_off(self, **kwargs) -> None:
        self.coordinator.voice_duck.enabled = False
        self.async_write_ha_state()
        # Anything ducked right now must come back immediately, not one lease later.
        await self.coordinator.voice_duck.async_release_all()
