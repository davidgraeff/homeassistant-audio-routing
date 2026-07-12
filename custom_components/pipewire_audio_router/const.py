"""Constants for the PipeWire Audio Router integration."""

DOMAIN = "pipewire_audio_router"

CONF_HOST = "host"
CONF_PORT = "port"
DEFAULT_PORT = 8080

# Bridge daemon polling interval (Section 6/9). Not push-based (no
# WebSocket subscription exists yet on the bridge daemon side) — plain
# polling of GET /api/media_players is enough for Phase 3's scope.
UPDATE_INTERVAL_SECONDS = 5
