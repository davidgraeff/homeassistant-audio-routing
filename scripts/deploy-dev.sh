#!/bin/bash
# Fast iteration loop against a real Home Assistant OS instance over SSH —
# tier 2 of the dev loop (tier 1 is tests/test_addon_*.sh on this machine,
# no Pi round-trip needed for ordinary logic changes; this script is for
# real-hardware/network validation once a change looks right locally).
#
# Two independent targets:
#   ./scripts/deploy-dev.sh addon         rsync the add-on to Supervisor's
#                                         local add-ons folder, then
#                                         install (first time) or rebuild
#                                         (subsequent times) and tail logs.
#   ./scripts/deploy-dev.sh integration   rsync the HA integration into
#                                         config/custom_components/ and do
#                                         a targeted HA *core* restart
#                                         (seconds, not a full OS reboot).
#
# Requires: SSH access to the HA OS host (default homeassistant.local,
# override with HA_HOST) with a user that can write /addons and /config
# and run the `ha` CLI — confirmed working as root over the built-in SSH
# add-on/HAOS SSH on the target instance this was developed against.
set -euo pipefail

HA_HOST="${HA_HOST:-homeassistant.local}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ADDON_SLUG="local_pipewire_audio_router"

usage() {
  echo "usage: $0 addon|integration" >&2
  exit 1
}

deploy_addon() {
  echo "--- rsyncing pipewire_audio_router/ to $HA_HOST:/addons/pipewire_audio_router/ ---"
  # bridge-daemon/target/ is a local build cache (gitignored) and is huge —
  # never worth syncing, Supervisor builds fresh via the Dockerfile anyway.
  rsync -az --delete \
    --exclude 'bridge-daemon/target/' \
    "$REPO_ROOT/pipewire_audio_router/" "root@$HA_HOST:/addons/pipewire_audio_router/"

  if ssh "root@$HA_HOST" "ha apps info $ADDON_SLUG" >/dev/null 2>&1; then
    echo "--- already installed, rebuilding $ADDON_SLUG ---"
    ssh "root@$HA_HOST" "ha apps rebuild $ADDON_SLUG"
  else
    echo "--- installing $ADDON_SLUG for the first time ---"
    ssh "root@$HA_HOST" "ha apps install $ADDON_SLUG"
  fi

  echo "--- starting (or restarting) $ADDON_SLUG ---"
  ssh "root@$HA_HOST" "ha apps restart $ADDON_SLUG"

  echo "--- tailing logs (Ctrl-C to stop) ---"
  ssh "root@$HA_HOST" "ha apps logs $ADDON_SLUG"
}

deploy_integration() {
  echo "--- rsyncing custom_components/pipewire_audio_router/ to $HA_HOST:/config/custom_components/pipewire_audio_router/ ---"
  rsync -az --delete \
    --exclude '__pycache__/' \
    --exclude 'tests/' \
    "$REPO_ROOT/custom_components/pipewire_audio_router/" \
    "root@$HA_HOST:/config/custom_components/pipewire_audio_router/"

  echo "--- restarting HA core (not the OS — seconds, not minutes) ---"
  ssh "root@$HA_HOST" "ha core restart"
}

case "${1:-}" in
  addon) deploy_addon ;;
  integration) deploy_integration ;;
  *) usage ;;
esac
