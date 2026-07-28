#!/bin/bash
# Run the add-on end-to-end integration suite (tests/test_addon_*.sh). Each test
# builds+boots the REAL add-on Docker image and drives it over its REST API,
# capturing actual audio with pw-record to prove the path — the container is the
# isolated environment for the code under test.
#
# This runner builds the add-on image ONCE and points every test at it (via the
# `IMAGE` env each test already honors), instead of each test rebuilding it.
#
# Unlike the rust/pytest runners, this one runs ON THE HOST: the tests
# orchestrate the host's Docker daemon (spinning up the add-on + helper
# containers on a bridge network) and use host-side `ffmpeg` and the `pw-*`
# tools for signal generation/capture. So the host needs: docker, ffmpeg, and
# pipewire-utils (pw-record/pw-cli). Tests that need Music Assistant's
# proprietary `cliraop` AirPlay sender self-SKIP unless CLIRAOP_PATH is set.
#
# Usage:
#   scripts/test-integration.sh                    # build image + run all addon e2e tests
#   scripts/test-integration.sh phase1 hot_reload  # only tests whose name matches a filter
#   IMAGE=pipewire_audio_router:dev scripts/test-integration.sh   # reuse a prebuilt image
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ADDON_DIR="$REPO_ROOT/pipewire_audio_router"
TESTS_DIR="$REPO_ROOT/tests"
export IMAGE="${IMAGE:-pipewire_audio_router:dev}"

command -v docker >/dev/null || { echo "docker is required on the host" >&2; exit 1; }

echo "=== building add-on image $IMAGE (once, shared by all tests) ==="
docker build -t "$IMAGE" "$ADDON_DIR"

# Select tests: all test_addon_*.sh, or only those matching a name filter arg.
mapfile -t all < <(ls "$TESTS_DIR"/test_addon_*.sh 2>/dev/null)
[ "${#all[@]}" -gt 0 ] || { echo "no tests/test_addon_*.sh found" >&2; exit 1; }
tests=()
if [ "$#" -eq 0 ]; then
    tests=("${all[@]}")
else
    for t in "${all[@]}"; do
        for pat in "$@"; do [[ "$(basename "$t")" == *"$pat"* ]] && { tests+=("$t"); break; }; done
    done
fi

pass=0 skip=0 fail=0
failed=()
for t in "${tests[@]}"; do
    name="$(basename "$t")"
    echo; echo "############################## $name ##############################"
    log="$(mktemp)"
    # Stream live AND keep a log; PIPESTATUS[0] is the test's exit code, not tee's.
    bash "$t" 2>&1 | tee "$log"
    rc="${PIPESTATUS[0]}"
    if [ "$rc" -ne 0 ]; then
        echo "→ FAIL ($name)"; fail=$((fail + 1)); failed+=("$name")
    elif grep -q "^SKIP:" "$log"; then
        echo "→ SKIP ($name)"; skip=$((skip + 1))
    else
        echo "→ PASS ($name)"; pass=$((pass + 1))
    fi
    rm -f "$log"
done

echo
echo "=== integration suite: $pass passed, $skip skipped, $fail failed ==="
if [ "$fail" -ne 0 ]; then
    printf '  failed: %s\n' "${failed[@]}"
    exit 1
fi
