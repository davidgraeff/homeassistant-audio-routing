"""Real config-flow tests against the actual HA config_entries machinery,
not just calling the flow class methods directly."""

from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

from homeassistant import config_entries
from homeassistant.data_entry_flow import FlowResultType

from custom_components.pipewire_audio_router.api import PipewireRouterApiError, RoutingMatrix, RtpSourceState
from custom_components.pipewire_audio_router.const import DOMAIN

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
EMPTY_ROUTING = RoutingMatrix(sources=[], outputs=[], links=[])
RTP_DISABLED = RtpSourceState(enabled=False, port=46000, loaded=False)


def _patch_setup():
    # Creating an entry auto-runs async_setup_entry, which polls players + the
    # RTP source, seeds routing over REST, and opens the routing WebSocket —
    # mock them all so setup stays offline.
    stack = ExitStack()
    stack.enter_context(patch(f"{API}.async_get_routing", new=AsyncMock(return_value=EMPTY_ROUTING)))
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=RTP_DISABLED)))
    stack.enter_context(patch(f"{COORD}.async_routing_ws_loop", new=AsyncMock()))
    return stack


async def test_user_flow_success(hass):
    with (
        patch(f"{API}.async_get_media_players", new=AsyncMock(return_value=[])),
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
        "custom_components.pipewire_audio_router.config_flow.PipewireRouterApiClient.async_get_media_players",
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
        patch(f"{API}.async_get_media_players", new=AsyncMock(return_value=[])),
        _patch_setup(),
    ):
        for expected_type in (FlowResultType.CREATE_ENTRY, FlowResultType.ABORT):
            result = await hass.config_entries.flow.async_init(DOMAIN, context={"source": config_entries.SOURCE_USER})
            result2 = await hass.config_entries.flow.async_configure(
                result["flow_id"], {"host": "127.0.0.1", "port": 8080}
            )
            assert result2["type"] == expected_type
