# syntax=docker/dockerfile:1
# PipeWire Audio Router — Home Assistant add-on.
#
# Three stages: build the Svelte web UI (frontend/), build the Rust bridge
# daemon against libpipewire-0.3-dev, then assemble the runtime image on the
# same Ubuntu 26.04 LTS base validated end-to-end (including real hardware) in
# spikes 1-4 — see ../PLAN.md Section 5.1 and ../spikes/ for why this exact
# base, why these exact PipeWire/WirePlumber versions, and why no ALSA/Bluez
# hardware monitors are enabled.
#
# EVERY BASE IS PINNED BY DIGEST, and that is a caching decision as much as a
# reproducibility one: a floating tag silently invalidates every layer beneath
# it. `rust:1-slim-trixie` was republished between two CI runs a day apart
# (3b2879047d42 → 8e8cf8f7fd54) and the daemon recompiled from scratch for 7½
# minutes on a *warm* registry cache, because the chain's root had moved. The
# tag is kept next to the digest so it stays readable; refresh them on purpose
# with `scripts/bump-base-images.sh` (which also says what changed).

# Web UI (frontend/, Vite + Svelte). Built to static assets the daemon serves;
# no Node in the runtime image. Pinned to the builder's native arch
# (--platform=$BUILDPLATFORM): the output is just static JS/CSS, so there's no
# reason to run npm under QEMU emulation for the target arch.
FROM --platform=$BUILDPLATFORM node:22-slim@sha256:d649c27dae7ba0137b3cef5dd75baa422c08dc3d9e3fc0c23dfb172dc3cc6436 AS frontend
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
#
# libevent-dev: the vendored libairptp (bridge-daemon/vendor/libairptp/, OwnTone's
# MIT AirPlay-2 gPTP grandmaster, compiled by build.rs — see src/ap2_ptp.rs) uses
# libevent for its PTP socket event loop. Provides <event2/*.h> + the link libs
# (-levent/-levent_pthreads). Unlike libasound2, libevent IS reachable at runtime
# (libairptp actively runs its event loop), so the runtime image installs the
# matching shared lib below.
#
# cmake + make: `opusic-sys` (the Opus encoder for the sendspin wire codec — see
# src/sendspin_codec.rs) builds its BUNDLED libopus, and its build script drives
# that build through CMake (`bundled` is its default feature, `bundled =
# ["dep:cmake"]`). `make` is needed too — this base is `rust:1-slim-*`, which ships
# no make, and CMake's default "Unix Makefiles" generator fails with
# "CMAKE_MAKE_PROGRAM is not set" without it. The cmake crate handles the cross case
# itself (it passes CMAKE_C_COMPILER=<triple>-gcc + CMAKE_SYSTEM_PROCESSOR), so no
# toolchain file is required. Vendoring libopus is deliberate: it ends up statically
# linked, so the runtime image needs no libopus .so and can't drift in version.
# Build-time only, like libasound2-dev. NOTE: `scripts/build-arm64.sh` cannot catch a
# missing build dep here — it builds container/Dockerfile, which has no Rust build
# stage at all; only `scripts/deploy-dev.sh addon` (or CI) exercises this stage.
FROM --platform=$BUILDPLATFORM rust:1-slim-trixie@sha256:8e8cf8f7fd54a2d23d5a743b3a03f56e26b6c774276c33fa0595111704ebb15c AS build
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
        # g++ for the same reason the cross branch below installs g++-<triplet>:
        # cc-rs crates that compile C++ (fdk-aac-sys) need cc1plus, and the
        # rust:1-slim base ships gcc WITHOUT g++ — so the native path needs it
        # spelled out too, or the AAC build dies with "cannot execute 'cc1plus'".
        apt-get install -y --no-install-recommends \
            pkg-config clang cmake make g++ libpipewire-0.3-dev libspa-0.2-dev libasound2-dev libevent-dev; \
    else \
        dpkg --add-architecture "$TARGETARCH"; \
        apt-get update; \
        # libc6-dev:<arch> puts the target's libc headers at the multiarch
        # path (/usr/include/<triplet>/bits/…) where the cross gcc AND bindgen's
        # clang look — without it, cc-based crates like `ring` fail with
        # "bits/libc-header-start.h: No such file or directory". Co-installs
        # cleanly alongside the base image's amd64 libc6-dev on trixie.
        # gcc-<triplet> is the C cross-compiler; g++-<triplet> is the matching
        # C++ one (provides cc1plus) — needed by cc-rs crates that build C++,
        # e.g. fdk-aac-sys. Without it the AAC build dies with "cannot execute
        # 'cc1plus'".
        apt-get install -y --no-install-recommends \
            pkg-config clang cmake make "gcc-${gnu}" "g++-${gnu}" "libc6-dev:$TARGETARCH" \
            "libpipewire-0.3-dev:$TARGETARCH" "libspa-0.2-dev:$TARGETARCH" "libasound2-dev:$TARGETARCH" \
            "libevent-dev:$TARGETARCH"; \
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
# The sendspin submodule. `bridge-daemon/Cargo.toml` reaches it as
# `../submodules/sendspin`, which from WORKDIR /build resolves to
# /submodules/sendspin — so it must be copied to an absolute path outside /build,
# not into it. It lives above bridge-daemon/ in the source tree so rustfmt's
# upward config search never reaches bridge-daemon/rustfmt.toml and reformats a
# third-party crate into our style.
#
# Copied before bridge-daemon/ because it changes far less often: an edit under
# bridge-daemon/ then still hits this layer's cache.
COPY submodules/ /submodules/
# An uninitialised submodule otherwise fails deep inside cargo as "failed to read
# /submodules/sendspin/Cargo.toml", which doesn't name the cause.
RUN test -f /submodules/sendspin/Cargo.toml || { \
        echo "ERROR: submodules/sendspin is empty — run 'git submodule update --init --recursive'" >&2; \
        exit 1; \
    }
