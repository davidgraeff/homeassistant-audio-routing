#!/bin/bash
# End-to-end check of **runtime reconfiguration** against the real add-on
# container (not a spike, not the native dev daemon): builds the actual
# multi-stage Dockerfile, starts it with nothing configured, then drives the
# runtime CRUD APIs and verifies the full chain live:
#
#   POST /api/sources -> daemon loads the source's PipeWire module/producer into
#   its own context -> node appears in /api/nodes -> PUT reconfigures it in place
#   -> DELETE unloads it -> node disappears -> a restart re-loads what /data
#   persisted.
#
# Plus the same "survives a restart" property for the adoption store: adding an
# output is a decision, and the daemon must still hold it after a reboot.
#
# This was the RAOP-output hot-reload test (`POST /api/outputs` ->
# libpipewire-module-raop-sink). The RAOP *output* path was removed in Phase 6
# (see raop_migration.rs, "drop-raop 2026-07"): every receiver is reached as an
# AirPlay-2 output now, outputs are discovery-only, and `/api/outputs` is a
# read-only listing. The runtime module-load-and-unload behaviour it was written
# to protect lives on the **source** side, which is where it is exercised here.
#
# The RTP source listens on a UDP port nothing sends to, so no audio flows — this
# proves the node is created/reconfigured/removed live, not delivery (for real
# signal see test_addon_phase2_e2e.sh).
set -euo pipefail

ADDON_DIR="$(dirname "$0")/../pipewire_audio_router"
IMAGE="${IMAGE:-pipewire_audio_router:dev}"
CONTAINER_NAME="pw-addon-hotreload-test"
DATA_DIR="$(mktemp -d)"
HOST_PORT="${HOST_PORT:-18081}"
BASE="http://localhost:$HOST_PORT"

cleanup() {
  docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $1"
  echo "--- container logs ---"
  docker logs "$CONTAINER_NAME" 2>&1 | tail -40 || true
  exit 1
}

# Wait (bounded) for $1 to appear in /api/nodes; $2 = "gone" to wait for absence.
wait_for_node() {
  local name="$1" want="${2:-present}"
  for _ in $(seq 1 15); do
    if curl -sf "$BASE/api/nodes" 2>/dev/null | grep -q "$name"; then
      [ "$want" = "present" ] && return 0
    else
      [ "$want" = "gone" ] && return 0
    fi
    sleep 1
  done
  return 1
}

echo "--- building add-on image ---"
docker build -t "$IMAGE" "$ADDON_DIR"

docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
# --cap-add SYS_NICE/IPC_LOCK mirror the add-on config.yaml privileges. Without
# IPC_LOCK, PipeWire's mem.mlock-all (50-mlock.conf) mlockall() fails and 1.6.2
# then aborts context creation, so PipeWire never comes up in the container.
docker run -d --cap-add SYS_NICE --cap-add IPC_LOCK --name "$CONTAINER_NAME" -v "$DATA_DIR:/data" -p "$HOST_PORT:8099" "$IMAGE" >/dev/null

echo "--- waiting for bridge-daemon HTTP API ---"
READY=""
for _ in $(seq 1 30); do
  if curl -sf "$BASE/health" >/dev/null 2>&1; then READY=1; break; fi
  sleep 1
done
[ -n "$READY" ] || fail "bridge-daemon never became healthy"
echo "OK: health endpoint responding"

echo "--- both listings start empty (no seeding from options.json) ---"
curl -s "$BASE/api/sources" | grep -q '"sources":\[\]' || fail "expected no sources at start: $(curl -s "$BASE/api/sources")"
[ "$(curl -s "$BASE/api/outputs")" = "[]" ] || fail "expected empty /api/outputs at start"

echo "--- POST /api/sources adds a source live ---"
ADD=$(curl -s -w '\n%{http_code}' -X POST "$BASE/api/sources" -H 'content-type: application/json' \
  -d '{"label":"Test Bridge","kind":"rtp","rtp":{"port":46123,"latency_msec":200}}')
ADD_CODE=$(echo "$ADD" | tail -1)
ADD_BODY=$(echo "$ADD" | head -n -1)
[ "$ADD_CODE" = "201" ] || fail "POST /api/sources returned $ADD_CODE, expected 201: $ADD_BODY"
# The id is the slug of the label, and the node name derives from it
# (sources_store.rs source_node_name) — assert the contract rather than
# rediscovering it from the response, so a change in either is caught.
echo "$ADD_BODY" | grep -q '"id":"test-bridge"' || fail "expected the id to be the label's slug: $ADD_BODY"
NODE="rtp-in-test-bridge"
echo "$ADD_BODY" | grep -q "\"node_name\":\"$NODE\"" || fail "unexpected node name: $ADD_BODY"

