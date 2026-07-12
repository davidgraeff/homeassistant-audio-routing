"""
Spike 3, full integration (PLAN.md Section 5.4b): prove the complete
"PipeWire audio in -> callback -> aiosendspin PushStream -> WS -> ESP32"
path described in the plan, using real PipeWire capture rather than
synthetic PCM (that half is spike03_sendspin_pushstream.py).

Topology, entirely within this one process/container:

  pw-cat (plays a real WAV) --> sendspin-test-sink (PipeWire Audio/Sink)
                                        |
                                        | pw-record --target sendspin-test-sink
                                        v
                              this script's asyncio loop
                                        |
                                        v
                    aiosendspin PushStream.prepare_audio()/commit_audio()
                                        |
                                        v (real WebSocket, ws://127.0.0.1:8927/sendspin)
                          aiosendspin client (stand-in for a real ESP32)

Requires PipeWire + WirePlumber already running in this container (started
by the wrapper shell script, not by this file) and a WAV file at
/tmp/tone.wav to play into the sink.
"""

import asyncio
import subprocess
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
BYTES_PER_FRAME = CHANNELS * (BIT_DEPTH // 8)
CHUNK_FRAMES = SAMPLE_RATE // 10  # 100ms per PushStream commit
CHUNK_BYTES = CHUNK_FRAMES * BYTES_PER_FRAME
SINK_NAME = "sendspin-test-sink"
TONE_WAV = "/tmp/tone.wav"


async def main() -> int:
    subprocess.run(
        [
            "pw-cli",
            "create-node",
            "adapter",
            f"{{ factory.name=support.null-audio-sink node.name={SINK_NAME} "
            "media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }",
        ],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    await asyncio.sleep(1)

    received_chunks: list[bytes] = []
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
            for _ in range(50):
                try:
                    client.info  # noqa: B018
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
            buffer_capacity=SAMPLE_RATE * BYTES_PER_FRAME * 2,
            supported_commands=[PlayerCommand.VOLUME, PlayerCommand.MUTE],
        )
        client = ClientSideSendspinClient(
            client_id="spike3-fake-esp32",
            client_name="Spike3 Fake ESP32",
            roles=[Roles.PLAYER],
            player_support=player_support,
        )

        def on_audio_chunk(_ts: int, audio_data: bytes, _format: object) -> None:
            received_chunks.append(audio_data)

        client.add_audio_chunk_listener(on_audio_chunk)
        await client.connect(f"ws://127.0.0.1:{PORT}/sendspin")

        try:
            await asyncio.wait_for(push_stream_ready.wait(), timeout=10)
        except asyncio.TimeoutError:
            print("FAIL: push stream was never created", file=sys.stderr)
            return 1

        push_stream = push_stream_holder["stream"]
        fmt = ServerAudioFormat(sample_rate=SAMPLE_RATE, bit_depth=BIT_DEPTH, channels=CHANNELS, sample_type="int")

        # Real PipeWire capture: pw-record subscribes to the sink's monitor and
        # streams raw PCM to stdout, exactly as the real sendspin sink adapter
        # (Section 5.4b) would to get audio out of the routing graph.
        record_proc = await asyncio.create_subprocess_exec(
            "pw-record",
            "--target",
            SINK_NAME,
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

        # Something routes real audio into the sink, standing in for a phone/PC
        # source being linked to this room's output in the real architecture.
        play_proc = await asyncio.create_subprocess_exec(
            "pw-cat", "--target", SINK_NAME, "--playback", TONE_WAV,
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
        )

        captured_bytes = 0
        pushed_chunks = 0
        deadline = loop.time() + 6.0
        buffer = b""
        assert record_proc.stdout is not None
        while loop.time() < deadline:
            try:
                data = await asyncio.wait_for(record_proc.stdout.read(CHUNK_BYTES), timeout=0.5)
            except asyncio.TimeoutError:
                continue
            if not data:
                break
            captured_bytes += len(data)
            buffer += data
            while len(buffer) >= CHUNK_BYTES:
                chunk, buffer = buffer[:CHUNK_BYTES], buffer[CHUNK_BYTES:]
                push_stream.prepare_audio(chunk, fmt)
                await push_stream.commit_audio()
                pushed_chunks += 1

        record_proc.terminate()
        with contextlib_suppress():
            await asyncio.wait_for(record_proc.wait(), timeout=2)
        await play_proc.wait()

        await asyncio.sleep(1.0)
        await client.disconnect()
        await server.close()

    received_bytes = sum(len(c) for c in received_chunks)
    print(f"captured from PipeWire: {captured_bytes} bytes")
    print(f"pushed to aiosendspin:  {pushed_chunks} chunks ({pushed_chunks * CHUNK_BYTES} bytes)")
    print(f"received by fake ESP32: {len(received_chunks)} chunks ({received_bytes} bytes)")

    if captured_bytes == 0:
        print("FAIL: pw-record captured zero bytes from the PipeWire sink", file=sys.stderr)
        return 1
    if not received_chunks:
        print("FAIL: fake ESP32 client received zero audio chunks", file=sys.stderr)
        return 1
    if received_bytes < captured_bytes * 0.5:
        print("FAIL: client received far less audio than was captured from PipeWire", file=sys.stderr)
        return 1

    print("PASS: real PipeWire playback -> pw-record capture -> aiosendspin PushStream -> real WS client")
    return 0


class contextlib_suppress:
    """Minimal inline contextlib.suppress(asyncio.TimeoutError) to avoid an extra import line."""

    def __enter__(self) -> None:
        return None

    def __exit__(self, exc_type: object, exc: object, tb: object) -> bool:
        return exc_type is not None


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
