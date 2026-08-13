"""Tests for the Bluetooth-bridge RTP source `switch` + `number` entities.

Loads the real config entry through hass.config_entries.async_setup (same
approach as test_media_player.py) so the coordinator, platform forwarding, and
real service dispatch are exercised — only the HTTP client is mocked.
"""

from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

import aiohttp
import pytest
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.pipewire_audio_router.api import (
    DaemonStatus,
    PipewireRouterApiClient,
    PipewireRouterApiError,
    RtpSourceState,
)
from custom_components.pipewire_audio_router.const import DOMAIN

API = "custom_components.pipewire_audio_router.api.PipewireRouterApiClient"
COORD = "custom_components.pipewire_audio_router.PipewireRouterCoordinator"
DAEMON_STATUS = DaemonStatus(version="0.3.0", host_model="Raspberry Pi 4 Model B", host_arch="aarch64")

SWITCH = "switch.pipewire_audio_router_bluetooth_bridge_rtp_source"
NUMBER = "number.pipewire_audio_router_bluetooth_bridge_rtp_port"
LATENCY = "number.pipewire_audio_router_bluetooth_bridge_rtp_jitter_buffer"

RTP_DISABLED = RtpSourceState(enabled=False, port=46000, latency_msec=200, loaded=False)


def _make_entry(hass):
    entry = MockConfigEntry(domain=DOMAIN, data={"host": "127.0.0.1", "port": 8080})
    entry.add_to_hass(hass)
    return entry


