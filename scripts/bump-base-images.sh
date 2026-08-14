#!/usr/bin/env bash
# Refresh the digest pins in pipewire_audio_router/Dockerfile.
#
# The bases are pinned as `tag@sha256:…` because a floating tag silently
# invalidates every layer under it — `rust:1-slim-trixie` was republished
# between two CI runs a day apart and the daemon recompiled from scratch for 7½
# minutes on a warm registry cache. Pinning turns that into a deliberate,
# reviewable change; this script is how you make it.
#
# Usage:
#   scripts/bump-base-images.sh            # show what would change
#   scripts/bump-base-images.sh --write    # rewrite the Dockerfile
#
# After --write, expect the next build to be slow (the pins moved, so the layers
# below them are cold) and the one after that to be fast again. Read the release
# notes of anything that jumped a major/minor before committing: the runtime base
# decides the shipped PipeWire/WirePlumber versions, and the two Rust bases decide
# the toolchain plus the GLIBC floor the agent binaries promise their downloaders.
set -euo pipefail

DOCKERFILE="$(cd "$(dirname "$0")/.." && pwd)/pipewire_audio_router/Dockerfile"
WRITE=0
# `if`, not `[ … ] && WRITE=1`: under `set -e` a bare test that fails IS the
# script's exit status, so the no-argument case would quit here.
if [ "${1:-}" = "--write" ]; then WRITE=1; fi

command -v docker >/dev/null || { echo "ERROR: docker (with buildx) is required" >&2; exit 1; }

# The tags to resolve are read out of the Dockerfile itself rather than listed
# here, so a new stage cannot be forgotten: every `FROM name:tag@sha256:…` line
# is picked up, whatever its stage name or --platform prefix.
mapfile -t pinned < <(grep -oE '^FROM (--platform=[^ ]+ )?[^ ]+@sha256:[0-9a-f]{64}' "$DOCKERFILE" | grep -oE '[^ ]+@sha256:[0-9a-f]{64}')
[ "${#pinned[@]}" -gt 0 ] || { echo "ERROR: no digest-pinned FROM lines found in $DOCKERFILE" >&2; exit 1; }

changed=0
for ref in "${pinned[@]}"; do
  tag="${ref%@*}"
  old="${ref#*@}"
  # imagetools reports the digest of the manifest *index*, which is what a
  # `FROM tag` resolves to and therefore what must be pinned — not the digest of
  # any single-platform manifest inside it (this image is built for two arches).
  # Resolved in two steps, and `|| true`: `set -o pipefail` plus an `awk … exit`
  # that closes the pipe early makes docker die of SIGPIPE, which fails the
  # assignment, which under `set -e` ends the script mid-loop with no message.
  # `tail` reads to EOF, so nothing gets a broken pipe.
  info="$(docker buildx imagetools inspect "$tag" 2>/dev/null || true)"
  new="$(printf '%s\n' "$info" | awk '/^Digest:/{print $2}' | tail -1)"
  if [ -z "$new" ]; then
    echo "!! $tag — could not resolve (offline? rate-limited?), left alone" >&2
    continue
  fi
  if [ "$new" = "$old" ]; then
    echo "== $tag already current (${old:0:19}…)"
    continue
  fi
  echo ">> $tag  ${old:0:19}… -> ${new:0:19}…"
  changed=1
  if [ "$WRITE" = 1 ]; then
    # The tag is unique per FROM line, so replacing "tag@old" is unambiguous.
    tmp="$(mktemp)"
    sed "s|${tag}@${old}|${tag}@${new}|g" "$DOCKERFILE" > "$tmp"
    mv "$tmp" "$DOCKERFILE"
  fi
done

if [ "$changed" = 0 ]; then
  echo "nothing to do — every base is at its tag's current digest"
elif [ "$WRITE" = 1 ]; then
  echo "Dockerfile updated. Build once to confirm, then commit the pins on their own."
else
  echo "run again with --write to apply"
fi
