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
