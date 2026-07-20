"""Real integration-setup tests: loads the actual config entry through
hass.config_entries.async_setup, not just instantiating the entity class
directly, so the coordinator/platform-forwarding wiring is exercised too.

Entities are driven by the routing *matrix* outputs (RAOP `raop-out-*` sinks
and auto-discovered sendspin devices `sendspin-dev-*`), not the polled
media_players feed — so an output that leaves the matrix loses its entity,
while a configured-but-offline one (present=False) stays as `unavailable`."""

import json
from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

import aiohttp
import pytest
from homeassistant.components.media_source import PlayMedia
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import area_registry as ar, device_registry as dr, entity_registry as er
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
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)


def _make_entry(hass):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8080})
    entry.add_to_hass(hass)
    return entry


def _patch_daemon(players, routing=EMPTY_ROUTING, rtp=RTP_DISABLED, sendspin_volumes=None):
    """Keep setup/refresh fully offline: mock the polled players + RTP-source +
    sendspin-volume fetches and the one-shot routing seed, and stub out the
    routing WebSocket loop so no real socket is opened (routing is driven
    directly in the WS tests)."""
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_get_media_players", new=AsyncMock(return_value=players)))
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=routing)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=rtp)))
    stack.enter_context(
        patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value=sendspin_volumes or {}))
    )
    stack.enter_context(patch(f"{COORD}.async_routing_ws_loop", new=AsyncMock()))
    return stack


# One kitchen RAOP output fed by shairport-sync, with bt-bridge available as a
# second selectable source — the shared fixture for the routing tests.
def _routing_players():
    return [MediaPlayerState(node_id=50, node_name="raop-out-kitchen", state="playing", volume=1.0)]


def _routing_matrix(links, outputs=None):
    return RoutingMatrix(
        sources=[
            RoutingNode(node_id=10, node_name="shairport-sync", display_name="shairport-sync"),
            RoutingNode(node_id=11, node_name="bt-bridge", display_name="bt-bridge"),
        ],
        outputs=outputs
        if outputs is not None
        else [RoutingNode(node_id=50, node_name="raop-out-kitchen", display_name="Kitchen")],
        links=links,
    )


async def test_entities_created_from_matrix_including_sendspin(hass):
    entry = _make_entry(hass)
    routing = RoutingMatrix(
        sources=[],
        outputs=[
            RoutingNode(node_id=35, node_name="raop-out-pioneer", display_name="Pioneer"),
            # Virtual sendspin device: no node_id, not in the media_players feed.
            RoutingNode(node_id=None, node_name="sendspin-dev-bath", display_name="Bath", configured=False),
        ],
        links=[],
    )
    players = [MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0)]
    with _patch_daemon(players, routing, sendspin_volumes={"sendspin-dev-bath": 50}):
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
        "media_player", DOMAIN, f"{entry.entry_id}_sendspin-dev-home_assistant_voice_093ca8"
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


async def test_set_volume_calls_bridge_daemon_api(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([])):
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


async def test_play_media_calls_announce_on_bridge_daemon(hass):
    """`play_media` (with or without `announce=True`) is this entity's only
    playback contract — it must go through the daemon's ducked `/announce`."""
    entry = _make_entry(hass)
    routing = _routing_matrix([], outputs=[RoutingNode(node_id=35, node_name="raop-out-pioneer", display_name="Pioneer")])
    players = [MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0)]
    with _patch_daemon(players, routing):
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


async def test_play_media_resolves_media_source_uri(hass):
    """`tts.speak`/the media browser hand the entity a `media-source://` URI,
    not a URL. It must be resolved (and made absolute) before it reaches the
    daemon's `/announce` — otherwise TTS silently does nothing."""
    entry = _make_entry(hass)
    routing = _routing_matrix([], outputs=[RoutingNode(node_id=35, node_name="raop-out-pioneer", display_name="Pioneer")])
    players = [MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0)]
    with _patch_daemon(players, routing):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        resolved = PlayMedia(url="/api/tts_proxy/abc123.mp3", mime_type="audio/mpeg")
        with (
            patch(f"{API}.async_announce", new=AsyncMock()) as mock_announce,
            patch(
                "custom_components.pipewire_audio_router.media_player.media_source.async_resolve_media",
                new=AsyncMock(return_value=resolved),
            ) as mock_resolve,
        ):
            await hass.services.async_call(
                "media_player",
                "play_media",
                {
                    "entity_id": "media_player.pioneer",
                    "media_content_type": "music",
                    "media_content_id": "media-source://tts/-/message=hi",
                    "announce": True,
                },
                blocking=True,
            )
            mock_resolve.assert_awaited_once()
            # Resolved to a relative HA URL → announced as an absolute one the
            # bridge daemon (a separate host) can fetch.
            (node_id, url), _ = mock_announce.await_args
            assert node_id == 35
            assert url.endswith("/api/tts_proxy/abc123.mp3")
            assert url.startswith("http://")


