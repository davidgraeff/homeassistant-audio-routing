#!/usr/bin/env python3
"""
Sendspin sink adapter — one process per configured sendspin output
(PLAN.md Section 5.4b/5.5). Embeds aiosendspin's SendspinServer (the same
library MA depends on) and feeds it from a real PipeWire capture, proven
end-to-end in spikes/03-sendspin-pushstream.md and
spikes/03_pipewire_capture_to_sendspin.py:

    pw-cat/other source --> this adapter's own PipeWire sink node
                                       |
                                       | pw-record --target <node-name>
                                       v
                              this process's asyncio loop
                                       |
                                       v
                aiosendspin PushStream.prepare_audio()/commit_audio()
                                       |
                                       v (real WebSocket)
                            connected ESP32 sendspin client

Spawned and supervised by the Rust bridge daemon (Section 5.5) — one
instance per configured sendspin output, not a shared multi-output
process. Deliberately proportional to Phase 2's scope ("one sendspin
output, manually linked"): handles one client connecting/disconnecting
over the adapter's lifetime, not MA-scale reconnect/multi-group
robustness.
"""

import argparse
import asyncio
import logging
import subprocess
import sys

from aiohttp import ClientSession
from aiosendspin.server import ClientAddedEvent, ClientRemovedEvent, SendspinServer
from aiosendspin.server.audio import AudioFormat as ServerAudioFormat

SAMPLE_RATE = 48000
CHANNELS = 2
BIT_DEPTH = 16
BYTES_PER_FRAME = CHANNELS * (BIT_DEPTH // 8)
CHUNK_FRAMES = SAMPLE_RATE // 10  # 100ms per PushStream commit
CHUNK_BYTES = CHUNK_FRAMES * BYTES_PER_FRAME

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("sendspin-adapter")


def create_pipewire_sink(node_name: str) -> None:
    """Create the PipeWire Audio/Sink node other things route audio into.

    Created once at adapter startup (not lazily) so it's a stable, always-
    present routing target — unlike RAOP/AirPlay-receive sources, whose
    nodes only exist while a session is actively playing (spikes 2/3b/
    shairport-sync-source), this adapter owns node creation directly and
    keeps it present for the adapter's entire lifetime.
    """
    subprocess.run(
        [
            "pw-cli",
            "create-node",
            "adapter",
            f"{{ factory.name=support.null-audio-sink node.name={node_name} "
            f"media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }}",
        ],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    log.info("created PipeWire sink node %s", node_name)


async def pump_audio(node_name: str, push_stream_holder: dict) -> None:
    """Continuously capture from the sink and push into the active
    PushStream, if any. Must keep reading from pw-record regardless of
    whether a client is connected — otherwise its stdout pipe fills up and
    it blocks, stalling the capture for whenever a client *does* connect.
    """
    proc = await asyncio.create_subprocess_exec(
        "pw-record",
        "--target",
        node_name,
        "--rate",
        str(SAMPLE_RATE),
        "--channels",
        str(CHANNELS),
        "--format",
        "s16",
        "-",
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL,
    )
    assert proc.stdout is not None
    fmt = ServerAudioFormat(sample_rate=SAMPLE_RATE, bit_depth=BIT_DEPTH, channels=CHANNELS, sample_type="int")
    buffer = b""
    while True:
        data = await proc.stdout.read(CHUNK_BYTES)
        if not data:
            log.warning("pw-record exited, capture pipe closed")
            return
        buffer += data
        while len(buffer) >= CHUNK_BYTES:
            chunk, buffer = buffer[:CHUNK_BYTES], buffer[CHUNK_BYTES:]
            push_stream = push_stream_holder.get("stream")
            if push_stream is not None:
                push_stream.prepare_audio(chunk, fmt)
                await push_stream.commit_audio()


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--node-name", required=True, help="PipeWire sink node name to create")
    parser.add_argument("--sendspin-name", required=True, help="Sendspin server display name")
    parser.add_argument("--sendspin-port", type=int, default=8927)
    args = parser.parse_args()

    create_pipewire_sink(args.node_name)

    push_stream_holder: dict = {"stream": None}

    async with ClientSession() as session:
        loop = asyncio.get_running_loop()
        server = SendspinServer(loop, f"sendspin-adapter-{args.node_name}", args.sendspin_name, session)
        await server.start_server(port=args.sendspin_port, host="0.0.0.0")
        log.info("sendspin server '%s' listening on port %d", args.sendspin_name, args.sendspin_port)

        def on_event(_server: SendspinServer, event: object) -> None:
            if isinstance(event, ClientAddedEvent):
                loop.create_task(on_client_added(event.client_id))
            elif isinstance(event, ClientRemovedEvent):
                log.info("client %s disconnected", event.client_id)
                push_stream_holder["stream"] = None

        async def on_client_added(client_id: str) -> None:
            client = server.get_client(client_id)
            if client is None:
                return
            for _ in range(50):
                try:
                    client.info  # noqa: B018 - raises AssertionError until hello is processed
                    break
                except AssertionError:
                    await asyncio.sleep(0.1)
            else:
                log.warning("client %s hello never completed", client_id)
                return
            log.info("client %s connected, starting push stream", client_id)
            push_stream_holder["stream"] = client.group.start_stream()

        server.add_event_listener(on_event)

        await pump_audio(args.node_name, push_stream_holder)

    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
