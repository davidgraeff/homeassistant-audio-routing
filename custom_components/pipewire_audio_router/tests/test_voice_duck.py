"""Voice-assistant ducking (voice_duck.py): satellite state → area → which
outputs get ducked, and the lease lifecycle around it.

Everything here drives the real config entry through `async_setup`, so the
coordinator, the entity platforms and the satellite state listener are all
wired as they are in production; only the daemon's HTTP calls are mocked.
"""

from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

from homeassistant.core import State
from homeassistant.helpers import area_registry as ar, device_registry as dr, entity_registry as er
from pytest_homeassistant_custom_component.common import MockConfigEntry, mock_restore_cache

from custom_components.pipewire_audio_router.api import (
    DaemonStatus,
    AppSettings,
    MusicGroup,
    OutputMeta,
    RoutingMatrix,
    RoutingNode,
    RtpSourceState,
)
from custom_components.pipewire_audio_router.const import (
    DOMAIN,
    VOICE_DUCK_SCOPE_AREA,
    VOICE_DUCK_SCOPE_MUSIC_GROUP,
    VOICE_DUCK_TTL_SECONDS,
)

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
DAEMON_STATUS = DaemonStatus(version="0.3.0", host_model="Raspberry Pi 4 Model B", host_arch="aarch64")
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)
SATELLITE = "assist_satellite.kitchen_voice"


def _make_entry(hass):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8080})
    entry.add_to_hass(hass)
    return entry


def _matrix(*node_names):
    return RoutingMatrix(
        sources=[RoutingNode(node_id=10, node_name="shairport-sync", display_name="shairport-sync")],
        outputs=[RoutingNode(node_id=None, node_name=n, display_name=n) for n in node_names],
        links=[],
    )


def _patch_daemon(routing, *, outputs=None, music_groups=None, expose_outputs=True):
    """Offline setup, plus AsyncMocks for the three duck calls (returned so tests
    can assert on exactly what the daemon was asked to duck)."""
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_health", new=AsyncMock(return_value=None)))
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=routing)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=RTP_DISABLED)))
    stack.enter_context(patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value={})))
    stack.enter_context(patch(f"{API}.async_get_outputs", new=AsyncMock(return_value=outputs or [])))
    stack.enter_context(patch(f"{API}.async_get_music_groups", new=AsyncMock(return_value=music_groups or [])))
    stack.enter_context(patch(f"{API}.async_get_announcement_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_status", new=AsyncMock(return_value=DAEMON_STATUS)))
    stack.enter_context(patch(f"{API}.async_get_agents", new=AsyncMock(return_value=[])))
    stack.enter_context(
        patch(
            f"{API}.async_get_settings",
            new=AsyncMock(return_value=AppSettings(expose_outputs_as_media_players=expose_outputs)),
        )
    )
    stack.enter_context(patch(f"{COORD}.async_routing_ws_loop", new=AsyncMock()))
    duck = stack.enter_context(patch(f"{API}.async_duck_start", new=AsyncMock(return_value=77)))
    renew = stack.enter_context(patch(f"{API}.async_duck_renew", new=AsyncMock(return_value=True)))
    release = stack.enter_context(patch(f"{API}.async_duck_release", new=AsyncMock()))
    return stack, duck, renew, release


def _place_satellite(hass, area_id, *, entity_area=None):
    """A satellite entity on a device in `area_id` (mimicking an ESPHome Voice PE),
    optionally with an entity-level area override."""
    dev_reg = dr.async_get(hass)
    ent_reg = er.async_get(hass)
    sat_entry = MockConfigEntry(domain="esphome", data={})
    sat_entry.add_to_hass(hass)
    device = dev_reg.async_get_or_create(
        config_entry_id=sat_entry.entry_id,
        identifiers={("esphome", "voice-pe-kitchen")},
        name="Kitchen Voice",
    )
    dev_reg.async_update_device(device.id, area_id=area_id)
    entity = ent_reg.async_get_or_create(
        "assist_satellite",
        "esphome",
        "voice-pe-kitchen-satellite",
        suggested_object_id="kitchen_voice",
        device_id=device.id,
        config_entry=sat_entry,
    )
    if entity_area is not None:
        ent_reg.async_update_entity(entity.entity_id, area_id=entity_area)
    return entity.entity_id


