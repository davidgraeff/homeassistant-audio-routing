"""Tests for the routing WebSocket client's frame handling.

The daemon multiplexes several frame types onto `/api/routing/ws` (routing.rs's
internally-tagged `Frame`): `matrix` on connect and on every change/meter tick,
plus `outputs`/`discovered`/`agents` listings — the first listing arriving
immediately after the initial matrix, on *every* connect.

This client consumes only the matrix. It used to parse every text frame as one,
which raised `KeyError: 'display_name'` on the first listing frame (an
`OutputInfo` has `node_name`/`name`), and the coordinator's catch-all turned that
into a 5-second reconnect loop — so the socket never survived its first second
and the "pushed, not polled" routing path silently became a polled one.
"""

from types import SimpleNamespace
from unittest.mock import MagicMock

import aiohttp

from custom_components.pipewire_audio_router.api import PipewireRouterApiClient

MATRIX_FRAME = {
    "type": "matrix",
    "sources": [{"node_name": "airplay-in", "display_name": "AirPlay", "present": True}],
    "outputs": [{"node_name": "ap2-dev-dusche", "display_name": "Shower", "present": True}],
    "links": [{"source": "airplay-in", "output": "ap2-dev-dusche"}],
}

# As `Frame::Outputs` serializes it: OutputInfo has `name`, never `display_name`.
OUTPUTS_FRAME = {
    "type": "outputs",
    "outputs": [{"node_name": "ap2-dev-dusche", "name": "Shower", "kind": "airplay2", "present": True}],
}

DISCOVERED_FRAME = {"type": "discovered", "outputs": [{"node_name": "ap2-dev-new", "name": "New"}]}
AGENTS_FRAME = {"type": "agents", "agents": [{"identity": "abc:david", "hostname": "david-pc"}]}


def _text(payload):
    return SimpleNamespace(type=aiohttp.WSMsgType.TEXT, json=lambda: payload)


def _client(frames) -> PipewireRouterApiClient:
    """A client whose routing socket replays `frames`, then closes."""

    class FakeWs:
        def __aiter__(self):
            async def gen():
                for frame in frames:
                    yield frame

            return gen()

    class FakeWsCtx:
        async def __aenter__(self):
            return FakeWs()

        async def __aexit__(self, *exc):
            return False

    session = MagicMock()
    session.ws_connect = MagicMock(return_value=FakeWsCtx())
    return PipewireRouterApiClient(session, "127.0.0.1", 8099)


async def _collect(frames):
    client = _client(frames)
    return [matrix async for matrix in client.async_routing_ws_messages()]


async def test_matrix_frame_is_parsed():
    [matrix] = await _collect([_text(MATRIX_FRAME)])
    assert [s.node_name for s in matrix.sources] == ["airplay-in"]
    assert [o.display_name for o in matrix.outputs] == ["Shower"]
    assert matrix.links == [("airplay-in", "ap2-dev-dusche")]


async def test_listing_frames_are_skipped_not_parsed():
    """The regression: a listing frame must neither raise nor be yielded.

    Yielding matters as much as raising — an `agents` frame has no `sources` or
    `outputs` keys at all, so parsing it would produce an *empty* matrix and blank
    every entity's source until the next matrix arrived.
    """
    matrices = await _collect(
        [
            _text(MATRIX_FRAME),
            _text(OUTPUTS_FRAME),
            _text(DISCOVERED_FRAME),
            _text(AGENTS_FRAME),
            _text(MATRIX_FRAME),
        ]
    )
    assert len(matrices) == 2
    assert all(m.sources for m in matrices)


async def test_untagged_frame_is_treated_as_a_matrix():
    """The matrix frame historically *was* the whole frame; the daemon tags it
    internally so that a reader predating the tag keeps working. Honour that in
    reverse, so an older daemon's untagged frame still works here."""
    untagged = {k: v for k, v in MATRIX_FRAME.items() if k != "type"}
    [matrix] = await _collect([_text(untagged)])
    assert [s.node_name for s in matrix.sources] == ["airplay-in"]


async def test_unknown_frame_type_is_ignored():
    """A frame type added by a newer daemon must not break an older client —
    otherwise every future frame (e.g. per-source now-playing metadata) is a
    breaking change. See docs/source-metadata-plan.md."""
    assert await _collect([_text({"type": "something_new", "payload": 1})]) == []


async def test_meters_frames_are_skipped_and_do_not_disturb_the_matrix():
    """`meters` is the socket's only high-rate frame (250 ms while watched) and
    carries peaks and xrun counts — nothing an HA entity has any use for. It must be
    dropped without being parsed as a matrix, which would otherwise blank every
    entity's source four times a second. See docs/source-metadata-plan.md WP7."""
    meters = {"type": "meters", "nodes": {"airplay-in": {"peak": 0.4, "xruns": 2}}}
    matrices = await _collect([_text(MATRIX_FRAME), _text(meters), _text(meters)])
    assert len(matrices) == 1
    assert [s.node_name for s in matrices[0].sources] == ["airplay-in"]


async def test_non_dict_and_close_frames_do_not_raise():
    frames = [
        _text([1, 2, 3]),
        SimpleNamespace(type=aiohttp.WSMsgType.CLOSE, json=lambda: None),
        _text(MATRIX_FRAME),  # after CLOSE: never reached
    ]
    assert await _collect(frames) == []
