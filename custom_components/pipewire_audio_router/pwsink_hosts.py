"""A Home Assistant device per adopted pw-sink host, so it can be given a room.

Every other output kind inherits its room from a device Home Assistant already
has: a sendspin speaker matches its ESPHome device, an AirPlay-2 receiver matches
the AV integration that talks to the same IP. A **pw-sink output is a PC running
`pwrouter-agent`**, and nothing in Home Assistant necessarily represents that
machine — so there is no device, no area, and voice ducking cannot tell which room
the host is in. It is the one output kind that is never ducked by a voice turn.

Correlating to whatever *else* knows the machine was tried and rejected: on the
author's instance one desktop appears as **three** device rows (Home Assistant
keys devices per config entry since 2026.8) — wake-on-LAN, device-tracker and
go-hass-agent — two carrying an area and the one with the most entities carrying
none. Any automatic rule there is a guess that changes meaning as the user adds
integrations, and a wrong guess is silent: music simply never ducks in that room.

So the integration creates the device itself and the user assigns the room once,
explicitly. Notes on why this holds up:

* **Keyed by the output's node name.** The daemon already pins that per agent
  identity — "a host that paired before keeps its stored name, or its routing and
  HA entity ids would break on a re-pairing" (`outputs/pwsink/agent.rs`) — so a
  renamed host or a re-pairing keeps this device, and with it the room.
* **The room survives a registry cleanup.** `device_registry.async_cleanup` reaps
  devices that have neither entities nor a live config entry, and this one belongs
  to ours — so the assignment holds with the daemon's per-output `media_player`
  toggle switched off, which is the case that matters, since that toggle being off
  is why ducking needs a device at all.
* It is also where the host's own facts go: the agent's build as the device
  version, and its current PipeWire sink as a diagnostic sensor (`sensor.py`).
  When per-output entities *are* exposed, the host's `media_player` attaches here
  too (via `find_output_ha_device`), so it reads as that machine rather than a slug.
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING

from homeassistant.core import callback
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.typing import UNDEFINED

from .const import DOMAIN

if TYPE_CHECKING:
    from homeassistant.config_entries import ConfigEntry
    from homeassistant.core import HomeAssistant

    from . import PipewireRouterCoordinator

_LOGGER = logging.getLogger(__name__)

# `pwsink:` namespaces the identifier so these rows can never be confused with the
# add-on's own service device (`service_device.py`), which is keyed by entry id.
_IDENTIFIER_PREFIX = "pwsink:"

DEVICE_MANUFACTURER = "PipeWire Audio Router add-on"
DEVICE_MODEL = "PipeWire host (pwrouter-agent)"


@callback
def pwsink_host_identifier(node_name: str) -> tuple[str, str]:
    """This host's device identifier. Public because both the device reconcile and
    the output→device lookup must agree on it."""
    return (DOMAIN, f"{_IDENTIFIER_PREFIX}{node_name}")


@callback
def async_find_pwsink_host_device(hass: HomeAssistant, node_name: str) -> dr.DeviceEntry | None:
    """The device for this pw-sink output, if it has been created yet."""
    return dr.async_get(hass).async_get_device(identifiers={pwsink_host_identifier(node_name)})


@callback
def async_reconcile_pwsink_host_devices(
    hass: HomeAssistant, entry: ConfigEntry, coordinator: PipewireRouterCoordinator
) -> None:
    """One device per adopted pw-sink output: create what's missing, keep the label
    current, and drop rows for outputs the user has removed.

    Driven off the routing matrix — the same "adopted outputs only" set the
    entities use — not off the agent listing, so a *discovered but not adopted*
    host doesn't litter the device list.
    """
    if not coordinator.last_update_success:
        # A daemon that just went away reports nothing. Removing rows on that basis
        # would throw away the user's room assignments over a restart.
        return

    # Imported lazily: media_player imports this module for the output→device
    # lookup, and the node-name prefixes are deliberately declared once, next to
    # the entity code that keys behaviour off them.
    from .media_player import PWSINK_DEV_PREFIX  # noqa: PLC0415

    # The matrix's display name for a pw-sink output is the daemon's agent label,
    # "<hostname> (<user>)" — what the machine calls itself.
    wanted = {
        output.node_name: output.display_name
        for output in coordinator.routing.outputs
        if output.node_name.startswith(PWSINK_DEV_PREFIX)
    }

    dev_reg = dr.async_get(hass)
    for node_name, label in wanted.items():
        agent = coordinator.agents.get(node_name)
        version = agent.version if agent is not None else None
        dev_reg.async_get_or_create(
            config_entry_id=entry.entry_id,
            identifiers={pwsink_host_identifier(node_name)},
            name=label,
            manufacturer=DEVICE_MANUFACTURER,
            model=DEVICE_MODEL,
            # The agent's build. The daemon only learns it from a live connection, so
            # a disconnected host reports none — pass `UNDEFINED` rather than `None`
            # there, which keeps the last version instead of blanking the field every
            # time the machine sleeps.
            sw_version=version if version is not None else UNDEFINED,
        )

    if not coordinator.routing.outputs:
        # An empty matrix is indistinguishable from "the WebSocket hasn't delivered
        # its first snapshot yet", so it is never taken as "the user removed
        # everything". Removals wait for a matrix that has something in it.
        return
    for device in dr.async_entries_for_config_entry(dev_reg, entry.entry_id):
        for domain, identifier in device.identifiers:
            if domain != DOMAIN or not identifier.startswith(_IDENTIFIER_PREFIX):
                continue
            node_name = identifier.removeprefix(_IDENTIFIER_PREFIX)
            if node_name not in wanted:
                _LOGGER.debug("pw-sink host %s is no longer an output; removing its device", node_name)
                dev_reg.async_remove_device(device.id)
