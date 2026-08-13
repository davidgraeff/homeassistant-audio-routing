"""A device per pw-sink host (pwsink_hosts.py), and the room it makes duckable.

A pw-sink output is a PC running `pwrouter-agent`. Unlike a sendspin speaker or an
AirPlay-2 receiver it has nothing in Home Assistant to inherit an area from, so
until this existed a voice turn could never duck it — the resolution in
`voice_duck.py` simply returned no area.
"""

from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

from homeassistant.helpers import area_registry as ar, device_registry as dr, entity_registry as er
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.pipewire_audio_router.api import (
    AppSettings,
    DaemonStatus,
    PwsinkAgent,
    RoutingMatrix,
    RoutingNode,
    RtpSourceState,
)
from custom_components.pipewire_audio_router.const import DOMAIN, VOICE_DUCK_TTL_SECONDS
from custom_components.pipewire_audio_router.pwsink_hosts import pwsink_host_identifier

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
DAEMON_STATUS = DaemonStatus(version="0.3.0", host_model="Raspberry Pi 4 Model B", host_arch="aarch64")
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)
HOST = "pwsink-dev-david_local_david"
# What the daemon calls the host: "<hostname> (<user>)", from its agent listing.
HOST_LABEL = "david-local (david)"


def _make_entry(hass):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8080})
    entry.add_to_hass(hass)
    return entry


def _matrix(*outputs):
    """`outputs` are node names, or (node_name, display_name) pairs — the daemon
    puts the agent's label in the matrix's display name for a pw-sink host."""
    nodes = [o if isinstance(o, tuple) else (o, o) for o in outputs]
    return RoutingMatrix(
        sources=[RoutingNode(node_id=10, node_name="shairport-sync", display_name="shairport-sync")],
        outputs=[RoutingNode(node_id=None, node_name=n, display_name=label) for n, label in nodes],
        links=[],
    )


