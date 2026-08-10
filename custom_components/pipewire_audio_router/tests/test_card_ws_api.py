"""Tests for the dashboard card's WebSocket API (ws_api.py).

This is the card's only channel: it never talks to the daemon directly, so the
snapshot shape, the push-on-change behaviour and the four routing commands are
the contract. The daemon client is mocked — what's under test is the proxy, not
the REST calls it forwards to.
"""

import asyncio
from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

import pytest
from homeassistant.setup import async_setup_component
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.pipewire_audio_router.api import (
    AppSettings,
    MusicGroup,
    PipewireRouterApiError,
    RoutingMatrix,
    RoutingNode,
    RtpSourceState,
)
from custom_components.pipewire_audio_router.const import DOMAIN

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)

KITCHEN = RoutingNode(node_id=None, node_name="ap2-dev-kitchen", display_name="Kitchen")
BATH = RoutingNode(node_id=None, node_name="sendspin-dev-bath", display_name="Bath")
LOFT = RoutingNode(node_id=None, node_name="ap2-dev-loft", display_name="Loft", present=False)
AIRPLAY = RoutingNode(node_id=10, node_name="shairport-sync", display_name="AirPlay")
BLUETOOTH = RoutingNode(node_id=11, node_name="bt-bridge", display_name="Bluetooth")

GROUND_FLOOR = MusicGroup(id="g1", name="Ground floor", members=["ap2-dev-kitchen", "ap2-dev-loft"])


def _matrix(links=(), sources=(AIRPLAY, BLUETOOTH), outputs=(KITCHEN, BATH, LOFT)) -> RoutingMatrix:
    return RoutingMatrix(sources=list(sources), outputs=list(outputs), links=list(links))


