#!/usr/bin/env bash
# Stage the shared cast-receiver app into the YouTube Music add-on's build context.
#
# The app is canonical at firmware/pi-ytmusic/receiver/, because the Raspberry Pi
# role is installed by `scp -r firmware/pi-ytmusic` and therefore has to stay
# self-contained. Docker cannot COPY from outside its build context, so the files are
# copied into ytmusic_receiver/receiver/ immediately before a build — and that copy is
# gitignored, so it can never become a second source of truth.
#
# Staged rather than symlinked (Docker does not follow symlinks out of its context)
# and rather than moved (that would break the Pi's install path).
#
# Called by both builders — scripts/deploy-dev.sh for a dev push and
# .github/workflows/build-addon.yml for a release — so the image CI publishes is the
# image a dev push produces.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src="$repo/firmware/pi-ytmusic/receiver"
dst="$repo/ytmusic_receiver/receiver"

[ -f "$src/index.js" ] || { echo "ERROR: $src/index.js missing" >&2; exit 1; }
echo "--- staging shared receiver app from firmware/pi-ytmusic/receiver ---"
rm -rf "$dst"
mkdir -p "$dst"
# Only the app files. node_modules is npm's job inside the image, and copying a host
# build of it would mean armv7 binaries in an aarch64 image.
cp "$src"/*.js "$src"/*.py "$src"/*.html "$src"/package.json "$dst/"
# Fail loudly on a missing piece rather than shipping an image that merely degrades:
#   ytdlp_daemon.py / jsc_resident.py  the long-lived resolver and its resident
#     JS-challenge provider — without them playback still works, but with a per-track
#     yt-dlp and a 15 s challenge.
#   cookie_jar.py                      the jar rules; without it the admin page cannot
#     judge an upload, so cookies cannot be provisioned at all.
#   admin.js / admin.html              the admin page itself, which is what ingress
#     proxies to — its absence is a blank panel.
for required in ytdlp_daemon.py jsc_resident.py jsc_worker.js cookie_jar.py admin.js admin.html; do
  [ -f "$dst/$required" ] || { echo "ERROR: $src/$required missing" >&2; exit 1; }
done
