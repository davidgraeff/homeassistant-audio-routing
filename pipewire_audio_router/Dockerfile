# PipeWire Audio Router — Home Assistant add-on.
#
# Two stages: build the Rust bridge daemon against libpipewire-0.3-dev,
# then assemble the runtime image on the same Ubuntu 26.04 LTS base
# validated end-to-end (including real hardware) in spikes 1-4 — see
# ../PLAN.md Section 5.1 and ../spikes/ for why this exact base, why
# these exact PipeWire/WirePlumber versions, and why no ALSA/Bluez
# hardware monitors are enabled.

FROM rust:1-slim-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config clang libpipewire-0.3-dev libspa-0.2-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY bridge-daemon/ .
RUN cargo build --release

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
        avahi-daemon \
        shairport-sync \
        python3 \
        python3-pip \
        ffmpeg \
    && rm -rf /var/lib/apt/lists/*
# ffmpeg: decodes fetched TTS/announce audio (arbitrary format, e.g. mp3)
# to WAV before pw-cat plays it — pw-cat/libsndfile can't decode mp3
# itself (Section 5.6 v1 announce-stream design).

# Sendspin sink adapter (Section 5.4b/5.5): a separate small Python
# process per configured sendspin output, embedding aiosendspin — proven
# in spikes/03-sendspin-pushstream.md. Same version MA pins.
RUN pip install --break-system-packages --no-cache-dir "aiosendspin[server]==6.1.1"

COPY --from=build /build/target/release/bridge-daemon /usr/local/bin/bridge-daemon
COPY rootfs/run.sh /run.sh
COPY sendspin-adapter/adapter.py /usr/local/bin/sendspin-adapter.py
RUN chmod +x /run.sh /usr/local/bin/bridge-daemon /usr/local/bin/sendspin-adapter.py

ENV XDG_RUNTIME_DIR=/run/pipewire

ENTRYPOINT ["/run.sh"]
