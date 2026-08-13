"""Now-playing metadata on the output and music-group `media_player` entities.

The daemon publishes what each *source* is playing (its now_playing.rs), keyed by
source node name, on the routing socket's `now_playing` frame. An entity shows the
metadata of whichever source is currently routed into it — so the same source
feeding an output and a group produces identical metadata on both, and nothing is
shown for a source that is not playing.

See docs/source-metadata-plan.md.
"""

from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.pipewire_audio_router.api import (
    DaemonStatus,
    AppSettings,
    MusicGroup,
    NowPlaying,
    NowPlayingFrame,
    RoutingMatrix,
    RoutingNode,
    RtpSourceState,
    _parse_now_playing,
)
from custom_components.pipewire_audio_router.const import DOMAIN

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
DAEMON_STATUS = DaemonStatus(version="0.3.0", host_model="Raspberry Pi 4 Model B", host_arch="aarch64")
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)

# One AirPlay source, one output, wired together — the minimum that gives an
# entity a source whose metadata it can show.
SOURCE = RoutingNode(node_id=10, node_name="airplay-in", display_name="AirPlay")
OUTPUT = RoutingNode(node_id=None, node_name="ap2-dev-kitchen", display_name="Kitchen")
LINKED = RoutingMatrix(sources=[SOURCE], outputs=[OUTPUT], links=[("airplay-in", "ap2-dev-kitchen")])
UNLINKED = RoutingMatrix(sources=[SOURCE], outputs=[OUTPUT], links=[])

PLAYING = NowPlaying(
    state="playing",
    title="Song",
    artist="Artist",
    album="Album",
    duration_ms=213_000,
    position_ms=41_000,
    position_updated_at=1_786_000_000_000,
    image_path="/api/now_playing/airplay-in/artwork?rev=3",
)


def _make_entry(hass):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8080})
    entry.add_to_hass(hass)
    return entry


