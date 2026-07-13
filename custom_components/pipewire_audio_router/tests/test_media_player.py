"""Real integration-setup tests: loads the actual config entry through
hass.config_entries.async_setup, not just instantiating the entity class
directly, so the coordinator/platform-forwarding wiring is exercised too."""

import json
from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

import aiohttp
import pytest
from homeassistant.exceptions import HomeAssistantError
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.pipewire_audio_router.api import (
    MediaPlayerState,
    PipewireRouterApiClient,
    RoutingMatrix,
    RoutingNode,
    RtpSourceState,
)
from custom_components.pipewire_audio_router.const import DOMAIN

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
EMPTY_ROUTING = RoutingMatrix(sources=[], outputs=[], links=[])
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, loaded=False)


def _make_entry(hass):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8080})
    entry.add_to_hass(hass)
    return entry


def _patch_daemon(players, routing=EMPTY_ROUTING, rtp=RTP_DISABLED):
    """Keep setup/refresh fully offline: mock the polled players + RTP-source
    fetches and the one-shot routing seed, and stub out the routing WebSocket
    loop so no real socket is opened (routing is driven directly in the WS
    tests)."""
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_get_media_players", new=AsyncMock(return_value=players)))
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=routing)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=rtp)))
    stack.enter_context(patch(f"{COORD}.async_routing_ws_loop", new=AsyncMock()))
    return stack


# One kitchen output fed by shairport-sync, with bt-bridge available as a
# second selectable source — the shared fixture for the routing tests.
def _routing_players():
    return [MediaPlayerState(node_id=50, node_name="sendspin-out-kitchen", state="playing", volume=1.0)]


def _routing_matrix(links):
    return RoutingMatrix(
        sources=[
            RoutingNode(node_id=10, node_name="shairport-sync", display_name="shairport-sync"),
            RoutingNode(node_id=11, node_name="bt-bridge", display_name="bt-bridge"),
        ],
        outputs=[RoutingNode(node_id=50, node_name="sendspin-out-kitchen", display_name="kitchen")],
        links=links,
    )


async def test_entities_created_from_bridge_daemon_state(hass):
    entry = _make_entry(hass)
    fake_players = [
        MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0),
        MediaPlayerState(node_id=50, node_name="sendspin-out-kitchen", state="playing", volume=0.5),
    ]
    with _patch_daemon(fake_players):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    pioneer = hass.states.get("media_player.pioneer")
    kitchen = hass.states.get("media_player.kitchen")
    assert pioneer is not None
    assert kitchen is not None
    assert pioneer.state == "idle"
    assert kitchen.state == "playing"
    assert float(kitchen.attributes["volume_level"]) == 0.5


async def test_set_volume_calls_bridge_daemon_api(hass):
    entry = _make_entry(hass)
    fake_players = [MediaPlayerState(node_id=50, node_name="sendspin-out-kitchen", state="idle", volume=1.0)]
    with _patch_daemon(fake_players):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with patch(f"{API}.async_set_volume", new=AsyncMock()) as mock_set_volume:
            await hass.services.async_call(
                "media_player",
                "volume_set",
                {"entity_id": "media_player.kitchen", "volume_level": 0.3},
                blocking=True,
            )
            # node_id (50) is resolved live from the snapshot, not stored.
            mock_set_volume.assert_awaited_once_with(50, 0.3)


async def test_play_media_calls_announce_on_bridge_daemon(hass):
    """`play_media` (with or without `announce=True`) is this entity's
    only playback contract — it must always go through the bridge
    daemon's ducked `/announce` endpoint, per Section 5.6/media_player.py's
    docstring, since there is no other playback mode implemented."""
    entry = _make_entry(hass)
    fake_players = [MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0)]
    with _patch_daemon(fake_players):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with patch(f"{API}.async_announce", new=AsyncMock()) as mock_announce:
            await hass.services.async_call(
                "media_player",
                "play_media",
                {
                    "entity_id": "media_player.pioneer",
                    "media_content_type": "music",
                    "media_content_id": "http://example.local/tts.mp3",
                    "announce": True,
                },
                blocking=True,
            )
            mock_announce.assert_awaited_once_with(35, "http://example.local/tts.mp3")


