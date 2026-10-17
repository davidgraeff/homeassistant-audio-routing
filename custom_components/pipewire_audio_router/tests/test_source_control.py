"""A controllable source as a media_player (`ControlledSourceMediaPlayer`).

The YouTube Music receiver announces its control API to the daemon; the daemon lists it
on the source as `control`; this integration creates one media_player per such source
and talks to the producer directly (`source_control.py`). Here the daemon client and the
producer client are mocked — Home Assistant itself (entity platform, service dispatch,
the media-browser websocket) is real.

The state bodies below are what the receiver's `GET /api/state` actually answers
(firmware/pi-ytmusic/receiver/control.js), so a change to that wire format shows up here.
"""

from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

import pytest
from homeassistant.components.media_player import MediaPlayerEntityFeature
from homeassistant.exceptions import HomeAssistantError
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.pipewire_audio_router.api import (
    AppSettings,
    DaemonStatus,
    PipewireRouterApiClient,
    Preset,
    PresetsInfo,
    RoutingMatrix,
    RoutingNode,
    RtpSourceState,
    SourceControl,
)
from custom_components.pipewire_audio_router.const import DOMAIN
from custom_components.pipewire_audio_router.source_control import (
    SourceControlError,
    parse_state,
    steady_position,
)

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
CTRL = "custom_components.pipewire_audio_router.source_control.SourceControlClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
ENTITY = "media_player.youtube_music"

NODE = "rtp-in-youtube-music"
SOURCE = RoutingNode(node_id=21, node_name=NODE, display_name="YouTube Music")
ROUTING = RoutingMatrix(sources=[SOURCE], outputs=[], links=[])
FEATURES = ["play", "pause", "stop", "next", "previous", "seek", "play_media", "browse_media", "queue"]
CONTROL = SourceControl(node_name=NODE, label="YouTube Music", url="http://127.0.0.1:8094", features=FEATURES)

STATE_BODY = {
    "protocol": 1,
    "name": "Musik (Home Assistant)",
    "rtp_port": 46002,
    "features": FEATURES,
    "state": "playing",
    "origin": "local",
    "cast": {"connected": False},
    "track": {
        "media_id": "ytmusic:track:fOT0BUpITw8",
        "video_id": "fOT0BUpITw8",
        "title": "BELLAKEO",
        "artist": "Peso Pluma & Anitta",
        "album": None,
        "duration": 235,
        "artwork_url": "https://i.ytimg.com/vi/fOT0BUpITw8/hq720.jpg",
        "playable": True,
        "position": 12.5,
        "position_at": 1_792_239_156_683,
    },
    "playlist": {
        "media_id": "ytmusic:playlist:PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
        "playlist_id": "PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
        "title": "Popular Music Videos",
    },
    "queue": {
        "index": 0,
        "length": 50,
        "more": True,
        "can_next": True,
        "can_previous": False,
        "items": [
            {
                "media_id": "ytmusic:queue:1",
                "video_id": "NFvDHYMzj9U",
                "title": "FN8",
                "artist": "Arcangel",
                "album": None,
                "duration": 650,
                "artwork_url": "https://i.ytimg.com/vi/NFvDHYMzj9U/hq720.jpg",
                "index": 1,
                "current": False,
                "playable": True,
            }
        ],
    },
    "catalogue": True,
    "error": None,
}

BROWSE_ROOT = {
    "id": "ytmusic:root",
    "title": "YouTube Music",
    "kind": "directory",
    "playable": False,
    "expandable": True,
    "thumbnail": None,
    "children": [
        {"id": "ytmusic:queue", "title": "Warteschlange", "kind": "directory", "playable": False, "expandable": True},
        {"id": "ytmusic:recent", "title": "Zuletzt gespielt", "kind": "directory", "playable": False, "expandable": True},
    ],
}
BROWSE_RECENT = {
    "id": "ytmusic:recent",
    "title": "Zuletzt gespielt",
    "kind": "directory",
    "playable": False,
    "expandable": True,
    "children": [
        {
            "id": "ytmusic:playlist:RDAMVMdQw4w9WgXcQ",
            "title": "Never Gonna Give You Up Mix",
            "kind": "mix",
            "playable": True,
            "expandable": True,
            "thumbnail": "https://i.ytimg.com/vi/dQw4w9WgXcQ/hq720.jpg",
            "played_at": 1_792_239_000_000,
        },
        {
            "id": "ytmusic:playlist:OLAK5uy_album",
            "title": "Whenever You Need Somebody",
            "kind": "album",
            "playable": True,
            "expandable": True,
        },
    ],
}


