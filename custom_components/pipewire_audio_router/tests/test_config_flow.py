"""Real config-flow tests against the actual HA config_entries machinery,
not just calling the flow class methods directly."""

from unittest.mock import AsyncMock, patch

from homeassistant import config_entries
from homeassistant.data_entry_flow import FlowResultType

from custom_components.pipewire_audio_router.api import PipewireRouterApiError
from custom_components.pipewire_audio_router.const import DOMAIN


async def test_user_flow_success(hass):
    with patch(
        "custom_components.pipewire_audio_router.config_flow.PipewireRouterApiClient.async_get_media_players",
        new=AsyncMock(return_value=[]),
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
    with patch(
        "custom_components.pipewire_audio_router.config_flow.PipewireRouterApiClient.async_get_media_players",
        new=AsyncMock(return_value=[]),
    ):
        for expected_type in (FlowResultType.CREATE_ENTRY, FlowResultType.ABORT):
            result = await hass.config_entries.flow.async_init(DOMAIN, context={"source": config_entries.SOURCE_USER})
            result2 = await hass.config_entries.flow.async_configure(
                result["flow_id"], {"host": "127.0.0.1", "port": 8080}
            )
            assert result2["type"] == expected_type