async def _setup(hass, stack, rtp=RTP_DISABLED):
    """Set up the entry with the daemon mocks in `stack` already active, so the
    mocks stay in force for any post-setup service call (each triggers a
    coordinator refresh that re-polls the daemon)."""
    entry = _make_entry(hass)
    stack.enter_context(patch(f"{API}.async_health", new=AsyncMock(return_value=None)))
    from custom_components.pipewire_audio_router.api import RoutingMatrix

    stack.enter_context(
        patch(f"{API}.async_get_routing", new=AsyncMock(return_value=RoutingMatrix(sources=[], outputs=[], links=[])))
    )
    stack.enter_context(patch(f"{API}.async_get_rtp_source", new=AsyncMock(return_value=rtp)))
    stack.enter_context(patch(f"{API}.async_get_sendspin_volumes", new=AsyncMock(return_value={})))
    # Also mock the secondary polls (outputs/groups/settings) so setup is fully
    # offline and doesn't depend on a refused connection — a socket-blocking
    # test env turns that refusal into a hard error instead.
    stack.enter_context(patch(f"{API}.async_get_outputs", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_music_groups", new=AsyncMock(return_value=[])))
    stack.enter_context(patch(f"{API}.async_get_announcement_groups", new=AsyncMock(return_value=[])))
    from custom_components.pipewire_audio_router.api import AppSettings

    stack.enter_context(patch(f"{API}.async_get_status", new=AsyncMock(return_value=DAEMON_STATUS)))
    stack.enter_context(patch(f"{API}.async_get_agents", new=AsyncMock(return_value=[])))
    stack.enter_context(
        patch(
            f"{API}.async_get_settings",
            new=AsyncMock(return_value=AppSettings(expose_outputs_as_media_players=False)),
        )
    )
    stack.enter_context(patch(f"{COORD}.async_events_ws_loop", new=AsyncMock()))
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    return entry


# ---- entity presence & state reflection ----------------------------------


async def test_entities_created_disabled_by_default(hass):
    with ExitStack() as stack:
        await _setup(hass, stack, RTP_DISABLED)
    switch = hass.states.get(SWITCH)
    number = hass.states.get(NUMBER)
    assert switch is not None and switch.state == "off"
    assert number is not None and float(number.state) == 46000


async def test_switch_and_number_reflect_enabled_state(hass):
    with ExitStack() as stack:
        await _setup(hass, stack, RtpSourceState(enabled=True, port=46001, latency_msec=200, loaded=True))
    assert hass.states.get(SWITCH).state == "on"
    # An enabled source's stored port is authoritative for the number.
    assert float(hass.states.get(NUMBER).state) == 46001


# ---- switch actions -------------------------------------------------------


async def test_turn_on_enables_with_desired_port(hass):
    with ExitStack() as stack:
        await _setup(hass, stack, RTP_DISABLED)
        with patch(f"{API}.async_set_rtp_source", new=AsyncMock()) as mock_set:
            await hass.services.async_call("switch", "turn_on", {"entity_id": SWITCH}, blocking=True)
            # Default desired port + latency when nothing set yet.
            mock_set.assert_awaited_once_with(46000, 200)


async def test_turn_off_disables(hass):
    with ExitStack() as stack:
        await _setup(hass, stack, RtpSourceState(enabled=True, port=46000, latency_msec=200, loaded=True))
        with patch(f"{API}.async_disable_rtp_source", new=AsyncMock()) as mock_disable:
            await hass.services.async_call("switch", "turn_off", {"entity_id": SWITCH}, blocking=True)
            mock_disable.assert_awaited_once_with()


# ---- number actions -------------------------------------------------------


async def test_set_port_while_enabled_repoints_live(hass):
    with ExitStack() as stack:
        await _setup(hass, stack, RtpSourceState(enabled=True, port=46000, latency_msec=200, loaded=True))
        with patch(f"{API}.async_set_rtp_source", new=AsyncMock()) as mock_set:
            await hass.services.async_call("number", "set_value", {"entity_id": NUMBER, "value": 47100}, blocking=True)
            # Enabled → applied live (module reloads on the new port), current
            # latency carried along since the daemon replaces the whole config.
            mock_set.assert_awaited_once_with(47100, 200)


async def test_latency_reflects_enabled_state(hass):
    with ExitStack() as stack:
        await _setup(hass, stack, RtpSourceState(enabled=True, port=46000, latency_msec=350, loaded=True))
    # An enabled source's stored latency is authoritative for the number.
    assert float(hass.states.get(LATENCY).state) == 350


async def test_set_latency_while_enabled_repoints_live(hass):
    with ExitStack() as stack:
        await _setup(hass, stack, RtpSourceState(enabled=True, port=46000, latency_msec=200, loaded=True))
        with patch(f"{API}.async_set_rtp_source", new=AsyncMock()) as mock_set:
            await hass.services.async_call("number", "set_value", {"entity_id": LATENCY, "value": 350}, blocking=True)
            # Enabled → applied live, current port carried along.
            mock_set.assert_awaited_once_with(46000, 350)


async def test_set_latency_while_disabled_only_remembers_then_used_on_enable(hass):
    with ExitStack() as stack:
        await _setup(hass, stack, RTP_DISABLED)
        with patch(f"{API}.async_set_rtp_source", new=AsyncMock()) as mock_set:
            await hass.services.async_call("number", "set_value", {"entity_id": LATENCY, "value": 350}, blocking=True)
            mock_set.assert_not_awaited()
        assert float(hass.states.get(LATENCY).state) == 350
        # The next enable uses the remembered latency (default port).
        with patch(f"{API}.async_set_rtp_source", new=AsyncMock()) as mock_set:
            await hass.services.async_call("switch", "turn_on", {"entity_id": SWITCH}, blocking=True)
            mock_set.assert_awaited_once_with(46000, 350)


async def test_set_port_while_disabled_only_remembers_then_used_on_enable(hass):
    with ExitStack() as stack:
        await _setup(hass, stack, RTP_DISABLED)
        # Setting the port while disabled must NOT touch the daemon...
        with patch(f"{API}.async_set_rtp_source", new=AsyncMock()) as mock_set:
            await hass.services.async_call("number", "set_value", {"entity_id": NUMBER, "value": 47100}, blocking=True)
            mock_set.assert_not_awaited()
        assert float(hass.states.get(NUMBER).state) == 47100
        # ...but the next enable uses the remembered port.
        with patch(f"{API}.async_set_rtp_source", new=AsyncMock()) as mock_set:
            await hass.services.async_call("switch", "turn_on", {"entity_id": SWITCH}, blocking=True)
            mock_set.assert_awaited_once_with(47100, 200)


# ---- API client: RTP endpoints -------------------------------------------


class _FakeResp:
    def __init__(self, payload, status=200):
        self._payload = payload
        self.status = status

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        return False

    async def json(self):
        return self._payload

    def raise_for_status(self):
        if self.status >= 400:
            raise aiohttp.ClientResponseError(None, None, status=self.status)


class _FakeHttpSession:
    """The RTP client now works against the `/api/sources` collection, so its
    get/set/disable each do TWO calls: a GET (list the sources) then the mutating
    verb (PUT/POST/DELETE on `/api/sources/{id}`). This fake serves `get_resp` to
    GET and `mutate_resp` to the mutating verbs (defaulting to `get_resp`)."""

    def __init__(self, get_resp, mutate_resp=None):
        self._get = get_resp
        self._mutate = mutate_resp if mutate_resp is not None else get_resp

    def get(self, url):
        return self._get

    def put(self, url, json=None):
        return self._mutate

    def post(self, url, json=None):
        return self._mutate

    def delete(self, url):
        return self._mutate


def _sources_list(*, present=True, port=46000, latency_msec=200, source_addr="239.255.42.42", ignore_ssrc=False, rate=48000):
    """A `GET /api/sources` body containing the single Bluetooth-bridge RTP
    source (legacy id) the client operates on."""
    return {
        "sources": [
            {
                "id": "bt-bridge-rtp",
                "label": "Bluetooth Bridge",
                "kind": "rtp",
                "present": present,
                "node_name": "bt-bridge-rtp",
                "airplay": None,
                "rtp": {"port": port, "latency_msec": latency_msec, "source_addr": source_addr, "ignore_ssrc": ignore_ssrc, "rate": rate},
            }
        ]
    }


async def test_get_rtp_source_parses_shape():
    # `enabled` = the rtp source exists in the collection; `loaded` = present.
    client = PipewireRouterApiClient(
        _FakeHttpSession(_FakeResp(_sources_list(present=True, port=46000, latency_msec=250))), "h", 8099
    )
    state = await client.async_get_rtp_source()
    assert (state.enabled, state.port, state.latency_msec, state.loaded) == (True, 46000, 250, True)


async def test_set_rtp_source_raises_daemon_message_on_ok_false():
    # set reads the collection (GET → existing source), then PUTs the update; a
    # 502 with ok:false on the PUT carries the daemon's reason — surfaced.
    client = PipewireRouterApiClient(
        _FakeHttpSession(
            _FakeResp(_sources_list()),
            _FakeResp({"ok": False, "message": "failed to load module"}, status=502),
        ),
        "h",
        8099,
    )
    with pytest.raises(PipewireRouterApiError, match="failed to load module"):
        await client.async_set_rtp_source(46000, 200)


async def test_disable_rtp_source_ok():
    # disable finds the source (GET) then DELETEs it (ok:true).
    client = PipewireRouterApiClient(
        _FakeHttpSession(
            _FakeResp(_sources_list()),
            _FakeResp({"ok": True, "message": "removed source 'bt-bridge-rtp'"}),
        ),
        "h",
        8099,
    )
    await client.async_disable_rtp_source()  # no raise