# The shared pw-control crate (volume pods + the module-load FFI, used by both the
# daemon and the receiver-side agent). `bridge-daemon/Cargo.toml` reaches it as
# `../pw-control`, which from WORKDIR /build resolves to /pw-control — same
# outside-of-/build trick as the submodule above, and copied before bridge-daemon/
# for the same cache reason. Manifest + sources only, like the agent stage: nothing
# else in that directory (rustfmt.toml) is read by the build.
COPY pw-control/Cargo.toml /pw-control/
COPY pw-control/src/ /pw-control/src/
COPY bridge-daemon/ .
# BuildKit cache mounts make this incremental. Without them every deploy
# recompiled all ~600 dependency crates (plus bundled libopus via cmake and
# libairptp via build.rs) from scratch, because ANY edit under bridge-daemon/
# invalidates the COPY above and therefore this RUN's layer cache — a layer
# cache can only be hit or missed whole, it can't be partially reused. The
# cache mounts live outside the layer graph, so cargo still finds last build's
# target/ fingerprints and rebuilds only what actually changed. The target
# cache is keyed by $TARGETARCH so an amd64 and an arm64 build never fight
# over the same artifacts.
#
# The `cp` MUST stay in this same RUN: /build/target is a cache mount, so it
# is NOT captured in the resulting layer — a separate `RUN cp` would find the
# directory empty. /bridge-daemon is outside the mount, so it does persist.
# (Fixed path so the runtime stage's COPY doesn't need the triple.)
#
# Caveat: cache mounts are builder-local (they live in the ha-addon-builder
# container's volume) and are not exported by --cache-to. `docker buildx rm
# ha-addon-builder` or `docker builder prune` throws them away and the next
# build is a full one again.
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=bridge-daemon-target-${TARGETARCH},target=/build/target,sharing=locked \
    . /build-env.sh \
    && cargo build --release \
    && cp "target/${CARGO_BUILD_TARGET}/release/bridge-daemon" /bridge-daemon

