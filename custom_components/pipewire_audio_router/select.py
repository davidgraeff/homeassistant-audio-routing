"""Select for how far voice-assistant ducking reaches (see voice_duck.py).

`area` (the default) ducks only the outputs in the talking satellite's own area,
even when one of them is mid-song inside a multi-room music group — the daemon's
per-device mix makes that possible, and it is what you want in a normal house.
`music_group` widens it to every member of a group that any of those outputs
belongs to: in an open-plan kitchen/hall the same track playing next door drowns
the response.

Nothing daemon-side holds this (the daemon has no concept of rooms), so the
choice is restored here across restarts.
"""

from __future__ import annotations

from homeassistant.components.select import SelectEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.restore_state import RestoreEntity
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import PipewireRouterCoordinator
from .const import DOMAIN, VOICE_DUCK_SCOPE_AREA, VOICE_DUCK_SCOPES


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    coordinator: PipewireRouterCoordinator = hass.data[DOMAIN][entry.entry_id]
    async_add_entities([PipewireVoiceDuckScopeSelect(coordinator, entry)])


class PipewireVoiceDuckScopeSelect(CoordinatorEntity[PipewireRouterCoordinator], SelectEntity, RestoreEntity):
    """Which speakers duck for a voice turn: the satellite's area, or the whole
    music group its speakers are in."""

    _attr_has_entity_name = True
    _attr_name = "Voice assistant duck scope"
    _attr_entity_category = EntityCategory.CONFIG
    _attr_icon = "mdi:select-group"
    _attr_options = VOICE_DUCK_SCOPES
    # The option *labels* come from `translations/en.json`
    # (`entity.select.voice_duck_scope.state`), which is the one place Home
    # Assistant will render an explanation of a setting rather than its raw value:
    # the dropdown says what each scope does instead of "area" / "music_group".
    # (`_attr_name` still wins for the name — see `switch.py`.)
    _attr_translation_key = "voice_duck_scope"

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_voice_duck_scope"

    async def async_added_to_hass(self) -> None:
        await super().async_added_to_hass()
        last = await self.async_get_last_state()
        if last is not None and last.state in VOICE_DUCK_SCOPES:
            self.coordinator.voice_duck.scope = last.state
            self.async_write_ha_state()

    @property
    def current_option(self) -> str:
        return self.coordinator.voice_duck.scope or VOICE_DUCK_SCOPE_AREA

    async def async_select_option(self, option: str) -> None:
        # Applies to the next voice turn; an open one keeps the targets it started
        # with, so nothing changes level mid-sentence.
        self.coordinator.voice_duck.scope = option
        self.async_write_ha_state()
