"""Real integration-setup tests: loads the actual config entry through
hass.config_entries.async_setup, not just instantiating the entity class
directly, so the coordinator/platform-forwarding wiring is exercised too.

Entities are driven by the routing *matrix* outputs — the auto-discovered
virtual AirPlay-2 (`ap2-dev-*`) and sendspin (`sendspin-dev-*`) devices — so an
output that leaves the matrix loses its entity, while a configured-but-offline
one (present=False) stays as `unavailable`."""

import json
from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

import aiohttp
import pytest
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import area_registry as ar, device_registry as dr, entity_registry as er
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.pipewire_audio_router.api import (
    AppSettings,
    OutputMeta,
    PipewireRouterApiClient,
    RoutingMatrix,
    RoutingNode,
    RtpSourceState,
)
from custom_components.pipewire_audio_router.const import DOMAIN

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
EMPTY_ROUTING = RoutingMatrix(sources=[], outputs=[], links=[])
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)


def _make_entry(hass):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8080})
    entry.add_to_hass(hass)
    return entry


def _patch_daemon(players, routing=EMPTY_ROUTING, rtp=RTP_DISABLED, sendspin_volumes=None, outputs=None):
    """Keep setup/refresh fully offline: mock the polled players + RTP-source +
    sendspin-volume + outputs-listing fetches and the one-shot routing seed, and
    stub out the routing WebSocket loop so no real socket is opened (routing is
    driven directly in the WS tests)."""
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_get_media_players", new=AsyncMock(return_value=players)))
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=routing)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=rtp)))
    stack.enter_context(
        patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value=sendspin_volumes or {}))
    )
    stack.enter_context(patch(f"{API}.async_get_outputs", new=AsyncMock(return_value=outputs or [])))
    # Groups + settings are polled too (they drive group/per-output entities);
    # patch them so setup is fully offline and doesn't depend on a connection
    # being refused (which a socket-blocking test env turns into a hard error).
    stack.enter_context(patch(f"{API}.async_get_music_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_announcement_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(
        patch(
            f"{API}.async_get_settings",
            new=AsyncMock(return_value=AppSettings(expose_outputs_as_media_players=True)),
        )
    )
    stack.enter_context(patch(f"{COORD}.async_routing_ws_loop", new=AsyncMock()))
    return stack


# The virtual outputs never appear in the polled media_players feed, so the
# routing tests run with an empty feed and drive everything off the matrix.
def _routing_players():
    return []


def _routing_matrix(links, outputs=None):
    return RoutingMatrix(
        sources=[
            RoutingNode(node_id=10, node_name="shairport-sync", display_name="shairport-sync"),
            RoutingNode(node_id=11, node_name="bt-bridge", display_name="bt-bridge"),
        ],
        outputs=outputs
        if outputs is not None
        else [RoutingNode(node_id=None, node_name="ap2-dev-kitchen", display_name="Kitchen")],
        links=links,
    )


async def test_entities_created_from_matrix_including_sendspin(hass):
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[
            # Both kinds are virtual: no node_id, not in the media_players feed.
            RoutingNode(node_id=None, node_name="ap2-dev-pioneer", display_name="Pioneer", configured=False),
            RoutingNode(node_id=None, node_name="sendspin-dev-bath", display_name="Bath", configured=False),
        ],
        links=[],
    )
    with _patch_daemon([], routing, sendspin_volumes={"sendspin-dev-bath": 50}):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    pioneer = hass.states.get("media_player.pioneer")
    bath = hass.states.get("media_player.bath")
    assert pioneer is not None and pioneer.state == "idle"
    # Sendspin device: exists (from the matrix), volume from the sendspin store,
    # state derived from routing (nothing linked -> idle).
    assert bath is not None and bath.state == "idle"
    assert float(bath.attributes["volume_level"]) == 0.5


