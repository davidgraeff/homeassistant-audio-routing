#!/bin/bash
set -e

export XDG_RUNTIME_DIR=/run/pipewire
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

# Restart-idempotency: a `docker restart` (and the HA supervisor's stop/start of
# an add-on) reuses the container's writable /run, so socket files from the
# previous boot survive. The pipewire sockets are normally re-bound fine, but a
# leftover from an unclean exit would block the daemon, so drop them so a
# restart boots exactly like a fresh start.
rm -f "$XDG_RUNTIME_DIR"/pipewire-0 "$XDG_RUNTIME_DIR"/pipewire-0-manager

# Private per-container D-Bus session bus. Some PipeWire modules probe for
# a portal/rtkit and fall back gracefully when absent
# (spikes/01-headless-pipewire.md) — cheap to provide regardless.
eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS

# RAOP outputs are NOT statically configured before pipewire starts anymore.
# The bridge daemon loads one libpipewire-module-raop-sink per output into its
# own PipeWire context at runtime — hot-reloadable via the /api/outputs API —
# so there's nothing to generate up front. See docs/decisions.md "Loading
# PipeWire modules at runtime" and bridge-daemon/src/pw_module.rs.

pipewire &
PIDS=("$!")

for _ in $(seq 1 50); do
    [ -S "$XDG_RUNTIME_DIR/pipewire-0" ] && break
    sleep 0.1
done

wireplumber &
PIDS+=("$!")

sleep 1

# The bridge daemon owns everything user-configurable at runtime, all
# in-process: it loads a raop-sink module per RAOP output, runs the native
# AirPlay-receive source (airplay_source.rs) and the RTP source, and hosts the
# embedded sendspin server per output (sendspin_server.rs). No source/adapter
# subprocesses to spawn or supervise anymore, and no boot-time process plan —
# outputs, the AirPlay/RTP sources, and sendspin outputs are all managed live
# via the API (see bridge-daemon/src/sources_store.rs and docs/decisions.md).
# mDNS discovery/advertising is all mdns-sd (no avahi/system-D-Bus daemon).
bridge-daemon serve &
PIDS+=("$!")

trap 'kill "${PIDS[@]}" 2>/dev/null' TERM INT

# Fail-fast: if any component dies unexpectedly, exit so the add-on
# supervisor (Docker/HA) restarts the whole container rather than limping
# along with a dead component.
wait -n "${PIDS[@]}"
