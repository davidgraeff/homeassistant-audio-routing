#!/usr/bin/env python3
"""Cut a release: bump the minor, stamp a build-timestamp revision, write the
changelog entry, commit, and tag.

    ./scripts/release.py              # cut the next minor
    ./scripts/release.py --dry-run    # print everything, touch nothing
    ./scripts/release.py --major      # 0.9.x -> 1.0.x instead of 0.10.x

Versions are MAJOR.MINOR.REVISION where REVISION is the UTC build timestamp in
ISO-8601 basic form *without the T separator* — `YYYYMMDDHHMMSS`, e.g.
`0.3.20260728203700`. The separator has to go because Cargo requires a numeric
patch segment (`0.3.20260728T203700` is rejected at manifest parse), and the
whole point of this scheme is that config.yaml, Cargo.toml and CHANGELOG.md all
carry the identical string. It stays correctly ordered for Supervisor either
way: the revision is a plain integer, so awesomeversion compares it numerically
(0.2.0 < 0.3.20260728203700 < 0.3.20260729090000 < 0.4.x), and it is fixed-width
so lexicographic order matches chronological order too.

Git tags are `vMAJOR.MINOR` only — the revision is a build stamp, not part of
the release's identity, so it is deliberately not in the tag. That also gives
the next run a stable range for `git log`.

Safeguard: bridge-daemon/Cargo.toml's version must equal the newest version in
CHANGELOG.md before we start. If they disagree, some previous release was
partial (or someone hand-edited a version) and bumping again would compound it.
"""

from __future__ import annotations

import argparse
import datetime as dt
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
ADDON = REPO / "pipewire_audio_router"

CHANGELOG = ADDON / "CHANGELOG.md"
CONFIG_YAML = ADDON / "config.yaml"
CARGO_TOML = ADDON / "bridge-daemon" / "Cargo.toml"
CARGO_LOCK = ADDON / "bridge-daemon" / "Cargo.lock"
PACKAGE_JSON = ADDON / "frontend" / "package.json"

# `## <version>` — hashes, one space, version, end of line. Same shape Home
# Assistant's hassio update entity greps for; see the comment in CHANGELOG.md.
# `[ \t]*$` rather than `\s*$`: \s matches newlines, which would let the match
# run past the heading line.
HEADING_RE = re.compile(r"^#+ (\d+\.\d+\.\d+)[ \t]*$", re.MULTILINE)
VERSION_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
# The `version = "..."` inside Cargo.toml's [package] table, and only there —
# the value span is group 1, so a [dependencies] entry can't be hit by accident.
CARGO_PKG_RE = re.compile(
    r"^\[package\](?:(?!^\[)[\s\S])*?^version\s*=\s*\"([^\"]+)\"", re.MULTILINE
)


class Abort(SystemExit):
    def __init__(self, msg: str) -> None:
        super().__init__(f"release: {msg}")


def git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=REPO, check=True, capture_output=True, text=True
    ).stdout.strip()


def require_clean_worktree(allow_untracked: bool) -> None:
    """A release commit must contain only what this script writes."""
    status = git("status", "--porcelain")
    if not status:
        return
    tracked = [l for l in status.splitlines() if not l.startswith("??")]
    untracked = [l for l in status.splitlines() if l.startswith("??")]
    if tracked:
        raise Abort(
            "working tree has uncommitted changes — commit or stash them first:\n  "
            + "\n  ".join(tracked)
        )
    if untracked and not allow_untracked:
        raise Abort(
            "working tree has untracked files (re-run with --allow-untracked to "
            "ignore them):\n  " + "\n  ".join(untracked)
        )


def read_changelog_version(text: str) -> str:
    """The newest version in the changelog — i.e. the first heading in the file."""
    m = HEADING_RE.search(text)
    if not m:
        raise Abort(
            f"no `## <major>.<minor>.<revision>` heading found in {rel(CHANGELOG)}"
        )
    return m.group(1)


def cargo_version_span(text: str) -> tuple[str, int, int]:
    """The [package] version, plus the span of the value so it can be replaced
    without re-running the (multi-line) pattern."""
    m = CARGO_PKG_RE.search(text)
    if not m:
        raise Abort(f"could not find [package] version in {rel(CARGO_TOML)}")
    return m.group(1), m.start(1), m.end(1)


def rel(p: Path) -> str:
    return str(p.relative_to(REPO))


def sub_once(text: str, pattern: str, repl, path: Path) -> str:
    """Substitute exactly one occurrence, or abort. Silent no-ops here would
    ship a release whose versions disagree, which is what the safeguard exists
    to prevent — so a missed pattern must be loud."""
    new, n = re.subn(pattern, repl, text, count=1, flags=re.M)
    if n != 1:
        raise Abort(f"pattern {pattern!r} did not match in {rel(path)}")
    return new


def previous_range_start(prev_tag: str) -> tuple[str | None, str]:
    """Where the new changelog entry's `git log` starts, and a human label.

    Normally the previous release's tag. Falling back to *all* history when that
    tag is missing would dump every commit ever into the entry, so prefer the
    commit that last touched CHANGELOG.md — that is "everything since the last
    changelog update", which is the range we actually want.
    """
    if prev_tag in git("tag").splitlines():
        return prev_tag, f"{prev_tag}..HEAD"
    last_touch = git("log", "-1", "--format=%H", "--", str(CHANGELOG))
    if last_touch:
        print(
            f"--- no {prev_tag} tag; using the last commit that touched "
            f"{rel(CHANGELOG)} ({last_touch[:7]}) as the range start",
            file=sys.stderr,
        )
        return last_touch, f"{last_touch[:7]}..HEAD"
    print(
        f"--- no {prev_tag} tag and {rel(CHANGELOG)} has no history; "
        "using the full history",
        file=sys.stderr,
    )
    return None, "the full history"


