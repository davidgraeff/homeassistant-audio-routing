#!/bin/bash
# Phase 4 check: the manual routing UI (PLAN.md Section 8) — through the
# real add-on binary, not a bare-PipeWire stand-in.
#
# Verifies, against a live container:
# 1. GET / serves the routing UI HTML.
# 2. GET /api/routing reports configured outputs and no sources when
#    nothing is playing yet.
# 3. A real source node appearing in the PipeWire registry shows up in the
#    matrix.
# 4. POST /api/routing/link actually creates the right per-channel
#    pw-link connections (confirmed via `pw-link -l`, not just the JSON
#    response) and the matrix reflects it.
# 5. POST /api/routing/unlink actually removes them.
# 6. The /api/events WebSocket, subscribed to the `matrix` topic, pushes a
#    live snapshot (the subscribe answers with the current state, then
#    another arrives) the moment a real link change happens in PipeWire —
#    driven by pw_thread.rs's own registry listener, not polled.
#
# Needs a WebSocket client; the host's system python3 is broken in this
# dev environment (missing libpython shared lib, unrelated to this
# project), so step 6 runs a throwaway `python3-slim` container with
# `--network host` instead of assuming a working local python3.
set -euo pipefail

ADDON_DIR="$(dirname "$0")/../pipewire_audio_router"
IMAGE="${IMAGE:-pipewire_audio_router:dev}"
CONTAINER_NAME="pw-addon-routing-ui-test"
NETWORK_NAME="pw-addon-routing-ui-net"
DATA_DIR="$(mktemp -d)"
HOST_PORT="${HOST_PORT:-18400}"

cleanup() {
  docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
  # Bounded with `timeout` on purpose: `docker network rm` right after
  # `docker rm -f` on the same network has been observed to hang for minutes.
  timeout 5 docker network rm "$NETWORK_NAME" >/dev/null 2>&1 || true
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT

echo "--- building add-on image ---"
docker build -t "$IMAGE" "$ADDON_DIR"

# No options.json seeding — two virtual test sinks are created below once the
# daemon is up.
docker network create "$NETWORK_NAME" >/dev/null 2>&1 || true
docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
# --cap-add SYS_NICE/IPC_LOCK mirror the add-on config.yaml privileges. Without
# IPC_LOCK, PipeWire's mem.mlock-all (50-mlock.conf) mlockall() fails and 1.6.2
# then aborts context creation, so PipeWire never comes up in the container.
docker run -d --cap-add SYS_NICE --cap-add IPC_LOCK --name "$CONTAINER_NAME" --network "$NETWORK_NAME" \
  -v "$DATA_DIR:/data" -p "$HOST_PORT:8099" "$IMAGE" >/dev/null

echo "--- waiting for the bridge-daemon HTTP API, then creating two virtual test sinks ---"
for _ in $(seq 1 30); do curl -sf "http://localhost:$HOST_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
# Sendspin outputs are discovery-only now (no create API) and need real devices
# on the network, which CI has none of. Stand in plain null-audio-sinks named
# with the `sendspin-out-` prefix so the daemon lists them as outputs exactly as
# real sendspin sinks would be — this test only exercises the routing matrix /
# link API against them, which is name-based.
for NAME in kitchen bedroom; do
  docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c "
pw-cli create-node adapter \"{ factory.name=support.null-audio-sink node.name=sendspin-out-$NAME media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }\" >/dev/null
" || { echo "FAIL: could not create the virtual test sink ($NAME)"; docker logs "$CONTAINER_NAME"; exit 1; }
  # Being in the graph is not enough: since the adoption gate (outputs_store.rs)
  # the matrix lists only *adopted* outputs, so a discovered-but-unadded device
  # is deliberately absent. Adding is what a user does on the Outputs page.
  # 200-or-nothing: a write's verdict is its status, there is no `ok` field in the
  # body (bridge-daemon/src/api/error/mod.rs; phase1_e2e explains the switch).
  ADOPT=$(curl -s -w '\n%{http_code}' -X POST "http://localhost:$HOST_PORT/api/outputs/sendspin-out-$NAME/adopt")
  [ "$(tail -1 <<<"$ADOPT")" = "200" ] || { echo "FAIL: could not add output sendspin-out-$NAME: $ADOPT"; exit 1; }
done

echo "--- waiting for both outputs ---"
READY=""
for _ in $(seq 1 30); do
  MATRIX=$(curl -sf "http://localhost:$HOST_PORT/api/routing" 2>/dev/null || true)
  if echo "$MATRIX" | grep -q 'sendspin-out-kitchen' && echo "$MATRIX" | grep -q 'sendspin-out-bedroom'; then
    READY=1
    break
  fi
  sleep 1
done
if [ -z "$READY" ]; then
  echo "FAIL: outputs never appeared in /api/routing: $MATRIX"
  docker logs "$CONTAINER_NAME"
  exit 1
fi
echo "OK: both outputs present, no sources yet: $MATRIX"
echo "$MATRIX" | grep -q '"sources":\[\]' || { echo "FAIL: expected no sources before any were created"; exit 1; }

echo "--- GET / serves the routing UI HTML ---"
HTML=$(curl -sf "http://localhost:$HOST_PORT/")
echo "$HTML" | grep -q "PipeWire Audio Router" || { echo "FAIL: routing UI HTML missing expected title"; exit 1; }
echo "OK: routing UI HTML served"

echo "--- creating a real source node ---"
docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" bash -c '
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=test-music-src media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }" >/dev/null
'
# Routing is name-based now (stable node names, not ephemeral PipeWire ids —
# see routing.rs / the /api/routing/link API), so the link/unlink calls below
# address endpoints by name. We only need to wait for the source to be
# classified and appear in the matrix.
SOURCE_NAME="test-music-src"
OUTPUT_NAME="sendspin-out-kitchen"
FOUND=""
for _ in $(seq 1 30); do
  MATRIX=$(curl -sf "http://localhost:$HOST_PORT/api/routing" 2>/dev/null || true)
  echo "$MATRIX" | grep -q "\"node_name\":\"$SOURCE_NAME\"" && { FOUND=1; break; }
  sleep 1