def _patch_daemon(controls=(CONTROL,)):
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_health", new=AsyncMock(return_value=None)))
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=ROUTING)))
    stack.enter_context(
        patch(
            f"{API}.async_get_rtp_source",
            new=AsyncMock(return_value=RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)),
        )
    )
    stack.enter_context(patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value={})))
    stack.enter_context(patch(f"{API}.async_get_outputs", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_music_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_announcement_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(
        patch(
            f"{API}.async_get_presets",
            new=AsyncMock(return_value=PresetsInfo(active="default", presets=[Preset(id="default", name="Default")])),
        )
    )
    stack.enter_context(patch(f"{API}.async_get_status", new=AsyncMock(return_value=DaemonStatus(version="0.4.0"))))
    stack.enter_context(patch(f"{API}.async_get_agents", new=AsyncMock(return_value=[])))
    stack.enter_context(
        patch(f"{API}.async_get_settings", new=AsyncMock(return_value=AppSettings(expose_outputs_as_media_players=False)))
    )
    stack.enter_context(patch(f"{API}.async_get_source_controls", new=AsyncMock(return_value=list(controls))))
    stack.enter_context(patch(f"{COORD}.async_events_ws_loop", new=AsyncMock()))
    return stack


@pytest.fixture(autouse=True)
def no_source_controls():
    """Overrides conftest's default: this module patches the announcement list itself,
    and one test exercises the real client method."""
    yield


@pytest.fixture
def producer():
    """The receiver's control API, mocked: one AsyncMock per call, state from STATE_BODY."""
    with (
        patch(f"{CTRL}.async_state", new=AsyncMock(side_effect=lambda *a, **k: parse_state(STATE_BODY))) as state,
        patch(f"{CTRL}.async_command", new=AsyncMock(return_value=True)) as command,
        patch(f"{CTRL}.async_play", new=AsyncMock(return_value={"title": "T"})) as play,
        patch(f"{CTRL}.async_browse", new=AsyncMock(return_value=BROWSE_ROOT)) as browse,
    ):
        yield type("Producer", (), {"state": state, "command": command, "play": play, "browse": browse})


async def _setup(hass, controls=(CONTROL,)):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8099})
    entry.add_to_hass(hass)
    stack = _patch_daemon(controls)
    stack.__enter__()
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    return entry, stack


async def test_a_controllable_source_gets_a_media_player_with_its_track_and_queue(hass, producer):
    _, stack = await _setup(hass)
    with stack:
        state = hass.states.get(ENTITY)
        assert state is not None, "named after the source: YouTube Music → media_player.youtube_music"
        assert state.state == "playing"
        a = state.attributes
        assert a["media_title"] == "BELLAKEO"
        assert a["media_artist"] == "Peso Pluma & Anitta"
        assert a["media_duration"] == 235
        assert a["media_position"] == 12
        assert a["media_content_id"] == "ytmusic:track:fOT0BUpITw8"
        assert a["media_playlist"] == "Popular Music Videos"
        assert "entity_picture" in a, "art is proxied through Home Assistant"
        assert a["queue"] == [
            {
                "media_content_id": "ytmusic:queue:1",
                "title": "FN8",
                "artist": "Arcangel",
                "album": None,
                "duration": 650,
                "image": "https://i.ytimg.com/vi/NFvDHYMzj9U/hq720.jpg",
                "playable": True,
            }
        ]
        assert a["queue_position"] == 0
        assert a["queue_size"] == 50
        assert a["playback_origin"] == "local"
        assert a["playlist_media_id"] == "ytmusic:playlist:PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI"
        assert a["cast_connected"] is False
        features = MediaPlayerEntityFeature(a["supported_features"])
        for f in (
            MediaPlayerEntityFeature.PLAY,
            MediaPlayerEntityFeature.PAUSE,
            MediaPlayerEntityFeature.STOP,
            MediaPlayerEntityFeature.NEXT_TRACK,
            MediaPlayerEntityFeature.PREVIOUS_TRACK,
            MediaPlayerEntityFeature.SEEK,
            MediaPlayerEntityFeature.PLAY_MEDIA,
            MediaPlayerEntityFeature.BROWSE_MEDIA,
        ):
            assert f in features, f
        assert MediaPlayerEntityFeature.VOLUME_SET not in features


