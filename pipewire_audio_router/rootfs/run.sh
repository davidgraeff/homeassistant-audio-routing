#!/bin/bash
set -e

export XDG_RUNTIME_DIR=/run/pipewire
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

# Restart-idempotency: a `docker restart` (and the HA supervisor's stop/start of
# an add-on) reuses the container's writable /run, so pid/socket files from the
# previous boot survive. `dbus-daemon --system` aborts hard if /run/dbus/pid
# already exists ("pid file exists, if the message bus is not running, remove
# this file"), which with `set -e` would kill the whole restart. Clear the
# stale runtime state so a restart boots exactly like a fresh start. (The
# pipewire sockets are normally re-bound fine, but a leftover from an unclean
# exit would block the daemon just the same, so drop them too.)
rm -f /run/dbus/pid
rm -f "$XDG_RUNTIME_DIR"/pipewire-0 "$XDG_RUNTIME_DIR"/pipewire-0-manager

# A real D-Bus *system* bus (not just the private session bus below) is
# required for avahi-daemon, which shairport-sync in turn hard-requires to
# even start (fatal exit otherwise) — confirmed in
# spikes/shairport-sync-source.md. Not needed by PipeWire/WirePlumber
# themselves (spikes/01-headless-pipewire.md), only by this source.
mkdir -p /run/dbus
dbus-daemon --system --fork

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

# avahi-daemon is needed by shairport-sync (the AirPlay-receive source) and
# for RAOP mDNS discovery. It's cheap and harmless to run unconditionally, and
# it must be up *before* the bridge daemon, which now spawns shairport-sync
# itself.
avahi-daemon --daemonize --no-drop-root

# The bridge daemon owns everything user-configurable at runtime: it loads a
# raop-sink module per RAOP output, and spawns/supervises the source/adapter
# processes (shairport-sync, sendspin-adapter.py) from its own persisted stores
# (seeded once from options.json). No boot-time process plan anymore — outputs,
# the AirPlay source, and sendspin outputs are all managed live via the API
# (see bridge-daemon/src/{supervisor,sources_store}.rs and docs/decisions.md).
bridge-daemon serve &
PIDS+=("$!")

trap 'kill "${PIDS[@]}" 2>/dev/null' TERM INT

# Fail-fast: if any component dies unexpectedly, exit so the add-on
# supervisor (Docker/HA) restarts the whole container rather than limping
# along with a dead component.
wait -n "${PIDS[@]}"
