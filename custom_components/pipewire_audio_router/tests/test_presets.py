"""Tests for music-group presets in Home Assistant (docs/music-group-presets-plan.md §7).

Two surfaces over one coordinator method: the `activate_preset` **service**, which
is what an automation uses (a name, no `entity_id`), and the **select entity**,
which is the only thing a template or a stock dashboard can read to find out which
grouping is on. Both are addressed by name here, because that is what the user
wrote in the add-on UI — the id is an implementation detail of the daemon's store.
"""

from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

import pytest
from homeassistant.exceptions import HomeAssistantError
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.pipewire_audio_router.api import (
    AppSettings,
    DaemonStatus,
    PipewireRouterApiError,
    Preset,
    PresetsInfo,
    RoutingMatrix,
    RtpSourceState,
)
from custom_components.pipewire_audio_router.const import DOMAIN, SERVICE_ACTIVATE_PRESET

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"

DAEMON_STATUS = DaemonStatus(version="0.3.0", host_model="Raspberry Pi 4 Model B", host_arch="aarch64")
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)
EMPTY_ROUTING = RoutingMatrix(sources=[], outputs=[], links=[])

TWO_PRESETS = PresetsInfo(
    active="default",
    presets=[Preset(id="default", name="Default"), Preset(id="house_party", name="House party")],
)
ONLY_DEFAULT = PresetsInfo(active="default", presets=[Preset(id="default", name="Default")])

PRESET_ENTITY = "select.pipewire_audio_router_music_group_preset"


def _patch_daemon(presets=TWO_PRESETS, *, presets_enabled=True):
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_health", new=AsyncMock(return_value=None)))
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=EMPTY_ROUTING)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=RTP_DISABLED)))
    stack.enter_context(patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value={})))
    stack.enter_context(patch(f"{API}.async_get_outputs", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_music_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_announcement_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_presets", new=AsyncMock(return_value=presets)))
    stack.enter_context(patch(f"{API}.async_get_status", new=AsyncMock(return_value=DAEMON_STATUS)))
    stack.enter_context(patch(f"{API}.async_get_agents", new=AsyncMock(return_value=[])))
    stack.enter_context(
        patch(
            f"{API}.async_get_settings",
            new=AsyncMock(
                return_value=AppSettings(
                    expose_outputs_as_media_players=False, presets_enabled=presets_enabled
                )
            ),
        )
    )
    stack.enter_context(patch(f"{COORD}.async_events_ws_loop", new=AsyncMock()))
    return stack


async def _setup(hass, presets=TWO_PRESETS, *, presets_enabled=True):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8080})
    entry.add_to_hass(hass)
    with _patch_daemon(presets, presets_enabled=presets_enabled):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
    return entry, hass.data[DOMAIN][entry.entry_id]


async def test_no_preset_entity_until_the_feature_is_switched_on(hass):
    """`presets_enabled` off is the default, and then this entity must not exist:
    the daemon still has exactly one preset, so it would offer a single choice on
    every install that never asked for the feature."""
    await _setup(hass, TWO_PRESETS, presets_enabled=False)
    assert hass.states.get(PRESET_ENTITY) is None


async def test_the_select_offers_every_preset_and_reports_the_active_one(hass):
    await _setup(hass)
    state = hass.states.get(PRESET_ENTITY)
    assert state is not None
    assert state.attributes["options"] == ["Default", "House party"]
    assert state.state == "Default"


async def test_selecting_an_option_activates_that_preset(hass):
    """One daemon call with the *id* — the regrouping and the routing are the
    daemon's single operation, not a walk over the groups from here."""
    _entry, coordinator = await _setup(hass)
    coordinator.client.async_activate_preset = AsyncMock()

    # Inside the offline patch: activating asks for an immediate refresh (the
    # grouping just changed), and that poll is a real fetch unless it is mocked.
    with _patch_daemon():
        await hass.services.async_call(
            "select",
            "select_option",
            {"entity_id": PRESET_ENTITY, "option": "House party"},
            blocking=True,
        )
        await hass.async_block_till_done()
    coordinator.client.async_activate_preset.assert_awaited_once_with("house_party")


