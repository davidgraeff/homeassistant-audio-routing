#!/bin/bash
# Run the Home Assistant custom-component test suite (pytest) inside a container,
# so it doesn't need `homeassistant` + `pytest-homeassistant-custom-component`
# installed on the host (the reason these couldn't be run locally before).
#
# The suite drives real HA internals (config-flow machinery, the
# DataUpdateCoordinator, entity platforms, the state machine) with only the
# daemon network client mocked — see custom_components/pipewire_audio_router/tests/.
# `pytest.ini` at the repo root supplies pythonpath + asyncio_mode.
#
# The repo is mounted READ-ONLY and bytecode/cache writes are disabled, so a
# containerized (root) run can't litter the host tree with root-owned
# __pycache__/.pytest_cache. Temp state goes to the container's own /tmp + HOME.
#
# Usage:
#   scripts/test-custom-components.sh                 # all
#   scripts/test-custom-components.sh -k rtp -v       # extra pytest args passed through
#   scripts/test-custom-components.sh custom_components/pipewire_audio_router/tests/test_rtp_source.py
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="pw-router-test-hass"

echo "=== building test image $IMAGE (cached) ==="
docker build -t "$IMAGE" - <<'DOCKERFILE'
# Full python:3.13 (not -slim): HA's dependency tree occasionally needs a
# compiler for a wheel; the full image ships the build tooling, avoiding flaky
# installs. pytest-homeassistant-custom-component pins the compatible
# homeassistant, so pip resolves the two together (the integration's own
# manifest requirements are empty).
FROM python:3.13
RUN pip install --no-cache-dir pytest-homeassistant-custom-component homeassistant
DOCKERFILE

# Default target = the whole suite; overridden if the caller passes their own
# path(s). We only inject the default when no positional path is given.
default_target="custom_components/pipewire_audio_router/tests/"
case " $* " in
  *" custom_components/"*|*" tests/"*) target="" ;;   # caller gave a path
  *) target="$default_target" ;;
esac

echo "=== pytest (in container) ==="
exec docker run --rm -t \
    -v "$REPO_ROOT":/repo:ro \
    -w /repo \
    -e HOME=/tmp \
    -e PYTHONDONTWRITEBYTECODE=1 \
    "$IMAGE" \
    python -m pytest $target \
        -p pytest_homeassistant_custom_component \
        -p no:cacheprovider \
        "$@"
