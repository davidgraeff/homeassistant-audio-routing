"""Diagnostics for a pw-sink host: which sink on that machine the audio lands in.

A `pwsink-dev-*` output is a PC running `pwrouter-agent`, and where the agent puts
the audio is genuinely a question you can only answer from that machine — the
router hands a stream to the agent, and the agent plays it into one of the host's
sinks (`alsa_output.pci-0000_0a_00.4.analog-stereo`, a USB dock, a headset). "Music
is routed there but I can't hear it" is normally answered by that string.

One sensor per adopted host, on that host's device (`pwsink_hosts.py`), so it sits
next to the room assignment. Diagnostic category: it is never a control, and the
value moves only when someone changes the default sink on the desktop.
"""

from __future__ import annotations

from homeassistant.components.sensor import SensorEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import PipewireRouterCoordinator
from .const import DOMAIN
from .media_player import PWSINK_DEV_PREFIX
from .pwsink_hosts import pwsink_host_identifier


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    coordinator: PipewireRouterCoordinator = hass.data[DOMAIN][entry.entry_id]
    # One per adopted pw-sink output, added and removed as hosts are adopted or
    # dropped — the same reconcile-on-update shape the media_player platform uses,
    # since the same routing matrix decides what exists.
    entities: dict[str, PwsinkSinkSensor] = {}

    @callback
    def _reconcile_entities() -> None:
        wanted = {
            output.node_name
            for output in coordinator.routing.outputs
            if output.node_name.startswith(PWSINK_DEV_PREFIX)
        }
        new = [PwsinkSinkSensor(coordinator, entry, name) for name in wanted if name not in entities]
        for sensor in new:
            entities[sensor.node_name] = sensor
        if new:
            async_add_entities(new)

        gone = [name for name in entities if name not in wanted]
        if gone:
            registry = er.async_get(hass)
            removed = [entities.pop(name) for name in gone]

            async def _remove() -> None:
                for sensor in removed:
                    if sensor.entity_id and registry.async_get(sensor.entity_id):
                        registry.async_remove(sensor.entity_id)
                    await sensor.async_remove(force_remove=True)

            hass.async_create_task(_remove())

    _reconcile_entities()
    entry.async_on_unload(coordinator.async_add_listener(_reconcile_entities))


class PwsinkSinkSensor(CoordinatorEntity[PipewireRouterCoordinator], SensorEntity):
    """The PipeWire sink the host's agent is playing into."""

    _attr_has_entity_name = True
    _attr_name = "Output device"
    _attr_entity_category = EntityCategory.DIAGNOSTIC
    _attr_icon = "mdi:audio-input-stereo-minijack"

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry, node_name: str) -> None:
        super().__init__(coordinator)
        self.node_name = node_name
        self._attr_unique_id = f"{entry.entry_id}_pwsink_sink_{node_name}"
        # Our own device, so `DeviceInfo` by identifier is the plain way to join it
        # (unlike an output entity adopting a *foreign* device, which since HA 2026.8
        # has to be linked through the entity registry — see media_player.py).
        self._attr_device_info = DeviceInfo(identifiers={pwsink_host_identifier(node_name)})

    @property
    def available(self) -> bool:
        # No agent on the socket means nobody can say which sink is in use; the
        # last-known value would be a guess about a machine that may have rebooted.
        agent = self.coordinator.agents.get(self.node_name)
        return super().available and agent is not None and agent.connected

    @property
    def native_value(self) -> str | None:
        agent = self.coordinator.agents.get(self.node_name)
        return agent.sink_name if agent is not None else None
