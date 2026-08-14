# Maintaining the add-on and its Home Assistant integration

Operational notes for keeping this repo in step with the Home Assistant it runs
on. Architecture and API live elsewhere ([api-reference.md](api-reference.md),
`pipewire_audio_router/docs/`); this file is about the moving target *underneath*
the code.

## 1. The Home Assistant version under test

Three versions have to be thought about together, and only two of them are ours:

| Where | What decides it | How to read it |
|---|---|---|
| **Production** | the instance's own updates | `ssh homeassistant.local 'curl -s -H "Authorization: Bearer $SUPERVISOR_TOKEN" http://supervisor/core/info'` → `.data.version` |
| **CI** | `custom_components/pipewire_audio_router/tests/requirements.txt` | the `pytest-homeassistant-custom-component` pin, whose `homeassistant==` dependency is the version |
| **Local runs** | the same file | `scripts/test-custom-components.sh` prints the pin as it builds its image |

`pytest-homeassistant-custom-component` bundles one exact `homeassistant` release
plus the pytest plugins these tests need, so **that single pin is the Home
Assistant under test** — there is one file to change, and CI and the local runner
both read it.

**Keep the pin at (or very near) the production version.** Home Assistant moves
entity naming, registry semantics and deprecations between releases, so a stale
pin means green tests that assert what production stopped doing. That is not
hypothetical here — see §2.

### Bumping it

1. Find the release that carries the Home Assistant you want:
   ```bash
   curl -s https://pypi.org/pypi/pytest-homeassistant-custom-component/json |
     python3 -c "import json,sys; d=json.load(sys.stdin)['info']; \
       print(d['version'], d['requires_python'], [r for r in d['requires_dist'] if r.startswith('homeassistant')])"
   ```
   Any specific version: append `/<version>` to that URL. Releases are frequent and
   roughly track HA patch releases.
2. Edit the pin in `custom_components/pipewire_audio_router/tests/requirements.txt`.
3. Check its `Requires-Python`. The pin currently needs **≥3.14**; if that moves,
   bump `python-version` in the `integration` job of
   [.github/workflows/ci.yml](../.github/workflows/ci.yml) *and* the `FROM python:`
   line in [scripts/test-custom-components.sh](../scripts/test-custom-components.sh).
   Getting this wrong is quiet rather than loud: pip resolves an *older*
   `pytest-homeassistant-custom-component` that still allows the interpreter
   instead of refusing the pin, and you end up testing a Home Assistant from
   months ago. (That is exactly how this repo drifted: the local image was built
   from an unpinned install on Python 3.13 and froze on HA 2026.2.3 while CI ran
   2026.7.2.)
4. `scripts/test-custom-components.sh` — the pin is a build arg, so the image
   rebuilds itself. Read the failures as *findings*, not as chores: each one is a
   place where Home Assistant changed under the integration.

## 2. What a version bump has already found

Worth reading before dismissing a naming or registry test failure as noise.

**Devices are keyed per config entry (HA 2026.8).**
`DeviceRegistryItems.get_entry()` now takes a `config_entry_id`, and
`async_get_or_create` passes it — so declaring another integration's device
identity in an entity's `DeviceInfo` **no longer merges into that device**, it
creates a second row with the same MAC. Consequences:

- Per-output `media_player`s attach to their speaker by writing the entity
  registry's `device_id` (`PipewireRouterMediaPlayer.async_added_to_hass`), the way
  the UI's "assign entity to a device" does. The old `device_info` approach left
  the output with no name and no area, which silently costs **voice ducking** the
  room, since ducking resolves a satellite's area against the outputs' areas.
- One physical speaker now legitimately has several device rows (one per
  integration). `_find_ha_device_by_mac_suffix` therefore picks the row with the
  most entities rather than treating several rows as ambiguous.

**A device prefixes its entities' displayed names.** In HA 2026.8 this applies to
every entity with a device, `has_entity_name` or not
(`entity_registry._async_get_full_entity_name`). That is why only the *settings*
entities live on the service device: a group would have read "PipeWire Audio Router
Everywhere". Don't assert a friendly name that a device prefix decides — assert
what the integration controls (`has_entity_name`, `original_name`, `device_id`).

## 3. User-facing text

Home Assistant loads **`translations/<lang>.json`**, never `strings.json`, for a
custom integration. `strings.json` stays the source and `translations/en.json` is
its copy; `tests/test_translations.py` fails if they drift.

There is **no per-entity description field** — only `name`, `state` and
`state_attributes` are translatable. The places an explanation can actually go:
the entity name, **select option labels** (`entity.select.<key>.state.<option>`),
the service device page, and the integration's README.

## 4. Testing and deploying

```bash
scripts/test-custom-components.sh              # integration (pytest, in a container)
scripts/test-custom-components.sh -k duck -v   # extra pytest args pass through
scripts/test-rust.sh                           # daemon
scripts/deploy-dev.sh integration              # rsync + restart HA core (seconds)
scripts/deploy-dev.sh addon                    # build, push to GHCR, force a pull
```

Three knobs on the add-on image build, all used by
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) and worth knowing locally:

- **`--target runtime`** builds everything except the two downloadable
  `pwrouter-agent` binaries. Those are a second and third Rust cross-compile that
  nothing in the e2e suite downloads, so the CI job skips them; a build with no
  `--target` is still the complete add-on.
- **`SKIP_IMAGE_BUILD=1`** makes each `tests/test_addon_*.sh` use the image in
  `$IMAGE` as it finds it instead of running `docker build` first — for driving the
  suite against an image you just built by hand, and how CI builds once for five
  tests.
- **`scripts/bump-base-images.sh`** refreshes the Dockerfile's digest pins. The
  bases are pinned because a floating tag invalidates every layer under it:
  `rust:1-slim-trixie` was republished between two CI runs and the daemon
  recompiled from scratch for 7½ minutes on a warm cache. Never pass `--no-cache`
  to a build you want to be fast, either — it *wipes* the `type=cache` mounts, so
  the next build recompiles every dependency too.

Things that need the real instance rather than a test:

- **Voice ducking** — `tests/voice_duck_dev.sh` drives both halves without
  speaking: `duck`/`unduck`/`holds` hit the daemon's `/api/duck` directly, and
  `fake <satellite> [secs]` writes a satellite state, which fires the same
  `state_changed` the integration listens for. `debug` turns on the integration's
  debug logging via the `logger.set_level` service.
- **Home Assistant's own API from this machine**, with no long-lived token: the SSH
  add-on exports `$SUPERVISOR_TOKEN`, and the supervisor proxies core —
  `ssh homeassistant.local 'curl -s -H "Authorization: Bearer $SUPERVISOR_TOKEN"
  http://supervisor/core/api/states'`. Pipe JSON bodies in over stdin and use
  `--data-binary @file`; inline `-d '{...}'` through two shells reliably produces
  `Invalid JSON specified`. `state_translated(...)` through
  `POST /core/api/template` is the quickest way to check a rendered label.
