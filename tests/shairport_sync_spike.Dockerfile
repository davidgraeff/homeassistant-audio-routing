# Spike: shairport-sync as the AirPlay-receive source (PLAN.md Section 5.2).
# Layers shairport-sync on top of the real pw-audio-router image so it's
# tested against the exact production PipeWire/WirePlumber setup. Not part
# of the production add-on Dockerfile yet — Section 5.2 wiring (spawning
# this per-source, linking its stream into the routing graph) is Phase 2
# work; this proves the mechanism first.
ARG BASE_IMAGE=pw-audio-router:dev
FROM ${BASE_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends \
        shairport-sync \
    && rm -rf /var/lib/apt/lists/*
