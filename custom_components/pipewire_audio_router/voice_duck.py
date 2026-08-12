"""Automatic voice-assistant ducking: while an `assist_satellite` in a room is
talking, the router's speakers *in that room* play quietly.

This replaces the community blueprint that divided every `media_player`'s
`volume_level` in the satellite's area and multiplied it back afterwards. The
split here is deliberate:

* **Home Assistant knows where things are.** Areas, and which satellite is
  talking, live in the registries — so the resolution "satellite → area →
  which router outputs" happens here, and the daemon is told plain output names.
  Nothing about rooms is duplicated daemon-side.
* **The daemon knows how to duck.** It applies a *mixer gain* on the per-device
  relay (`overlay_mixer.rs`), so a device's user-visible volume never moves,
  there is nothing to restore, and the duck lands on the next audio chunk. It
  also means one speaker of a synchronized multi-room group can duck while its
  groupmates keep playing — which volume-based ducking cannot express.

Two consequences worth knowing, both departures from the blueprint:

* **The satellite's own output is ducked too, not skipped.** The blueprint
  excluded the satellite's *response player* — the entity about to speak. Our
  `sendspin-dev-<voice-pe>` output is a different thing: it carries **music** to
  that same physical speaker, so it is the most important one to quieten.
* **Outputs are not filtered by "is it playing".** A hold on a silent output is
  inaudible and costs nothing, it covers music that *starts* during the turn,
  and it avoids depending on entity state (which `pwsink-dev-` outputs don't
  report at all).

Holds are leased: we renew every `VOICE_DUCK_RENEW_SECONDS` while a turn is
open, and the daemon un-ducks on its own if we stop — so a config-entry reload
or a lost network mid-turn cannot leave music quiet.
"""

from __future__ import annotations

import logging
from datetime import timedelta
from typing import TYPE_CHECKING

from homeassistant.core import Event, EventStateChangedData, HomeAssistant, callback
from homeassistant.helpers import area_registry as ar, device_registry as dr, entity_registry as er
from homeassistant.helpers.event import (
    TrackStates,
    async_track_state_change_filtered,
    async_track_time_interval,
)
from .api import PipewireRouterApiError
from .const import (
    ASSIST_SATELLITE_DOMAIN,
    DEFAULT_VOICE_DUCK_ENABLED,
    DEFAULT_VOICE_DUCK_LEVEL,
    DOMAIN,
    VOICE_DUCK_ACTIVE_STATES,
    VOICE_DUCK_RENEW_SECONDS,
    VOICE_DUCK_SCOPE_AREA,
    VOICE_DUCK_SCOPE_MUSIC_GROUP,
    VOICE_DUCK_TTL_SECONDS,
)

if TYPE_CHECKING:
    from homeassistant.config_entries import ConfigEntry

    from . import PipewireRouterCoordinator

_LOGGER = logging.getLogger(__name__)


def _entity_area(hass: HomeAssistant, entity_id: str) -> str | None:
    """The area an entity is in: its own override if set, else its device's."""
    ent_reg = er.async_get(hass)
    entry = ent_reg.async_get(entity_id)
    if entry is None:
        return None
    if entry.area_id:
        return entry.area_id
    if entry.device_id:
        device = dr.async_get(hass).async_get(entry.device_id)
        if device is not None:
            return device.area_id
    return None