def collect_commits(start: str | None) -> list[str]:
    rng = f"{start}..HEAD" if start else "HEAD"
    log = git("log", "--oneline", "--no-decorate", "--no-merges", rng)
    return log.splitlines() if log else []


def render_entry(version: str, date: str, commits: list[str], rng: str) -> str:
    lines = [f"## {version}", "", f"_{date}_", ""]
    if commits:
        lines += [f"- {c}" for c in commits]
    else:
        lines.append(f"- No changes recorded ({rng}).")
    lines.append("")
    return "\n".join(lines)


def insert_entry(text: str, entry: str) -> str:
    """Put the new section directly above the previous newest one, so the
    file's preamble (and the maintainer comment block) stays on top."""
    m = HEADING_RE.search(text)
    assert m is not None  # read_changelog_version already validated this
    return text[: m.start()] + entry + "\n" + text[m.start() :]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--dry-run", action="store_true", help="print, change nothing")
    ap.add_argument("--major", action="store_true", help="bump major, reset minor to 0")
    ap.add_argument(
        "--allow-untracked", action="store_true", help="tolerate untracked files"
    )
    ap.add_argument(
        "--no-tag", action="store_true", help="make the commit but skip the git tag"
    )
    args = ap.parse_args()

    require_clean_worktree(args.allow_untracked)

    changelog = CHANGELOG.read_text()
    cargo = CARGO_TOML.read_text()

    last = read_changelog_version(changelog)
    cargo_version, cv_start, cv_end = cargo_version_span(cargo)

    # The safeguard.
    if cargo_version != last:
        raise Abort(
            f"version drift: {rel(CARGO_TOML)} is {cargo_version} but the newest "
            f"{rel(CHANGELOG)} entry is {last}.\n"
            "        Reconcile them by hand (they should be identical) before releasing."
        )

    vm = VERSION_RE.match(last)
    if not vm:
        raise Abort(f"cannot parse {last!r} as major.minor.revision")
    major, minor = int(vm.group(1)), int(vm.group(2))

    if args.major:
        major, minor = major + 1, 0
    else:
        minor += 1

    now = dt.datetime.now(dt.timezone.utc)
    revision = now.strftime("%Y%m%d%H%M%S")
    version = f"{major}.{minor}.{revision}"
    tag = f"v{major}.{minor}"

    if not args.no_tag and tag in git("tag").splitlines():
        raise Abort(f"tag {tag} already exists")

    start, rng = previous_range_start(f"v{vm.group(1)}.{vm.group(2)}")
    commits = collect_commits(start)
    entry = render_entry(version, now.strftime("%Y-%m-%d"), commits, rng)

    print(f"--- releasing {version} (was {last}), tag {tag}")
    print(f"--- {len(commits)} commit(s) from {rng}\n")
    print(entry)

    if args.dry_run:
        print("--- dry run: nothing written")
        return 0

    # config.yaml: what Supervisor and .github/workflows/build-addon.yml read.
    config = CONFIG_YAML.read_text()
    config = sub_once(
        config, r'^version: *"?[^"\n]+"?$', f'version: "{version}"', CONFIG_YAML
    )
    # Cargo.toml [package]: the fallback for the daemon's logged version when
    # ADDON_VERSION isn't baked in (see bridge-daemon/src/main.rs). Spliced by
    # the span found above, since the pattern that locates it spans lines.
    cargo_new = cargo[:cv_start] + version + cargo[cv_end:]
    # Cargo.lock's own bridge-daemon entry. The Dockerfile does not build
    # --locked, so a stale lock would only self-heal rather than fail, but
    # leaving it behind means every subsequent build shows a dirty lockfile.
    lock = CARGO_LOCK.read_text()
    lock = sub_once(
        lock,
        r'(^name = "bridge-daemon"\nversion = )"[^"]+"',
        lambda m: m.group(1) + f'"{version}"',
        CARGO_LOCK,
    )
    # The UI is private/unpublished, but it ships inside the same image, so keep
    # its version honest rather than frozen at whatever it was scaffolded with.
    pkg = PACKAGE_JSON.read_text()
    pkg = sub_once(
        pkg, r'^(\s*"version"\s*:\s*)"[^"]+"', lambda m: m.group(1) + f'"{version}"', PACKAGE_JSON
    )

    CHANGELOG.write_text(insert_entry(changelog, entry))
    CONFIG_YAML.write_text(config)
    CARGO_TOML.write_text(cargo_new)
    CARGO_LOCK.write_text(lock)
    PACKAGE_JSON.write_text(pkg)

    paths = [rel(p) for p in (CHANGELOG, CONFIG_YAML, CARGO_TOML, CARGO_LOCK, PACKAGE_JSON)]
    git("add", *paths)
    git("commit", "-m", f"release: {version}")
    print(f"\n--- committed release {version}")
    if not args.no_tag:
        git("tag", "-a", tag, "-m", f"Release {version}")
        print(f"--- tagged {tag}")

    print("\nNext: review, then publish with")
    print(f"    git show HEAD --stat")
    print(f"    git push --follow-tags")
    print(f"    ./scripts/deploy-dev.sh addon   # (stamps its own dev revision)")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except subprocess.CalledProcessError as e:
        cmd = " ".join(e.cmd)
        sys.exit(f"release: `{cmd}` failed:\n{e.stderr.strip()}")
