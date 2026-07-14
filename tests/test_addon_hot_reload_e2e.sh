#!/bin/bash
# End-to-end check of RAOP output hot-reload against the real add-on
# container (not a spike, not the native dev daemon): builds the actual
# multi-stage Dockerfile, starts it with NO configured outputs, then drives
# the runtime CRUD API and verifies the full chain live:
#
#   POST /api/outputs -> daemon loads libpipewire-module-raop-sink into its
#   own context -> node appears in /api/nodes -> DELETE unloads it -> node
#   disappears -> a restart with a persisted store re-loads it at startup.
#
# Uses an unroutable placeholder IP (192.0.2.1, RFC 5737 TEST-NET-1, same
# convention as tests/test_addon_phase1_e2e.sh) so the raop-sink never
# actually streams — we only prove the node is created/removed live. This
# does NOT prove real RAOP delivery (see test_spike02_raop_real_device.sh).
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

echo "--- building add-on image ---"
docker build -t "$IMAGE" "$ADDON_DIR"

# Start with no outputs at all.
echo '{ "outputs": [] }' > "$DATA_DIR/options.json"

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

echo "--- outputs list starts empty ---"
[ "$(curl -s "$BASE/api/outputs")" = "[]" ] || fail "expected empty /api/outputs at start"

echo "--- POST /api/outputs adds an output live ---"
ADD_CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/outputs" \
  -H 'content-type: application/json' \
  -d '{"name":"Test Placeholder","ip":"192.0.2.1","port":7000,"encryption":"auth_setup"}')
[ "$ADD_CODE" = "201" ] || fail "POST /api/outputs returned $ADD_CODE, expected 201"

echo "--- the raop-sink node appears in the live registry (no restart) ---"
FOUND=""
for _ in $(seq 1 15); do
  if curl -sf "$BASE/api/nodes" 2>/dev/null | grep -q "raop-out-test_placeholder"; then FOUND=1; break; fi
  sleep 1
done
[ -n "$FOUND" ] || fail "added output never appeared in /api/nodes"
curl -s "$BASE/api/outputs" | grep -q '"present":true' || fail "/api/outputs did not report present:true"
echo "OK: output loaded live"

echo "--- duplicate add is rejected (409) ---"
DUP_CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/outputs" \
  -H 'content-type: application/json' -d '{"name":"Test Placeholder","ip":"192.0.2.9"}')
[ "$DUP_CODE" = "409" ] || fail "duplicate add returned $DUP_CODE, expected 409"

echo "--- DELETE removes the output and its node live ---"
DEL_CODE=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/outputs/raop-out-test_placeholder")
[ "$DEL_CODE" = "200" ] || fail "DELETE returned $DEL_CODE, expected 200"
GONE=""
for _ in $(seq 1 15); do
  if ! curl -sf "$BASE/api/nodes" 2>/dev/null | grep -q "raop-out-test_placeholder"; then GONE=1; break; fi
  sleep 1
done
[ -n "$GONE" ] || fail "deleted output's node never disappeared from /api/nodes"
echo "OK: output unloaded live"

echo "--- DELETE of a nonexistent output is 404 ---"
NF_CODE=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/outputs/raop-out-nope")
[ "$NF_CODE" = "404" ] || fail "DELETE nonexistent returned $NF_CODE, expected 404"

echo "--- persisted store is re-loaded at startup ---"
# Add one back, confirm it persisted, then restart the container.
curl -s -X POST "$BASE/api/outputs" -H 'content-type: application/json' \
  -d '{"name":"Persisted One","ip":"192.0.2.5"}' >/dev/null
grep -q "Persisted One" "$DATA_DIR/raop-outputs.json" || fail "output was not persisted to /data/raop-outputs.json"
docker restart "$CONTAINER_NAME" >/dev/null
for _ in $(seq 1 30); do curl -sf "$BASE/health" >/dev/null 2>&1 && break; sleep 1; done
FOUND=""
for _ in $(seq 1 15); do
  if curl -sf "$BASE/api/nodes" 2>/dev/null | grep -q "raop-out-persisted_one"; then FOUND=1; break; fi
  sleep 1
done
[ -n "$FOUND" ] || fail "persisted output was not loaded at startup after restart"

echo
echo "PASS: add -> node appears live; dup -> 409; delete -> node disappears live; 404 for unknown; persisted store loads at startup"
