# Minimal headless PipeWire + WirePlumber container.
# Multi-arch: build for linux/amd64 (local dev) and linux/arm64 (Raspberry Pi 4).
#
# Base is Ubuntu 26.04 LTS, not Debian bookworm: bookworm ships PipeWire
# 0.3.65 (2022-era), which creates RAOP sink nodes fine locally but never
# actually performs the RTSP handshake with a real receiver — a silent,
# version-specific bug (spikes/02-raop-static-sink.md). Ubuntu 26.04 LTS
# ships PipeWire 1.6.2 / WirePlumber 0.5.13, confirmed working end-to-end
# against real hardware, and is a genuine 5(+)-year-support LTS — newer
# PipeWire than Debian trixie's 1.4.2 (also verified working), and
# comparable support horizon. See PLAN.md Section 5.1/7 for the full
# version comparison across Debian/Ubuntu releases.
FROM ubuntu:26.04

RUN apt-get update && apt-get install -y --no-install-recommends \
        pipewire \
        pipewire-audio-client-libraries \
        wireplumber \
        libspa-0.2-modules \
        dbus \
        dbus-x11 \
        procps \
        bash \
    && rm -rf /var/lib/apt/lists/*

# No ALSA/Bluez hardware monitors: keep the graph purely virtual (network
# router mode). Unlike Debian bookworm's WirePlumber 0.4.13, this
# version's default config boots headless without crashing on a missing
# logind — no bluetooth.lua override needed (confirmed empirically).
COPY etc-pipewire/ /etc/pipewire/
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENV XDG_RUNTIME_DIR=/run/pipewire

ENTRYPOINT ["/entrypoint.sh"]
