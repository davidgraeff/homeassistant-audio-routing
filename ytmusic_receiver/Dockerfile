# YouTube Music cast receiver add-on.
#
# Same shape as the audio-router add-on's runtime stage — PipeWire in the
# container, no systemd — but with no Rust to build: the receiver is plain ESM
# JavaScript, so this is a single-stage image.
#
# The app itself is SHARED with the Raspberry Pi deployment and lives at
# firmware/pi-ytmusic/receiver/. That is outside this build context, so
# scripts/deploy-dev.sh (target `ytmusic`) stages a copy into ./receiver/ before
# building. Keeping the Pi copy canonical is deliberate: that role is deployed by
# `scp -r firmware/pi-ytmusic`, which must stay self-contained.
FROM ubuntu:26.04

# Ubuntu 26.04 ships nodejs 22.22 — which matters twice over: yt-cast-receiver
# needs >= 18, and yt-dlp's JS-challenge provider REFUSES node < 22
# (`NodeJsRuntime.MIN_SUPPORTED_VERSION = (22, 0, 0)`). On the armv7 Pi that
# forced a private Node tarball; here the distro package covers both roles.
#
# systemd-standalone-sysusers instead of full systemd, and dbus-x11 rather than
# dbus-user-session, for exactly the reasons documented in the router's
# Dockerfile: pipewire and wireplumber pull those in as alternatives, and these
# are the small ones. Nothing here runs systemd as PID 1.
RUN apt-get update && apt-get install -y --no-install-recommends \
        systemd-standalone-sysusers \
        pipewire \
        pipewire-audio-client-libraries \
        wireplumber \
        libspa-0.2-modules \
        dbus \
        dbus-x11 \
        procps \
        bash \
        ca-certificates \
        # The player. mpv owns resolution (via yt-dlp), decoding, buffering and
        # seeking; the receiver drives it over JSON IPC.
        mpv \
        nodejs \
        npm \
        python3 \
        python3-venv \
        # Fallback JS engine for the challenge, if node is ever rejected. Tiny,
        # and it is what the armv7 Pi has to use.
        quickjs \
    && rm -rf /var/lib/apt/lists/* \
    && rm -rf /usr/share/doc/* /usr/share/man/* /usr/share/locale/*

# yt-dlp comes from pip, never apt: distro packages go stale in weeks and a stale
# resolver is the single most likely cause of "casting connects but nothing plays".
# Baked into the image so a fresh container starts ready; run.sh additionally
# refreshes it in the background at boot, so a long-running add-on keeps up with
# YouTube without a redeploy.
#
#   yt-dlp-ejs           the JS challenge-solver scripts
#   yt-dlp-remote-cipher solves challenges on a yt-cipher server (~3x faster)
RUN python3 -m venv /opt/ytdlp \
    && /opt/ytdlp/bin/pip install --no-cache-dir --upgrade pip \
    && /opt/ytdlp/bin/pip install --no-cache-dir yt-dlp yt-dlp-ejs yt-dlp-remote-cipher \
    && /opt/ytdlp/bin/yt-dlp --version

# The receiver app + its single dependency (yt-cast-receiver). `npm ci` would need
# a committed lockfile; this app has none, matching the Pi install.
WORKDIR /app
COPY receiver/package.json ./
RUN npm install --omit=dev --no-audit --no-fund \
    && npm cache clean --force
COPY receiver/*.js ./

COPY rootfs/ /

# Links the GHCR package to this repository. Two consequences, both wanted:
#   * A newly created package inherits the repo's visibility instead of defaulting
#     to private — which is what made Supervisor's first pull fail with 401.
#   * The repo's GITHUB_TOKEN can then manage the package, so
#     .github/workflows/prune-packages.yml needs no PAT.
LABEL org.opencontainers.image.source=https://github.com/davidgraeff/homeassistant-audio-routing

ARG ADDON_VERSION=dev
ENV ADDON_VERSION=${ADDON_VERSION}

CMD [ "/run.sh" ]