class VoiceDucker:
    """Watches every `assist_satellite` and holds a duck on the router outputs in
    the talking satellite's area. One instance per config entry."""

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry, coordinator: PipewireRouterCoordinator) -> None:
        self.hass = hass
        self.entry = entry
        self.coordinator = coordinator
        # Settable from the entities in switch.py / number.py / select.py, which
        # also restore them across restarts (there's nothing daemon-side to
        # persist — the daemon has no concept of rooms or satellites).
        self.enabled = DEFAULT_VOICE_DUCK_ENABLED
        self.level = DEFAULT_VOICE_DUCK_LEVEL
        self.scope = VOICE_DUCK_SCOPE_AREA
        # Satellite entity_id -> the daemon hold covering its room, while its turn
        # is open. Per satellite, so two rooms talking at once don't interfere.
        self._holds: dict[str, int] = {}
        self._unsub_states = None
        self._unsub_renew = None

    # --- lifecycle ---

    @callback
    def async_start(self) -> None:
        """Begin watching. Tracks the whole `assist_satellite` domain rather than
        a configured list, so a satellite added later is covered with no setup."""
        self._unsub_states = async_track_state_change_filtered(
            self.hass,
            TrackStates(False, set(), {ASSIST_SATELLITE_DOMAIN}),
            self._async_satellite_changed,
        ).async_remove
        self._unsub_renew = async_track_time_interval(
            self.hass,
            self._async_renew_holds,
            timedelta(seconds=VOICE_DUCK_RENEW_SECONDS),
        )

    async def async_stop(self) -> None:
        """Stop watching and release every hold we still own, so unloading the
        entry doesn't leave music ducked until the lease runs out."""
        for unsub in (self._unsub_states, self._unsub_renew):
            if unsub is not None:
                unsub()
        self._unsub_states = None
        self._unsub_renew = None
        for entity_id in list(self._holds):
            await self._async_release(entity_id)

    # --- the trigger ---

    @callback
    def _async_satellite_changed(self, event: Event[EventStateChangedData]) -> None:
        entity_id = event.data["entity_id"]
        new_state = event.data["new_state"]
        active = new_state is not None and new_state.state in VOICE_DUCK_ACTIVE_STATES
        if active and self.enabled:
            if entity_id not in self._holds:
                self.entry.async_create_background_task(
                    self.hass, self._async_duck_for(entity_id), f"voice_duck {entity_id}"
                )
        elif entity_id in self._holds:
            # Turn over (idle), or the satellite went away mid-turn — either way
            # the music comes back. Also covers `enabled` being switched off
            # during a turn, on that turn's next state change.
            self.entry.async_create_background_task(
                self.hass, self._async_release(entity_id), f"voice_unduck {entity_id}"
            )

    async def _async_duck_for(self, entity_id: str) -> None:
        targets = self.async_targets_for(entity_id)
        if not targets:
            return
        try:
            hold_id = await self.coordinator.client.async_duck_start(
                targets, self.level, VOICE_DUCK_TTL_SECONDS * 1000
            )
        except PipewireRouterApiError as err:
            _LOGGER.warning("voice ducking for %s failed: %s", entity_id, err)
            return
        # A turn that ended while the request was in flight: release immediately
        # rather than waiting for the lease.
        self._holds[entity_id] = hold_id
        _LOGGER.debug("voice ducking %s: hold %s on %s at %.2f", entity_id, hold_id, targets, self.level)
        if not self._async_is_talking(entity_id):
            await self._async_release(entity_id)

    async def async_release_all(self) -> None:
        """Drop every hold we own, now — used when the feature is switched off
        mid-turn, so the music returns immediately rather than one lease later."""
        for entity_id in list(self._holds):
            await self._async_release(entity_id)

    async def _async_release(self, entity_id: str) -> None:
        hold_id = self._holds.pop(entity_id, None)
        if hold_id is None:
            return
        try:
            await self.coordinator.client.async_duck_release(hold_id)
        except PipewireRouterApiError as err:
            # Not fatal: the lease expires on its own within seconds.
            _LOGGER.debug("releasing duck hold %s failed (lease will expire): %s", hold_id, err)

    async def _async_renew_holds(self, _now) -> None:
        """Keep open turns' leases alive. A hold the daemon has forgotten (it
        restarted, or a renewal was missed for too long) is re-established rather
        than left silently not ducking."""
        for entity_id, hold_id in list(self._holds.items()):
            if not self._async_is_talking(entity_id):
                # Belt and braces: a satellite that stopped without a state event
                # reaching us (e.g. it dropped off the network) still un-ducks.
                await self._async_release(entity_id)
                continue
            try:
                alive = await self.coordinator.client.async_duck_renew(hold_id, VOICE_DUCK_TTL_SECONDS * 1000)
            except PipewireRouterApiError as err:
                _LOGGER.debug("renewing duck hold %s failed: %s", hold_id, err)
                continue
            if not alive:
                _LOGGER.debug("duck hold %s is gone; starting a new one for %s", hold_id, entity_id)
                self._holds.pop(entity_id, None)
                await self._async_duck_for(entity_id)

    @callback
    def _async_is_talking(self, entity_id: str) -> bool:
        state = self.hass.states.get(entity_id)
        return state is not None and state.state in VOICE_DUCK_ACTIVE_STATES and self.enabled

    # --- resolution: satellite -> area -> output node names ---

    @callback
    def async_targets_for(self, entity_id: str) -> list[str]:
        """The output node names to duck for this satellite's turn."""
        area_id = _entity_area(self.hass, entity_id)
        if area_id is None:
            _LOGGER.debug("voice ducking: %s is in no area; nothing to duck", entity_id)
            return []

        in_area = [o.node_name for o in self.coordinator.routing.outputs if self._async_output_area(o.node_name) == area_id]
        if not in_area:
            area = ar.async_get(self.hass).async_get_area(area_id)
            _LOGGER.debug(
                "voice ducking: no router outputs in area %s (for %s)",
                area.name if area else area_id,
                entity_id,
            )
            return []

        targets = set(in_area)
        if self.scope == VOICE_DUCK_SCOPE_MUSIC_GROUP:
            # Widen to whole synchronized groups: in an open-plan space the same
            # track playing next door drowns the response. An output in no music
            # group contributes only itself, so this collapses to the area scope.
            for group in self.coordinator.music_groups:
                if any(member in targets for member in group.members):
                    targets.update(group.members)
        return sorted(targets)

    @callback
    def _async_output_area(self, node_name: str) -> str | None:
        """The area a router output is in.

        Prefer our own per-output `media_player` when it exists (only when the
        daemon's `expose_outputs_as_media_players` is on) — it carries the user's
        override and is already linked to the adopted device. Otherwise fall back
        to the same device correlation the entity would have used, so ducking
        works whether or not per-output entities are exposed.
        """
        ent_reg = er.async_get(self.hass)
        entity_id = ent_reg.async_get_entity_id("media_player", DOMAIN, f"{self.entry.entry_id}_out_{node_name}")
        if entity_id is not None:
            return _entity_area(self.hass, entity_id)
        # Imported lazily: media_player imports this module's siblings, and the
        # correlation rules (mDNS hostname for sendspin, receiver IP for AP2) are
        # deliberately defined once, next to the adoption they were written for.
        from .media_player import find_output_ha_device  # noqa: PLC0415

        device = find_output_ha_device(self.hass, node_name, self.coordinator.outputs_meta)
        return device.area_id if device is not None else None