async def test_sendspin_device_adopts_matching_ha_device_name_and_area(hass):
    """A sendspin output is correlated to the ESPHome device for the same
    speaker by its mDNS hostname (which appears in that device's ESPHome entity
    ids), then links via the device's real full-MAC connection so it inherits
    HA's friendly name + area instead of the cryptic daemon hostname."""
    dev_reg = dr.async_get(hass)
    ent_reg = er.async_get(hass)

    # Pre-existing ESPHome device + entity for the physical speaker, exactly as
    # the ESPHome integration registers it: full-MAC connection, a user-set
    # name, an area, and an `update` entity whose unique_id carries the hostname.
    esphome_entry = MockConfigEntry(domain="esphome", data={})
    esphome_entry.add_to_hass(hass)
    device = dev_reg.async_get_or_create(
        config_entry_id=esphome_entry.entry_id,
        connections={(dr.CONNECTION_NETWORK_MAC, "20:f8:3b:09:3c:a8")},
        name="Home Assistant Voice 093ca8",
    )
    dev_reg.async_update_device(device.id, name_by_user="Home Assistant Voice Badezimmer")
    area_reg = ar.async_get(hass)
    area = area_reg.async_get_or_create("Badezimmer")
    dev_reg.async_update_device(device.id, area_id=area.id)
    ent_reg.async_get_or_create(
        "update",
        "esphome",
        "20:F8:3B:09:3C:A8-update-home_assistant_voice_093ca8",
        device_id=device.id,
    )

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
    with _patch_daemon([], routing):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    # The entity is registered against the ESPHome device (linked by MAC)...
    entity_id = ent_reg.async_get_entity_id(
        "media_player", DOMAIN, f"{entry.entry_id}_out_sendspin-dev-home_assistant_voice_093ca8"
    )
    assert entity_id is not None
    reg_entry = ent_reg.async_get(entity_id)
    assert reg_entry.device_id == device.id
    # ...so its friendly name is HA's device name and it lives in the same area
    # (inherited from the linked device, since the entity sets no area of its own).
    state = hass.states.get(entity_id)
    assert state is not None
    # HA device name + the "Audio Routing" suffix (distinct from the speaker's
    # own built-in "… Media Player" on the same device).
    assert state.attributes["friendly_name"] == "Home Assistant Voice Badezimmer Audio Routing"
    assert dev_reg.async_get(device.id).area_id == area.id


async def test_sendspin_device_without_matching_ha_device_keeps_derived_name(hass):
    """No matching HA device → fall back to the daemon-derived display name and
    no device link (the previous behaviour), never a blank name."""
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[RoutingNode(node_id=None, node_name="sendspin-dev-bath", display_name="Bath", configured=False)],
        links=[],
    )
    with _patch_daemon([], routing):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    reg_entry = er.async_get(hass).async_get("media_player.bath")
    assert reg_entry is not None and reg_entry.device_id is None
    assert hass.states.get("media_player.bath").attributes["friendly_name"] == "Bath"


async def test_ap2_output_becomes_media_player_routable_with_volume(hass):
    """An `ap2-dev-*` receiver surfaces as a media_player: state derived from
    routing (like sendspin), source-selectable via the matrix, and now
    volume/mute-capable via the daemon's AP2 control plane. When the daemon
    reports no `ap2_volume` (genuinely unknown), `volume_level` is None — not a
    fabricated full scale."""
    from homeassistant.components.media_player import MediaPlayerEntityFeature

    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[RoutingNode(node_id=10, node_name="shairport-sync", display_name="shairport-sync")],
        outputs=[RoutingNode(node_id=None, node_name="ap2-dev-dusche", display_name="Dusche", configured=False)],
        links=[("shairport-sync", "ap2-dev-dusche")],
    )
    # No ap2_volume => unknown; ap2_muted absent too.
    outputs = [OutputMeta(node_name="ap2-dev-dusche", kind="airplay2", ip=None)]
    with _patch_daemon([], routing, outputs=outputs):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    dusche = hass.states.get("media_player.dusche")
    assert dusche is not None
    # A present source is linked → playing, derived purely from routing.
    assert dusche.state == "playing"
    assert dusche.attributes["source"] == "shairport-sync"
    # Volume unknown (daemon reported none) → honest None, not fabricated 1.0.
    assert dusche.attributes.get("volume_level") is None
    features = dusche.attributes["supported_features"]
    assert features & MediaPlayerEntityFeature.SELECT_SOURCE
    assert features & MediaPlayerEntityFeature.VOLUME_SET
    assert features & MediaPlayerEntityFeature.VOLUME_MUTE


