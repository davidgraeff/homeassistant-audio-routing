"""The add-on itself, as one Home Assistant *service* device.

The add-on's *settings* — voice ducking, the Bluetooth-bridge RTP source — used to
be a loose pile of entities under the config entry, findable only by someone who
already knew what to look for. Home Assistant has no page for "the settings of an
integration" and no description field to explain an entity with, but it does have a
device page, and that page is the one place it will group entities by category,
carry a version, and offer a link out. So: one service device per config entry,
with the settings on it.

Three deliberate exclusions, all for the same reason — a device prefixes its
entities' displayed names, which is right for a setting and wrong for a speaker:

* **Per-output `media_player`s stay on the real speaker's device**
  (`media_player.device_info`). That link is what gives them the speaker's name and
  area — and the area is what voice ducking resolves against, so re-homing them
  here would break ducking to make a list look tidier.
* **Group `media_player`s stay standalone.** They are the add-on's own construct,
  so by that logic they belonged here, but they are also the entities people
  actually call by name in a media card or a script: joining made "Everywhere" read
  "PipeWire Audio Router Everywhere", and a group created afterwards would have
  carried that into its entity_id.
* **No `via_device`.** Showing the speakers as "connected via" the router reads
  nicely in the device tree, but that field would have to be written onto device
  rows the ESPHome and MusicCast integrations own.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.device_registry import DeviceEntryType, DeviceInfo

from .const import DOMAIN

from homeassistant.core import callback

if TYPE_CHECKING:
    from homeassistant.config_entries import ConfigEntry
    from homeassistant.core import HomeAssistant

    from . import PipewireRouterCoordinator

# What the device page reads as: "Bridge daemon by PipeWire Audio Router add-on".
DEVICE_NAME = "PipeWire Audio Router"
DEVICE_MANUFACTURER = "PipeWire Audio Router add-on"
DEVICE_MODEL = "Bridge daemon"


def service_device_info(entry: ConfigEntry, coordinator: PipewireRouterCoordinator) -> DeviceInfo:
    """The service device's identity, as the entities declare it.

    Keyed by the config entry id rather than anything about the daemon: the host
    can change (a moved add-on, a renamed instance) without the device — and every
    entity's history with it — being replaced.

    `configuration_url` points at the daemon's own web UI, which is the actual
    console for everything this integration only pokes at over REST. It is the
    daemon's direct address rather than the add-on's ingress path, because an
    ingress URL is minted per session and is not ours to build.
    """
    status = coordinator.status
    host = None
    if status is not None and status.host_model:
        host = f"{status.host_model} ({status.host_arch})" if status.host_arch else status.host_model
    return DeviceInfo(
        identifiers={(DOMAIN, entry.entry_id)},
        entry_type=DeviceEntryType.SERVICE,
        name=DEVICE_NAME,
        manufacturer=DEVICE_MANUFACTURER,
        model=DEVICE_MODEL,
        sw_version=status.version if status is not None and status.version else None,
        hw_version=host,
        configuration_url=coordinator.client.base_url,
    )


@callback
def async_register_service_device(
    hass: HomeAssistant, entry: ConfigEntry, coordinator: PipewireRouterCoordinator
) -> None:
    """Create/refresh the device row up front, so it exists (with its version)
    before any entity is added, and is corrected after an add-on update — which the
    entities' own `device_info` would only pick up on a reload."""
    dr.async_get(hass).async_get_or_create(
        config_entry_id=entry.entry_id,
        **service_device_info(entry, coordinator),
    )