# ---------------------------------------------------------------------------
# pwrouter-agent — the receiver-side helper users install on the machines they
# want to stream to (docs/receiver-agent.md §10). Built for BOTH
# architectures regardless of the add-on's own, because the add-on serves the
# binaries for download and the machine on the other end is usually not the same
# arch as the Pi.
#
# Bookworm, not the trixie base above: the binaries run on the *user's* distro,
# so what matters is the glibc symbol versions they end up referencing. A modern
# toolchain floors at GLIBC_2.34 (the release where pthread/dl merged into libc),
# which is Ubuntu 22.04 LTS / Fedora 35 / Debian 12 — old enough to cover current
# systems. Bookworm also ships PipeWire 0.3.65 headers, new enough for
# pipewire-rs 0.10. Building on trixie instead would demand its much newer glibc
# from every downloader for no benefit.
FROM --platform=$BUILDPLATFORM rust:1-slim-bookworm@sha256:2775a09d208ff0d7c1f50490c45b62db929e87ba1dcbc3f2132ac71a704bcdd3 AS agent
# Both targets' libs side by side. Only the *foreign* architecture is added:
# `dpkg --add-architecture` for the native one makes apt resolve unqualified
# package names against the wrong architecture, which fails as
# "gcc-12-x86-64-linux-gnu:arm64 : Depends: … not installable". The native arch
# comes from dpkg rather than a build ARG so this works the same under podman,
# buildah and BuildKit.
RUN set -eux; \
    native="$(dpkg --print-architecture)"; \
    case "$native" in \
      amd64) foreign=arm64; foreign_gnu=aarch64-linux-gnu ;; \
      arm64) foreign=amd64; foreign_gnu=x86_64-linux-gnu ;; \
      *) echo "unsupported builder architecture $native" >&2; exit 1 ;; \
    esac; \
    dpkg --add-architecture "$foreign"; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        pkg-config clang "gcc-${foreign_gnu}" \
        libpipewire-0.3-dev libspa-0.2-dev \
        "libc6-dev:${foreign}" "libpipewire-0.3-dev:${foreign}" "libspa-0.2-dev:${foreign}"; \
    rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; \
    rm -rf /var/lib/apt/lists/*
WORKDIR /agent
# Only what cargo actually reads, not the whole directory: this stage cross-builds
# TWO binaries and its target/ cache mount is not exported to the registry cache,
# so on a fresh builder (CI, or after a GC pass) an invalidated layer here means
# both arches compile from zero. Copying `pwrouter-agent/` wholesale spent that on
# a README.md edit, which the build does not read.
# `pwrouter-agent.service` *is* read: `autostart.rs` `include_str!`s it so the
# binary can install its own systemd unit with nothing to download. Editing it
# therefore does — correctly — mean recompiling both arches.
# pw-control's own Cargo.lock is deliberately not copied — cargo resolves a path
# dependency through the *root* crate's lock, so that file is build-irrelevant
# churn. pw-control comes first because it changes less often than the agent.
COPY pw-control/Cargo.toml /pw-control/
COPY pw-control/src/ /pw-control/src/
COPY pwrouter-agent/Cargo.toml pwrouter-agent/Cargo.lock pwrouter-agent/pwrouter-agent.service pwrouter-agent/build.rs ./
COPY pwrouter-agent/src/ ./src/
# The add-on's version, compiled *into* the agent binaries as their build id (build.rs
# reads PWROUTER_BUILD). Unlike the daemon — which reads ADDON_VERSION from its
# container env at runtime — the agent runs on someone else's machine, so the only way
# it can answer "which build am I?" is to carry the answer. That is what makes a
# downloaded binary comparable with the add-on that served it. Declared here so the
# existing `--build-arg ADDON_VERSION=…` (scripts/deploy-dev.sh, CI) reaches this stage
# too, and placed after the COPYs so a version bump does not invalidate them.
ARG ADDON_VERSION=dev
# One loop per arch: build, strip, then assert the binary really does floor at
# GLIBC_2.34 — shipping one that needs more would silently break the minimum
# distro versions the in-app help text promises. The objdump must be the matching
# target's, since a cross binutils only reliably reads its own architecture.
#
# No comments inside the RUN below: BuildKit strips comment lines before joining
# continuations, which would break the loop.
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=agent-target,target=/agent/target,sharing=locked \
    set -eux; \
    export PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR=/; \
    mkdir -p /agent-out; \
    for arch in amd64 arm64; do \
      case "$arch" in \
        amd64) triple=x86_64-unknown-linux-gnu;  gnu=x86_64-linux-gnu;  out=pwrouter-agent-x86_64 ;; \
        arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu; out=pwrouter-agent-aarch64 ;; \
      esac; \
      up_triple="$(echo "$triple" | tr 'a-z-' 'A-Z_')"; \
      env "CARGO_TARGET_${up_triple}_LINKER=${gnu}-gcc" \
          "PKG_CONFIG_PATH=/usr/lib/${gnu}/pkgconfig" \
          "BINDGEN_EXTRA_CLANG_ARGS=--target=${gnu} --sysroot=/ -I/usr/include/${gnu}" \
          "PWROUTER_BUILD=${ADDON_VERSION}" \
          cargo build --release --target "$triple"; \
      cp "target/${triple}/release/pwrouter-agent" "/agent-out/${out}"; \
      "${gnu}-strip" "/agent-out/${out}"; \
      max="$("${gnu}-objdump" -T "/agent-out/${out}" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -uV | tail -1)"; \
      echo "${out} requires up to ${max}"; \
      case "$max" in \
        GLIBC_2.1?|GLIBC_2.2?|GLIBC_2.3|GLIBC_2.3[0-4]) ;; \
        *) echo "ERROR: ${out} needs ${max}, above the GLIBC_2.34 floor the help text promises" >&2; exit 1 ;; \
      esac; \
    done

# Just the two agent binaries, nothing else — the stage
# .github/workflows/release.yml targets to get them OUT of the build and onto a
# GitHub release (`--target agent-dist --output type=local`). From `scratch`
# because a local export writes the whole stage filesystem: exporting `agent`
# itself would mean unpacking the ~1.5 GB Rust toolchain image to disk for two
# files. Nothing in the add-on image depends on this stage, so a plain image
# build never runs it.
FROM scratch AS agent-dist
COPY --from=agent /agent-out/ /

# The add-on, all but the agent downloads — and a stage name, because that split
# is the difference between the e2e suite waiting for one Rust build or three.
# The `agent` stage cross-compiles pwrouter-agent TWICE (x86_64 + aarch64) and
# nothing in the end-to-end tests downloads either binary, yet a plain build of
# this image needs them for the COPY below, so they were built on every CI run
# and competed with the daemon's own compile for the runner's few cores
# (measured: 273 s of agent alongside 451 s of daemon). `--target runtime` is
# the whole image minus that, which is what .github/workflows/ci.yml builds.
FROM ubuntu:26.04@sha256:678c6550cc43645e08669028bc177f50be4e7c5b8cca677067b1914d4afc7a03 AS runtime
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
        # The system CA store. Not optional: our reqwest uses the `rustls`
        # feature, which is `rustls-platform-verifier` — it loads roots from the
        # *system* store, and with an empty /etc/ssl/certs EVERY
        # `Client::builder().build()` fails with "No CA certificates were loaded
        # from the system". That broke two unrelated things at once: fetching an
        # announcement/TTS clip by URL (`api.rs::fetch_to_file`) and the
        # Bluetooth-bridge diagnostics probe, which reported every bridge's page
        # as unreachable while it was serving fine (~400KB, and it is what makes
        # any https:// clip URL work at all).
        ca-certificates \
        # Runtime shared libs for the vendored libairptp (AirPlay-2 gPTP master,
        # src/ap2_ptp.rs) — it runs a libevent loop, so unlike libasound2 the
        # DT_NEEDED is NOT dropped and the .so must be present. Names carry the
        # t64 (64-bit time_t) suffix on Ubuntu 26.04.
        libevent-2.1-7t64 \
        libevent-pthreads-2.1-7t64 \
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
#
# No avahi-daemon (and no system D-Bus started in run.sh) either: it was only
# ever there for the shairport-sync subprocess, which hard-required avahi + a
# real system bus to start. The native AirPlay receiver replaced it, and all
# mDNS discovery/advertising (AirPlay-2 + sendspin outputs, AirPlay receive) now
# goes through the pure-Rust `mdns-sd` crate, which needs no daemon — see
# docs/decisions.md "Native AirPlay-receive source".

# Real-time scheduling without RTKit. The `pipewire` daemon, `wireplumber`, AND
# every PipeWire *client* (client.conf — this includes the bridge daemon, which
# hosts the group anchor / RTP-source nodes in its own context, so its data-loop
# is the most latency-critical of the three) load `libpipewire-module-rt`. By default
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

# The shipped image = `runtime` plus the downloadable receiver-agent binaries,
# served by the daemon's static file handler
# (frontend/src/components/outputs/ReceiverAgentDocs.svelte links to them). This
# is the LAST stage, so every build that names no target — Supervisor,
# scripts/deploy-dev.sh, build-addon.yml — still gets the complete add-on;
# only the e2e job asks for `runtime` and skips the two agent cross-compiles.
# ENV/ENTRYPOINT are inherited from the base stage, so nothing is repeated here.
FROM runtime
COPY --from=agent /agent-out/ /app/www/agent/