async def test_ap2_reports_device_authoritative_volume_and_mute(hass):
    """When the daemon reports an `ap2_volume`/`ap2_muted`, the entity surfaces
    them (0.0–1.0 volume, mute flag) rather than None."""
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[RoutingNode(node_id=None, node_name="ap2-dev-dusche", display_name="Dusche", configured=False)],
        links=[],
    )
    outputs = [OutputMeta(node_name="ap2-dev-dusche", kind="airplay2", ip=None, ap2_volume=0.42, ap2_muted=True)]
    with _patch_daemon([], routing, outputs=outputs):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    dusche = hass.states.get("media_player.dusche")
    assert dusche is not None
    assert float(dusche.attributes["volume_level"]) == 0.42
    assert dusche.attributes["is_volume_muted"] is True


async def test_pwsink_host_reports_its_own_master_volume(hass):
    """A `pwsink-dev-*` output is a remote PipeWire host with a paired agent. Its
    volume/mute are the *host's* master out, reported by that agent — so they show
    up when reported and stay `None` when no agent is connected (the value belongs
    to that desktop; fabricating one would fight the user)."""
    from homeassistant.components.media_player import MediaPlayerEntityFeature

    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[RoutingNode(node_id=10, node_name="shairport-sync", display_name="shairport-sync")],
        outputs=[RoutingNode(node_id=None, node_name="pwsink-dev-desk_dave", display_name="desk (dave)", configured=False)],
        links=[("shairport-sync", "pwsink-dev-desk_dave")],
    )
    outputs = [
        OutputMeta(
            node_name="pwsink-dev-desk_dave", kind="pwsink", ip=None, pwsink_volume=0.37, pwsink_muted=False
        )
    ]
    with _patch_daemon([], routing, outputs=outputs):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    host = hass.states.get("media_player.desk_dave")
    assert host is not None
    assert host.state == "playing"
    assert host.attributes["volume_level"] == 0.37
    assert host.attributes["is_volume_muted"] is False
    features = host.attributes["supported_features"]
    assert features & MediaPlayerEntityFeature.VOLUME_SET
    assert features & MediaPlayerEntityFeature.VOLUME_MUTE


async def test_pwsink_volume_is_none_without_a_connected_agent(hass):
    """No agent connected → the daemon omits the level, and the entity reports
    `None` rather than a fabricated full scale."""
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[RoutingNode(node_id=None, node_name="pwsink-dev-desk_dave", display_name="desk (dave)", configured=False)],
        links=[],
    )
    outputs = [OutputMeta(node_name="pwsink-dev-desk_dave", kind="pwsink", ip=None)]
    with _patch_daemon([], routing, outputs=outputs):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    host = hass.states.get("media_player.desk_dave")
    assert host is not None
    assert host.attributes.get("volume_level") is None


async def test_set_volume_and_mute_on_pwsink_use_the_pwsink_api(hass):
    """pw-sink volume/mute go through `PUT /api/pwsink/volume|mute`, never the
    node-volume, sendspin or AP2 paths."""
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[RoutingNode(node_id=None, node_name="pwsink-dev-desk_dave", display_name="desk (dave)", configured=False)],
        links=[],
    )
    outputs = [OutputMeta(node_name="pwsink-dev-desk_dave", kind="pwsink", ip=None)]
    with _patch_daemon([], routing, outputs=outputs):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with (
            patch(f"{API}.async_set_pwsink_volume", new=AsyncMock()) as mock_vol,
            patch(f"{API}.async_set_pwsink_mute", new=AsyncMock()) as mock_mute,
            patch(f"{API}.async_set_ap2_volume", new=AsyncMock()) as mock_ap2_vol,
            patch(f"{API}.async_set_volume", new=AsyncMock()) as mock_node_vol,
        ):
            await hass.services.async_call(
                "media_player",
                "volume_set",
                {"entity_id": "media_player.desk_dave", "volume_level": 0.6},
                blocking=True,
            )
            await hass.services.async_call(
                "media_player",
                "volume_mute",
                {"entity_id": "media_player.desk_dave", "is_volume_muted": True},
                blocking=True,
            )
            mock_vol.assert_awaited_once_with("pwsink-dev-desk_dave", 0.6)
            mock_mute.assert_awaited_once_with("pwsink-dev-desk_dave", True)
            mock_ap2_vol.assert_not_awaited()
            mock_node_vol.assert_not_awaited()