async def test_play_media_with_wyoming_extra_calls_announce_wyoming(hass):
    """Section 5.6 v2: a caller opts into the Wyoming path per call via
    `play_media`'s standard `extra` dict — additive, v1 untouched."""
    entry = _make_entry(hass)
    routing = _routing_matrix([], outputs=[RoutingNode(node_id=35, node_name="raop-out-pioneer", display_name="Pioneer")])
    players = [MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0)]
    with _patch_daemon(players, routing):
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


async def test_offline_output_is_unavailable(hass):
    """A configured output that goes offline stays in the matrix with
    present=False → the entity shows unavailable (not removed, not crashing)."""
    entry = _make_entry(hass)
    online = _routing_matrix([], outputs=[RoutingNode(node_id=35, node_name="raop-out-pioneer", display_name="Pioneer", present=True)])
    with _patch_daemon([MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0)], online):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    coordinator = hass.data[DOMAIN][entry.entry_id]
    coordinator._apply_routing(
        _routing_matrix([], outputs=[RoutingNode(node_id=None, node_name="raop-out-pioneer", display_name="Pioneer", present=False)])
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
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "raop-out-kitchen")])):
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
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "raop-out-kitchen")])):
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
            mock_unlink.assert_awaited_once_with("shairport-sync", "raop-out-kitchen")
            mock_link.assert_awaited_once_with("bt-bridge", "raop-out-kitchen")


async def test_select_source_none_unlinks_without_linking(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "raop-out-kitchen")])):
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
            mock_unlink.assert_awaited_once_with("shairport-sync", "raop-out-kitchen")
            mock_link.assert_not_awaited()


async def test_select_source_already_selected_is_noop(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "raop-out-kitchen")])):
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
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "raop-out-kitchen")])):
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
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "raop-out-kitchen")])):
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
            mock_link.assert_awaited_once_with("bt-bridge", "raop-out-kitchen")
            mock_unlink.assert_not_awaited()


async def test_unlink_service_named_source(hass):
    entry = _make_entry(hass)
    with _patch_daemon(_routing_players(), _routing_matrix([("shairport-sync", "raop-out-kitchen")])):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

        with patch(f"{API}.async_unlink", new=AsyncMock()) as mock_unlink:
            await hass.services.async_call(
                DOMAIN,
                "unlink",
                {"entity_id": "media_player.kitchen", "source": "shairport-sync"},
                blocking=True,
            )
            mock_unlink.assert_awaited_once_with("shairport-sync", "raop-out-kitchen")


async def test_unlink_service_without_source_drops_all(hass):
    entry = _make_entry(hass)
    with _patch_daemon(
        _routing_players(),
        _routing_matrix([("shairport-sync", "raop-out-kitchen"), ("bt-bridge", "raop-out-kitchen")]),
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
            assert unlinked == {("shairport-sync", "raop-out-kitchen"), ("bt-bridge", "raop-out-kitchen")}


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
            f"{entry.entry_id}_raop-out-ghost",
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
    coordinator._apply_routing(_routing_matrix([("shairport-sync", "raop-out-kitchen")]))
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
            "outputs": [{"node_id": 50, "node_name": "raop-out-kitchen", "display_name": "Kitchen", "present": True, "configured": True}],
            "links": [{"source": "shairport-sync", "output": "raop-out-kitchen"}],
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
    assert received[0].links == [("shairport-sync", "raop-out-kitchen")]
    assert [s.display_name for s in received[0].sources] == ["shairport-sync"]
    assert [o.node_name for o in received[0].outputs] == ["raop-out-kitchen"]
