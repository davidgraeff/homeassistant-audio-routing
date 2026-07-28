#!/usr/bin/env bash
# Build the bridge-daemon binary in a container WITHOUT leaving root-owned
# files on the host.
#
# The problem this avoids: building the Rust daemon by bind-mounting the source
# into a container (`docker run -v "$PWD:/build" rust:… cargo build`) runs cargo
# as the container's root and writes `target/` back onto the host as `root:root`
# — files you then can't touch without sudo. Instead this builds the
# Dockerfile's `build` stage (same toolchain + libpipewire-0.3-dev the shipped
# add-on image uses, so it's a faithful build) and extracts just the compiled
# binary with `create` + `cp`, which the *client* writes as you — never root.
#
# It prefers podman (default + rootless on Fedora, so even bind mounts would be
# user-owned) and falls back to docker. Both are handled the same way here.
#
# Usage:
#   scripts/build-daemon.sh                 # -> pipewire_audio_router/bridge-daemon/dist/bridge-daemon
#   OUT=/somewhere scripts/build-daemon.sh  # override output directory
#   ENGINE=docker scripts/build-daemon.sh   # force a specific engine
#
# NOTE: the produced binary is linked against the container base's libraries
# (Debian trixie), so it's the artifact for the *image*, not necessarily
# runnable on this host. For fast host-side iteration and tests, a plain
# `cargo build` / `cargo test` in bridge-daemon/ is the right tool (native, and
# already user-owned).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ADDON_DIR="$REPO_ROOT/pipewire_audio_router"
OUT="${OUT:-$ADDON_DIR/bridge-daemon/dist}"

# The sendspin server role is a git submodule (pipewire_audio_router/submodules/
# sendspin), so a clone without --recursive leaves it empty. Fail here rather than
# deep inside cargo, which reports only "failed to read .../Cargo.toml".
if [ ! -f "$REPO_ROOT/pipewire_audio_router/submodules/sendspin/Cargo.toml" ]; then
  echo "ERROR: the sendspin submodule is not checked out." >&2
  echo "       Run: git submodule update --init --recursive" >&2
  exit 1
fi

TAG="pipewire-bridge-daemon-build:local"

# Engine: honor $ENGINE if set, else prefer podman (rootless), else docker.
if [ -n "${ENGINE:-}" ]; then
  command -v "$ENGINE" >/dev/null 2>&1 || { echo "error: requested ENGINE '$ENGINE' not found" >&2; exit 1; }
elif command -v podman >/dev/null 2>&1; then
  ENGINE=podman
elif command -v docker >/dev/null 2>&1; then
  ENGINE=docker
else
  echo "error: need podman or docker on PATH" >&2
  exit 1
fi
echo "--- building bridge-daemon with $ENGINE (output owned by $(id -un)) ---"

# Build only the Dockerfile's `build` stage — stops after `cargo build
# --release`, no need to assemble the full runtime image. Nothing is written to
# the host by this step (it all lives inside the image layers).
"$ENGINE" build --target build -t "$TAG" "$ADDON_DIR"

# Extract the binary. `create` makes a (non-running) container; `cp` streams the
# file out and the CLI writes it to the host as the invoking user. This is what
# keeps the result out of root's hands, and it works identically on podman and
# docker without depending on BuildKit output drivers.
mkdir -p "$OUT"
cid="$("$ENGINE" create "$TAG")"
trap '"$ENGINE" rm -f "$cid" >/dev/null 2>&1 || true' EXIT
# The build stage cross-compiles into target/<triple>/release/ (CARGO_BUILD_TARGET
# is always set in the Dockerfile) and copies the result to a fixed /bridge-daemon
# so extraction doesn't need to know the triple. Copy from that fixed path.
"$ENGINE" cp "$cid:/bridge-daemon" "$OUT/bridge-daemon"

echo "--- done ---"
ls -l "$OUT/bridge-daemon"
