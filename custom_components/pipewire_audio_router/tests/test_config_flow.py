"""Real config-flow tests against the actual HA config_entries machinery,
not just calling the flow class methods directly."""

from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

from homeassistant import config_entries
from homeassistant.data_entry_flow import FlowResultType

from custom_components.pipewire_audio_router.api import (
    AppSettings,
    PipewireRouterApiError,
    RoutingMatrix,
    RtpSourceState,
)
from custom_components.pipewire_audio_router.const import DOMAIN

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
EMPTY_ROUTING = RoutingMatrix(sources=[], outputs=[], links=[])
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)


def _patch_setup():
    # Creating an entry auto-runs async_setup_entry, which polls players + the
    # RTP source, seeds routing over REST, and opens the routing WebSocket —
    # mock them all so setup stays offline.
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=EMPTY_ROUTING)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=RTP_DISABLED)))
    stack.enter_context(patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value={})))
    # The outputs listing, the two group listings and the settings fetch are
    # polled too. The coordinator treats them as secondary and swallows a
    # PipewireRouterApiError — but a socket-blocking test env raises before that,
    # so leaving them unpatched fails the run on "the test opens sockets".
    stack.enter_context(patch(f"{API}.async_get_outputs", new=AsyncMock(return_value=[])))
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


async def test_user_flow_success(hass):
    with (
        patch(f"{API}.async_health", new=AsyncMock(return_value=None)),
        _patch_setup(),
    ):
        result = await hass.config_entries.flow.async_init(DOMAIN, context={"source": config_entries.SOURCE_USER})
        assert result["type"] == FlowResultType.FORM

        result2 = await hass.config_entries.flow.async_configure(
            result["flow_id"], {"host": "127.0.0.1", "port": 8080}
        )
        assert result2["type"] == FlowResultType.CREATE_ENTRY
        assert result2["title"] == "127.0.0.1:8080"
        assert result2["data"] == {"host": "127.0.0.1", "port": 8080}


async def test_user_flow_cannot_connect(hass):
    with patch(
        "custom_components.pipewire_audio_router.config_flow.PipewireRouterApiClient.async_health",
        new=AsyncMock(side_effect=PipewireRouterApiError("no route")),
    ):
        result = await hass.config_entries.flow.async_init(DOMAIN, context={"source": config_entries.SOURCE_USER})
        result2 = await hass.config_entries.flow.async_configure(
            result["flow_id"], {"host": "127.0.0.1", "port": 8080}
        )
        assert result2["type"] == FlowResultType.FORM
        assert result2["errors"] == {"base": "cannot_connect"}


async def test_duplicate_entry_aborts(hass):
    with (
        patch(f"{API}.async_health", new=AsyncMock(return_value=None)),
        _patch_setup(),
    ):
        for expected_type in (FlowResultType.CREATE_ENTRY, FlowResultType.ABORT):
            result = await hass.config_entries.flow.async_init(DOMAIN, context={"source": config_entries.SOURCE_USER})
            result2 = await hass.config_entries.flow.async_configure(
                result["flow_id"], {"host": "127.0.0.1", "port": 8080}
            )
            assert result2["type"] == expected_type