@pytest.mark.parametrize(
    ("service", "data", "command", "extra"),
    [
        ("media_play", {}, "play", {}),
        ("media_pause", {}, "pause", {}),
        ("media_stop", {}, "stop", {}),
        ("media_next_track", {}, "next", {}),
        ("media_previous_track", {}, "previous", {}),
        ("media_seek", {"seek_position": 42}, "seek", {"position": 42}),
    ],
)
async def test_transport_services_command_the_producer(hass, producer, service, data, command, extra):
    _, stack = await _setup(hass)
    with stack:
        reads_before = producer.state.await_count
        await hass.services.async_call("media_player", service, {"entity_id": ENTITY, **data}, blocking=True)
        producer.command.assert_awaited_once_with(command, **extra)
        # Re-read at once, so the new state shows without waiting for the next poll.
        assert producer.state.await_count == reads_before + 1


async def test_play_media_hands_the_producer_its_own_id(hass, producer):
    _, stack = await _setup(hass)
    with stack:
        await hass.services.async_call(
            "media_player",
            "play_media",
            {"entity_id": ENTITY, "media_content_id": "ytmusic:playlist:RDAMVMdQw4w9WgXcQ", "media_content_type": "playlist"},
            blocking=True,
        )
        producer.play.assert_awaited_once_with("ytmusic:playlist:RDAMVMdQw4w9WgXcQ", "playlist")


async def test_a_producer_refusal_is_the_users_error_message(hass, producer):
    producer.play.side_effect = SourceControlError("YouTube Music returned an empty queue for PLx")
    _, stack = await _setup(hass)
    with stack, pytest.raises(HomeAssistantError, match="empty queue"):
        await hass.services.async_call(
            "media_player",
            "play_media",
            {"entity_id": ENTITY, "media_content_id": "ytmusic:playlist:PLx", "media_content_type": "playlist"},
            blocking=True,
        )


async def test_media_source_items_are_refused(hass, producer):
    _, stack = await _setup(hass)
    with stack:
        with pytest.raises(HomeAssistantError, match="own service"):
            await hass.services.async_call(
                "media_player",
                "play_media",
                {"entity_id": ENTITY, "media_content_id": "media-source://tts/x", "media_content_type": "music"},
                blocking=True,
            )
        producer.play.assert_not_awaited()


async def test_browse_maps_the_producers_tree(hass, hass_ws_client, producer):
    _, stack = await _setup(hass)
    with stack:
        client = await hass_ws_client(hass)
        await client.send_json({"id": 1, "type": "media_player/browse_media", "entity_id": ENTITY})
        root = (await client.receive_json())["result"]
        assert root["title"] == "YouTube Music"
        assert root["media_class"] == "directory"
        assert [c["media_content_id"] for c in root["children"]] == ["ytmusic:queue", "ytmusic:recent"]
        assert [c["title"] for c in root["children"]] == ["Warteschlange", "Zuletzt gespielt"]
        # Home Assistant's language goes to the producer, which owns the words.
        producer.browse.assert_awaited_with(None, hass.config.language)

        producer.browse.return_value = BROWSE_RECENT
        await client.send_json(
            {
                "id": 2,
                "type": "media_player/browse_media",
                "entity_id": ENTITY,
                "media_content_id": "ytmusic:recent",
                "media_content_type": "directory",
            }
        )
        recent = (await client.receive_json())["result"]
        assert [(c["media_content_id"], c["media_class"], c["can_play"], c["can_expand"]) for c in recent["children"]] == [
            ("ytmusic:playlist:RDAMVMdQw4w9WgXcQ", "playlist", True, True),
            ("ytmusic:playlist:OLAK5uy_album", "album", True, True),
        ]
        assert recent["children"][0]["thumbnail"] == "https://i.ytimg.com/vi/dQw4w9WgXcQ/hq720.jpg"


async def test_no_announcement_no_entity(hass, producer):
    _, stack = await _setup(hass, controls=())
    with stack:
        assert hass.states.get(ENTITY) is None


