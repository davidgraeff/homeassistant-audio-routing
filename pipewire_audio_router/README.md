# PipeWire Audio Router (Home Assistant add-on)

A Home Assistant add-on that runs PipeWire + WirePlumber as a whole-home
audio router, controlled by a small Rust "bridge daemon" that exposes a
REST/WebSocket API. This is the piece that actually moves audio around —
the [HA integration](../custom_components/pipewire_audio_router/README.md)
and the [Bluetooth bridge firmware](../firmware/bt-bridge/README.md) are
separate, optional pieces that talk to this one.

For the system-wide picture (how this fits with the other components)
see [../docs/architecture.md](../docs/architecture.md). For *why* it's
built this way, see [../docs/decisions.md](../docs/decisions.md).

## Installing

This directory is a HA add-on (`config.yaml` + `Dockerfile`), and
the repo root's `repository.yaml` makes the whole repo installable as an
add-on repository:

1. Home Assistant → Settings → Add-ons → Add-on Store → ⋮ (top right) →
   **Repositories** → add this repo's URL.
2. Find **PipeWire Audio Router** in the store and install it.
3. Start it — there's nothing to configure up front; outputs and sources
   are added at runtime (see [Configuration](#configuration)).
4. `host_network: true` is required and already set in `config.yaml` —
   RAOP and RTP-sourced audio both need to send/receive unsolicited LAN
   traffic that default Docker bridge networking blocks (see
   [decisions.md](../docs/decisions.md#host_network-true-is-required-not-a-convenience)).

## Configuration

There are **no add-on options** to fill in. Everything user-facing is
configured at runtime via the daemon's REST API / web UI and persisted
under `/data`, so it survives restarts.

- **RAOP (AirPlay) outputs** — AV receivers this add-on streams *out* to
  (e.g. Yamaha/Pioneer). Managed via `/api/outputs`, and auto-discovered
  over mDNS. `port` defaults to `7000` (RAOP's advertised RTSP port is
  often *not* 5000); `encryption` defaults to `auth_setup` — the only mode
  that worked against real hardware tested here (`none`/`RSA` are
  fallbacks, for a reason — see
  [decisions.md](../docs/decisions.md#raop-quirks-found-only-by-testing-against-real-hardware)).
- **AirPlay-receive source** — the name phones/PCs cast *in* to. A single
  source, managed via `/api/source/airplay` (empty name = disabled).
- **Bluetooth bridge (RTP) source** — receives the RTP stream from the
  [ESP32 Bluetooth bridge firmware](../firmware/bt-bridge/README.md) and
  exposes it as a routable source (`bt-bridge-rtp`). A single source,
  managed via `/api/source/rtp` (set the listen port to match the
  firmware; disabled by default). Loaded as a native PipeWire module like
  RAOP outputs.
- **Sendspin outputs** — ESPHome speaker devices (e.g. HA Voice PE) that
  connect *in* to this add-on (no IP needed). Auto-discovered over mDNS and
  surfaced as virtual routing outputs; you route to them through the routing
  matrix and set per-device volume via `/api/sendspin/volume`. Devices fed by
  the same set of sources are automatically formed into one synchronized
  group.

See [Managing outputs at runtime](#managing-outputs-at-runtime) below and
the [full API reference](../docs/api-reference.md).

## Managing outputs at runtime

RAOP outputs are hot-reloadable: the daemon loads one
`libpipewire-module-raop-sink` module per output into its own PipeWire
context at runtime, so adding or removing one doesn't restart PipeWire or
interrupt audio on the other outputs (see
[decisions.md](../docs/decisions.md#loading-pipewire-modules-at-runtime)
for how this works). The set is persisted to `/data/raop-outputs.json`.

```bash
# list outputs (and whether each is loaded right now)
curl http://<add-on-host>:8099/api/outputs

# add one — appears in the graph live, no restart
curl -X POST http://<add-on-host>:8099/api/outputs \
  -H 'content-type: application/json' \
  -d '{"name":"Pioneer VSX-934","ip":"192.168.178.35","port":7000,"encryption":"auth_setup"}'

# remove one by node name — its sink node disappears live
curl -X DELETE http://<add-on-host>:8099/api/outputs/raop-out-pioneer_vsx_934
```

Full endpoint reference (status codes, failure modes):
[api-reference.md](../docs/api-reference.md#outputs-raop-hot-reloadable).

## What's inside

- **PipeWire + WirePlumber**: the actual audio graph, on the Ubuntu 26.04
  LTS base validated in `spikes/01`–`04` (see repo-root `spikes/`).
- **Bridge daemon** (`bridge-daemon/`, Rust): observes the live PipeWire
  registry on a dedicated thread, serves the REST/WebSocket API on `:8099`,
  and hosts everything user-configurable — RAOP outputs, the AirPlay
  source, and sendspin outputs — in-process. Full endpoint reference:
  [../docs/api-reference.md](../docs/api-reference.md).
  - **Sendspin outputs**: an embedded sendspin server role
    (`sendspin_server.rs`, vendored `sendspin-rs`) creates each sink node,
    captures its PCM (`sendspin_capture.rs`), and pushes it over the
    Sendspin WebSocket protocol to the ESPHome device.
  - **AirPlay-receive source**: an embedded pure-Rust RAOP server
    (`airplay_source.rs`, vendored `shairplay`) feeds decoded PCM into a
    PipeWire source node the daemon owns (node `airplay-in`).
  - **RTP (Bluetooth bridge) source**: a native PipeWire module, same as
    RAOP outputs.

  mDNS discovery and advertising (RAOP, AirPlay, sendspin) all use the
  pure-Rust `mdns-sd` crate — no avahi or system D-Bus daemon involved.

Startup order (`rootfs/run.sh`): D-Bus session bus → `pipewire` →
`wireplumber` → `bridge-daemon serve`. The daemon then loads a `raop-sink`
module per stored RAOP output and brings up the AirPlay and RTP sources from
its persisted config; sendspin devices are discovered and grouped
dynamically as they appear. It's all in-process and reconfigurable live via
the API (`/api/outputs`, `/api/source/airplay`, `/api/source/rtp`,
`/api/sendspin/volume`), no restart. If a top-level component dies, the whole
container exits so HA's supervisor restarts everything rather than limping
along with a dead piece.

## Web UI

The daemon serves a small admin web app (Vite + Svelte, in
[`frontend/`](frontend/), built into the image and served as static files).
It's a dark/light-themed console — styled to match Home Assistant — covering
the whole API: a live source × output **routing matrix** with per-output
volume, RAOP **output** management, the AirPlay **source** and **sendspin**
outputs, and an **announce** test. Live-updated over WebSocket as the PipeWire
graph changes; independent of the HA integration (which exposes the
`media_player` entities).

Reach it two ways:

- **In Home Assistant's sidebar** via ingress (authenticated, no separate
  login) — `ingress: true` in `config.yaml`.
- **Directly** at `http://<add-on-host>:8099/`.

The UI uses relative asset/API paths, so the same build works under both. Dev:
`cd frontend && npm install && npm run dev` (point it at a running daemon), or
`npm run build` → `dist/` (what the daemon serves via `--static-dir`).

## Development

```
cd bridge-daemon && cargo test        # unit tests: config parsing, module-args + outputs store
cd bridge-daemon && cargo build       # fast host-side build for local runs/tests (native, user-owned)
../scripts/build-daemon.sh            # build the daemon binary in a container, rootless-safe (no root-owned files)
docker compose -f ../container/docker-compose.yml up   # bare PipeWire sandbox (see container/README.md) — NOT this add-on
docker build -t pipewire-audio-router .                # build the real add-on image locally
../scripts/build-arm64.sh                              # cross-build for the Pi 4 (linux/arm64)
```

Use `../scripts/build-daemon.sh` for any **container** build of the daemon
— it prefers rootless podman and extracts the binary via `create`+`cp`, so
it never leaves `root:root` files on the host the way an ad-hoc
`docker run -v "$PWD:/build" … cargo build` does. For the fast inner loop,
plain `cargo build`/`cargo test` on the host is fine (native → already
user-owned).

`container/` is a separate, throwaway bare-PipeWire dev sandbox used for
early spikes — it does not contain the bridge daemon and isn't what gets
installed as the add-on. Don't confuse the two Dockerfiles.

End-to-end verification (real add-on binary, real PipeWire, real signal
measurements via `ffmpeg astats` — not just "it compiled") lives in
`../tests/test_addon_*.sh`. Each corresponds to a phase in
[../docs/roadmap.md](../docs/roadmap.md).
