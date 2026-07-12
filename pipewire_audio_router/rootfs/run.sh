#!/bin/bash
set -e

export XDG_RUNTIME_DIR=/run/pipewire
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

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

# Static RAOP output config must exist *before* pipewire starts — PipeWire's
# core protocol has no remote "load module" RPC (confirmed against
# pipewire/core.h directly), so outputs can't be added/changed at runtime
# without a restart. See bridge-daemon/src/pw_config_gen.rs.
mkdir -p /etc/pipewire/pipewire.conf.d
bridge-daemon generate-config \
    --options /data/options.json \
    --out /etc/pipewire/pipewire.conf.d/10-outputs.conf

pipewire &
PIDS=("$!")

for _ in $(seq 1 50); do
    [ -S "$XDG_RUNTIME_DIR/pipewire-0" ] && break
    sleep 0.1
done

wireplumber &
PIDS+=("$!")

sleep 1

# avahi-daemon is only actually needed if an AirPlay-receive source is
# configured, but starting it unconditionally is harmless and simpler
# than conditioning on runtime-plan's output twice.
avahi-daemon --daemonize --no-drop-root

# Every source/output process beyond PipeWire/WirePlumber/the bridge
# daemon itself, derived from options.json by the same code that already
# parses it for generate-config — see bridge-daemon's
# `runtime-plan` subcommand doc for the line format.
while IFS=$'\t' read -r kind a b c; do
    case "$kind" in
        airplay_source)
            shairport-sync -a "$a" &
            PIDS+=("$!")
            ;;
        sendspin_adapter)
            python3 /usr/local/bin/sendspin-adapter.py \
                --node-name "$a" --sendspin-name "$b" --sendspin-port "$c" &
            PIDS+=("$!")
            ;;
        *)
            echo "run.sh: unknown runtime-plan component kind '$kind', skipping" >&2
            ;;
    esac
done < <(bridge-daemon runtime-plan --options /data/options.json)

sleep 1

bridge-daemon serve --options /data/options.json &
PIDS+=("$!")

trap 'kill "${PIDS[@]}" 2>/dev/null' TERM INT

# Fail-fast: if any component dies unexpectedly, exit so the add-on
# supervisor (Docker/HA) restarts the whole container rather than limping
# along with a dead component.
wait -n "${PIDS[@]}"
