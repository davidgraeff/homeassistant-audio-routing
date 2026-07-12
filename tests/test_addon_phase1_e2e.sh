#!/bin/bash
# Phase 1 end-to-end check for the real add-on (pipewire_audio_router/),
# not a spike: builds the actual multi-stage Dockerfile, runs it with a
# test RAOP output pointed at an unroutable placeholder IP (192.0.2.1,
# RFC 5737 TEST-NET-1, same convention as spikes/02), and verifies the
# full chain end to end:
#
#   options.json -> generate-config -> pipewire loads raop-sink ->
#   bridge-daemon's registry listener discovers the node ->
#   REST API reports it
#
# This does NOT prove real RAOP delivery to hardware (see
# tests/test_spike02_raop_real_device.sh for that, same caveat applies:
# no network path to real devices from this sandbox).
set -euo pipefail

ADDON_DIR="$(dirname "$0")/../pipewire_audio_router"
IMAGE="${IMAGE:-pipewire_audio_router:dev}"
CONTAINER_NAME="pw-addon-e2e-test"
DATA_DIR="$(mktemp -d)"
HOST_PORT="${HOST_PORT:-18080}"

cleanup() {
  docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT

echo "--- building add-on image ---"
docker build -t "$IMAGE" "$ADDON_DIR"

cat > "$DATA_DIR/options.json" << 'EOF'
{
  "outputs": [
    { "name": "Test Placeholder", "ip": "192.0.2.1", "port": 7000, "encryption": "auth_setup" }
  ]
}
EOF

docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER_NAME" -v "$DATA_DIR:/data" -p "$HOST_PORT:8080" "$IMAGE" >/dev/null

echo "--- waiting for bridge-daemon HTTP API ---"
READY=""
for _ in $(seq 1 30); do
  if curl -sf "http://localhost:$HOST_PORT/health" >/dev/null 2>&1; then
    READY=1
    break
  fi
  sleep 1
done
if [ -z "$READY" ]; then
  echo "FAIL: bridge-daemon never became healthy"
  docker logs "$CONTAINER_NAME"
  exit 1
fi
echo "OK: health endpoint responding"

echo "--- waiting for the RAOP node to be discovered ---"
FOUND=""
for _ in $(seq 1 15); do
  if curl -sf "http://localhost:$HOST_PORT/api/nodes" 2>/dev/null | grep -q "raop-out-test_placeholder"; then
    FOUND=1
    break
  fi
  sleep 1
done
if [ -z "$FOUND" ]; then
  echo "FAIL: configured output never appeared in /api/nodes"
  echo "--- container logs ---"
  docker logs "$CONTAINER_NAME"
  exit 1
fi

echo "--- /api/nodes response ---"
curl -s "http://localhost:$HOST_PORT/api/nodes"
echo
echo "PASS: options.json -> pipewire.conf.d -> PipeWire RAOP node -> bridge-daemon registry -> REST API, full chain verified"
