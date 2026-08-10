#!/bin/bash
# Fast iteration loop against a real Home Assistant OS instance over SSH —
# tier 2 of the dev loop (tier 1 is tests/test_addon_*.sh on this machine,
# no Pi round-trip needed for ordinary logic changes; this script is for
# real-hardware/network validation once a change looks right locally).
#
# Two independent targets:
#   ./scripts/deploy-dev.sh addon         cross-build the add-on image on THIS
#                                         host (docker buildx), push it to
#                                         GHCR, then have Supervisor pull it —
#                                         no on-device compile of the Rust
#                                         bridge daemon (that's minutes on a
#                                         Pi). See deploy_addon() for the why.
#   ./scripts/deploy-dev.sh integration   rsync the HA integration into
#                                         config/custom_components/ and do
#                                         a targeted HA *core* restart
#                                         (seconds, not a full OS reboot).
#
# Requires:
#   * SSH access to the HA OS host (default homeassistant.local, override with
#     HA_HOST) with a user that can write /addons and /config and run the `ha`
#     CLI — confirmed as root over the built-in SSH add-on/HAOS SSH.
#   * `addon` target only: docker with the buildx plugin on this host, and a
#     GHCR login that can push (docker login ghcr.io -u <user>, PAT with
#     write:packages — or set GHCR_TOKEN). Pruning old dev tags additionally
#     needs delete:packages (same token, or a working `gh auth`).
set -euo pipefail

HA_HOST="${HA_HOST:-homeassistant.local}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ADDON_SLUG="local_pipewire_audio_router"
ADDON_NAME="pipewire_audio_router"     # dir under repo root and image suffix

# The sendspin server role is a git submodule (pipewire_audio_router/submodules/
# sendspin), so a clone without --recursive leaves it empty. Fail here rather than
# deep inside cargo, which reports only "failed to read .../Cargo.toml".
if [ ! -f "$REPO_ROOT/pipewire_audio_router/submodules/sendspin/Cargo.toml" ]; then
  echo "ERROR: the sendspin submodule is not checked out." >&2
  echo "       Run: git submodule update --init --recursive" >&2
  exit 1
fi

GHCR_OWNER="${GHCR_OWNER:-davidgraeff}" # ghcr.io/<owner>/<arch>-addon-<name>
BUILDER="ha-addon-builder"             # dedicated buildx builder (see below)
DEV_TAGS_KEEP=3                        # how many dev image tags to retain on GHCR

usage() {
  echo "usage: $0 addon|ytmusic|integration" >&2
  exit 1
}