done
if [ -z "$FOUND" ]; then
  echo "FAIL: $SOURCE_NAME never appeared as a source in /api/routing: $MATRIX"
  exit 1
fi
echo "OK: source '$SOURCE_NAME' present, output '$OUTPUT_NAME' present"

echo "--- POST /api/routing/link ---"
LINK_RESPONSE=$(curl -s -w '\n%{http_code}' -X POST "http://localhost:$HOST_PORT/api/routing/link" -H 'Content-Type: application/json' \
  -d "{\"source\":\"$SOURCE_NAME\",\"output\":\"$OUTPUT_NAME\"}")
[ "$(tail -1 <<<"$LINK_RESPONSE")" = "200" ] || { echo "FAIL: link request was refused: $LINK_RESPONSE"; exit 1; }
sleep 0.5
REAL_LINKS=$(docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" pw-link -l)
echo "$REAL_LINKS" | grep -q "test-music-src:capture_FL" || { echo "FAIL: real pw-link state missing the FL link after /api/routing/link"; echo "$REAL_LINKS"; exit 1; }
MATRIX=$(curl -sf "http://localhost:$HOST_PORT/api/routing")
echo "$MATRIX" | grep -q "\"source\":\"$SOURCE_NAME\",\"output\":\"$OUTPUT_NAME\"" || { echo "FAIL: matrix does not show the new link: $MATRIX"; exit 1; }
echo "OK: link created and reflected both in real PipeWire state and the matrix"

echo "--- POST /api/routing/unlink ---"
UNLINK_RESPONSE=$(curl -s -w '\n%{http_code}' -X POST "http://localhost:$HOST_PORT/api/routing/unlink" -H 'Content-Type: application/json' \
  -d "{\"source\":\"$SOURCE_NAME\",\"output\":\"$OUTPUT_NAME\"}")
[ "$(tail -1 <<<"$UNLINK_RESPONSE")" = "200" ] || { echo "FAIL: unlink request was refused: $UNLINK_RESPONSE"; exit 1; }
sleep 0.5
REAL_LINKS=$(docker exec -e XDG_RUNTIME_DIR=/run/pipewire "$CONTAINER_NAME" pw-link -l)
echo "$REAL_LINKS" | grep -q "test-music-src:capture_FL" && { echo "FAIL: real pw-link state still shows the FL link after /api/routing/unlink"; echo "$REAL_LINKS"; exit 1; }
MATRIX=$(curl -sf "http://localhost:$HOST_PORT/api/routing")
echo "$MATRIX" | grep -q "\"source\":\"$SOURCE_NAME\",\"output\":\"$OUTPUT_NAME\"" && { echo "FAIL: matrix still shows the link after unlink: $MATRIX"; exit 1; }
echo "OK: unlink removed the real PipeWire link and the matrix no longer shows it"

echo "--- WebSocket: initial snapshot + live push on a real link change ---"
# "|| true" on the assignment below, because of set -e: a failing assertion inside
# the python exits the container non-zero, which makes the assignment fail, which
# killed this script right there — before the echo could print the traceback saying
# what the socket actually pushed. The grep two lines further down is the
# assertion; the assignment only has to survive to reach it. (No backticks in
# these comments: everything up to the closing quote is inside a double-quoted
# string, where a backtick would start a command substitution.)
WS_LOG=$(docker run --rm --network host python:3.13-slim bash -c "
pip install --quiet --no-cache-dir websockets 2>&1 >/dev/null
python3 - << PYEOF
import asyncio, json, urllib.request
import websockets

# One socket for every push feed (events/mod.rs), internally tagged by 'type':
# matrix, outputs, discovered, agents, now_playing, meters, align, … plus the
# 'subscribed' acknowledgement. Only the matrix frame carries links — reading
# 'links' off any other would KeyError, so every read below skips what it isn't
# looking for.
async def matrix(ws, timeout=10):
    while True:
        frame = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        if frame.get('type') == 'matrix':
            return frame

async def main():
    # Topics are SUBSCRIBED, not in the URL: this replaced the four status sockets
    # (/api/routing/ws among them), because a browser gives one host six HTTP/1.1
    # connections and the pages held four of them open. Subscribing sends that
    # topic's current state at once, which is what the old connect-snapshot became.
    async with websockets.connect('ws://localhost:$HOST_PORT/api/events') as ws:
        await ws.send(json.dumps({'op': 'subscribe', 'topics': ['matrix']}))
        initial = await matrix(ws)
        assert initial['links'] == [], f'expected no links in initial snapshot, got {initial[\"links\"]}'
        req = urllib.request.Request(
            'http://localhost:$HOST_PORT/api/routing/link',
            data=json.dumps({'source': '$SOURCE_NAME', 'output': '$OUTPUT_NAME'}).encode(),
            headers={'Content-Type': 'application/json'},
        )
        urllib.request.urlopen(req).read()
        # Read matrix frames until the link shows up (bounded) rather than
        # assuming the very next one reflects it: the change notifier can coalesce
        # and the listing frames interleave.
        want = {'source': '$SOURCE_NAME', 'output': '$OUTPUT_NAME'}
        pushed = {'links': None}
        found = False
        for _ in range(40):
            pushed = await matrix(ws)
            if want in pushed['links']:
                found = True
                break
        assert found, f'link never appeared in a pushed snapshot; last saw {pushed[\"links\"]}'
        print('WS_OK')

asyncio.run(main())
PYEOF
" 2>&1) || true
echo "$WS_LOG" | tail -20
echo "$WS_LOG" | grep -q "WS_OK" || { echo "FAIL: WebSocket did not push a live snapshot reflecting the real link change"; exit 1; }
echo "OK: WebSocket pushed a live snapshot driven by the real PipeWire registry change"

echo "PASS: manual routing UI (matrix API, link/unlink, live WebSocket updates) verified end-to-end against the real add-on binary"
