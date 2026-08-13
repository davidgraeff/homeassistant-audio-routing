"""The integration's `select` entities.

**Voice-assistant duck scope** (see voice_duck.py) and **music-group preset**
(docs/music-group-presets-plan.md) — unrelated settings that happen to share a
platform, so read them separately.

Voice-assistant ducking reach:

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
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.restore_state import RestoreEntity
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import PipewireRouterCoordinator
from .const import DOMAIN, VOICE_DUCK_SCOPE_AREA, VOICE_DUCK_SCOPES
from .service_device import service_device_info


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    coordinator: PipewireRouterCoordinator = hass.data[DOMAIN][entry.entry_id]
    async_add_entities([PipewireVoiceDuckScopeSelect(coordinator, entry)])

    # The preset select exists only for someone who works with presets — with the
    # add-on's switch off there is one grouping and nothing to choose. Added when
    # the flag turns on (here if it already is, otherwise from the coordinator's
    # next update), and never removed again: it reports `unavailable` if the flag
    # goes back off, which is honest and leaves a dashboard card in place for
    # someone who is switching the feature on and off while trying it out.
    added = False

    @callback
    def _reconcile() -> None:
        nonlocal added
        if added or not coordinator.presets_enabled:
            return
        added = True
        async_add_entities([PipewirePresetSelect(coordinator, entry)])

    _reconcile()
    entry.async_on_unload(coordinator.async_add_listener(_reconcile))


class PipewirePresetSelect(CoordinatorEntity[PipewireRouterCoordinator], SelectEntity):
    """Which music-group preset is in force — the whole grouping of the house as
    one dropdown (docs/music-group-presets-plan.md §7).

    Selecting one regroups the speakers and routes what the preset says, in a
    single daemon call. Unlike the duck scope beside it there is no
    `translation_key`: the options are the user's own preset names, so there is
    nothing to translate and nowhere to explain them but the add-on UI."""

    _attr_has_entity_name = True
    _attr_name = "Music group preset"
    _attr_entity_category = EntityCategory.CONFIG
    _attr_icon = "mdi:playlist-music"

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_music_preset"
        self._attr_device_info = service_device_info(entry, coordinator)

    @property
    def available(self) -> bool:
        return bool(self.coordinator.presets_enabled and self.coordinator.presets)

    @property
    def options(self) -> list[str]:
        return [p.name for p in self.coordinator.presets]

    @property
    def current_option(self) -> str | None:
        """The active preset's *name*, since that is what `options` holds. `None`
        while the daemon hasn't answered yet — better unknown than a guess at
        which grouping the house is on."""
        return next((p.name for p in self.coordinator.presets if p.id == self.coordinator.active_preset), None)

    async def async_select_option(self, option: str) -> None:
        # By name, through the coordinator's one activation path — the same one the
        # service takes, so both mean the same thing by "House party".
        await self.coordinator.async_activate_preset(option)


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
        self._attr_device_info = service_device_info(entry, coordinator)

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
