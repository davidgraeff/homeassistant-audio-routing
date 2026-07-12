# Dev sandbox (not the real add-on)

A throwaway bare PipeWire + WirePlumber container used for the earliest
spikes (`../spikes/01-headless-pipewire.md` and on) — proving PipeWire
runs headless, RAOP works outbound, etc., before any bridge daemon code
existed.

**This is not what gets installed as the Home Assistant add-on.** That's
[`../pipewire_audio_router/`](../pipewire_audio_router/README.md), which
has its own, separate `Dockerfile` (multi-stage: builds the Rust bridge
daemon, then assembles the runtime image on the same Ubuntu 26.04 LTS
base validated here).

Useful for: fast local iteration on PipeWire/WirePlumber config changes
in isolation, without rebuilding the Rust daemon.

```
docker compose up --build
```

`../scripts/build-arm64.sh` cross-builds the same Dockerfile for
`linux/arm64` as a periodic sanity check — not part of the fast dev loop.