# The YouTube Music receiver add-on shares its application code with the
# Raspberry Pi deployment, which lives at firmware/pi-ytmusic/receiver/ and stays
# canonical: that role is installed by `scp -r firmware/pi-ytmusic`, so it has to
# remain self-contained. Docker cannot COPY from outside its build context, so the
# app is staged into the add-on directory here, immediately before the build.
#
# Staged rather than symlinked (Docker does not follow symlinks out of context) and
# rather than moved (that would break the Pi's install path).
stage_ytmusic_receiver() {
  local src="$REPO_ROOT/firmware/pi-ytmusic/receiver"
  local dst="$REPO_ROOT/ytmusic_receiver/receiver"
  [ -f "$src/index.js" ] || { echo "ERROR: $src/index.js missing" >&2; exit 1; }
  echo "--- staging shared receiver app from firmware/pi-ytmusic/receiver ---"
  rm -rf "$dst"
  mkdir -p "$dst"
  # Only the app files. node_modules is npm's job inside the image, and copying a
  # host build of it would mean armv7 binaries in an aarch64 image.
  cp "$src"/*.js "$src"/package.json "$dst/"
}

# Map the target's `uname -m` to Home Assistant's arch name and the buildx
# --platform string. config.yaml's image ref uses the HA arch as {arch}, so
# these must line up with what Supervisor substitutes on the target.
remote_ha_arch() {
  local m
  m="$(ssh "root@$HA_HOST" "uname -m")"
  case "$m" in
    x86_64) echo amd64 ;;
    aarch64) echo aarch64 ;;
    armv7l) echo armv7 ;;
    *) echo "unsupported target arch: $m" >&2; exit 1 ;;
  esac
}
platform_for() {
  case "$1" in
    amd64) echo linux/amd64 ;;
    aarch64) echo linux/arm64 ;;
    armv7) echo linux/arm/v7 ;;
  esac
}

# Fail early with an actionable message if this host can't push to GHCR,
# rather than deep inside a multi-minute buildx run. GHCR_TOKEN (a PAT with
# write:packages) auto-logs-in; otherwise we require an existing docker login.
preflight_ghcr() {
  command -v docker >/dev/null || { echo "ERROR: docker not found on this host" >&2; exit 1; }
  docker buildx version >/dev/null 2>&1 || { echo "ERROR: docker buildx plugin not available" >&2; exit 1; }
  if [ -n "${GHCR_TOKEN:-}" ]; then
    echo "$GHCR_TOKEN" | docker login ghcr.io -u "$GHCR_OWNER" --password-stdin >/dev/null
    return
  fi
  if [ -f "$HOME/.docker/config.json" ] && grep -q 'ghcr.io' "$HOME/.docker/config.json"; then
    return
  fi
  echo "ERROR: not logged in to ghcr.io. Either:" >&2
  echo "  docker login ghcr.io -u <github-user>   # paste a PAT with write:packages" >&2
  echo "  # or: export GHCR_TOKEN=<that PAT> and re-run" >&2
  exit 1
}

# A docker-container buildx builder (the default 'docker' driver can't export
# a registry cache and is awkward for cross-arch). Created once, reused after.
# Cross-arch emulation needs QEMU binfmt registered on the host; install it if
# the target platform isn't already advertised.
#
# --buildkitd-config: BuildKit's default GC policy caps cache mounts at 512MB/48h,
# which silently deletes the cargo caches this build leans on — see
# scripts/buildkitd.toml for the full reasoning. The config only takes effect at
# creation time, so a builder made before this existed keeps the default policy;
# that case gets a warning rather than a surprise recreate (recreating throws the
# whole cache away, which is the caller's call to make).
ensure_builder() {
  local platform="$1"
  if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
    echo "--- creating buildx builder '$BUILDER' (docker-container driver) ---"
    docker buildx create --name "$BUILDER" --driver docker-container \
      --buildkitd-config "$REPO_ROOT/scripts/buildkitd.toml" \
      --buildkitd-flags '--allow-insecure-entitlement=network.host' \
      --bootstrap >/dev/null
  elif ! docker buildx inspect "$BUILDER" | grep -qE 'Filters:[[:space:]]+type==exec\.cachemount$'; then
    echo "warning: builder '$BUILDER' predates scripts/buildkitd.toml, so BuildKit's" >&2
    echo "  default GC still caps its cargo cache mounts at 512MB/48h — a deploy after" >&2
    echo "  a two-day pause will recompile everything. Recreate it once (costs one full" >&2
    echo "  build) with:  docker buildx rm $BUILDER" >&2
  fi
  if ! docker buildx inspect "$BUILDER" | grep -q "$platform"; then
    echo "--- registering QEMU binfmt for cross-arch builds ---"
    docker run --privileged --rm docker.io/tonistiigi/binfmt --install all >/dev/null 2>&1 \
      || echo "warning: could not auto-install QEMU binfmt; cross-build may fail" >&2
    docker buildx inspect "$BUILDER" --bootstrap >/dev/null 2>&1 || true
  fi
}

# Resolve a GHCR API token for tag pruning: explicit env, then gh CLI, then
# reuse the credential docker login already stored. Empty if none available.
ghcr_token() {
  if [ -n "${GHCR_TOKEN:-}" ]; then echo "$GHCR_TOKEN"; return; fi
  local t
  t="$(gh auth token 2>/dev/null || true)"
  if [ -n "$t" ]; then echo "$t"; return; fi
  python3 - <<'PY' 2>/dev/null || true
import json, base64, os
try:
    d = json.load(open(os.path.expanduser("~/.docker/config.json")))
    a = d.get("auths", {}).get("ghcr.io", {}).get("auth")
    if a:
        print(base64.b64decode(a).decode().split(":", 1)[1])
except Exception:
    pass
PY
}

# Keep only the newest $DEV_TAGS_KEEP dev tags on GHCR. Dev and release tags now
# have the *same* shape — MAJOR.MINOR.<14-digit timestamp> — so, unlike the old
# 3-vs-4-segment scheme, segment count can no longer tell them apart. Instead we
# treat every version listed in the add-on's CHANGELOG.md as a release and never
# delete it; anything else with a timestamp revision is a dev build. 'latest',
# 'buildcache' and any other non-version tag never match and are left alone.
# Best-effort: a missing/under-scoped token just skips pruning with a warning.
prune_dev_tags() {
  local ha_arch="$1" token
  token="$(ghcr_token)"
  if [ -z "$token" ]; then
    echo "--- skip prune: no GHCR token (set GHCR_TOKEN or run 'gh auth login' with delete:packages)" >&2
    return 0
  fi
  GHCR_API_TOKEN="$token" python3 - "$GHCR_OWNER" "${ha_arch}-addon-${ADDON_NAME}" "$DEV_TAGS_KEEP" \
      "$REPO_ROOT/$ADDON_NAME/CHANGELOG.md" <<'PY'
import os, sys, re, json, urllib.request
token, owner, pkg, keep = os.environ["GHCR_API_TOKEN"], sys.argv[1], sys.argv[2], int(sys.argv[3])
changelog = sys.argv[4]
base = f"https://api.github.com/users/{owner}/packages/container/{pkg}/versions"
def api(url, method="GET"):
    req = urllib.request.Request(url, method=method)
    req.add_header("Authorization", f"Bearer {token}")
    req.add_header("Accept", "application/vnd.github+json")
    return urllib.request.urlopen(req)
# Released versions are protected. If the changelog is unreadable, protect
# nothing rather than guessing — but say so, since that could delete a release.
released = set()
try:
    with open(changelog) as f:
        released = set(re.findall(r"^#+ (\d+\.\d+\.\d+)[ \t]*$", f.read(), re.M))
except OSError as e:
    print(f"--- prune: cannot read {changelog} ({e}); no releases protected", file=sys.stderr)
try:
    versions = json.load(api(base + "?per_page=100"))
except Exception as e:
    print(f"--- prune: list failed ({e}); skipping", file=sys.stderr); sys.exit(0)
devs = []
for v in versions:
    for t in v.get("metadata", {}).get("container", {}).get("tags", []):
        m = re.match(r"^(\d+)\.(\d+)\.(\d{14})$", t)
        if m and t not in released:
            devs.append((tuple(int(g) for g in m.groups()), t, v["id"])); break
# Sort on the numeric triple, not the string: "0.9.x" > "0.10.x" lexically, so a
# string sort would prune the newer minor's builds first once minor hits 10.
devs.sort(reverse=True)
for _key, tag, vid in devs[keep:]:
    try:
        api(f"{base}/{vid}", method="DELETE"); print(f"--- pruned old dev image tag {tag}")
    except Exception as e:
        print(f"--- prune: delete {tag} failed ({e})", file=sys.stderr)
PY
}

deploy_addon() {
  # Why this shape: the add-on's config.yaml carries an `image:` field, so
  # Supervisor treats it as image-based (build: false) and PULLS the image
  # instead of compiling the Rust bridge daemon on-device — even for a local
  # /addons/ add-on. (An earlier note in this repo claimed local add-ons
  # always build regardless; that's wrong once `image:` is set — verified on
  # the target: `ha addons info` shows build:false and the ghcr.io image is
  # present in Docker.) So the fast path is: build the image HERE, push to
  # GHCR, and let Supervisor pull it.
  local ha_arch platform image base_version dev_version
  ha_arch="$(remote_ha_arch)"
  platform="$(platform_for "$ha_arch")"
  image="ghcr.io/${GHCR_OWNER}/${ha_arch}-addon-${ADDON_NAME}"

  # Supervisor only re-pulls when the version increases, and for a local
  # add-on the "latest" version is whatever config.yaml in /addons says. So we
  # stamp a unique, monotonically-increasing dev version per deploy and tag the
  # image to match, so `ha apps update` always fires and pulls.
  #
  # Versions are MAJOR.MINOR.REVISION with REVISION abused as a build timestamp
  # in UTC ISO-8601 basic form minus the T separator (YYYYMMDDHHMMSS) — see
  # scripts/release.py for why the T can't be there (Cargo needs a numeric patch
  # segment). A dev deploy keeps the committed MAJOR.MINOR and *replaces* the
  # revision with the current timestamp, which is necessarily larger than the
  # revision stamped at release time, so the version always goes up. UTC, not
  # local time, so a DST fall-back can't produce a lower revision than the
  # previous deploy. The committed config.yaml is never touched; only the
  # rsync'd /addons copy is.
  base_version="$(grep '^version:' "$REPO_ROOT/$ADDON_NAME/config.yaml" | head -1 | sed -E 's/^version: *"?([^"]+)"?.*/\1/')"
  case "$base_version" in
    *.*.*) ;;
    *) echo "ERROR: config.yaml version '$base_version' is not MAJOR.MINOR.REVISION" >&2; exit 1 ;;
  esac
  dev_version="${base_version%.*}.$(date -u +%Y%m%d%H%M%S)"
  echo "=== deploying dev version $dev_version (from committed $base_version) ==="

  preflight_ghcr
  ensure_builder "$platform"

  echo "--- building & pushing $image:$dev_version ($platform) ---"
  # --cache-{from,to} keep unchanged layers (apt, cargo deps) warm across
  # deploys, so only what actually changed rebuilds under emulation.
  docker buildx build \
    --builder "$BUILDER" \
    --platform "$platform" \
    --tag "$image:$dev_version" \
    --build-arg "ADDON_VERSION=$dev_version" \
    --cache-from "type=registry,ref=$image:buildcache" \
    --cache-to "type=registry,ref=$image:buildcache,mode=max" \
    --push \
    "$REPO_ROOT/$ADDON_NAME"

  echo "--- rsyncing add-on metadata to $HA_HOST:/addons/$ADDON_NAME/ ---"
  # Supervisor needs config.yaml (and the other add-on metadata) present to
  # know the add-on and its image ref; it does NOT build from this tree.
  # So exclude every build cache: an anchored 'bridge-daemon/target/' misses
  # the nested bridge-daemon/vendor/*/target/ dirs, which are ~8.6G of Rust
  # artifacts — that turns a 15M metadata sync into an hours-long transfer
  # that also nearly fills the Pi's disk. Unanchored patterns match at any
  # depth, which is what we want here.
  rsync -az --delete --info=stats1 \
    --exclude 'target/' \
    --exclude 'node_modules/' \
    --exclude 'dist/' \
    "$REPO_ROOT/$ADDON_NAME/" "root@$HA_HOST:/addons/$ADDON_NAME/"

  echo "--- pinning local add-on to dev version $dev_version ---"
  # `ha store reload` (NOT `ha addons reload`) is what re-reads the local
  # /addons repository and refreshes version_latest — confirmed on the
  # target; addons reload leaves version_latest stale so no update fires.
  ssh "root@$HA_HOST" \
    "sed -i -E 's/^version: .*/version: \"$dev_version\"/' /addons/$ADDON_NAME/config.yaml && ha store reload"

  # Detecting install state is subtle: `ha apps info` exposes an `installed:`
  # field ONLY in the not-installed (store) view, where it's `false`. Once
  # the add-on is installed that key is absent (null in --raw-json) and the
  # YAML has no `installed:` line at all — so grepping for `installed: true`
  # (or trusting the exit code) always reads "not installed" and the next
  # `install` dies with "already installed". Reliable rule: install iff
  # .data.installed == false, otherwise update.
  if [ "$(ssh "root@$HA_HOST" "ha apps info $ADDON_SLUG --raw-json 2>/dev/null | jq -r '.data.installed'")" = "false" ]; then
    echo "--- installing $ADDON_SLUG for the first time (pulls $image:$dev_version) ---"
    ssh "root@$HA_HOST" "ha apps install $ADDON_SLUG"
  else
    echo "--- updating $ADDON_SLUG (pulls $image:$dev_version) ---"
    ssh "root@$HA_HOST" "ha apps update $ADDON_SLUG"
  fi

  echo "--- pruning old dev image tags on GHCR (keeping newest $DEV_TAGS_KEEP) ---"
  prune_dev_tags "$ha_arch"

  # Start only if it isn't already running. `ha apps update` restarts an add-on
  # that *was* running, so the supervisor has already started the new container
  # by now; `ha apps install` leaves it stopped, so a first install still needs
  # a start. An unconditional `ha apps restart` here produced a **second**
  # stop/start 0.6 s after the supervisor's own update-triggered start —
  # doubling the disruption per deploy, and making the add-on log unreadable
  # for exactly the restart questions this repo keeps debugging (the new
  # container gets killed ~0.5 s into its startup, mid-handshake with the
  # sendspin speakers). See
  # pipewire_audio_router/docs/sendspin-group-churn-plan.md §4.11.
  echo "--- ensuring $ADDON_SLUG is running ---"
  # `|| true` because this is a plain assignment under `set -e`: unlike the
  # `if [ "$(…)" ]` form used for the install check above, a failing command
  # substitution here would abort the deploy. Falling back to `unknown` sends us
  # down the start path, which is the safe default when we can't tell.
  state="$(ssh "root@$HA_HOST" "ha apps info $ADDON_SLUG --raw-json 2>/dev/null | jq -r '.data.state'" || true)"
  state="${state:-unknown}"
  case "$state" in
    started | startup)
      # `startup` is the transient state between the supervisor's start and the
      # app reporting ready; treat it as running rather than racing it.
      echo "    already running (state=$state) — no restart needed"
      ;;
    *)
      echo "    state=$state — starting"
      ssh "root@$HA_HOST" "ha apps start $ADDON_SLUG"
      ;;
  esac

  # Repeated here because the build/push/pull output above is long enough that
  # the version printed at the start has scrolled away by now — and this is the
  # string to match against the daemon's startup log line below.
  echo "=== deployed dev version $dev_version ==="

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
  ytmusic)
    # Same cross-build/push/pull machinery as the router: deploy_addon() is already
    # parameterised by these two globals. Assembling this image on the Pi would be
    # apt + npm + pip under emulation, which is the slow part even without a compiler.
    ADDON_SLUG="local_ytmusic_receiver"
    ADDON_NAME="ytmusic_receiver"
    stage_ytmusic_receiver
    deploy_addon
    ;;
  integration) deploy_integration ;;
  *) usage ;;
esac