async def test_the_select_appears_when_the_feature_is_switched_on_later(hass):
    """The switch lives in the add-on UI, so the entity has to arrive on a poll —
    otherwise turning presets on means restarting Home Assistant to see it."""
    _entry, coordinator = await _setup(hass, TWO_PRESETS, presets_enabled=False)
    assert hass.states.get(PRESET_ENTITY) is None

    coordinator.presets_enabled = True
    coordinator.async_update_listeners()
    await hass.async_block_till_done()
    assert hass.states.get(PRESET_ENTITY) is not None


async def test_the_select_goes_unavailable_when_the_feature_is_switched_off(hass):
    """Deleting it instead would take a dashboard card with it, and someone trying
    the feature out switches it back and forth."""
    _entry, coordinator = await _setup(hass)
    assert hass.states.get(PRESET_ENTITY).state == "Default"

    coordinator.presets_enabled = False
    coordinator.async_update_listeners()
    await hass.async_block_till_done()
    assert hass.states.get(PRESET_ENTITY).state == "unavailable"


async def test_service_activates_by_name(hass):
    """What an automation is written with: the name shown in the add-on UI, and no
    entity_id to look up."""
    _entry, coordinator = await _setup(hass)
    coordinator.client.async_activate_preset = AsyncMock()

    with _patch_daemon():
        await hass.services.async_call(
            DOMAIN, SERVICE_ACTIVATE_PRESET, {"preset": "House party"}, blocking=True
        )
        await hass.async_block_till_done()
    coordinator.client.async_activate_preset.assert_awaited_once_with("house_party")


async def test_service_accepts_an_id_and_ignores_case(hass):
    """An id is what an older automation (or the card) may hold, and a name typed
    from memory rarely matches capitalisation."""
    _entry, coordinator = await _setup(hass)
    coordinator.client.async_activate_preset = AsyncMock()

    with _patch_daemon():
        await hass.services.async_call(DOMAIN, SERVICE_ACTIVATE_PRESET, {"preset": "house_party"}, blocking=True)
        await hass.services.async_call(DOMAIN, SERVICE_ACTIVATE_PRESET, {"preset": "hOUSE pARTY"}, blocking=True)
        await hass.async_block_till_done()
    assert coordinator.client.async_activate_preset.await_count == 2


async def test_service_names_the_presets_it_knows_when_asked_for_one_it_doesnt(hass):
    """A renamed or deleted preset is the likely cause, so the error has to say
    what *is* configured — and nothing may be activated on a guess."""
    _entry, coordinator = await _setup(hass)
    coordinator.client.async_activate_preset = AsyncMock()

    with pytest.raises(HomeAssistantError) as err:
        await hass.services.async_call(DOMAIN, SERVICE_ACTIVATE_PRESET, {"preset": "Rave"}, blocking=True)
    assert "Rave" in str(err.value)
    assert "House party" in str(err.value)
    coordinator.client.async_activate_preset.assert_not_awaited()


async def test_a_refused_activation_surfaces_the_daemons_own_sentence(hass):
    """The daemon refuses an empty or unknown preset with a reason; swallowing it
    would leave the house un-regrouped with nothing said."""
    _entry, coordinator = await _setup(hass)
    coordinator.client.async_activate_preset = AsyncMock(
        side_effect=PipewireRouterApiError("music group has no members")
    )

    with pytest.raises(HomeAssistantError) as err:
        await hass.services.async_call(
            DOMAIN, SERVICE_ACTIVATE_PRESET, {"preset": "House party"}, blocking=True
        )
    assert "music group has no members" in str(err.value)


async def test_one_lone_preset_still_gets_its_entity(hass):
    """With the feature on, `Default` alone is a real (if dull) state to report —
    the *card* is what hides a one-option picker, not the entity."""
    await _setup(hass, ONLY_DEFAULT)
    state = hass.states.get(PRESET_ENTITY)
    assert state is not None
    assert state.attributes["options"] == ["Default"]