def _place_output(hass, entry, node_name, area_id):
    """Put our per-output media_player entity in an area, the way device adoption
    (or a user override) would."""
    ent_reg = er.async_get(hass)
    entity_id = ent_reg.async_get_entity_id("media_player", DOMAIN, f"{entry.entry_id}_out_{node_name}")
    assert entity_id is not None, f"no per-output entity for {node_name}"
    ent_reg.async_update_entity(entity_id, area_id=area_id)
    return entity_id


async def _talk(hass, entity_id, state):
    hass.states.async_set(entity_id, state)
    await hass.async_block_till_done()


async def test_ducks_only_the_outputs_in_the_satellites_area(hass):
    """The default `area` scope: the kitchen speaker ducks, the bathroom one does
    not — even though both are router outputs."""
    entry = _make_entry(hass)
    area_reg = ar.async_get(hass)
    kitchen = area_reg.async_get_or_create("Kitchen")
    bath = area_reg.async_get_or_create("Bathroom")
    stack, duck, _renew, release = _patch_daemon(_matrix("sendspin-dev-kitchen", "sendspin-dev-bath"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        _place_output(hass, entry, "sendspin-dev-bath", bath.id)
        satellite = _place_satellite(hass, kitchen.id)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )

        await _talk(hass, satellite, "listening")
        duck.assert_awaited_once_with(["sendspin-dev-kitchen"], 0.25, VOICE_DUCK_TTL_SECONDS * 1000)

        # Turn over → the hold is handed back, not left to expire.
        await _talk(hass, satellite, "idle")
        release.assert_awaited_once_with(77)


async def test_enabled_by_default_ducks_with_no_setup_at_all(hass):
    """Nothing to configure and nothing to switch on: a fresh install ducks on the
    first voice turn. The feature has no settings of its own (satellites, areas and
    outputs all come from registries HA already has), so shipping it off would ship
    it undiscovered — there is no error and no log line to hint at it."""
    entry = _make_entry(hass)
    kitchen = ar.async_get(hass).async_get_or_create("Kitchen")
    stack, duck, _renew, _release = _patch_daemon(_matrix("sendspin-dev-kitchen"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        satellite = _place_satellite(hass, kitchen.id)
        assert hass.states.get("switch.pipewire_audio_router_voice_assistant_ducking").state == "on"

        await _talk(hass, satellite, "listening")
        duck.assert_awaited_once_with(["sendspin-dev-kitchen"], 0.25, VOICE_DUCK_TTL_SECONDS * 1000)


async def test_a_remembered_off_survives_a_restart(hass):
    """Someone who switched it off (still running the ducking blueprint, say) keeps
    it off — the on-by-default only applies when there is nothing remembered."""
    mock_restore_cache(hass, [State("switch.pipewire_audio_router_voice_assistant_ducking", "off")])
    entry = _make_entry(hass)
    kitchen = ar.async_get(hass).async_get_or_create("Kitchen")
    stack, duck, _renew, _release = _patch_daemon(_matrix("sendspin-dev-kitchen"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        satellite = _place_satellite(hass, kitchen.id)
        assert hass.states.get("switch.pipewire_audio_router_voice_assistant_ducking").state == "off"

        await _talk(hass, satellite, "listening")
        duck.assert_not_awaited()


async def test_an_unavailable_restored_state_is_not_a_choice_to_disable(hass):
    """The entity was down when HA shut down, so its restored state is
    `unavailable` — not the user's decision. It comes back on."""
    mock_restore_cache(hass, [State("switch.pipewire_audio_router_voice_assistant_ducking", "unavailable")])
    entry = _make_entry(hass)
    stack, _duck, _renew, _release = _patch_daemon(_matrix("sendspin-dev-kitchen"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        assert hass.states.get("switch.pipewire_audio_router_voice_assistant_ducking").state == "on"


async def test_responding_keeps_the_duck_and_idle_ends_it(hass):
    """`listening` → `processing` → `responding` is one turn: one hold, held until
    idle (the satellite speaks its answer on its own speaker)."""
    entry = _make_entry(hass)
    kitchen = ar.async_get(hass).async_get_or_create("Kitchen")
    stack, duck, _renew, release = _patch_daemon(_matrix("sendspin-dev-kitchen"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        satellite = _place_satellite(hass, kitchen.id)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )

        for state in ("listening", "processing", "responding"):
            await _talk(hass, satellite, state)
        assert duck.await_count == 1, "one hold for the whole turn"
        release.assert_not_awaited()

        await _talk(hass, satellite, "idle")
        release.assert_awaited_once_with(77)


async def test_unavailable_satellite_releases_the_duck(hass):
    """A satellite that drops off the network mid-turn must not leave music quiet."""
    entry = _make_entry(hass)
    kitchen = ar.async_get(hass).async_get_or_create("Kitchen")
    stack, _duck, _renew, release = _patch_daemon(_matrix("sendspin-dev-kitchen"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        satellite = _place_satellite(hass, kitchen.id)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )
        await _talk(hass, satellite, "listening")
        await _talk(hass, satellite, "unavailable")
        release.assert_awaited_once_with(77)


async def test_music_group_scope_widens_to_every_group_member(hass):
    """`music_group`: the hall speaker ducks too, because it plays the same
    synchronized stream as the kitchen one."""
    entry = _make_entry(hass)
    area_reg = ar.async_get(hass)
    kitchen = area_reg.async_get_or_create("Kitchen")
    hall = area_reg.async_get_or_create("Hall")
    groups = [MusicGroup(id="downstairs", name="Downstairs", members=["sendspin-dev-kitchen", "sendspin-dev-hall"])]
    stack, duck, _renew, _release = _patch_daemon(
        _matrix("sendspin-dev-kitchen", "sendspin-dev-hall"), music_groups=groups
    )
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        _place_output(hass, entry, "sendspin-dev-hall", hall.id)
        satellite = _place_satellite(hass, kitchen.id)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )
        await hass.services.async_call(
            "select",
            "select_option",
            {"entity_id": "select.pipewire_audio_router_voice_assistant_duck_scope", "option": VOICE_DUCK_SCOPE_MUSIC_GROUP},
            blocking=True,
        )

        await _talk(hass, satellite, "listening")
        duck.assert_awaited_once_with(
            ["sendspin-dev-hall", "sendspin-dev-kitchen"], 0.25, VOICE_DUCK_TTL_SECONDS * 1000
        )


async def test_area_scope_does_not_widen_to_the_group(hass):
    """Same setup as above on the default scope: only the satellite's own room,
    mid-song inside the group — what per-device ducking makes possible."""
    entry = _make_entry(hass)
    area_reg = ar.async_get(hass)
    kitchen = area_reg.async_get_or_create("Kitchen")
    hall = area_reg.async_get_or_create("Hall")
    groups = [MusicGroup(id="downstairs", name="Downstairs", members=["sendspin-dev-kitchen", "sendspin-dev-hall"])]
    stack, duck, _renew, _release = _patch_daemon(
        _matrix("sendspin-dev-kitchen", "sendspin-dev-hall"), music_groups=groups
    )
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        _place_output(hass, entry, "sendspin-dev-hall", hall.id)
        satellite = _place_satellite(hass, kitchen.id)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )
        assert hass.states.get("select.pipewire_audio_router_voice_assistant_duck_scope").state == VOICE_DUCK_SCOPE_AREA

        await _talk(hass, satellite, "listening")
        duck.assert_awaited_once_with(["sendspin-dev-kitchen"], 0.25, VOICE_DUCK_TTL_SECONDS * 1000)


async def test_the_satellites_own_output_is_ducked_not_skipped(hass):
    """The Voice PE's own speaker carries *music* from the router while the device
    speaks locally — so it is the one output that must go quiet."""
    entry = _make_entry(hass)
    kitchen = ar.async_get(hass).async_get_or_create("Kitchen")
    stack, duck, _renew, _release = _patch_daemon(_matrix("sendspin-dev-kitchen_voice"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        # The output entity and the satellite share one device: adoption links our
        # media_player to the very device the satellite lives on.
        out_entity = _place_output(hass, entry, "sendspin-dev-kitchen_voice", kitchen.id)
        satellite = _place_satellite(hass, kitchen.id)
        ent_reg = er.async_get(hass)
        sat_device = ent_reg.async_get(satellite).device_id
        ent_reg.async_update_entity(out_entity, device_id=sat_device, area_id=None)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )

        await _talk(hass, satellite, "listening")
        duck.assert_awaited_once_with(["sendspin-dev-kitchen_voice"], 0.25, VOICE_DUCK_TTL_SECONDS * 1000)


async def test_two_satellites_in_two_areas_duck_independently(hass):
    """Overlapping turns: one hold each, released independently — the blueprint's
    `mode: single` could not do this."""
    entry = _make_entry(hass)
    area_reg = ar.async_get(hass)
    kitchen = area_reg.async_get_or_create("Kitchen")
    bath = area_reg.async_get_or_create("Bathroom")
    stack, duck, _renew, release = _patch_daemon(_matrix("sendspin-dev-kitchen", "sendspin-dev-bath"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        _place_output(hass, entry, "sendspin-dev-bath", bath.id)
        kitchen_sat = _place_satellite(hass, kitchen.id)
        # A second satellite, in the bathroom.
        ent_reg = er.async_get(hass)
        dev_reg = dr.async_get(hass)
        sat_entry = MockConfigEntry(domain="esphome", data={})
        sat_entry.add_to_hass(hass)
        device = dev_reg.async_get_or_create(
            config_entry_id=sat_entry.entry_id, identifiers={("esphome", "voice-pe-bath")}, name="Bath Voice"
        )
        dev_reg.async_update_device(device.id, area_id=bath.id)
        bath_sat = ent_reg.async_get_or_create(
            "assist_satellite",
            "esphome",
            "voice-pe-bath-satellite",
            suggested_object_id="bath_voice",
            device_id=device.id,
            config_entry=sat_entry,
        ).entity_id
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )

        duck.side_effect = [11, 22]
        await _talk(hass, kitchen_sat, "listening")
        await _talk(hass, bath_sat, "listening")
        assert [c.args[0] for c in duck.await_args_list] == [["sendspin-dev-kitchen"], ["sendspin-dev-bath"]]

        await _talk(hass, kitchen_sat, "idle")
        release.assert_awaited_once_with(11)
        await _talk(hass, bath_sat, "idle")
        assert [c.args[0] for c in release.await_args_list] == [11, 22]


async def test_satellite_with_no_area_ducks_nothing(hass):
    """No area ⇒ no way to know which room: do nothing rather than guess."""
    entry = _make_entry(hass)
    kitchen = ar.async_get(hass).async_get_or_create("Kitchen")
    stack, duck, _renew, _release = _patch_daemon(_matrix("sendspin-dev-kitchen"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        satellite = _place_satellite(hass, None)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )

        await _talk(hass, satellite, "listening")
        duck.assert_not_awaited()


async def test_entity_area_override_beats_the_devices_area(hass):
    """A satellite whose entity carries its own area wins over its device's — the
    documented way to fix a mis-adopted room without moving the device."""
    entry = _make_entry(hass)
    area_reg = ar.async_get(hass)
    kitchen = area_reg.async_get_or_create("Kitchen")
    bath = area_reg.async_get_or_create("Bathroom")
    stack, duck, _renew, _release = _patch_daemon(_matrix("sendspin-dev-kitchen", "sendspin-dev-bath"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        _place_output(hass, entry, "sendspin-dev-bath", bath.id)
        # Device says Kitchen, the entity override says Bathroom.
        satellite = _place_satellite(hass, kitchen.id, entity_area=bath.id)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )

        await _talk(hass, satellite, "listening")
        duck.assert_awaited_once_with(["sendspin-dev-bath"], 0.25, VOICE_DUCK_TTL_SECONDS * 1000)


async def test_turning_the_switch_off_mid_turn_un_ducks_immediately(hass):
    """Not one lease later: switching the feature off hands every hold back."""
    entry = _make_entry(hass)
    kitchen = ar.async_get(hass).async_get_or_create("Kitchen")
    stack, _duck, _renew, release = _patch_daemon(_matrix("sendspin-dev-kitchen"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        satellite = _place_satellite(hass, kitchen.id)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )
        await _talk(hass, satellite, "listening")

        await hass.services.async_call(
            "switch", "turn_off", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )
        release.assert_awaited_once_with(77)


async def test_duck_level_number_is_used_for_the_hold(hass):
    """The level entity is a gain, and it's what the daemon is asked for."""
    entry = _make_entry(hass)
    kitchen = ar.async_get(hass).async_get_or_create("Kitchen")
    stack, duck, _renew, _release = _patch_daemon(_matrix("sendspin-dev-kitchen"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        satellite = _place_satellite(hass, kitchen.id)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )
        await hass.services.async_call(
            "number",
            "set_value",
            {"entity_id": "number.pipewire_audio_router_voice_assistant_duck_level", "value": 0.1},
            blocking=True,
        )

        await _talk(hass, satellite, "listening")
        duck.assert_awaited_once_with(["sendspin-dev-kitchen"], 0.1, VOICE_DUCK_TTL_SECONDS * 1000)


async def test_unloading_the_entry_releases_holds(hass):
    """A reload mid-turn must not leave music quiet until the lease runs out."""
    entry = _make_entry(hass)
    kitchen = ar.async_get(hass).async_get_or_create("Kitchen")
    stack, _duck, _renew, release = _patch_daemon(_matrix("sendspin-dev-kitchen"))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        _place_output(hass, entry, "sendspin-dev-kitchen", kitchen.id)
        satellite = _place_satellite(hass, kitchen.id)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )
        await _talk(hass, satellite, "listening")

        assert await hass.config_entries.async_unload(entry.entry_id)
        await hass.async_block_till_done()
        release.assert_awaited_once_with(77)


async def test_ducks_an_ap2_output_via_its_adopted_device_area(hass):
    """With per-output entities *off*, the area comes from the same device
    correlation adoption uses — here an AirPlay-2 receiver matched by IP."""
    entry = _make_entry(hass)
    kitchen = ar.async_get(hass).async_get_or_create("Kitchen")
    outputs = [OutputMeta(node_name="ap2-dev-pioneer", kind="airplay2", ip="192.168.1.50")]
    stack, duck, _renew, _release = _patch_daemon(
        _matrix("ap2-dev-pioneer"), outputs=outputs, expose_outputs=False
    )
    with stack:
        # A receiver's own integration (e.g. Onkyo) pointed at that IP, in Kitchen.
        recv_entry = MockConfigEntry(domain="onkyo", data={"host": "192.168.1.50"})
        recv_entry.add_to_hass(hass)
        dev_reg = dr.async_get(hass)
        device = dev_reg.async_get_or_create(
            config_entry_id=recv_entry.entry_id, identifiers={("onkyo", "pioneer")}, name="Pioneer"
        )
        dev_reg.async_update_device(device.id, area_id=kitchen.id)

        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        satellite = _place_satellite(hass, kitchen.id)
        await hass.services.async_call(
            "switch", "turn_on", {"entity_id": "switch.pipewire_audio_router_voice_assistant_ducking"}, blocking=True
        )

        await _talk(hass, satellite, "listening")
        duck.assert_awaited_once_with(["ap2-dev-pioneer"], 0.25, VOICE_DUCK_TTL_SECONDS * 1000)
