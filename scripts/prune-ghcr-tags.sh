#!/bin/bash
# Prune old dev image tags from GHCR, keeping the newest N per package.
#
# deploy-dev.sh already prunes the add-on it just deployed (prune_dev_tags), but
# only that one, only for the arch it deployed to, and only when a deploy happens.
# This script is the standalone sweep over *every* package this repo publishes —
# run it by hand, or let .github/workflows/prune-packages.yml do it on a schedule.
#
#   ./scripts/prune-ghcr-tags.sh                  # prune, keeping 3 per package
#   ./scripts/prune-ghcr-tags.sh --dry-run        # show what would go
#   ./scripts/prune-ghcr-tags.sh --keep 5
#   ./scripts/prune-ghcr-tags.sh --untagged       # also drop untagged versions
#
# WHAT COUNTS AS PRUNABLE
#   Only dev tags: `MAJOR.MINOR.YYYYMMDDHHMMSS`, the shape deploy-dev.sh mints.
#   Everything else is left alone by construction — release tags (`1.2.3`),
#   `latest`, and `buildcache`. That rule is deliberately the inverse of "delete
#   what I think is safe": a tag this script does not recognise is never touched,
#   so a new tagging scheme cannot silently lose images.
#
# Requires a token with `delete:packages` (plus `read:packages`): GHCR_TOKEN, or a
# `gh auth login` that carries the scope. Deletion is per package *version*, which
# is what GHCR calls an image; there is no way to delete just one tag of a
# multi-tag version, so a version carrying any protected tag is skipped entirely.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GHCR_OWNER="${GHCR_OWNER:-davidgraeff}"
KEEP=3
DRY_RUN=0
PRUNE_UNTAGGED=0

# Every package this repo pushes. Arch prefixes are part of the package name (see
# the {arch} substitution in each add-on's config.yaml), so they are listed out.
PACKAGES=(
  "aarch64-addon-pipewire_audio_router"
  "amd64-addon-pipewire_audio_router"
  "aarch64-addon-ytmusic_receiver"
  "amd64-addon-ytmusic_receiver"
)

usage() {
  echo "usage: $0 [--keep N] [--dry-run] [--untagged] [--package NAME]" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP="${2:?--keep needs a number}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --untagged) PRUNE_UNTAGGED=1; shift ;;
    --package) PACKAGES=("${2:?--package needs a name}"); shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done

# Token resolution, in order of how likely it is to actually carry the scope:
#
#   1. GHCR_TOKEN            explicit wins.
#   2. ~/.docker/config.json what `docker login ghcr.io` stored — and on this repo's
#                            workstation that is the PAT with delete:packages, i.e.
#                            the one that works. Checked before the gh CLI because
#                            a normal `gh auth login` token has NO package scopes
#                            (verified: 403 on every package endpoint), which
#                            otherwise looks like "the packages are missing".
#   3. gh auth token         for setups where gh was logged in with the scope.
ghcr_token() {
  if [ -n "${GHCR_TOKEN:-}" ]; then
    printf '%s' "$GHCR_TOKEN"
    return
  fi
  local from_docker
  from_docker="$(python3 - <<'PY' 2>/dev/null || true
import base64, json, os
try:
    with open(os.path.expanduser("~/.docker/config.json")) as f:
        auth = json.load(f).get("auths", {}).get("ghcr.io", {}).get("auth", "")
    print(base64.b64decode(auth).decode().split(":", 1)[1] if auth else "")
except Exception:
    print("")
PY
)"
  if [ -n "$from_docker" ]; then
    printf '%s' "$from_docker"
    return
  fi
  if command -v gh >/dev/null 2>&1; then
    gh auth token 2>/dev/null || true
  fi
}

TOKEN="$(ghcr_token)"
if [ -z "$TOKEN" ]; then
  echo "ERROR: no GHCR token. Set GHCR_TOKEN (PAT with delete:packages) or run" >&2
  echo "       'gh auth login' with that scope." >&2
  exit 1
fi

echo "=== pruning GHCR dev tags for $GHCR_OWNER (keep $KEEP each$([ $DRY_RUN = 1 ] && echo ', DRY RUN'))"

GHCR_API_TOKEN="$TOKEN" python3 - "$GHCR_OWNER" "$KEEP" "$DRY_RUN" "$PRUNE_UNTAGGED" "${PACKAGES[@]}" <<'PY'
import json, os, re, sys, urllib.error, urllib.request