async def test_set_volume_on_ap2_device_uses_ap2_api(hass):
    """AirPlay-2 volume_set must go through the AP2 control plane
    (`PUT /api/ap2/volume`, 0.0–1.0), not the node-volume or sendspin paths."""
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[RoutingNode(node_id=None, node_name="ap2-dev-dusche", display_name="Dusche", configured=False)],
        links=[],
    )
    outputs = [OutputMeta(node_name="ap2-dev-dusche", kind="airplay2", ip=None)]
    with _patch_daemon([], routing, outputs=outputs):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with (
            patch(f"{API}.async_set_ap2_volume", new=AsyncMock()) as mock_ap2_vol,
            patch(f"{API}.async_set_volume", new=AsyncMock()) as mock_node_vol,
            patch(f"{API}.async_set_sendspin_volume", new=AsyncMock()) as mock_sendspin_vol,
        ):
            await hass.services.async_call(
                "media_player",
                "volume_set",
                {"entity_id": "media_player.dusche", "volume_level": 0.6},
                blocking=True,
            )
            mock_ap2_vol.assert_awaited_once_with("ap2-dev-dusche", 0.6)
            mock_node_vol.assert_not_awaited()
            mock_sendspin_vol.assert_not_awaited()


async def test_mute_on_ap2_device_uses_ap2_api(hass):
    """AirPlay-2 mute goes through `PUT /api/ap2/mute`."""
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[RoutingNode(node_id=None, node_name="ap2-dev-dusche", display_name="Dusche", configured=False)],
        links=[],
    )
    outputs = [OutputMeta(node_name="ap2-dev-dusche", kind="airplay2", ip=None)]
    with _patch_daemon([], routing, outputs=outputs):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with patch(f"{API}.async_set_ap2_mute", new=AsyncMock()) as mock_ap2_mute:
            await hass.services.async_call(
                "media_player",
                "volume_mute",
                {"entity_id": "media_player.dusche", "is_volume_muted": True},
                blocking=True,
            )
            mock_ap2_mute.assert_awaited_once_with("ap2-dev-dusche", True)


async def test_ap2_device_adopts_matching_ha_device_by_ip(hass):
    """An AirPlay-2 receiver is correlated to an existing HA device (e.g. its
    MusicCast/Onkyo entry) by the receiver's IP, then adopts that device's area
    and friendly name — linking via the device's own identifiers when it has no
    MAC connection."""
    dev_reg = dr.async_get(hass)
    area_reg = ar.async_get(hass)
    ent_reg = er.async_get(hass)

    # A MusicCast device for the same physical receiver, keyed by identifiers
    # (no MAC connection), reachable at the IP the daemon will report.
    musiccast_entry = MockConfigEntry(domain="yamaha_musiccast", data={"host": "192.168.1.55"})
    musiccast_entry.add_to_hass(hass)
    device = dev_reg.async_get_or_create(
        config_entry_id=musiccast_entry.entry_id,
        identifiers={("yamaha_musiccast", "abc-def-123")},
        name="Dusche",
    )
    area = area_reg.async_get_or_create("Badezimmer")
    dev_reg.async_update_device(device.id, area_id=area.id)

    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[RoutingNode(node_id=None, node_name="ap2-dev-dusche", display_name="Dusche", configured=False)],
        links=[],
    )
    outputs = [OutputMeta(node_name="ap2-dev-dusche", kind="airplay2", ip="192.168.1.55")]
    with _patch_daemon([], routing, outputs=outputs):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    entity_id = ent_reg.async_get_entity_id("media_player", DOMAIN, f"{entry.entry_id}_out_ap2-dev-dusche")
    assert entity_id is not None
    reg_entry = ent_reg.async_get(entity_id)
    assert reg_entry.device_id == device.id
    state = hass.states.get(entity_id)
    assert state is not None
    assert state.attributes["friendly_name"] == "Dusche Audio Routing"
    assert dev_reg.async_get(device.id).area_id == area.id


