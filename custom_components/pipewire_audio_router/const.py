"""Constants for the PipeWire Audio Router integration."""

DOMAIN = "pipewire_audio_router"

CONF_HOST = "host"
CONF_PORT = "port"
DEFAULT_PORT = 8099

# Default UDP port for the Bluetooth-bridge RTP source. Matches the daemon's
# own default (bridge-daemon/src/rtp_source.rs) and the firmware example —
# used to seed the port `number` entity before the daemon reports a value.
DEFAULT_RTP_PORT = 46000

# Default receiver-side jitter buffer (ms) for the RTP source. Matches the
# daemon's own default (bridge-daemon/src/rtp_source.rs) — used to seed the
# latency `number` entity before the daemon reports a value. Raise it on a
# weak-signal bridge to trade latency for fewer audible dropouts.
DEFAULT_RTP_LATENCY_MSEC = 200

# Poll interval for the daemon state that has no push channel (RTP source,
# per-device volumes, output metadata, named groups). Routing is NOT polled:
# it's pushed live over /api/routing/ws (see PipewireRouterCoordinator).
UPDATE_INTERVAL_SECONDS = 5

# Delay before the routing WebSocket task reconnects after the socket drops.
ROUTING_WS_RECONNECT_SECONDS = 5

# Shown as an extra option in every output's `source_list` — selecting it
# unlinks whatever source currently feeds the output (an "off"/disconnect).
SOURCE_NONE = "None"

# Low-level routing services (the escape hatch beyond media_player's
# exclusive `select_source`): `link` is additive, `unlink` removes.
SERVICE_LINK = "link"
SERVICE_UNLINK = "unlink"
ATTR_SOURCE = "source"

# Integration-wide service to purge media_player entities the daemon no longer
# reports (e.g. renamed/removed devices left behind as `unavailable`).
SERVICE_CLEANUP_ENTITIES = "cleanup_entities"

# Put a music-group preset in force — the whole grouping of the house in one call
# (docs/music-group-presets-plan.md). A service *and* a select entity, on purpose:
# this is what an automation wants (address it by name, no entity_id to look up),
# while the select is the only thing a template or a stock dashboard can read to
# find out which preset is on.
SERVICE_ACTIVATE_PRESET = "activate_preset"
ATTR_PRESET = "preset"

# --- Voice-assistant ducking (voice_duck.py) ---------------------------------
# Replaces the community "duck every media_player in the satellite's area via
# volume_set" blueprint: the daemon holds a leased mixer-gain duck instead, so
# nothing touches a device's user-visible volume and a per-room duck works even
# inside a multi-room music group.

# Music gain while a voice assistant in the room is talking. A *gain*, not the
# blueprint's divisor: 0.25 = quarter volume. Seeds the `number` entity.
DEFAULT_VOICE_DUCK_LEVEL = 0.25

# On by default: the feature needs no configuration at all (satellites and areas
# come from HA's registries), so an installation that shipped it switched off is
# one nobody discovers — there is no error, no log line, just music that never
# ducks. Anyone still running the volume-ducking blueprint gets *both* until they
# delete it, which is audible and self-inflicted rather than silent. An explicit
# "off" is remembered across restarts (`switch.py`).
DEFAULT_VOICE_DUCK_ENABLED = True

# Lease we ask the daemon for, and how often we renew it while a turn is open.
# The daemon un-ducks on its own one lease after we stop renewing, so a reload
# or crash mid-turn can't leave music quiet; renewing at a third of the lease
# leaves room for two missed renewals.
VOICE_DUCK_TTL_SECONDS = 30
VOICE_DUCK_RENEW_SECONDS = 10

# Which speakers duck for a satellite's turn.
#   area        — only the outputs in the satellite's own area, even when they
#                 are mid-song inside a multi-room group (the default).
#   music_group — those, plus every other member of a music group they belong
#                 to: for open-plan rooms where the same track next door
#                 drowns the response.
VOICE_DUCK_SCOPE_AREA = "area"
VOICE_DUCK_SCOPE_MUSIC_GROUP = "music_group"
VOICE_DUCK_SCOPES = [VOICE_DUCK_SCOPE_AREA, VOICE_DUCK_SCOPE_MUSIC_GROUP]

# `assist_satellite` states that mean "a turn is in progress". Everything else
# (idle, unavailable, unknown) releases the duck. `responding` is included on
# purpose: the satellite speaks its answer through its own speaker, so the
# room's music has to stay out of the way until the turn is fully over.
ASSIST_SATELLITE_DOMAIN = "assist_satellite"
VOICE_DUCK_ACTIVE_STATES = frozenset({"listening", "processing", "responding"})
