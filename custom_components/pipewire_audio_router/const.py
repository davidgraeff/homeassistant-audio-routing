"""Constants for the PipeWire Audio Router integration."""

DOMAIN = "pipewire_audio_router"

CONF_HOST = "host"
CONF_PORT = "port"
DEFAULT_PORT = 8099

# Default UDP port for the Bluetooth-bridge RTP source. Matches the daemon's
# own default (bridge-daemon/src/rtp_source.rs) and the firmware example —
# used to seed the port `number` entity before the daemon reports a value.
DEFAULT_RTP_PORT = 46000

# Poll interval for GET /api/media_players (volume + playing/idle state) —
# there's no push channel for those. Routing is NOT polled: it's pushed live
# over the daemon's /api/routing/ws WebSocket (see PipewireRouterCoordinator).
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
