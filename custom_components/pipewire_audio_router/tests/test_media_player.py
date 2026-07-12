"""Real integration-setup tests: loads the actual config entry through
hass.config_entries.async_setup, not just instantiating the entity class
directly, so the coordinator/platform-forwarding wiring is exercised too."""

from unittest.mock import AsyncMock, patch

from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.pipewire_audio_router.api import MediaPlayerState
from custom_components.pipewire_audio_router.const import DOMAIN


def _make_entry(hass):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8080})
    entry.add_to_hass(hass)
    return entry


async def test_entities_created_from_bridge_daemon_state(hass):
    entry = _make_entry(hass)
    fake_players = [
        MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0),
        MediaPlayerState(node_id=50, node_name="sendspin-out-kitchen", state="playing", volume=0.5),
    ]
    with patch(
        "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_get_media_players",
        new=AsyncMock(return_value=fake_players),
    ):
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
    with patch(
        "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_get_media_players",
        new=AsyncMock(return_value=fake_players),
    ):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    with patch(
        "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_set_volume",
        new=AsyncMock(),
    ) as mock_set_volume:
        await hass.services.async_call(
            "media_player",
            "volume_set",
            {"entity_id": "media_player.kitchen", "volume_level": 0.3},
            blocking=True,
        )
        mock_set_volume.assert_awaited_once_with(50, 0.3)


async def test_play_media_calls_announce_on_bridge_daemon(hass):
    """`play_media` (with or without `announce=True`) is this entity's
    only playback contract — it must always go through the bridge
    daemon's ducked `/announce` endpoint, per Section 5.6/media_player.py's
    docstring, since there is no other playback mode implemented."""
    entry = _make_entry(hass)
    fake_players = [MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0)]
    with patch(
        "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_get_media_players",
        new=AsyncMock(return_value=fake_players),
    ):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    with patch(
        "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_announce",
        new=AsyncMock(),
    ) as mock_announce:
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
    with patch(
        "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_get_media_players",
        new=AsyncMock(return_value=fake_players),
    ):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    with (
        patch(
            "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_announce_wyoming",
            new=AsyncMock(),
        ) as mock_announce_wyoming,
        patch(
            "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_announce",
            new=AsyncMock(),
        ) as mock_announce_url,
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
    with patch(
        "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_get_media_players",
        new=AsyncMock(
            return_value=[MediaPlayerState(node_id=35, node_name="raop-out-pioneer", state="idle", volume=1.0)]
        ),
    ):
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()

    with patch(
        "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_get_media_players",
        new=AsyncMock(return_value=[]),
    ):
        coordinator = hass.data[DOMAIN][entry.entry_id]
        await coordinator.async_refresh()
        await hass.async_block_till_done()

    state = hass.states.get("media_player.pioneer")
    assert state is not None
    assert state.state == "unavailable"
