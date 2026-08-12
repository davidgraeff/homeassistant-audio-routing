#!/usr/bin/env bash
# Iterate on voice ducking without talking to a voice assistant.
#
# The feature has two independent halves, and they fail for different reasons:
#
#   daemon half  POST /api/duck -> OverlayMixer gain on the per-device relay
#   HA half      assist_satellite goes non-idle -> area -> outputs -> POST /api/duck
#
# So test them separately. `duck`/`unduck` exercise the daemon half only (no HA
# involved at all). `fake` drives the HA half by writing a satellite's state, which
# is exactly the trigger `voice_duck.py` subscribes to — same code path as speaking,
# minus the wake word and the pipeline.
#
# The HA-side calls go over ssh and borrow the SSH add-on's SUPERVISOR_TOKEN, so no
# long-lived token is needed.
#
# Usage:
#   ./voice_duck_dev.sh holds                    # GET /api/duck — who is ducked, and how far
#   ./voice_duck_dev.sh duck [level] [ttl_ms]    # duck every output (daemon half)
#   ./voice_duck_dev.sh unduck                   # release every hold this script can see
#   ./voice_duck_dev.sh sats                     # satellites + their areas + the switch state
#   ./voice_duck_dev.sh fake <sat-substring> [s] # hold a satellite non-idle for [s] seconds (default 15)
#   ./voice_duck_dev.sh debug                    # turn on voice_duck debug logging in HA
#   ./voice_duck_dev.sh log                      # the daemon's duck lines
set -euo pipefail

HOST="${HA_HOST:-homeassistant.local}"
API="http://${HOST}:8099"
SSH="ssh -o ConnectTimeout=8 ${HOST}"

# Call the HA core API from the instance, with a JSON body on stdin (or none).
ha_api() {
	local method="$1" path="$2"
	# shellcheck disable=SC2029  # $method/$path are ours, expanding them here is the point
	$SSH "cat > /tmp/vd_body.json; curl -s -m 15 -X ${method} \
		-H \"Authorization: Bearer \$SUPERVISOR_TOKEN\" -H 'Content-Type: application/json' \
		${method:+--data-binary @/tmp/vd_body.json} http://supervisor/core/api/${path}"
}

tmpl() { ha_api POST template | sed -e 's/\\n/\n/g'; }

holds() {
	curl -s -m 8 "${API}/api/duck" |
		python3 -c 'import json,sys
h = json.load(sys.stdin)
if not h:
    print("no live duck holds")
for x in h:
    print("%-55s hold %-5s level %s" % (x["output"], x["hold_id"], x["level"]))'
}

outputs() { curl -s -m 8 "${API}/api/outputs" | python3 -c 'import json,sys; print(json.dumps([o["node_name"] for o in json.load(sys.stdin)]))'; }

case "${1:-holds}" in
holds) holds ;;

duck)
	level="${2:-0.15}"
	ttl="${3:-30000}"
	curl -s -m 8 -X POST -H 'Content-Type: application/json' \
		-d "{\"targets\": $(outputs), \"level\": ${level}, \"ttl_ms\": ${ttl}}" \
		"${API}/api/duck"
	echo
	;;

unduck)
	# One hold usually covers several outputs, so GET /api/duck repeats its id per output.
	for id in $(curl -s -m 8 "${API}/api/duck" | python3 -c 'import json,sys; print(" ".join(str(i) for i in sorted({x["hold_id"] for x in json.load(sys.stdin)})))'); do
		echo -n "release ${id}: "
		curl -s -m 8 -X DELETE "${API}/api/duck/${id}"
		echo
	done
	holds
	;;

sats)
	printf '{"template": "{%% for e in states.assist_satellite %%}SAT {{e.entity_id}} | {{ area_name(e.entity_id) or \\"NO AREA\\" }} | {{ e.state }}\\n{%% endfor %%}switch.voice_assistant_ducking = {{ states(\\"switch.voice_assistant_ducking\\") }} | level {{ states(\\"number.voice_assistant_duck_level\\") }} | scope {{ states(\\"select.voice_assistant_duck_scope\\") }}"}' | tmpl
	echo
	;;

fake)
	sat="${2:?usage: fake <satellite-substring> [seconds]}"
	secs="${3:-15}"
	entity="$(printf '{"template": "{%% for e in states.assist_satellite %%}{%% if \\"%s\\" in e.entity_id %%}{{ e.entity_id }}{%% endif %%}{%% endfor %%}"}' "$sat" | tmpl)"
	[ -n "$entity" ] || {
		echo "no assist_satellite matching '${sat}'" >&2
		exit 1
	}
	echo "--> ${entity} = listening (for ${secs}s)"
	printf '{"state": "listening"}' | ha_api POST "states/${entity}" >/dev/null
	sleep 2
	holds
	sleep "$((secs - 2))"
	echo "--> ${entity} = idle"
	printf '{"state": "idle"}' | ha_api POST "states/${entity}" >/dev/null
	sleep 2
	holds
	;;

debug)
	printf '{"custom_components.pipewire_audio_router.voice_duck": "debug", "custom_components.pipewire_audio_router.api": "debug"}' |
		ha_api POST services/logger/set_level
	echo "debug logging on (until HA restarts)"
	;;

log) $SSH 'docker logs --tail 3000 app_local_pipewire_audio_router 2>&1 | grep -iE "duck|unduck" | tail -30' ;;

*)
	sed -n '/^# Usage:/,/^set -e/p' "$0" | head -n -1
	exit 1
	;;
esac
