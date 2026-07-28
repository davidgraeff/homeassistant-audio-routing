"""Numbers for the Bluetooth-bridge RTP source: the listen (UDP) port and the
receiver-side jitter-buffer latency.

The port must match what the ESP32 firmware sends to. The latency is the
receiver's jitter buffer in ms — raise it on a weak-signal bridge to trade
latency for fewer audible dropouts. Changing either while the source is enabled
re-points it live (`PUT /api/sources/{id}` reloads the module); while it's
disabled the value is just remembered for the next enable.

The add-on's daemon only persists these while the source is *enabled* — when
disabled it reports the defaults — so the chosen values live on the coordinator
(`rtp_desired_port` / `rtp_desired_latency_msec`) and are restored here across
restarts so a value set before enabling isn't lost. Because the daemon replaces
the whole RTP config on each `PUT`, every setter sends *both* knobs: the one it
changes plus the coordinator's current value for the other.
"""

from __future__ import annotations

from homeassistant.components.number import NumberMode, RestoreNumber
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import PipewireRouterCoordinator
from .const import DOMAIN


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    coordinator: PipewireRouterCoordinator = hass.data[DOMAIN][entry.entry_id]
    async_add_entities(
        [
            PipewireRtpPortNumber(coordinator, entry),
            PipewireRtpLatencyNumber(coordinator, entry),
        ]
    )


class PipewireRtpPortNumber(CoordinatorEntity[PipewireRouterCoordinator], RestoreNumber):
    """The RTP source's listen port."""

    _attr_has_entity_name = True
    _attr_name = "Bluetooth bridge RTP port"
    _attr_entity_category = EntityCategory.CONFIG
    _attr_icon = "mdi:ethernet"
    _attr_native_min_value = 1
    _attr_native_max_value = 65535
    _attr_native_step = 1
    _attr_mode = NumberMode.BOX

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_rtp_source_port"

    async def async_added_to_hass(self) -> None:
        await super().async_added_to_hass()
        last = await self.async_get_last_number_data()
        # Restore the last user-set port, but only when the daemon isn't
        # already authoritative: an enabled source's stored port wins (the
        # first poll has already seeded `rtp_desired_port` from it).
        if (
            last is not None
            and last.native_value is not None
            and not (self.coordinator.rtp is not None and self.coordinator.rtp.enabled)
        ):
            self.coordinator.rtp_desired_port = int(last.native_value)
            self.async_write_ha_state()

    @property
    def available(self) -> bool:
        return super().available and self.coordinator.rtp is not None

    @property
    def native_value(self) -> float | None:
        return float(self.coordinator.rtp_desired_port)

    async def async_set_native_value(self, value: float) -> None:
        port = int(value)
        self.coordinator.rtp_desired_port = port
        if self.coordinator.rtp is not None and self.coordinator.rtp.enabled:
            # Apply live — the daemon reloads the module on the new port,
            # keeping the current latency (the PUT replaces the whole config).
            await self.coordinator.client.async_set_rtp_source(port, self.coordinator.rtp_desired_latency_msec)
            await self.coordinator.async_request_refresh()
        else:
            # Disabled: just remember it for the next enable.
            self.async_write_ha_state()


class PipewireRtpLatencyNumber(CoordinatorEntity[PipewireRouterCoordinator], RestoreNumber):
    """The RTP source's receiver-side jitter-buffer latency, in milliseconds."""

    _attr_has_entity_name = True
    _attr_name = "Bluetooth bridge RTP jitter buffer"
    _attr_entity_category = EntityCategory.CONFIG
    _attr_icon = "mdi:timer-sand"
    _attr_native_min_value = 20
    _attr_native_max_value = 2000
    _attr_native_step = 10
    _attr_native_unit_of_measurement = "ms"
    _attr_mode = NumberMode.BOX

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_rtp_source_latency_msec"

    async def async_added_to_hass(self) -> None:
        await super().async_added_to_hass()
        last = await self.async_get_last_number_data()
        # Restore the last user-set latency, but only when the daemon isn't
        # already authoritative (an enabled source's stored value wins — the
        # first poll has already seeded `rtp_desired_latency_msec` from it).
        if (
            last is not None
            and last.native_value is not None
            and not (self.coordinator.rtp is not None and self.coordinator.rtp.enabled)
        ):
            self.coordinator.rtp_desired_latency_msec = int(last.native_value)
            self.async_write_ha_state()

    @property
    def available(self) -> bool:
        return super().available and self.coordinator.rtp is not None

    @property
    def native_value(self) -> float | None:
        return float(self.coordinator.rtp_desired_latency_msec)

    async def async_set_native_value(self, value: float) -> None:
        latency = int(value)
        self.coordinator.rtp_desired_latency_msec = latency
        if self.coordinator.rtp is not None and self.coordinator.rtp.enabled:
            # Apply live — the daemon reloads the module with the new latency,
            # keeping the current port (the PUT replaces the whole config).
            await self.coordinator.client.async_set_rtp_source(self.coordinator.rtp_desired_port, latency)
            await self.coordinator.async_request_refresh()
        else:
            # Disabled: just remember it for the next enable.
            self.async_write_ha_state()
