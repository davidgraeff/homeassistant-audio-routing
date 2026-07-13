# PipeWire Audio Router — Home Assistant add-on.
#
# Three stages: build the Svelte web UI (frontend/), build the Rust bridge
# daemon against libpipewire-0.3-dev, then assemble the runtime image on the
# same Ubuntu 26.04 LTS base validated end-to-end (including real hardware) in
# spikes 1-4 — see ../PLAN.md Section 5.1 and ../spikes/ for why this exact
# base, why these exact PipeWire/WirePlumber versions, and why no ALSA/Bluez
# hardware monitors are enabled.

# Web UI (frontend/, Vite + Svelte). Built to static assets the daemon serves;
# no Node in the runtime image. Pinned to the builder's native arch
# (--platform=$BUILDPLATFORM): the output is just static JS/CSS, so there's no
# reason to run npm under QEMU emulation for the target arch.
FROM --platform=$BUILDPLATFORM node:22-slim AS frontend
WORKDIR /ui
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

# Build the Rust bridge daemon. Pinned to the BUILDER's native platform
# (--platform=$BUILDPLATFORM) so rustc runs natively and CROSS-compiles to
# $TARGETARCH — the heavy codegen is never emulated under QEMU, which is the
# slow part of a plain multi-arch build. The pipewire/spa/alsa -sys crates
# link C libraries AND run bindgen, so for a cross target we install the
# target arch's dev libs (dpkg multiarch) + a cross gcc/linker, and point
# cargo, pkg-config, and bindgen at them. A native target (TARGETARCH ==
# BUILDARCH, e.g. amd64 on an amd64 runner) skips all the cross plumbing.
#
# libasound2-dev: the embedded sendspin server role (src/sendspin_server.rs,
# a `sendspin` git dependency — see docs/decisions.md) pulls in `cpal`/`alsa`
# transitively even though this daemon only uses sendspin-rs's server-role
# audio *input* path, not its client-side playback (`cpal`) — that crate has
# no feature flag to opt out of its own audio-output module. Only needed to
# link at build time; the -dev package doesn't ship in the runtime image.
FROM --platform=$BUILDPLATFORM rust:1-slim-trixie AS build
ARG TARGETARCH
ARG BUILDARCH
# Resolve arch-specific names, install deps, and write /build-env.sh with the
# cargo/pkg-config/bindgen env the actual build step sources. Debian trixie
# carries arm64 on the main mirror, and its cross gcc is multiarch-aware
# (finds :arch libs under /usr/lib/<triplet>), which is why this stays simple.
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu;  gnu=x86_64-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu ;; \
      *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "export CARGO_BUILD_TARGET=$triple" > /build-env.sh; \
    if [ "$TARGETARCH" = "$BUILDARCH" ]; then \
        apt-get update; \
        apt-get install -y --no-install-recommends \
            pkg-config clang libpipewire-0.3-dev libspa-0.2-dev libasound2-dev; \
    else \
        dpkg --add-architecture "$TARGETARCH"; \
        apt-get update; \
        # libc6-dev:<arch> puts the target's libc headers at the multiarch
        # path (/usr/include/<triplet>/bits/…) where the cross gcc AND bindgen's
        # clang look — without it, cc-based crates like `ring` fail with
        # "bits/libc-header-start.h: No such file or directory". Co-installs
        # cleanly alongside the base image's amd64 libc6-dev on trixie.
        apt-get install -y --no-install-recommends \
            pkg-config clang "gcc-${gnu}" "libc6-dev:$TARGETARCH" \
            "libpipewire-0.3-dev:$TARGETARCH" "libspa-0.2-dev:$TARGETARCH" "libasound2-dev:$TARGETARCH"; \
        up_triple="$(echo "$triple" | tr 'a-z-' 'A-Z_')"; \
        { echo "export CARGO_TARGET_${up_triple}_LINKER=${gnu}-gcc"; \
          echo "export PKG_CONFIG_ALLOW_CROSS=1"; \
          echo "export PKG_CONFIG_SYSROOT_DIR=/"; \
          echo "export PKG_CONFIG_PATH=/usr/lib/${gnu}/pkgconfig"; \
          echo "export BINDGEN_EXTRA_CLANG_ARGS='--target=${gnu} --sysroot=/ -I/usr/include/${gnu}'"; \
        } >> /build-env.sh; \
    fi; \
    rustup target add "$triple"; \
    rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY bridge-daemon/ .