token = os.environ["GHCR_API_TOKEN"]
owner, keep, dry_run, prune_untagged = sys.argv[1], int(sys.argv[2]), sys.argv[3] == "1", sys.argv[4] == "1"
packages = sys.argv[5:]

#: Dev tag shapes. Both are "a release version with a build stamp appended", which
#: is what makes them recognisably not releases (a release tag is exactly X.Y.Z):
#:   current  MAJOR.MINOR.YYYYMMDDHHMMSS   (deploy-dev.sh today)
#:   legacy   MAJOR.MINOR.PATCH.<stamp>    (earlier schemes: HHMM and unix epoch;
#:                                          still on GHCR and were being protected
#:                                          by a pattern that only knew the first)
DEV_TAGS = (
    re.compile(r"^\d+\.\d+\.\d{14}$"),
    re.compile(r"^\d+\.\d+\.\d+\.\d{3,14}$"),
)


def is_dev_tag(tag):
    return any(p.match(tag) for p in DEV_TAGS)


def api(url, method="GET"):
    req = urllib.request.Request(url, method=method)
    req.add_header("Authorization", f"Bearer {token}")
    req.add_header("Accept", "application/vnd.github+json")
    return urllib.request.urlopen(req)


def versions(pkg):
    """Every version of a package, following pagination.

    Without this a package with >100 versions would silently prune only within the
    first page — i.e. appear to work while leaving the real backlog untouched.
    """
    out, page = [], 1
    while True:
        url = f"https://api.github.com/users/{owner}/packages/container/{pkg}/versions?per_page=100&page={page}"
        batch = json.load(api(url))
        out.extend(batch)
        if len(batch) < 100:
            return out
        page += 1


total_deleted = total_kept = 0
for pkg in packages:
    base = f"https://api.github.com/users/{owner}/packages/container/{pkg}/versions"
    try:
        vs = versions(pkg)
    except urllib.error.HTTPError as e:
        if e.code == 404:
            # Normal: not every arch of every add-on has been pushed.
            print(f"--- {pkg}: no such package (404) — nothing to prune")
        elif e.code in (401, 403):
            # Distinguished from 404 on purpose: a scope problem otherwise reads
            # as "these packages do not exist" and the sweep looks successful.
            print(f"--- {pkg}: NOT AUTHORISED ({e.code}). The token lacks "
                  f"read:packages/delete:packages —")
            print("      a plain `gh auth login` token does not have them. Use a PAT:")
            print("      GHCR_TOKEN=<pat> ./scripts/prune-ghcr-tags.sh")
        else:
            print(f"--- {pkg}: skipped ({e.code} {e.reason})")
        continue
    except Exception as e:  # noqa: BLE001
        print(f"--- {pkg}: list failed ({e})")
        continue

    devs, protected, untagged = [], [], []
    for v in vs:
        tags = v.get("metadata", {}).get("container", {}).get("tags", [])
        if not tags:
            untagged.append(v)
            continue
        # A version is prunable only if EVERY tag on it is a dev tag: deleting a
        # version deletes all of its tags at once, so one protected tag protects it.
        if all(is_dev_tag(t) for t in tags):
            # Ordered by the API's own created_at, not by parsing the tag. Parsing
            # invites two bugs at once: a lexical compare puts "0.9.x" above
            # "0.10.x", and any *new* tag scheme silently sorts wrongly against the
            # old ones. The registry already knows when each version was pushed.
            devs.append((v.get("created_at", ""), ",".join(tags), v["id"]))
        else:
            protected.append(tags)

    devs.sort(reverse=True)
    doomed = devs[keep:]
    print(f"--- {pkg}: {len(devs)} dev, {len(protected)} protected, {len(untagged)} untagged"
          f" -> deleting {len(doomed)}{' + untagged' if prune_untagged and untagged else ''}")
    for tags in protected:
        print(f"      protected: {','.join(tags)}")

    targets = [(t, i) for _k, t, i in doomed]
    if prune_untagged:
        targets += [("<untagged>", v["id"]) for v in untagged]

    for tag, vid in targets:
        if dry_run:
            print(f"      would delete {tag}")
            continue
        try:
            api(f"{base}/{vid}", method="DELETE")
            print(f"      deleted {tag}")
            total_deleted += 1
        except Exception as e:  # noqa: BLE001
            print(f"      FAILED to delete {tag}: {e}")
    total_kept += min(len(devs), keep)

print(f"=== {'would delete' if dry_run else 'deleted'} {total_deleted}, kept {total_kept} dev versions")
PY
