#!/bin/bash
# Phase 1 end-to-end check for the real add-on (pipewire_audio_router/),
# not a spike: builds the actual multi-stage Dockerfile, runs it with nothing
# configured, and verifies the two halves of the "config becomes reality" chain
# that everything else in the add-on stands on:
#
#   inputs:  POST /api/sources -> the daemon loads the source into its own
#            PipeWire context (module-rtp-source / the native AirPlay receiver's
#            producer) -> its registry listener discovers the node ->
#            GET /api/nodes and GET /api/sources report it
#
#   outputs: POST /api/outputs/<name>/adopt -> the adoption store (outputs_store.rs)
#            -> the output is in GET /api/outputs and in the routing matrix
#
# The output half used to be `POST /api/outputs` creating a RAOP sink at an
# unroutable placeholder IP. That path was removed in Phase 6 (raop_migration.rs,
# "drop-raop 2026-07"): receivers are reached as AirPlay-2 outputs, discovery only
# offers them and *adopting* is what makes one real, so adoption is the chain to
# prove. Nothing is behind `ap2-dev-test-placeholder`, which is deliberate — an
# adopted output stays listed while absent, and that is exactly the placeholder's
# old job.
#
# This does NOT prove audio delivery to a real device (see
# tests/test_addon_phase2_e2e.sh for a real sender and signal analysis).
set -euo pipefail

ADDON_DIR="$(dirname "$0")/../pipewire_audio_router"
IMAGE="${IMAGE:-pipewire_audio_router:dev}"
CONTAINER_NAME="pw-addon-e2e-test"
DATA_DIR="$(mktemp -d)"
HOST_PORT="${HOST_PORT:-18080}"
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

echo "--- building add-on image ---"
docker build -t "$IMAGE" "$ADDON_DIR"

# No options.json seeding — the daemon starts empty and everything below is
# created at runtime through the API. /data is mounted for its own stores.
docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
# --cap-add SYS_NICE/IPC_LOCK mirror the add-on config.yaml privileges. Without
# IPC_LOCK, PipeWire's mem.mlock-all (50-mlock.conf) mlockall() fails and 1.6.2
# then aborts context creation, so PipeWire never comes up in the container.
docker run -d --cap-add SYS_NICE --cap-add IPC_LOCK --name "$CONTAINER_NAME" -v "$DATA_DIR:/data" -p "$HOST_PORT:8099" "$IMAGE" >/dev/null

echo "--- waiting for bridge-daemon HTTP API ---"
READY=""
for _ in $(seq 1 30); do
  if curl -sf "$BASE/health" >/dev/null 2>&1; then
    READY=1
    break
  fi
  sleep 1
done
[ -n "$READY" ] || fail "bridge-daemon never became healthy"
echo "OK: health endpoint responding"

echo "--- adding both source kinds via the API ---"
# Both kinds, because they reach PipeWire by different mechanisms: RTP loads
# libpipewire-module-rtp-source, AirPlay runs the native receiver and pushes
# through its own producer stream. One passing tells you nothing about the other.
for BODY in \
  '{"label":"Test Bridge","kind":"rtp","rtp":{"port":46200}}' \
  '{"label":"Test AirPlay","kind":"airplay"}'; do
  CODE=$(curl -s -o /tmp/phase1_add.json -w '%{http_code}' -X POST "$BASE/api/sources" \
    -H 'Content-Type: application/json' -d "$BODY")
  [ "$CODE" = "201" ] || fail "POST /api/sources returned $CODE for $BODY: $(cat /tmp/phase1_add.json)"
done
rm -f /tmp/phase1_add.json

echo "--- waiting for both source nodes to be discovered ---"
# Node names derive from the label's slug (sources_store.rs source_node_name).
for NODE in rtp-in-test-bridge airplay-in-test-airplay; do
  FOUND=""
  for _ in $(seq 1 20); do
    if curl -sf "$BASE/api/nodes" 2>/dev/null | grep -q "$NODE"; then FOUND=1; break; fi
    sleep 1
  done
  [ -n "$FOUND" ] || fail "source node '$NODE' never appeared in /api/nodes"
done
SOURCES=$(curl -s "$BASE/api/sources")
echo "$SOURCES" | grep -q '"present":false' && fail "a configured source is not running: $SOURCES"
echo "OK: both sources loaded into the daemon's PipeWire context and reported present"

echo "--- an unadopted output is offered, not routable ---"
# Nothing has been added yet, so both listings that mean "the system's outputs"
# must be empty. This is the gate itself: discovery alone must never make a
# device routable or turn it into a Home Assistant entity.
[ "$(curl -s "$BASE/api/outputs")" = "[]" ] || fail "expected no adopted outputs before adding one"
curl -s "$BASE/api/routing" | grep -q '"outputs":\[\]' || fail "expected no outputs in the matrix before adding one"

echo "--- adopting an output puts it in the listing and the matrix ---"
# The verdict on a write is the HTTP STATUS, not a field in the body: success is
# `200 {message}`, a refusal is its own 4xx/5xx with `{kind, message}` — see
# bridge-daemon/src/api/error/mod.rs, which says why there is no `ok` flag any
# more ("the status already said so"). These assertions used to grep for
# `"ok":true`, which silently became unprovable when that field went: the daemon
# answered 200 and the test called it a failure. Same `\n%{http_code}` idiom the
# source assertions above and hot_reload_e2e use, so the message still shows the
# body — `$ADOPT` is body-then-status.
ADOPT=$(curl -s -w '\n%{http_code}' -X POST "$BASE/api/outputs/ap2-dev-test-placeholder/adopt")
[ "$(tail -1 <<<"$ADOPT")" = "200" ] || fail "adopt was refused: $ADOPT"
OUTS=$(curl -s "$BASE/api/outputs")
echo "$OUTS" | grep -q '"state":"adopted"' || fail "output not reported as adopted: $OUTS"
MATRIX=$(curl -s "$BASE/api/routing")
echo "$MATRIX" | grep -q 'ap2-dev-test-placeholder' || fail "adopted output missing from the matrix: $MATRIX"
echo "OK: adopted output reaches both the outputs listing and the routing matrix"

echo "--- /api/nodes response ---"
curl -s "$BASE/api/nodes"
echo
echo "PASS: POST /api/sources -> daemon loads the source -> PipeWire node -> registry -> REST, and adopt -> outputs listing + routing matrix, full chain verified"
