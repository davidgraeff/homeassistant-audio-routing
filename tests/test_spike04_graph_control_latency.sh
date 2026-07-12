#!/bin/bash
# Spike 4 (PLAN.md Section 7): is shelling out to pw-cli/pw-link/pw-dump for
# every graph mutation ("no native Python binding for libpipewire's graph
# API exists" — see spikes/04-graph-control.md) actually fast enough for a
# bridge daemon that needs routing changes to feel instant when a user
# clicks something in the web UI?
#
# Measures wall-clock latency of the three operations the bridge daemon
# would actually perform per routing change: read current graph state
# (pw-dump), create a link (pw-link), destroy a link (pw-link -d).
set -euo pipefail

IMAGE="${1:-pw-audio-router:dev}"
ITERATIONS="${ITERATIONS:-20}"

docker run --rm --entrypoint bash "$IMAGE" -c "
set -e
export XDG_RUNTIME_DIR=/run/pipewire
mkdir -p \"\$XDG_RUNTIME_DIR\"
eval \"\$(dbus-launch --sh-syntax)\"
export DBUS_SESSION_BUS_ADDRESS
pipewire > /tmp/pw.log 2>&1 &
sleep 1
wireplumber > /tmp/wp.log 2>&1 &
sleep 2

pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=spike4-source media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }' >/dev/null
pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=spike4-sink media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }' >/dev/null
sleep 1

now_ms() { local ns=\$(date +%s%N); echo \"\${ns:0:13}\"; }  # %3N width isn't honored by every date build; truncate manually instead

echo '--- pw-dump (state read) x $ITERATIONS ---'
total=0
for i in \$(seq 1 $ITERATIONS); do
  t0=\$(now_ms)
  pw-dump >/dev/null
  t1=\$(now_ms)
  total=\$((total + t1 - t0))
done
echo \"avg: \$((total / $ITERATIONS))ms\"

echo '--- pw-link create+destroy round trip x $ITERATIONS ---'
total=0
for i in \$(seq 1 $ITERATIONS); do
  t0=\$(now_ms)
  pw-link spike4-source:capture_FL spike4-sink:playback_FL >/dev/null 2>&1
  pw-link spike4-source:capture_FR spike4-sink:playback_FR >/dev/null 2>&1
  t1=\$(now_ms)
  pw-link -d spike4-source:capture_FL spike4-sink:playback_FL >/dev/null 2>&1
  pw-link -d spike4-source:capture_FR spike4-sink:playback_FR >/dev/null 2>&1
  t2=\$(now_ms)
  total=\$((total + t2 - t0))
done
echo \"avg round trip (create 2 links + destroy 2 links): \$((total / $ITERATIONS))ms\"
"
