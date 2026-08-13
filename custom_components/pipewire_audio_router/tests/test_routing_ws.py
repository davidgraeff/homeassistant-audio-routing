"""Tests for the events-socket client's frame handling.

The daemon has one push socket, `/api/events`, and a client chooses what it receives by
subscribing to topics on it (events.rs). This integration asks for `matrix` and
`now_playing`, so most of what follows is about *not* mis-reading anything else: a frame
for a topic we did not ask for (a daemon newer than this integration, or a stray),
the subscribe acknowledgement, and the non-dict/close cases.

The regression these protect is worth keeping in mind: this client used to parse every
text frame as a matrix, which raised `KeyError: 'display_name'` on the first listing
frame (an `OutputInfo` has `node_name`/`name`), and the coordinator's catch-all turned
that into a 5-second reconnect loop — so the socket never survived its first second and
the "pushed, not polled" routing path silently became a polled one.
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
    """A client whose events socket replays `frames`, then closes. `FakeWs.sent` records
    the control messages, because *subscribing* is now part of the contract: without it
    the daemon sends nothing at all."""

    class FakeWs:
        def __init__(self):
            self.sent = []

        async def send_json(self, payload):
            self.sent.append(payload)

        def __aiter__(self):
            async def gen():
                for frame in frames:
                    yield frame

            return gen()

    class FakeWsCtx:
        async def __aenter__(self):
            self.ws = FakeWs()
            sent_control.append(self.ws)
            return self.ws

        async def __aexit__(self, *exc):
            return False

    session = MagicMock()
    session.ws_connect = MagicMock(return_value=FakeWsCtx())
    return PipewireRouterApiClient(session, "127.0.0.1", 8099)


sent_control: list = []


async def _collect(frames):
    sent_control.clear()
    client = _client(frames)
    return [matrix async for matrix in client.async_event_messages()]


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


async def test_unknown_frame_type_is_ignored():
    """A frame type added by a newer daemon must not break an older client —
    otherwise every future frame (e.g. per-source now-playing metadata) is a
    breaking change. See docs/source-metadata-plan.md."""
    assert await _collect([_text({"type": "something_new", "payload": 1})]) == []


async def test_the_subscription_is_sent_and_names_only_what_this_client_uses():
    """Nothing arrives without it, and asking for less is the point: the daemon would
    otherwise push `meters` at 4 Hz — peaks and xrun counts no HA entity can use, which
    the previous single-socket version had to skip four times a second."""
    await _collect([_text(MATRIX_FRAME)])
    [ws] = sent_control
    assert ws.sent == [{"op": "subscribe", "topics": ["matrix", "now_playing"]}]


async def test_the_subscribe_ack_is_consumed_and_unknown_topics_are_logged(caplog):
    """The ack is not a matrix and must not be parsed as one; an `unknown` topic is a
    misconfiguration that would otherwise look like an eternally silent socket."""
    ack = {"type": "subscribed", "topics": ["matrix"], "unknown": ["matrics"]}
    assert await _collect([_text(ack), _text(MATRIX_FRAME)]) != []
    assert "matrics" in caplog.text


async def test_frames_for_unsubscribed_topics_are_skipped_not_parsed():
    """Belt and braces: even though the daemon only sends what was asked for, a frame
    for another topic must be dropped rather than parsed as a matrix — parsing an
    `agents` frame would produce an *empty* matrix and blank every entity's source."""
    meters = {"type": "meters", "nodes": {"airplay-in": {"peak": 0.4, "xruns": 2}}}
    matrices = await _collect([_text(MATRIX_FRAME), _text(meters), _text(AGENTS_FRAME)])
    assert len(matrices) == 1
    assert [s.node_name for s in matrices[0].sources] == ["airplay-in"]


async def test_non_dict_and_close_frames_do_not_raise():
    frames = [
        _text([1, 2, 3]),
        SimpleNamespace(type=aiohttp.WSMsgType.CLOSE, json=lambda: None),
        _text(MATRIX_FRAME),  # after CLOSE: never reached
    ]
    assert await _collect(frames) == []