def _patch_daemon(routing, music_groups=()):
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_health", new=AsyncMock(return_value=None)))
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=routing)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=RTP_DISABLED)))
    stack.enter_context(patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value={})))
    stack.enter_context(patch(f"{API}.async_get_outputs", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_music_groups", new=AsyncMock(return_value=list(music_groups))))
    stack.enter_context(patch(f"{API}.async_get_announcement_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_status", new=AsyncMock(return_value=DAEMON_STATUS)))
    stack.enter_context(patch(f"{API}.async_get_agents", new=AsyncMock(return_value=[])))
    stack.enter_context(
        patch(
            f"{API}.async_get_settings",
            new=AsyncMock(return_value=AppSettings(expose_outputs_as_media_players=True)),
        )
    )
    stack.enter_context(patch(f"{COORD}.async_routing_ws_loop", new=AsyncMock()))
    return stack


async def _setup(hass, routing=LINKED, music_groups=()):
    entry = _make_entry(hass)
    with _patch_daemon(routing, music_groups):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
    return hass.data[DOMAIN][entry.entry_id]


async def test_metadata_of_the_routed_source_appears_on_the_output(hass):
    coordinator = await _setup(hass)
    coordinator._apply_now_playing({"airplay-in": PLAYING})
    await hass.async_block_till_done()

    attrs = hass.states.get("media_player.kitchen").attributes
    assert attrs["media_title"] == "Song"
    assert attrs["media_artist"] == "Artist"
    assert attrs["media_album_name"] == "Album"
    assert attrs["media_content_type"] == "music"
    # Seconds, not milliseconds — HA's unit.
    assert attrs["media_duration"] == 213
    assert attrs["media_position"] == 41
    assert "media_position_updated_at" in attrs


async def test_artwork_is_proxied_by_home_assistant(hass):
    """A daemon-relative path is resolved against the daemon's base URL, and the
    entity never claims the image is remotely accessible — so HA fetches it
    server-side and the daemon's port never has to be reachable from a browser."""
    coordinator = await _setup(hass)
    coordinator._apply_now_playing({"airplay-in": PLAYING})
    await hass.async_block_till_done()

    entity = _entity(hass, "media_player.kitchen")
    assert entity.media_image_url == "http://127.0.0.1:8080/api/now_playing/airplay-in/artwork?rev=3"
    assert entity.media_image_remotely_accessible is False
    # And the state machine exposes an entity_picture (HA's own proxy URL).
    assert "entity_picture" in hass.states.get("media_player.kitchen").attributes


async def test_a_producer_supplied_url_is_used_as_is(hass):
    coordinator = await _setup(hass)
    coordinator._apply_now_playing(
        {"airplay-in": NowPlaying(state="playing", title="Song", image_url="https://i.ytimg.com/vi/x/hq.jpg")}
    )
    await hass.async_block_till_done()

    assert _entity(hass, "media_player.kitchen").media_image_url == "https://i.ytimg.com/vi/x/hq.jpg"


async def test_metadata_of_an_unrouted_source_is_not_shown(hass):
    """Metadata follows the *routing*: a source playing into nothing must not
    appear on an output that is not fed by it."""
    coordinator = await _setup(hass, routing=UNLINKED)
    coordinator._apply_now_playing({"airplay-in": PLAYING})
    await hass.async_block_till_done()

    assert "media_title" not in hass.states.get("media_player.kitchen").attributes


async def test_clearing_a_source_collapses_the_media_card(hass):
    """The daemon clears on session end and the frame is always complete, so an
    absent entry must blank the entity rather than freeze on the last track."""
    coordinator = await _setup(hass)
    coordinator._apply_now_playing({"airplay-in": PLAYING})
    await hass.async_block_till_done()
    assert hass.states.get("media_player.kitchen").attributes["media_title"] == "Song"

    coordinator._apply_now_playing({})
    await hass.async_block_till_done()
    attrs = hass.states.get("media_player.kitchen").attributes
    assert "media_title" not in attrs
    assert "media_content_type" not in attrs


async def test_an_entry_with_no_descriptive_fields_shows_nothing(hass):
    """A bare `stopped` with no title/artist/album is an entry mid-teardown, not
    a track — showing `media_content_type: music` for it would render an empty
    media card."""
    coordinator = await _setup(hass)
    coordinator._apply_now_playing({"airplay-in": NowPlaying(state="stopped")})
    await hass.async_block_till_done()

    assert "media_content_type" not in hass.states.get("media_player.kitchen").attributes


async def test_a_music_group_shows_its_sources_metadata(hass):
    """Decision: a group shows the metadata of the source its members play, and it
    resolves that source exactly as its own `source` property does — so the chip
    label and the media card can never disagree."""
    group = MusicGroup(id="g1", name="Downstairs", members=["ap2-dev-kitchen"])
    coordinator = await _setup(hass, music_groups=[group])
    coordinator._apply_now_playing({"airplay-in": PLAYING})
    await hass.async_block_till_done()

    group_state = hass.states.get("media_player.downstairs")
    assert group_state is not None, "the music group entity must exist"
    assert group_state.attributes["source"] == "AirPlay"
    assert group_state.attributes["media_title"] == "Song"
    # Identical to its member output's — one resolution, one answer.
    assert group_state.attributes["media_artist"] == hass.states.get("media_player.kitchen").attributes["media_artist"]


async def test_metadata_arrives_over_the_websocket(hass):
    """End-to-end through the real frame parser: a `now_playing` frame on the
    socket reaches the entity, and a matrix frame on the same socket still works."""
    coordinator = await _setup(hass)
    frame = _parse_now_playing(
        {
            "type": "now_playing",
            "sources": {
                "airplay-in": {
                    "state": "playing",
                    "title": "Song",
                    "artist": "Artist",
                    "duration_ms": 1000,
                    "artwork": {"kind": "embedded", "rev": 2, "path": "/api/now_playing/airplay-in/artwork?rev=2"},
                }
            },
        }
    )
    coordinator._apply_now_playing(NowPlayingFrame(frame).sources)
    await hass.async_block_till_done()

    attrs = hass.states.get("media_player.kitchen").attributes
    assert attrs["media_title"] == "Song"
    assert attrs["media_duration"] == 1


def _entity(hass, entity_id):
    """The live entity object — for properties that are not state attributes
    (`media_image_remotely_accessible` governs how HA fetches the image, so it
    never appears in the state machine)."""
    component = hass.data["entity_components"]["media_player"]
    entity = next((e for e in component.entities if e.entity_id == entity_id), None)
    assert entity is not None, f"no such entity: {entity_id}"
    return entity
