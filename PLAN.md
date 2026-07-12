# PLAN.md — superseded

This file was the original planning/investigation document for the
project, written incrementally while the system was being designed and
built. That work is now done — see [docs/roadmap.md](docs/roadmap.md)
for status — and its content has been migrated into proper reference
documentation:

- **What the system is, how it fits together**:
  [docs/architecture.md](docs/architecture.md)
- **Why it's built this way** (every investigation/finding this file
  used to accumulate — RAOP quirks, PipeWire's real volume/module-
  loading capabilities, Rust vs. Python, MQTT vs. custom integration,
  ESP32 hardware constraints, and more):
  [docs/decisions.md](docs/decisions.md)
- **The bridge daemon's REST/WebSocket API**:
  [docs/api-reference.md](docs/api-reference.md)
- **Phase-by-phase status and what's left**:
  [docs/roadmap.md](docs/roadmap.md)
- **Per-component usage docs**: each of
  [`pipewire_audio_router/`](pipewire_audio_router/README.md),
  [`custom_components/pipewire_audio_router/`](custom_components/pipewire_audio_router/README.md),
  and [`firmware/bt-bridge/`](firmware/bt-bridge/README.md) has its own
  README now.
- **Raw empirical evidence per experiment**: [spikes/](spikes/) (write-ups)
  and [tests/](tests/) (the scripts backing them) — unchanged, still the
  primary source of truth for "was this actually verified."

Start at the repo-root [README.md](README.md) if you're new here.