async def test_ap2_device_without_ip_match_keeps_derived_name(hass):
    """No HA device at the receiver's IP → standalone entity with the daemon-
    derived name and no device link (user can set the area manually)."""
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[RoutingNode(node_id=None, node_name="ap2-dev-dusche", display_name="Dusche", configured=False)],
        links=[],
    )
    outputs = [OutputMeta(node_name="ap2-dev-dusche", kind="airplay2", ip="192.168.1.55")]
    with _patch_daemon([], routing, outputs=outputs):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    reg_entry = er.async_get(hass).async_get("media_player.dusche")
    assert reg_entry is not None and reg_entry.device_id is None
    assert hass.states.get("media_player.dusche").attributes["friendly_name"] == "Dusche"


async def test_ap2_group_membership_attribute(hass):
    """Two AirPlay-2 receivers routed from the same source form a synchronized
    group; each exposes the group's members under `airplay2_group_members`
    (kept separate from sendspin's key)."""
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[RoutingNode(node_id=10, node_name="shairport-sync", display_name="shairport-sync")],
        outputs=[
            RoutingNode(node_id=None, node_name="ap2-dev-dusche", display_name="Dusche", configured=False),
            RoutingNode(node_id=None, node_name="ap2-dev-salon", display_name="Salon", configured=False),
        ],
        links=[("shairport-sync", "ap2-dev-dusche"), ("shairport-sync", "ap2-dev-salon")],
    )
    with _patch_daemon([], routing):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    dusche = hass.states.get("media_player.dusche")
    assert dusche is not None
    assert dusche.attributes.get("airplay2_group_members") == ["Dusche", "Salon"]
    assert "sendspin_group_members" not in dusche.attributes


async def test_set_volume_on_sendspin_device_uses_sendspin_api(hass):
    """A sendspin device has no PipeWire node volume — volume_set must go
    through the in-band per-device sendspin volume API (0.0-1.0 -> 0-100)."""
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[RoutingNode(node_id=None, node_name="sendspin-dev-bath", display_name="Bath", configured=False)],
        links=[],
    )
    with _patch_daemon([], routing):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with (
            patch(f"{API}.async_set_sendspin_volume", new=AsyncMock()) as mock_sendspin_vol,
            patch(f"{API}.async_set_volume", new=AsyncMock()) as mock_node_vol,
        ):
            await hass.services.async_call(
                "media_player",
                "volume_set",
                {"entity_id": "media_player.bath", "volume_level": 0.4},
                blocking=True,
            )
            mock_sendspin_vol.assert_awaited_once_with("sendspin-dev-bath", 40)
            mock_node_vol.assert_not_awaited()


async def test_sendspin_group_membership_attribute(hass):
    """Two sendspin devices routed from the same source form a synchronized
    group; each exposes the group's members as an attribute."""
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[RoutingNode(node_id=10, node_name="shairport-sync", display_name="shairport-sync")],
        outputs=[
            RoutingNode(node_id=None, node_name="sendspin-dev-bath", display_name="Bath", configured=False),
            RoutingNode(node_id=None, node_name="sendspin-dev-den", display_name="Den", configured=False),
        ],
        links=[("shairport-sync", "sendspin-dev-bath"), ("shairport-sync", "sendspin-dev-den")],
    )
    with _patch_daemon([], routing):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    bath = hass.states.get("media_player.bath")
    assert bath is not None
    assert bath.attributes.get("sendspin_group_members") == ["Bath", "Den"]


async def test_offline_output_is_unavailable(hass):
    """A configured output that goes offline stays in the matrix with
    present=False → the entity shows unavailable (not removed, not crashing)."""
    entry = _make_entry(hass)
    online = _routing_matrix([], outputs=[RoutingNode(node_id=None, node_name="ap2-dev-pioneer", display_name="Pioneer", present=True)])
    with _patch_daemon([], online):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    coordinator = hass.data[DOMAIN][entry.entry_id]
    coordinator._apply_routing(
        _routing_matrix([], outputs=[RoutingNode(node_id=None, node_name="ap2-dev-pioneer", display_name="Pioneer", present=False)])
    )
    await hass.async_block_till_done()

    state = hass.states.get("media_player.pioneer")
    assert state is not None and state.state == "unavailable"