def _patch_daemon(routing, *, expose_outputs=False, agents=None):
    """Offline setup. `expose_outputs` defaults to *off* here — the case that
    matters, since a host device is what makes ducking work without per-output
    entities."""
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_health", new=AsyncMock(return_value=None)))
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=routing)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=RTP_DISABLED)))
    stack.enter_context(patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value={})))
    stack.enter_context(patch(f"{API}.async_get_outputs", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_music_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_announcement_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_status", new=AsyncMock(return_value=DAEMON_STATUS)))
    stack.enter_context(patch(f"{API}.async_get_agents", new=AsyncMock(return_value=agents or [])))
    stack.enter_context(
        patch(
            f"{API}.async_get_settings",
            new=AsyncMock(return_value=AppSettings(expose_outputs_as_media_players=expose_outputs)),
        )
    )
    stack.enter_context(patch(f"{COORD}.async_routing_ws_loop", new=AsyncMock()))
    duck = stack.enter_context(patch(f"{API}.async_duck_start", new=AsyncMock(return_value=77)))
    stack.enter_context(patch(f"{API}.async_duck_renew", new=AsyncMock(return_value=True)))
    stack.enter_context(patch(f"{API}.async_duck_release", new=AsyncMock()))
    return stack, duck


def _host_device(hass):
    return dr.async_get(hass).async_get_device(identifiers={pwsink_host_identifier(HOST)})


async def test_an_adopted_host_gets_a_device_named_after_the_machine(hass):
    """Named from the daemon's own label so it is recognizable as that PC, and
    keyed by the output's node name — which the daemon pins per agent identity, so
    a re-pairing or a renamed host keeps this device and its room."""
    entry = _make_entry(hass)
    stack, _duck = _patch_daemon(_matrix((HOST, HOST_LABEL)))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    device = _host_device(hass)
    assert device is not None
    assert device.name == HOST_LABEL
    assert device.model == "PipeWire host (pwrouter-agent)"


async def test_the_room_on_that_device_is_what_gets_ducked(hass):
    """The whole point. Assign the host's device to a room, and a voice turn in
    that room ducks the PC — with per-output `media_player`s switched off, so the
    only thing carrying the area is this device."""
    entry = _make_entry(hass)
    stack, duck = _patch_daemon(_matrix((HOST, HOST_LABEL)))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        office = ar.async_get(hass).async_get_or_create("Büro")
        dr.async_get(hass).async_update_device(_host_device(hass).id, area_id=office.id)
        satellite = _place_satellite(hass, office.id)

        hass.states.async_set(satellite, "listening")
        await hass.async_block_till_done()

    duck.assert_awaited_once_with([HOST], 0.25, VOICE_DUCK_TTL_SECONDS * 1000)


async def test_without_a_room_the_host_is_simply_not_ducked(hass):
    """Unassigned is not an error and must stay silent: the device exists, has no
    area, and a voice turn elsewhere ducks nothing here."""
    entry = _make_entry(hass)
    stack, duck = _patch_daemon(_matrix((HOST, HOST_LABEL)))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        office = ar.async_get(hass).async_get_or_create("Büro")
        satellite = _place_satellite(hass, office.id)

        hass.states.async_set(satellite, "listening")
        await hass.async_block_till_done()

    assert _host_device(hass) is not None
    duck.assert_not_awaited()


async def test_the_host_device_survives_a_registry_cleanup(hass):
    """`device_registry.async_cleanup` reaps devices with neither entities nor a
    live config entry. This one belongs to ours, so the room assignment is safe even
    in the shape that would otherwise qualify — per-output players off, leaving only
    the diagnostic sink sensor."""
    entry = _make_entry(hass)
    stack, _duck = _patch_daemon(_matrix((HOST, HOST_LABEL)))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        device = _host_device(hass)
        ent_reg = er.async_get(hass)
        assert {e.entity_id for e in er.async_entries_for_device(ent_reg, device.id)} == {
            "sensor.david_local_david_output_device"
        }

        dr.async_cleanup(hass, dr.async_get(hass), ent_reg)
        await hass.async_block_till_done()

    assert _host_device(hass) is not None


async def test_the_device_shows_the_agents_build(hass):
    """The version the host reports over its connection, as the device's firmware
    line — the answer to "is that machine running the agent I just built"."""
    entry = _make_entry(hass)
    agents = [PwsinkAgent(node_name=HOST, label=HOST_LABEL, paired=True, connected=True, version="0.2.1")]
    stack, _duck = _patch_daemon(_matrix((HOST, HOST_LABEL)), agents=agents)
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    assert _host_device(hass).sw_version == "0.2.1"


async def test_a_sleeping_host_keeps_its_last_known_build(hass):
    """The daemon only learns the version from a live connection, so a disconnected
    host reports none. Blanking the field every time the machine sleeps would make
    the device page flicker between "0.2.1" and nothing."""
    entry = _make_entry(hass)
    agents = [PwsinkAgent(node_name=HOST, label=HOST_LABEL, paired=True, connected=True, version="0.2.1")]
    stack, _duck = _patch_daemon(_matrix((HOST, HOST_LABEL)), agents=agents)
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        offline = [PwsinkAgent(node_name=HOST, label=HOST_LABEL, paired=True, connected=False, version=None)]
        stack.enter_context(patch(f"{API}.async_get_agents", new=AsyncMock(return_value=offline)))
        await hass.data[DOMAIN][entry.entry_id].async_refresh()
        await hass.async_block_till_done()

    assert _host_device(hass).sw_version == "0.2.1"


async def test_the_sink_sensor_reports_where_the_agent_plays(hass):
    """"Routed there but I can't hear it" is usually answered by which sink on that
    machine the agent chose, and only the agent can say."""
    entry = _make_entry(hass)
    agents = [
        PwsinkAgent(
            node_name=HOST,
            label=HOST_LABEL,
            paired=True,
            connected=True,
            version="0.2.1",
            sink_name="alsa_output.pci-0000_0a_00.4.analog-stereo",
        )
    ]
    stack, _duck = _patch_daemon(_matrix((HOST, HOST_LABEL)), agents=agents)
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    sink = hass.states.get("sensor.david_local_david_output_device")
    assert sink is not None
    assert sink.state == "alsa_output.pci-0000_0a_00.4.analog-stereo"
    assert sink.attributes["friendly_name"] == f"{HOST_LABEL} Output device"


async def test_the_sink_sensor_is_unavailable_with_no_agent_connected(hass):
    """A sink name from a machine that may have rebooted since is a guess, so it
    goes unavailable rather than showing a stale device."""
    entry = _make_entry(hass)
    agents = [
        PwsinkAgent(node_name=HOST, label=HOST_LABEL, paired=True, connected=False, sink_name="alsa_output.old")
    ]
    stack, _duck = _patch_daemon(_matrix((HOST, HOST_LABEL)), agents=agents)
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    assert hass.states.get("sensor.david_local_david_output_device").state == "unavailable"


async def test_the_sink_sensor_goes_when_the_host_does(hass):
    """It is tied to the output, so removing the host takes the sensor with it
    rather than leaving an unavailable entity behind."""
    entry = _make_entry(hass)
    agents = [PwsinkAgent(node_name=HOST, label=HOST_LABEL, paired=True, connected=True, sink_name="alsa_output.x")]
    stack, _duck = _patch_daemon(_matrix((HOST, HOST_LABEL)), agents=agents)
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        assert hass.states.get("sensor.david_local_david_output_device") is not None

        coordinator = hass.data[DOMAIN][entry.entry_id]
        coordinator._apply_routing(_matrix("sendspin-dev-kitchen"))
        await hass.async_block_till_done()

    assert hass.states.get("sensor.david_local_david_output_device") is None


async def test_an_exposed_output_entity_joins_its_host_device(hass):
    """With per-output entities on, the host's `media_player` links to the same
    device — so it reads as that machine and inherits the room instead of standing
    alone under a slug."""
    entry = _make_entry(hass)
    stack, _duck = _patch_daemon(_matrix((HOST, HOST_LABEL)), expose_outputs=True)
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    ent_reg = er.async_get(hass)
    entity_id = ent_reg.async_get_entity_id("media_player", DOMAIN, f"{entry.entry_id}_out_{HOST}")
    assert entity_id is not None
    assert ent_reg.async_get(entity_id).device_id == _host_device(hass).id


async def test_a_host_that_leaves_the_matrix_loses_its_device(hass):
    """The user removing the output is the one thing that should drop the row —
    otherwise a stale machine keeps a room assignment nothing can reach."""
    entry = _make_entry(hass)
    stack, _duck = _patch_daemon(_matrix((HOST, HOST_LABEL)))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        assert _host_device(hass) is not None

        coordinator = hass.data[DOMAIN][entry.entry_id]
        coordinator._apply_routing(_matrix("sendspin-dev-kitchen"))
        await hass.async_block_till_done()

    assert _host_device(hass) is None


async def test_an_empty_matrix_is_not_a_removal(hass):
    """A matrix with nothing in it means "the daemon hasn't told us yet", not "the
    user removed everything" — treating the two alike would throw away rooms on
    every restart where the WebSocket is slow."""
    entry = _make_entry(hass)
    stack, _duck = _patch_daemon(_matrix((HOST, HOST_LABEL)))
    with stack:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        coordinator = hass.data[DOMAIN][entry.entry_id]
        coordinator._apply_routing(RoutingMatrix(sources=[], outputs=[], links=[]))
        await hass.async_block_till_done()

    assert _host_device(hass) is not None


def _place_satellite(hass, area_id):
    """An `assist_satellite` on a device in `area_id`, as an ESPHome Voice PE."""
    dev_reg = dr.async_get(hass)
    sat_entry = MockConfigEntry(domain="esphome", data={})
    sat_entry.add_to_hass(hass)
    device = dev_reg.async_get_or_create(
        config_entry_id=sat_entry.entry_id,
        identifiers={("esphome", "voice-pe-office")},
        name="Office Voice",
    )
    dev_reg.async_update_device(device.id, area_id=area_id)
    entity = er.async_get(hass).async_get_or_create(
        "assist_satellite",
        "esphome",
        "voice-pe-office-satellite",
        suggested_object_id="office_voice",
        device_id=device.id,
        config_entry=sat_entry,
    )
    return entity.entity_id