def _patch_daemon(routing: RoutingMatrix, groups=()):
    """Offline setup: every polled endpoint mocked, and the daemon's own routing
    socket stubbed out so the only pushes are the ones a test makes."""
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_health", new=AsyncMock(return_value=None)))
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=routing)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=RTP_DISABLED)))
    stack.enter_context(patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value={})))
    stack.enter_context(patch(f"{API}.async_get_outputs", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_music_groups", new=AsyncMock(return_value=list(groups))))
    stack.enter_context(patch(f"{API}.async_get_announcement_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(
        patch(
            f"{API}.async_get_settings",
            new=AsyncMock(return_value=AppSettings(expose_outputs_as_media_players=False)),
        )
    )
    stack.enter_context(patch(f"{COORD}.async_routing_ws_loop", new=AsyncMock()))
    return stack


async def _setup(hass, routing: RoutingMatrix, groups=(), *, host="127.0.0.1"):
    """Load one config entry and return its coordinator."""
    entry = MockConfigEntry(domain=DOMAIN, data={"host": host, "port": 8080})
    entry.add_to_hass(hass)
    with _patch_daemon(routing, groups):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
    return entry, hass.data[DOMAIN][entry.entry_id]


async def _subscribe(client, **extra):
    """Subscribe and return the first snapshot."""
    await client.send_json({"id": 1, "type": f"{DOMAIN}/subscribe", **extra})
    result = await client.receive_json()
    assert result["success"], result
    event = await client.receive_json()
    assert event["type"] == "event"
    return event["event"]


async def test_snapshot_shape(hass, hass_ws_client):
    """Inputs, outputs, links and music groups — by stable node name, with
    `present` carried so the card can gray an absent endpoint."""
    await _setup(
        hass,
        _matrix(links=[("shairport-sync", "ap2-dev-kitchen")]),
        groups=[GROUND_FLOOR],
    )
    snapshot = await _subscribe(await hass_ws_client(hass))

    assert snapshot["sources"] == [
        {"node_name": "shairport-sync", "display_name": "AirPlay", "present": True},
        {"node_name": "bt-bridge", "display_name": "Bluetooth", "present": True},
    ]
    # `present: False` for the configured-but-absent one; no volumes, meters or
    # node ids anywhere — the card shows none of them.
    assert snapshot["outputs"] == [
        {"node_name": "ap2-dev-kitchen", "display_name": "Kitchen", "present": True},
        {"node_name": "sendspin-dev-bath", "display_name": "Bath", "present": True},
        {"node_name": "ap2-dev-loft", "display_name": "Loft", "present": False},
    ]
    assert snapshot["links"] == [{"source": "shairport-sync", "output": "ap2-dev-kitchen"}]
    assert snapshot["groups"] == [
        {"id": "g1", "name": "Ground floor", "members": ["ap2-dev-kitchen", "ap2-dev-loft"]}
    ]


async def test_push_on_matrix_change(hass, hass_ws_client):
    """A matrix pushed by the daemon reaches the card without it asking."""
    _entry, coordinator = await _setup(hass, _matrix())
    client = await hass_ws_client(hass)
    assert (await _subscribe(client))["links"] == []  # the state on connect

    coordinator._apply_routing(_matrix(links=[("bt-bridge", "sendspin-dev-bath")]))
    event = await client.receive_json()
    assert event["type"] == "event"
    assert event["event"]["links"] == [{"source": "bt-bridge", "output": "sendspin-dev-bath"}]


async def test_unchanged_state_sends_nothing(hass, hass_ws_client):
    """The coordinator notifies its listeners on every poll; only real changes may
    reach the card, or every open dashboard wakes up every few seconds for nothing."""
    _entry, coordinator = await _setup(hass, _matrix())
    client = await hass_ws_client(hass)
    await _subscribe(client)

    coordinator.async_update_listeners()  # a poll that changed nothing
    coordinator._apply_routing(_matrix())  # the same matrix again
    with pytest.raises(asyncio.TimeoutError):
        async with asyncio.timeout(0.2):
            await client.receive_json()


async def test_link_and_unlink(hass, hass_ws_client):
    """Lone-output routing is additive link/unlink, by node name."""
    _entry, coordinator = await _setup(hass, _matrix())
    client = await hass_ws_client(hass)
    coordinator.client.async_link = AsyncMock()
    coordinator.client.async_unlink = AsyncMock()

    await client.send_json(
        {"id": 5, "type": f"{DOMAIN}/link", "source": "bt-bridge", "output": "sendspin-dev-bath"}
    )
    assert (await client.receive_json())["success"]
    coordinator.client.async_link.assert_awaited_once_with("bt-bridge", "sendspin-dev-bath")

    await client.send_json(
        {"id": 6, "type": f"{DOMAIN}/unlink", "source": "bt-bridge", "output": "sendspin-dev-bath"}
    )
    assert (await client.receive_json())["success"]
    coordinator.client.async_unlink.assert_awaited_once_with("bt-bridge", "sendspin-dev-bath")


async def test_group_route_is_exclusive_call(hass, hass_ws_client):
    """A group goes through the group endpoint — the same reconciling call as its
    `select_source`, which is what keeps a group from ending up mixed."""
    _entry, coordinator = await _setup(hass, _matrix(), groups=[GROUND_FLOOR])
    client = await hass_ws_client(hass)
    coordinator.client.async_route_music_group = AsyncMock()
    coordinator.client.async_unroute_music_group = AsyncMock()

    await client.send_json(
        {"id": 7, "type": f"{DOMAIN}/route_group", "group_id": "g1", "source": "shairport-sync"}
    )
    assert (await client.receive_json())["success"]
    coordinator.client.async_route_music_group.assert_awaited_once_with("g1", "shairport-sync")

    await client.send_json({"id": 8, "type": f"{DOMAIN}/unroute_group", "group_id": "g1"})
    assert (await client.receive_json())["success"]
    coordinator.client.async_unroute_music_group.assert_awaited_once_with("g1")


async def test_daemon_failure_is_reported(hass, hass_ws_client):
    """A refused route answers with an error the card can show, rather than
    succeeding and leaving the picture wrong until the next push."""
    _entry, coordinator = await _setup(hass, _matrix())
    client = await hass_ws_client(hass)
    coordinator.client.async_link = AsyncMock(side_effect=PipewireRouterApiError("no matching channel ports"))

    await client.send_json(
        {"id": 9, "type": f"{DOMAIN}/link", "source": "bt-bridge", "output": "ap2-dev-kitchen"}
    )
    msg = await client.receive_json()
    assert not msg["success"]
    assert msg["error"]["code"] == "daemon_error"
    assert "no matching channel ports" in msg["error"]["message"]


async def test_no_entry_loaded(hass, hass_ws_client):
    """The commands exist as soon as the component does, so the card gets a real
    message instead of `unknown_command` when nothing is configured."""
    assert await async_setup_component(hass, DOMAIN, {})
    client = await hass_ws_client(hass)
    await client.send_json({"id": 1, "type": f"{DOMAIN}/subscribe"})
    msg = await client.receive_json()
    assert not msg["success"]
    assert msg["error"]["code"] == "no_entry"


async def test_two_routers_need_entry_id(hass, hass_ws_client):
    """With several daemons the card must say which one — guessing would reroute
    the wrong house."""
    _first, first = await _setup(hass, _matrix(), host="10.0.0.1")
    second_entry, _second = await _setup(hass, _matrix(links=[("bt-bridge", "ap2-dev-kitchen")]), host="10.0.0.2")
    client = await hass_ws_client(hass)

    await client.send_json({"id": 1, "type": f"{DOMAIN}/subscribe"})
    msg = await client.receive_json()
    assert not msg["success"]
    assert msg["error"]["code"] == "ambiguous_entry"

    # Named explicitly, it subscribes to that one.
    await client.send_json({"id": 2, "type": f"{DOMAIN}/subscribe", "entry_id": second_entry.entry_id})
    assert (await client.receive_json())["success"]
    event = await client.receive_json()
    assert event["event"]["links"] == [{"source": "bt-bridge", "output": "ap2-dev-kitchen"}]
    assert first.routing.links == []


async def test_unknown_entry_id(hass, hass_ws_client):
    await _setup(hass, _matrix())
    client = await hass_ws_client(hass)
    await client.send_json({"id": 1, "type": f"{DOMAIN}/subscribe", "entry_id": "nope"})
    msg = await client.receive_json()
    assert not msg["success"]
    assert msg["error"]["code"] == "no_entry"
