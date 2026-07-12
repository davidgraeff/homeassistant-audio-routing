"""
Spike 3 (PLAN.md Section 5.4b / 7): prove aiosendspin's server-side
SendspinServer + PushStream can deliver real audio to a connected client,
without needing a real ESP32 sendspin device.

Runs BOTH a SendspinServer and a real aiosendspin client (the same
client-side implementation ESPHome-style hardware would use) in one
process, connects the client over a real WebSocket to the server, then
pushes PCM audio into the server-side PushStream and verifies the client
actually receives audio chunks with the expected byte count.

This validates the exact mechanism Section 5.4b's design depends on:
"PipeWire capture -> callback -> PushStream -> WS -> ESP32" — the capture
half is stubbed here with synthetic PCM (a sine wave, so a human could
also just listen to the recorded output to sanity check it), since this
script's job is proving the aiosendspin plumbing, not PipeWire capture
(that integration is exercised for real in
tests/spike03_pipewire_capture_to_sendspin.py).
"""

import asyncio
import math
import struct
import sys

from aiohttp import ClientSession

from aiosendspin.client.client import SendspinClient as ClientSideSendspinClient
from aiosendspin.models.player import ClientHelloPlayerSupport, SupportedAudioFormat
from aiosendspin.models.types import AudioCodec, PlayerCommand, Roles
from aiosendspin.server import ClientAddedEvent, SendspinServer
from aiosendspin.server.audio import AudioFormat as ServerAudioFormat

PORT = 8927
SAMPLE_RATE = 48000
CHANNELS = 2
BIT_DEPTH = 16
CHUNK_MS = 100
TOTAL_CHUNKS = 20  # 2 seconds of audio


def make_sine_chunk(chunk_index: int) -> bytes:
    """Generate one CHUNK_MS chunk of a 440Hz sine wave as interleaved s16le stereo PCM."""
    samples_per_chunk = SAMPLE_RATE * CHUNK_MS // 1000
    start_sample = chunk_index * samples_per_chunk
    frames = []
    for i in range(samples_per_chunk):
        t = (start_sample + i) / SAMPLE_RATE
        value = int(32767 * 0.3 * math.sin(2 * math.pi * 440 * t))
        frames.append(struct.pack("<hh", value, value))
    return b"".join(frames)


async def main() -> int:
    received_chunks: list[tuple[int, bytes]] = []
    push_stream_ready = asyncio.Event()
    push_stream_holder: dict[str, object] = {}

    async with ClientSession() as session:
        loop = asyncio.get_running_loop()
        server = SendspinServer(loop, "spike3-server", "Spike3 Server", session)
        await server.start_server(
            port=PORT, host="127.0.0.1", advertise_addresses=["127.0.0.1"], discover_clients=False
        )

        def on_server_event(_server: SendspinServer, event: object) -> None:
            if isinstance(event, ClientAddedEvent):
                loop.create_task(setup_push_stream(event.client_id))

        async def setup_push_stream(client_id: str) -> None:
            client = server.get_client(client_id)
            if client is None:
                return
            # Hello handshake completes asynchronously after ClientAddedEvent fires;
            # poll for it like MA's own sendspin provider does.
            for _ in range(50):
                try:
                    client.info  # noqa: B018 - raises AssertionError until hello is processed
                    break
                except AssertionError:
                    await asyncio.sleep(0.1)
            else:
                print("FAIL: client hello never completed", file=sys.stderr)
                return
            group = client.group
            push_stream_holder["stream"] = group.start_stream()
            push_stream_ready.set()

        server.add_event_listener(on_server_event)

        player_support = ClientHelloPlayerSupport(
            supported_formats=[
                SupportedAudioFormat(
                    codec=AudioCodec.PCM, channels=CHANNELS, sample_rate=SAMPLE_RATE, bit_depth=BIT_DEPTH
                )
            ],
            # Bytes, not chunks — must comfortably exceed one PushStream commit's
            # worth of audio (chunk_bytes below) or the server blocks waiting for
            # the buffer to drain. A few seconds' worth is a realistic device value.
            buffer_capacity=SAMPLE_RATE * CHANNELS * (BIT_DEPTH // 8) * 2,
            supported_commands=[PlayerCommand.VOLUME, PlayerCommand.MUTE],
        )
        client = ClientSideSendspinClient(
            client_id="spike3-fake-esp32",
            client_name="Spike3 Fake ESP32",
            roles=[Roles.PLAYER],
            player_support=player_support,
        )

        def on_audio_chunk(server_timestamp_us: int, audio_data: bytes, _format: object) -> None:
            received_chunks.append((server_timestamp_us, audio_data))

        client.add_audio_chunk_listener(on_audio_chunk)

        await client.connect(f"ws://127.0.0.1:{PORT}/sendspin")

        try:
            await asyncio.wait_for(push_stream_ready.wait(), timeout=10)
        except asyncio.TimeoutError:
            print("FAIL: push stream was never created (client never registered?)", file=sys.stderr)
            return 1

        push_stream = push_stream_holder["stream"]
        fmt = ServerAudioFormat(sample_rate=SAMPLE_RATE, bit_depth=BIT_DEPTH, channels=CHANNELS, sample_type="int")

        for i in range(TOTAL_CHUNKS):
            push_stream.prepare_audio(make_sine_chunk(i), fmt)
            await push_stream.commit_audio()
            await asyncio.sleep(CHUNK_MS / 1000)

        # Give the last chunks time to arrive over the websocket.
        await asyncio.sleep(1.0)

        await client.disconnect()
        await server.close()

    total_bytes = sum(len(data) for _, data in received_chunks)
    expected_bytes = TOTAL_CHUNKS * (SAMPLE_RATE * CHUNK_MS // 1000) * CHANNELS * (BIT_DEPTH // 8)
    print(f"chunks received: {len(received_chunks)}")
    print(f"bytes received: {total_bytes} (sent: {expected_bytes})")

    if not received_chunks:
        print("FAIL: fake ESP32 client received zero audio chunks", file=sys.stderr)
        return 1
    if total_bytes < expected_bytes * 0.5:
        print(
            f"FAIL: received far less audio than sent ({total_bytes} < 50% of {expected_bytes})",
            file=sys.stderr,
        )
        return 1

    print("PASS: PipeWire-style PCM pushed via aiosendspin PushStream reached a real client over the wire")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
