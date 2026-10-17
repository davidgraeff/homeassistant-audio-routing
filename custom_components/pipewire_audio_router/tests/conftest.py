from unittest.mock import AsyncMock, patch

import pytest


@pytest.fixture(autouse=True)
def auto_enable_custom_integrations(enable_custom_integrations):
    """Every test in this package needs custom_components/ loadable."""
    yield


@pytest.fixture(autouse=True)
def no_source_controls():
    """By default no source announces a control API.

    Every coordinator poll asks the daemon for controllable sources; a test that mocks the
    daemon call by call would otherwise reach a real socket for this one (blocked under
    pytest-homeassistant-custom-component) and fail setup. Tests about controllable
    sources patch it again with what they need — the inner patch wins.
    """
    with patch(
        "custom_components.pipewire_audio_router.api.PipewireRouterApiClient.async_get_source_controls",
        new=AsyncMock(return_value=[]),
    ):
        yield