echo "--- the source's node appears in the live registry (no restart) ---"
wait_for_node "$NODE" || fail "added source never appeared in /api/nodes"
curl -s "$BASE/api/sources" | grep -q '"present":true' || fail "/api/sources did not report present:true"
echo "OK: source loaded live"

echo "--- a second source on the same RTP port is rejected (400) ---"
DUP_CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/sources" \
  -H 'content-type: application/json' -d '{"label":"Clashing","kind":"rtp","rtp":{"port":46123}}')
[ "$DUP_CODE" = "400" ] || fail "duplicate RTP port returned $DUP_CODE, expected 400"

echo "--- PUT reconfigures the running source in place ---"
PUT_CODE=$(curl -s -o /dev/null -w '%{http_code}' -X PUT "$BASE/api/sources/test-bridge" \
  -H 'content-type: application/json' -d '{"rtp":{"port":46124,"latency_msec":350}}')
[ "$PUT_CODE" = "200" ] || fail "PUT /api/sources/test-bridge returned $PUT_CODE, expected 200"
# The node keeps its name (derived from the immutable id), so "still there" is
# only half the check — the new config has to be what the daemon is running on.
wait_for_node "$NODE" || fail "source node disappeared after a reconfigure"
VIEW=$(curl -s "$BASE/api/sources/test-bridge")
echo "$VIEW" | grep -q '"port":46124' || fail "reconfigured port not reported: $VIEW"
echo "$VIEW" | grep -q '"latency_msec":350' || fail "reconfigured latency not reported: $VIEW"
grep -q 46124 "$DATA_DIR/sources.json" || fail "reconfigure was not persisted to /data/sources.json"
echo "OK: reconfigured live and persisted"

echo "--- DELETE removes the source and its node live ---"
DEL_CODE=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/sources/test-bridge")
[ "$DEL_CODE" = "200" ] || fail "DELETE returned $DEL_CODE, expected 200"
wait_for_node "$NODE" gone || fail "deleted source's node never disappeared from /api/nodes"
echo "OK: source unloaded live"

echo "--- DELETE of a nonexistent source is 404 ---"
NF_CODE=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/sources/nope")
[ "$NF_CODE" = "404" ] || fail "DELETE nonexistent returned $NF_CODE, expected 404"

echo "--- adding an output is remembered too (adoption store) ---"
# No device is behind this name — that is the point: an adopted output stays
# listed while absent (grayed in the UI) so its routing survives the speaker
# being off. Same RFC 5737 spirit as the placeholder IP this test used to use.
# 200-or-nothing: a write's verdict is its status, there is no `ok` field in the
# body (bridge-daemon/src/api/error/mod.rs; phase1_e2e explains the switch).
ADOPT=$(curl -s -w '\n%{http_code}' -X POST "$BASE/api/outputs/ap2-dev-test-placeholder/adopt")
[ "$(tail -1 <<<"$ADOPT")" = "200" ] || fail "adopt was refused: $ADOPT"
OUTS=$(curl -s "$BASE/api/outputs")
echo "$OUTS" | grep -q 'ap2-dev-test-placeholder' || fail "adopted output missing from /api/outputs: $OUTS"
echo "$OUTS" | grep -q '"present":false' || fail "an output with no device behind it should not be present: $OUTS"
grep -q "ap2-dev-test-placeholder" "$DATA_DIR/outputs.json" || fail "adoption was not persisted to /data/outputs.json"

echo "--- persisted state is re-loaded at startup ---"
curl -s -X POST "$BASE/api/sources" -H 'content-type: application/json' \
  -d '{"label":"Persisted One","kind":"rtp","rtp":{"port":46125}}' >/dev/null
grep -q "Persisted One" "$DATA_DIR/sources.json" || fail "source was not persisted to /data/sources.json"
docker restart "$CONTAINER_NAME" >/dev/null
for _ in $(seq 1 30); do curl -sf "$BASE/health" >/dev/null 2>&1 && break; sleep 1; done
wait_for_node "rtp-in-persisted-one" || fail "persisted source was not loaded at startup after restart"
curl -s "$BASE/api/outputs" | grep -q 'ap2-dev-test-placeholder' || fail "adoption did not survive the restart"

echo
echo "PASS: add -> node appears live; clashing port -> 400; reconfigure in place; delete -> node disappears live; 404 for unknown; sources and adoptions both reload at startup"