async def test_output_gone_from_matrix_is_removed(hass):
    """An output that leaves the matrix entirely (a discovered device that's
    gone) has its entity removed rather than lingering as unavailable."""
    entry = _make_entry(hass)
    with _patch_daemon([], _routing_matrix([], outputs=[RoutingNode(node_id=None, node_name="sendspin-dev-bath", display_name="Bath", configured=False)])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
    assert hass.states.get("media_player.bath") is not None

    coordinator = hass.data[DOMAIN][entry.entry_id]
    coordinator._apply_routing(_routing_matrix([], outputs=[]))
    await hass.async_block_till_done()

    assert hass.states.get("media_player.bath") is None


# ---- Routing: source selection & link/unlink services --------------------


async def test_source_list_and_current_source_from_routing(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "ap2-dev-kitchen")])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    state = hass.states.get("media_player.kitchen")
    assert state is not None
    assert state.attributes["source_list"] == ["None", "shairport-sync", "bt-bridge"]
    assert state.attributes["source"] == "shairport-sync"


async def test_current_source_is_none_when_nothing_linked(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    assert hass.states.get("media_player.kitchen").attributes["source"] == "None"


async def test_select_source_unlinks_previous_then_links_new(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "ap2-dev-kitchen")])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with (
            patch(f"{API}.async_link", new=AsyncMock()) as mock_link,
            patch(f"{API}.async_unlink", new=AsyncMock()) as mock_unlink,
        ):
            await hass.services.async_call(
                "media_player",
                "select_source",
                {"entity_id": "media_player.kitchen", "source": "bt-bridge"},
                blocking=True,
            )
            mock_unlink.assert_awaited_once_with("shairport-sync", "ap2-dev-kitchen")
            mock_link.assert_awaited_once_with("bt-bridge", "ap2-dev-kitchen")


async def test_select_source_none_unlinks_without_linking(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "ap2-dev-kitchen")])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with (
            patch(f"{API}.async_link", new=AsyncMock()) as mock_link,
            patch(f"{API}.async_unlink", new=AsyncMock()) as mock_unlink,
        ):
            await hass.services.async_call(
                "media_player",
                "select_source",
                {"entity_id": "media_player.kitchen", "source": "None"},
                blocking=True,
            )
            mock_unlink.assert_awaited_once_with("shairport-sync", "ap2-dev-kitchen")
            mock_link.assert_not_awaited()


async def test_select_source_already_selected_is_noop(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "ap2-dev-kitchen")])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with (
            patch(f"{API}.async_link", new=AsyncMock()) as mock_link,
            patch(f"{API}.async_unlink", new=AsyncMock()) as mock_unlink,
        ):
            await hass.services.async_call(
                "media_player",
                "select_source",
                {"entity_id": "media_player.kitchen", "source": "shairport-sync"},
                blocking=True,
            )
            mock_unlink.assert_not_awaited()
            mock_link.assert_not_awaited()


async def test_select_unknown_source_raises(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "ap2-dev-kitchen")])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with pytest.raises(HomeAssistantError):
            await hass.services.async_call(
                "media_player",
                "select_source",
                {"entity_id": "media_player.kitchen", "source": "does-not-exist"},
                blocking=True,
            )


async def test_link_service_is_additive(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "ap2-dev-kitchen")])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with (
            patch(f"{API}.async_link", new=AsyncMock()) as mock_link,
            patch(f"{API}.async_unlink", new=AsyncMock()) as mock_unlink,
        ):
            await hass.services.async_call(
                DOMAIN,
                "link",
                {"entity_id": "media_player.kitchen", "source": "bt-bridge"},
                blocking=True,
            )
            mock_link.assert_awaited_once_with("bt-bridge", "ap2-dev-kitchen")
            mock_unlink.assert_not_awaited()


async def test_unlink_service_named_source(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "ap2-dev-kitchen")])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with patch(f"{API}.async_unlink", new=AsyncMock()) as mock_unlink:
            await hass.services.async_call(
                DOMAIN,
                "unlink",
                {"entity_id": "media_player.kitchen", "source": "shairport-sync"},
                blocking=True,
            )
            mock_unlink.assert_awaited_once_with("shairport-sync", "ap2-dev-kitchen")