async def test_unavailable_while_the_producer_does_not_answer(hass, producer):
    entry, stack = await _setup(hass)
    with stack:
        producer.state.side_effect = SourceControlError("could not reach")
        await hass.data[DOMAIN][entry.entry_id].async_refresh()
        await hass.async_block_till_done()
        assert hass.states.get(ENTITY).state == "unavailable"
        # It comes back on its own once the producer answers again.
        producer.state.side_effect = lambda *a, **k: parse_state(STATE_BODY)
        await hass.data[DOMAIN][entry.entry_id].async_refresh()
        await hass.async_block_till_done()
        assert hass.states.get(ENTITY).state == "playing"


async def test_unavailable_while_the_router_is_unreachable(hass, producer):
    """The router's poll is what refreshes the producer's state, so without it the last
    read would be shown as if it were current."""
    from custom_components.pipewire_audio_router.api import PipewireRouterApiError

    entry, stack = await _setup(hass)
    with stack, patch(f"{API}.async_health", new=AsyncMock(side_effect=PipewireRouterApiError("down"))):
        await hass.data[DOMAIN][entry.entry_id].async_refresh()
        await hass.async_block_till_done()
        assert hass.states.get(ENTITY).state == "unavailable"


async def test_a_lapsed_announcement_keeps_the_entity_but_unavailable(hass, producer):
    entry, stack = await _setup(hass)
    with stack, patch(f"{API}.async_get_source_controls", new=AsyncMock(return_value=[])):
        await hass.data[DOMAIN][entry.entry_id].async_refresh()
        await hass.async_block_till_done()
        assert hass.states.get(ENTITY).state == "unavailable"


async def test_an_unknown_protocol_is_not_driven(hass, producer):
    newer = SourceControl(node_name=NODE, label="YouTube Music", url="http://127.0.0.1:8094", features=FEATURES, protocol=2)
    _, stack = await _setup(hass, controls=(newer,))
    with stack:
        assert hass.states.get(ENTITY) is None
        producer.state.assert_not_awaited()


# ---- the wire format, without Home Assistant ------------------------------------------


def test_parse_state_reads_the_receivers_body():
    s = parse_state(STATE_BODY)
    assert (s.state, s.origin, s.queue_index, s.queue_length, s.can_next, s.can_previous) == (
        "playing",
        "local",
        0,
        50,
        True,
        False,
    )
    assert s.track.title == "BELLAKEO"
    assert s.track.image_url.endswith("hq720.jpg")
    assert [t.media_id for t in s.queue] == ["ytmusic:queue:1"]
    # An idle receiver answers almost nothing, and that must still parse.
    idle = parse_state({"state": "idle", "queue": {"items": []}})
    assert idle.track is None and idle.queue == [] and idle.queue_length == 0


def test_a_running_position_keeps_its_stamp_and_a_seek_does_not():
    first = parse_state(STATE_BODY)
    later = parse_state(
        {**STATE_BODY, "track": {**STATE_BODY["track"], "position": 17.6, "position_at": STATE_BODY["track"]["position_at"] + 5000}}
    )
    steady = steady_position(first, later)
    assert (steady.track.position, steady.track.position_at) == (12.5, STATE_BODY["track"]["position_at"])
    jumped = parse_state(
        {**STATE_BODY, "track": {**STATE_BODY["track"], "position": 90.0, "position_at": STATE_BODY["track"]["position_at"] + 5000}}
    )
    assert steady_position(first, jumped).track.position == 90.0


async def test_the_api_client_lists_only_sources_with_a_control():
    class _Resp:
        status = 200

        async def __aenter__(self):
            return self

        async def __aexit__(self, *exc):
            return False

        async def json(self):
            return {
                "sources": [
                    {"id": "bt-bridge-rtp", "label": "Bluetooth Bridge", "kind": "rtp", "node_name": "bt-bridge-rtp", "control": None},
                    {
                        "id": "youtube-music",
                        "label": "YouTube Music",
                        "kind": "rtp",
                        "node_name": NODE,
                        "control": {"url": "http://127.0.0.1:8094/", "features": ["pause", "next"], "protocol": 1},
                    },
                ]
            }

        def raise_for_status(self):
            return None

    class _Session:
        def get(self, url):
            return _Resp()

    controls = await PipewireRouterApiClient(_Session(), "h", 8099).async_get_source_controls()
    assert controls == [
        SourceControl(node_name=NODE, label="YouTube Music", url="http://127.0.0.1:8094", features=["pause", "next"], protocol=1)
    ]
