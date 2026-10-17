#!/bin/bash
# Run the bridge daemon's Rust test suite (`cargo test`) inside a container, so
# it doesn't depend on host-installed PipeWire / clang / libevent / ALSA dev
# libraries. The throwaway image mirrors the deps the add-on Dockerfile's
# `build` stage installs on its NATIVE path (see pipewire_audio_router/Dockerfile).
#
# Cargo caches (crate registry, git deps, and the compiled `target/`) live in
# named Docker volumes so reruns are incremental. The target dir is deliberately
# a container-local volume, NOT the host's bridge-daemon/target — the host tree
# is built with a different toolchain/glibc and mixing the two thrashes the
# cache (and can produce confusing fingerprint errors).
#
# Usage:
#   scripts/test-rust.sh                    # all tests
#   scripts/test-rust.sh rtp_membership     # a cargo test name filter
#   scripts/test-rust.sh -- --nocapture     # args after -- go to the test binary
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# The whole add-on directory is mounted, not just bridge-daemon/: the daemon has
# path dependencies on its siblings (../pw-control, ../submodules/sendspin).
ADDON_DIR="$REPO_ROOT/pipewire_audio_router"

# The sendspin server role is a git submodule (pipewire_audio_router/submodules/
# sendspin), so a clone without --recursive leaves it empty. Fail here rather than
# deep inside cargo, which reports only "failed to read .../Cargo.toml".
if [ ! -f "$REPO_ROOT/pipewire_audio_router/submodules/sendspin/Cargo.toml" ]; then
  echo "ERROR: the sendspin submodule is not checked out." >&2
  echo "       Run: git submodule update --init --recursive" >&2
  exit 1
fi

IMAGE="pw-router-test-rust"

echo "=== building test image $IMAGE (cached) ==="
docker build -t "$IMAGE" - <<'DOCKERFILE'
FROM rust:1-slim-trixie
# Same deps as the add-on Dockerfile's `build` stage: the pipewire/spa/alsa
# -sys crates link C libraries AND run bindgen (needs clang/libclang); libevent
# is for the vendored libairptp build.rs; git because the daemon pulls a
# `sendspin` git dependency. `g++` is required for the C++ crate `fdk-aac-sys`
# (provides cc1plus) — the add-on Dockerfile's native path assumes the base
# image ships it, but rust:1-slim-trixie does not, so install it explicitly.
# cmake + make build the bundled libopus for `opusic-sys` (sendspin's codec).
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config clang cmake make g++ git \
        libpipewire-0.3-dev libspa-0.2-dev libasound2-dev libevent-dev \
    && rm -rf /var/lib/apt/lists/*
DOCKERFILE

echo "=== cargo test (in container) ==="
exec docker run --rm -t \
    -v "$ADDON_DIR":/build \
    -v pw-router-cargo-registry:/usr/local/cargo/registry \
    -v pw-router-cargo-git:/usr/local/cargo/git \
    -v pw-router-rust-target:/target \
    -e CARGO_TARGET_DIR=/target \
    -w /build/bridge-daemon \
    "$IMAGE" \
    cargo test "$@"