async def test_unlink_service_without_source_drops_all(hass):
    entry = _make_entry(hass)
    with _patch_daemon(
        _routing_players(),
        _routing_matrix([("shairport-sync", "ap2-dev-kitchen"), ("bt-bridge", "ap2-dev-kitchen")]),
    ):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with patch(f"{API}.async_unlink", new=AsyncMock()) as mock_unlink:
            await hass.services.async_call(
                DOMAIN,
                "unlink",
                {"entity_id": "media_player.kitchen"},
                blocking=True,
            )
            assert mock_unlink.await_count == 2
            unlinked = {call.args for call in mock_unlink.await_args_list}
            assert unlinked == {("shairport-sync", "ap2-dev-kitchen"), ("bt-bridge", "ap2-dev-kitchen")}


# ---- cleanup_entities service --------------------------------------------


async def test_cleanup_entities_service_removes_stale_registry_entries(hass):
    """The domain service deletes media_player registry entries whose output
    the daemon no longer reports, keeping the ones still in the matrix."""
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        registry = er.async_get(hass)
        # A leftover entity from a previous run: registered, no longer in the matrix.
        ghost = registry.async_get_or_create(
            "media_player",
            DOMAIN,
            f"{entry.entry_id}_ap2-dev-ghost",
            config_entry=entry,
            suggested_object_id="ghost",
        )
        assert registry.async_get(ghost.entity_id) is not None

        await hass.services.async_call(DOMAIN, "cleanup_entities", {}, blocking=True)
        await hass.async_block_till_done()

        # Ghost purged; the live kitchen entity kept.
        assert registry.async_get(ghost.entity_id) is None
        assert hass.states.get("media_player.kitchen") is not None


# ---- Routing over the WebSocket (push, not poll) -------------------------


async def test_routing_push_updates_source_live(hass):
    """A matrix pushed over the WebSocket re-renders the entity's `source`
    immediately, without waiting for a poll."""
    entry = _make_entry(hass)
    # Seed with the kitchen output present but nothing linked, so the entity
    # exists at setup; the push then adds a link.
    with _patch_daemon(_routing_players(), _routing_matrix([])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    assert hass.states.get("media_player.kitchen").attributes["source"] == "None"

    coordinator = hass.data[DOMAIN][entry.entry_id]
    coordinator._apply_routing(_routing_matrix([("shairport-sync", "ap2-dev-kitchen")]))
    await hass.async_block_till_done()

    assert hass.states.get("media_player.kitchen").attributes["source"] == "shairport-sync"


class _FakeWSMessage:
    def __init__(self, type_, data=""):
        self.type = type_
        self._data = data

    def json(self):
        return json.loads(self._data)


class _FakeWS:
    """Stands in for aiohttp's ws_connect context manager + message stream."""

    def __init__(self, messages):
        self._messages = messages

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        return False

    def __aiter__(self):
        self._it = iter(self._messages)
        return self

    async def __anext__(self):
        try:
            return next(self._it)
        except StopIteration:
            raise StopAsyncIteration


class _FakeSession:
    def __init__(self, ws):
        self._ws = ws

    def ws_connect(self, url, heartbeat=None):
        return self._ws


async def test_async_routing_ws_messages_parses_pushes():
    """The WS client yields a parsed RoutingMatrix per TEXT frame and stops
    cleanly when the socket closes."""
    matrix_json = json.dumps(
        {
            "sources": [{"node_id": 10, "node_name": "shairport-sync", "display_name": "shairport-sync", "present": True, "configured": True}],
            "outputs": [{"node_id": 50, "node_name": "ap2-dev-kitchen", "display_name": "Kitchen", "present": True, "configured": True}],
            "links": [{"source": "shairport-sync", "output": "ap2-dev-kitchen"}],
        }
    )
    ws = _FakeWS(
        [
            _FakeWSMessage(aiohttp.WSMsgType.TEXT, matrix_json),
            _FakeWSMessage(aiohttp.WSMsgType.CLOSED),
        ]
    )
    client = PipewireRouterApiClient(_FakeSession(ws), "host", 8099)

    received = [m async for m in client.async_routing_ws_messages()]

    assert len(received) == 1
    assert received[0].links == [("shairport-sync", "ap2-dev-kitchen")]
    assert [s.display_name for s in received[0].sources] == ["shairport-sync"]
    assert [o.node_name for o in received[0].outputs] == ["ap2-dev-kitchen"]
