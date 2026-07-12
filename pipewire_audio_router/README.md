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

This directory is a real HA add-on (`config.yaml` + `Dockerfile`), and
the repo root's `repository.yaml` makes the whole repo installable as an
add-on repository:

1. Home Assistant → Settings → Add-ons → Add-on Store → ⋮ (top right) →
   **Repositories** → add this repo's URL.
2. Find **PipeWire Audio Router** in the store and install it.
3. Configure your outputs (see schema below), then start it.
4. `host_network: true` is required and already set in `config.yaml` —
   RAOP and RTP-sourced audio both need to send/receive unsolicited LAN
   traffic that default Docker bridge networking blocks (see
   [decisions.md](../docs/decisions.md#host_network-true-is-required-not-a-convenience)).

## Configuration

| Option | Type | Default | Notes |
|---|---|---|---|
| `outputs` | list of `{name, ip, port?, encryption?}` | `[]` | RAOP (AirPlay) receivers this add-on connects **out** to — AV receivers like a Yamaha/Pioneer. `port` defaults to `7000` (RAOP's actual advertised RTSP port is often *not* 5000 — check via mDNS if unsure). `encryption` defaults to `auth_setup`, the only mode that worked against real hardware tested here; `none`/`RSA` are fallbacks, not the default, for a real reason — see [decisions.md](../docs/decisions.md#raop-quirks-found-only-by-testing-against-real-hardware). |
| `sendspin_outputs` | list of `{name}` | `[]` | Sendspin (ESPHome speaker) outputs. No IP needed — these devices connect **in** to this add-on, not the other way around. |
| `airplay_source_name` | string | `"PipeWire Router"` | Display/service name for the single AirPlay-receive source (phones/PCs casting in). Set to an empty string to disable this source entirely. |

Changing `outputs` requires a restart (PipeWire has no way to hot-load a
module — see [decisions.md](../docs/decisions.md#pipewire-has-no-runtime-load-module-rpc)),
same as most HA add-ons already require on config changes.

## What's inside

- **PipeWire + WirePlumber**: the actual audio graph, on the Ubuntu 26.04
  LTS base validated in `spikes/01`–`04` (see repo-root `spikes/`).
- **Bridge daemon** (`bridge-daemon/`, Rust): generates the static RAOP
  `pipewire.conf.d` config, observes the live PipeWire registry on a
  dedicated thread, and serves the REST/WebSocket API on `:8080`. Full
  endpoint reference: [../docs/api-reference.md](../docs/api-reference.md).
- **`sendspin-adapter.py`**: one process per configured sendspin output,
  embedding `aiosendspin` and capturing PipeWire audio via `pw-record` to
  push over the Sendspin WebSocket protocol to the real ESPHome device.
- **`shairport-sync`**: the AirPlay-receive source, if
  `airplay_source_name` isn't empty. Needs a real D-Bus system bus +
  `avahi-daemon`, both started by `rootfs/run.sh` — see
  [decisions.md](../docs/decisions.md#shairport-sync-needs-a-real-d-bus-system-bus--avahi).

Startup order (`rootfs/run.sh`): D-Bus system + session buses →
`bridge-daemon generate-config` (must complete before PipeWire starts) →
`pipewire` → `wireplumber` → `avahi-daemon` → every configured source/
adapter process (via `bridge-daemon runtime-plan`) → `bridge-daemon
serve`. If any component dies, the whole container exits so HA's
supervisor restarts everything rather than limping along with a dead
piece.

## Manual routing UI

Open `http://<add-on-host>:8080/` directly (not through Home Assistant)
for a source × output matrix — click cells to link/unlink, drag volume
sliders per output. Live-updated over WebSocket as the real PipeWire
graph changes. This is independent of the HA integration; useful for
routing changes you don't want to wire into an automation.

## Development

```
cd bridge-daemon && cargo test        # unit tests: config parsing, config-file rendering
docker compose -f ../container/docker-compose.yml up   # bare PipeWire sandbox (see container/README.md) — NOT this add-on
docker build -t pipewire-audio-router .                # build the real add-on image locally
../scripts/build-arm64.sh                              # cross-build for the Pi 4 (linux/arm64)
```

`container/` is a separate, throwaway bare-PipeWire dev sandbox used for
early spikes — it does not contain the bridge daemon and isn't what gets
installed as the add-on. Don't confuse the two Dockerfiles.

End-to-end verification (real add-on binary, real PipeWire, real signal
measurements via `ffmpeg astats` — not just "it compiled") lives in
`../tests/test_addon_*.sh`. Each corresponds to a phase in
[../docs/roadmap.md](../docs/roadmap.md).