RUN . /build-env.sh && cargo build --release
# Copy to a fixed path so the runtime stage's COPY doesn't need the triple.
RUN . /build-env.sh && cp "target/${CARGO_BUILD_TARGET}/release/bridge-daemon" /bridge-daemon

FROM ubuntu:26.04
# systemd-standalone-sysusers: `pipewire`'s Depends is `systemd |
# systemd-standalone-sysusers` (either satisfies its need to create system
# users at install time) — apt defaults to the first alternative, the full
# `systemd` + `libsystemd-shared` (~18MB), even though we never run systemd
# as PID 1 (run.sh is) and don't need anything else from it. Listing the
# lightweight alternative explicitly (340KB) satisfies the same dependency;
# confirmed via `apt-get install -s` that it changes nothing else about the
# resolved package set.
#
# dbus-x11 stays: `wireplumber` itself hard-Depends on some session-bus
# provider (`<default-dbus-session-bus>` → `dbus-user-session` or
# `dbus-x11`), not just something run.sh happens to invoke — confirmed by
# simulating the install without it. The only alternative,
# `dbus-user-session`, itself hard-Depends on `systemd`/`libpam-systemd`,
# reintroducing the exact ~18MB this comment's first fix removes. So
# dbus-x11 (118KB) + its libx11 chain (~2.8MB) is genuinely the smaller of
# the two real options here, not an oversight.
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
        avahi-daemon \
    && rm -rf /var/lib/apt/lists/* \
    # Man pages, translated message catalogs, and long-form package docs are
    # dead weight in a container that only ever talks over its own REST API
    # and logs in English — safe to drop, same reasoning as everything else
    # in this stage. Must happen in this same layer (not a later RUN) or the
    # underlying layer still carries the bytes despite the delete marker.
    && rm -rf /usr/share/doc/* /usr/share/man/* /usr/share/locale/*
# No ffmpeg: announce/TTS audio (Section 5.6) is decoded by the bridge
# daemon itself via symphonia (bridge-daemon/src/decode.rs), a pure-Rust
# decoder with no system dependencies. Ubuntu's ffmpeg package used to
# pull ~250-300MB of unrelated GPU/video-transcoding libraries (mesa,
# LLVM, libx265, libplacebo, VA-API/OpenCL, X11) into this image for what
# was only ever "decode a short spoken-word clip" — confirmed via
# `docker history` and an isolated `apt-get install -s ffmpeg` on a clean
# base; see docs/decisions.md.
#
# No python3/pip/aiosendspin either, for the same reason: the sendspin sink
# adapter used to be a separate Python process per configured output,
# embedding aiosendspin[server] (~212MB of av/numpy/pillow). It's now an
# embedded native server (src/sendspin_server.rs, a `sendspin` git
# dependency pinned to a commit — see docs/decisions.md for why that
# calculus changed). No `libasound2` needed at runtime either, despite the
# build-time `libasound2-dev` above: `ldd` on the compiled binary shows no
# ALSA link at all (just libpipewire/libc/libgcc/libm) — the linker drops
# cpal/alsa's DT_NEEDED entry once it sees nothing in the reachable code path
# actually calls into it, since this daemon only uses sendspin-rs's server
# role, never its client-side audio *output* module that cpal belongs to.

# Real-time scheduling without RTKit. The `pipewire` daemon, `wireplumber`, AND
# every PipeWire *client* (client.conf — this includes the bridge daemon, which
# hosts the raop-sink/sendspin nodes in its own context, so its data-loop is the
# most latency-critical of the three) load `libpipewire-module-rt`. By default
# (`rtkit.enabled` unset -> true) it tries to obtain RT priority through the
# RTKit D-Bus service. There is no rtkit-daemon on this container's system bus,
# so that path only spams `RTKit error: org.freedesktop.DBus.Error.ServiceUnknown`
# at startup and then silently falls back to non-RT scheduling — the worst of
# both. We instead force module-rt's direct path (`rlimits`, enabled by
# default): with CAP_SYS_NICE granted to the container (config.yaml
# `privileged: [SYS_NICE]`) it calls sched_setscheduler(SCHED_FIFO) directly,
# giving the audio data-loop real RT priority that preempts CPU-bound
# normal-priority work (e.g. an OS update or compile running on the same Pi).
#
# module-rt lives inside a `context.modules` array; drop-in configs only ever
# *append* to arrays (pipewire.conf(5)), so its args can't be overridden that
# way without loading the module twice — hence patching the shipped base files.
# rtportal.enabled is also forced off (the XDG desktop portal isn't present
# either). Self-checking: `set -eux` plus the greps fail the build if a future
# base image drops/renames the module block, surfacing the drift here instead
# of as silent non-RT audio in production. (The mem-locking half of the
# hardening — mem.mlock-all — is set via drop-ins below, since those props live
# in the dict-merged `context.properties`, not the module array.)
RUN set -eux; \
    for conf in /usr/share/pipewire/pipewire.conf /usr/share/pipewire/client.conf /usr/share/wireplumber/wireplumber.conf; do \
        grep -q 'libpipewire-module-rt' "$conf"; \
        awk '/libpipewire-module-rt/{rt=1} \
             rt && /args[ \t]*=[ \t]*\{/ { \
                 print; \
                 print "        rtkit.enabled = false"; \
                 print "        rtportal.enabled = false"; \
                 rt=0; next \
             } \
             {print}' "$conf" > "$conf.tmp"; \
        mv "$conf.tmp" "$conf"; \
        grep -q 'rtkit.enabled = false' "$conf"; \
    done

COPY --from=build /bridge-daemon /usr/local/bin/bridge-daemon
# Built web UI served by the daemon (default --static-dir /app/www).
COPY --from=frontend /ui/dist /app/www
COPY rootfs/run.sh /run.sh
# WirePlumber drop-in disabling unused hardware monitors (camera, bluetooth)
# and the snap/logind integrations — silences their harmless "missing host
# bits" startup warnings at the source. See the file's own header for details.
COPY rootfs/etc/wireplumber/wireplumber.conf.d/50-headless.conf /etc/wireplumber/wireplumber.conf.d/50-headless.conf
# PipeWire mem-locking drop-ins (daemon + client contexts): mlockall() the
# audio path so host memory pressure can't page it out mid-stream. Pairs with
# the RT patch above and CAP_IPC_LOCK in config.yaml. See the files' headers.
COPY rootfs/etc/pipewire/pipewire.conf.d/50-mlock.conf /etc/pipewire/pipewire.conf.d/50-mlock.conf
COPY rootfs/etc/pipewire/client.conf.d/50-mlock.conf /etc/pipewire/client.conf.d/50-mlock.conf
RUN chmod +x /run.sh /usr/local/bin/bridge-daemon

ENV XDG_RUNTIME_DIR=/run/pipewire

# The HA add-on version, passed at build time (scripts/deploy-dev.sh and the CI
# workflow set --build-arg ADDON_VERSION=<config.yaml version / dev tag>). The
# daemon logs it at startup. Placed last so a version-only change invalidates
# just this trivial layer, never the expensive build/apt layers above.
ARG ADDON_VERSION=dev
ENV ADDON_VERSION=${ADDON_VERSION}

ENTRYPOINT ["/run.sh"]