async def test_play_media_with_wyoming_extra_calls_announce_wyoming(hass):
    """Section 5.6 v2 (Phase 3.5): a caller opts into the Wyoming path
    per call via `play_media`'s standard `extra` dict — additive, the
    v1 `async_announce` path must not be touched for this call."""
    entry = _make_entry(hass)
    fake_players = [MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0)]
    with _patch_daemon(fake_players):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with (
            patch(f"{API}.async_announce_wyoming", new=AsyncMock()) as mock_announce_wyoming,
            patch(f"{API}.async_announce", new=AsyncMock()) as mock_announce_url,
        ):
            await hass.services.async_call(
                "media_player",
                "play_media",
                {
                    "entity_id": "media_player.pioneer",
                    "media_content_type": "music",
                    "media_content_id": "",
                    "extra": {"wyoming": {"host": "127.0.0.1", "port": 10200, "text": "hello"}},
                },
                blocking=True,
            )
            mock_announce_wyoming.assert_awaited_once_with(35, host="127.0.0.1", text="hello", port=10200, voice=None)
            mock_announce_url.assert_not_awaited()


async def test_unknown_node_is_unavailable_not_crashing(hass):
    """An output the coordinator no longer reports (e.g. its RAOP module
    failed to load) must show as unavailable, not raise."""
    entry = _make_entry(hass)
    with _patch_daemon([MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0)]):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    with _patch_daemon([]):
        coordinator = hass.data[DOMAIN][entry.entry_id]
        await coordinator.async_refresh()
        await hass.async_block_till_done()

    state = hass.states.get("media_player.pioneer")
    assert state is not None
    assert state.state == "unavailable"


# ---- Routing: source selection & link/unlink services --------------------


async def test_source_list_and_current_source_from_routing(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([(10, 50)])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    state = hass.states.get("media_player.kitchen")
    assert state is not None
    # "None" is always offered first, then every source the daemon reports.
    assert state.attributes["source_list"] == ["None", "shairport-sync", "bt-bridge"]
    # shairport-sync (node 10) is the one linked into kitchen (node 50).
    assert state.attributes["source"] == "shairport-sync"


async def test_current_source_is_none_when_nothing_linked(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    assert hass.states.get("media_player.kitchen").attributes["source"] == "None"


async def test_select_source_unlinks_previous_then_links_new(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([(10, 50)])):
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
            # Exclusive swap: old source (10) dropped, new source (11) linked.
            mock_unlink.assert_awaited_once_with(10, 50)
            mock_link.assert_awaited_once_with(11, 50)


async def test_select_source_none_unlinks_without_linking(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([(10, 50)])):
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
            mock_unlink.assert_awaited_once_with(10, 50)
            mock_link.assert_not_awaited()


async def test_select_source_already_selected_is_noop(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([(10, 50)])):
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
    with _patch_daemon(_routing_players(), _routing_matrix([(10, 50)])):
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
    with _patch_daemon(_routing_players(), _routing_matrix([(10, 50)])):
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
            # Additive: bt-bridge added, shairport-sync left connected.
            mock_link.assert_awaited_once_with(11, 50)
            mock_unlink.assert_not_awaited()


async def test_unlink_service_named_source(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([(10, 50)])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with patch(f"{API}.async_unlink", new=AsyncMock()) as mock_unlink:
            await hass.services.async_call(
                DOMAIN,
                "unlink",
                {"entity_id": "media_player.kitchen", "source": "shairport-sync"},
                blocking=True,
            )
            mock_unlink.assert_awaited_once_with(10, 50)


async def test_unlink_service_without_source_drops_all(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([(10, 50), (11, 50)])):
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
            assert unlinked == {(10, 50), (11, 50)}


# ---- Routing over the WebSocket (push, not poll) -------------------------


async def test_routing_push_updates_source_live(hass):
    """A matrix pushed over the WebSocket (applied via `_apply_routing`)
    re-renders the entity's `source` immediately, without waiting for a
    poll."""
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), EMPTY_ROUTING):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    # Seeded empty at setup → nothing linked yet.
    assert hass.states.get("media_player.kitchen").attributes["source"] == "None"

    coordinator = hass.data[DOMAIN][entry.entry_id]
    coordinator._apply_routing(_routing_matrix([(10, 50)]))
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
            "sources": [{"node_id": 10, "node_name": "shairport-sync", "display_name": "shairport-sync"}],
            "outputs": [{"node_id": 50, "node_name": "sendspin-out-kitchen", "display_name": "kitchen"}],
            "links": [[10, 50]],
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
    assert received[0].links == [(10, 50)]
    assert [s.display_name for s in received[0].sources] == ["shairport-sync"]
    assert [o.node_name for o in received[0].outputs] == ["sendspin-out-kitchen"]
