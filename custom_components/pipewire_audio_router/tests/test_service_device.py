"""The add-on as one Home Assistant service device (service_device.py).

The point of the device is grouping and discoverability, so what these tests
protect is *which* entities live on it — and, just as much, which do not: the
per-output `media_player`s belong to the real speaker's device, because that is
where their area comes from and the area is what voice ducking resolves against.
"""

from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

from homeassistant.helpers import area_registry as ar, device_registry as dr, entity_registry as er
from homeassistant.helpers.device_registry import DeviceEntryType
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.pipewire_audio_router.api import (
    AnnouncementGroup,
    AppSettings,
    DaemonStatus,
    MusicGroup,
    PipewireRouterApiError,
    RoutingMatrix,
    RoutingNode,
    RtpSourceState,
)
from custom_components.pipewire_audio_router.const import DOMAIN

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
DAEMON_STATUS = DaemonStatus(version="0.3.0", host_model="Raspberry Pi 4 Model B", host_arch="aarch64")
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)
EMPTY_ROUTING = RoutingMatrix(sources=[], outputs=[], links=[])


def _make_entry(hass):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8080})
    entry.add_to_hass(hass)
    return entry


def _patch_daemon(routing=EMPTY_ROUTING, *, status=DAEMON_STATUS, music_groups=None, announcement_groups=None):
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_health", new=AsyncMock(return_value=None)))
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=routing)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=RTP_DISABLED)))
    stack.enter_context(patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value={})))
    stack.enter_context(patch(f"{API}.async_get_outputs", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_music_groups", new=AsyncMock(return_value=music_groups or [])))
    stack.enter_context(
        patch(f"{API}.async_get_announcement_groups", new=AsyncMock(return_value=announcement_groups or []))
    )
    stack.enter_context(patch(f"{API}.async_get_status", new=AsyncMock(return_value=status)))
    stack.enter_context(
        patch(
            f"{API}.async_get_settings",
            new=AsyncMock(return_value=AppSettings(expose_outputs_as_media_players=True)),
        )
    )
    stack.enter_context(patch(f"{COORD}.async_routing_ws_loop", new=AsyncMock()))
    return stack


def _service_device(hass, entry):
    return dr.async_get(hass).async_get_device(identifiers={(DOMAIN, entry.entry_id)})


async def test_the_add_on_is_one_service_device(hass):
    """Keyed by the config entry id, not by host: a moved add-on keeps its device
    and every entity's history with it. The version and the host come from
    `/api/status`, and `configuration_url` is the daemon's own UI — the console for
    everything the integration only pokes at over REST."""
    entry = _make_entry(hass)
    with _patch_daemon():
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    device = _service_device(hass, entry)
    assert device is not None
    assert device.entry_type is DeviceEntryType.SERVICE
    assert device.sw_version == "0.3.0"
    assert device.hw_version == "Raspberry Pi 4 Model B (aarch64)"
    assert device.configuration_url == "http://127.0.0.1:8080"


async def test_a_daemon_without_the_status_endpoint_still_gets_a_device(hass):
    """`/api/status` is best-effort — an older add-on simply has no version to
    show, and must not cost the device (and with it every service entity's home)."""
    entry = _make_entry(hass)
    with _patch_daemon() as stack:
        stack.enter_context(
            patch(f"{API}.async_get_status", new=AsyncMock(side_effect=PipewireRouterApiError("404")))
        )
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    device = _service_device(hass, entry)
    assert device is not None and device.sw_version is None


async def test_an_add_on_update_refreshes_the_version(hass):
    """The version is re-registered when it changes, so a device page doesn't keep
    claiming the build that was running when Home Assistant last restarted."""
    entry = _make_entry(hass)
    with _patch_daemon() as stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        assert _service_device(hass, entry).sw_version == "0.3.0"

        stack.enter_context(
            patch(
                f"{API}.async_get_status",
                new=AsyncMock(return_value=DaemonStatus(version="0.4.0", host_model="Raspberry Pi 4 Model B")),
            )
        )
        coordinator = hass.data[DOMAIN][entry.entry_id]
        await coordinator.async_refresh()
        await hass.async_block_till_done()

    assert _service_device(hass, entry).sw_version == "0.4.0"


async def test_the_settings_live_on_the_device(hass):
    """Exactly the six settings, and nothing else. This is what makes the device
    page a usable index: one place listing everything about the add-on that isn't a
    speaker or a group."""
    entry = _make_entry(hass)
    with _patch_daemon():
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    device = _service_device(hass, entry)
    ent_reg = er.async_get(hass)
    # Compared by unique-id suffix rather than entity_id: how Home Assistant builds
    # an entity_id from a device + entity name has changed between releases, and
    # this test is about *which* entities are on the device, not what they ended up
    # called.
    ours = {
        e.unique_id.removeprefix(f"{entry.entry_id}_")
        for e in er.async_entries_for_device(ent_reg, device.id, include_disabled_entities=True)
    }
    assert ours == {
        "rtp_source",
        "voice_ducking",
        "rtp_source_port",
        "rtp_source_latency_msec",
        "voice_duck_level",
        "voice_duck_scope",
    }


async def test_a_group_stays_off_the_device_to_keep_its_name(hass):
    """Groups are the add-on's own construct and belonged on the device by that
    logic, but they are also the entities people actually call — in a media card, in
    `tts.speak`, in a script. Home Assistant prefixes an entity's displayed name
    with its device's, so joining made "Everywhere" read "PipeWire Audio Router
    Everywhere", and a group created afterwards would have taken that into its
    entity_id too. The user's own name for a group wins over tidiness."""
    entry = _make_entry(hass)
    groups = [MusicGroup(id="downstairs", name="Downstairs", members=["sendspin-dev-kitchen"])]
    announcements = [
        AnnouncementGroup(id="everywhere", name="Everywhere", targets=["sendspin-dev-kitchen"], priority=5, duck=0.2)
    ]
    with _patch_daemon(music_groups=groups, announcement_groups=announcements):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    ent_reg = er.async_get(hass)
    for kind, group_id, expected in (("mg", "downstairs", "Downstairs"), ("ag", "everywhere", "Everywhere")):
        entity_id = ent_reg.async_get_entity_id("media_player", DOMAIN, f"{entry.entry_id}_{kind}_{group_id}")
        assert ent_reg.async_get(entity_id).device_id is None
        assert hass.states.get(entity_id).attributes["friendly_name"] == expected


async def test_an_output_stays_on_its_speakers_device(hass):
    """The one thing the service device must not swallow. A per-output
    `media_player` adopts the real speaker's device to inherit its area, and voice
    ducking resolves a satellite's room against exactly that area — re-homing these
    would silently stop ducking to make a list look tidier."""
    dev_reg = dr.async_get(hass)
    esphome_entry = MockConfigEntry(domain="esphome", data={})
    esphome_entry.add_to_hass(hass)
    speaker = dev_reg.async_get_or_create(
        config_entry_id=esphome_entry.entry_id,
        connections={(dr.CONNECTION_NETWORK_MAC, "20:f8:3b:09:3c:a8")},
        name="Home Assistant Voice 093ca8",
    )
    dev_reg.async_update_device(speaker.id, area_id=ar.async_get(hass).async_get_or_create("Badezimmer").id)

    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[
            RoutingNode(
                node_id=None,
                node_name="sendspin-dev-home_assistant_voice_093ca8",
                display_name="Home Assistant Voice 093ca8",
                configured=False,
            )
        ],
        links=[],
    )
    with _patch_daemon(routing):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    ent_reg = er.async_get(hass)
    entity_id = ent_reg.async_get_entity_id(
        "media_player", DOMAIN, f"{entry.entry_id}_out_sendspin-dev-home_assistant_voice_093ca8"
    )
    assert ent_reg.async_get(entity_id).device_id == speaker.id
    assert ent_reg.async_get(entity_id).device_id != _service_device(hass, entry).id
