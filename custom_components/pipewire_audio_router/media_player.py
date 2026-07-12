"""One MediaPlayerEntity per PipeWire output node the bridge daemon reports.

Deliberately scoped for Phase 3 (PLAN.md): state (playing/idle, derived
from whether anything is currently linked into this output — Section 6),
volume control, and ducked TTS/announce playback (Section 5.6, backed by
bridge-daemon's `/announce` endpoint and verified in
spikes/05-tts-ducking-mechanism.md), all real and tested. There is no
PipeWire-native concept of "paused" for a passive routing sink — PLAY/
PAUSE are not exposed for the same reason.
"""

from __future__ import annotations

from homeassistant.components.media_player import (
    MediaPlayerEntity,
    MediaPlayerEntityFeature,
    MediaPlayerState,
    MediaType,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import PipewireRouterCoordinator
from .api import MediaPlayerState as ApiMediaPlayerState
from .const import DOMAIN

# Must match RAOP_NODE_PREFIX (pw_config_gen.rs) and SENDSPIN_NODE_PREFIX
# (config.rs) on the bridge-daemon side exactly — there's no shared
# source of truth across the Rust/Python boundary, just this comment.
RAOP_NODE_PREFIX = "raop-out-"
SENDSPIN_NODE_PREFIX = "sendspin-out-"


def _display_name(node_name: str) -> str:
    """Strips the bridge daemon's node-name prefix and turns
    e.g. "sendspin-out-kitchen" into "Kitchen" for entity naming."""
    for prefix in (RAOP_NODE_PREFIX, SENDSPIN_NODE_PREFIX):
        if node_name.startswith(prefix):
            return node_name[len(prefix) :].replace("_", " ").title()
    return node_name


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    coordinator: PipewireRouterCoordinator = hass.data[DOMAIN][entry.entry_id]
    known_node_ids: set[int] = set()

    def _add_new_entities() -> None:
        new_entities = [
            PipewireRouterMediaPlayer(coordinator, entry, player.node_id, player.node_name)
            for player in coordinator.data
            if player.node_id not in known_node_ids
        ]
        if new_entities:
            known_node_ids.update(e.node_id for e in new_entities)
            async_add_entities(new_entities)

    _add_new_entities()
    entry.async_on_unload(coordinator.async_add_listener(_add_new_entities))


class PipewireRouterMediaPlayer(CoordinatorEntity[PipewireRouterCoordinator], MediaPlayerEntity):
    """A single PipeWire output (RAOP or sendspin) exposed as a media_player."""

    _attr_supported_features = (
        MediaPlayerEntityFeature.VOLUME_SET
        | MediaPlayerEntityFeature.PLAY_MEDIA
        | MediaPlayerEntityFeature.MEDIA_ANNOUNCE
    )
    _attr_has_entity_name = True

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry, node_id: int, node_name: str) -> None:
        super().__init__(coordinator)
        self.node_id = node_id
        self._node_name = node_name
        self._attr_unique_id = f"{entry.entry_id}_{node_name}"
        self._attr_name = _display_name(node_name)

    def _current(self) -> ApiMediaPlayerState | None:
        return next((p for p in self.coordinator.data if p.node_id == self.node_id), None)

    @property
    def available(self) -> bool:
        # The node can disappear from the registry entirely (e.g. a RAOP
        # device whose module failed to load) — treat that as unavailable
        # rather than crashing on a missing entry.
        return self._current() is not None

    @property
    def state(self) -> MediaPlayerState | None:
        current = self._current()
        if current is None:
            return None
        return MediaPlayerState.PLAYING if current.state == "playing" else MediaPlayerState.IDLE

    @property
    def volume_level(self) -> float | None:
        current = self._current()
        return current.volume if current else None

    async def async_set_volume_level(self, volume: float) -> None:
        await self.coordinator.client.async_set_volume(self.node_id, volume)
        await self.coordinator.async_request_refresh()

    async def async_play_media(self, media_type: MediaType | str, media_id: str, **kwargs) -> None:
        # This entity has no "primary playback" contract of its own (no
        # queue, no PLAY/PAUSE) — the only thing `play_media` can mean for
        # a passive routing sink is the ducked announce-stream mix from
        # Section 5.6, regardless of whether the caller set `announce`
        # (tts.speak already does; a doorbell automation calling
        # play_media directly might not, per PLAN.md Section 6's
        # reasoning for declaring MEDIA_ANNOUNCE explicitly).
        #
        # Section 5.6 v2 (Phase 3.5), additive: a caller opts into the
        # Wyoming path per call via the core `play_media` service's
        # `extra` dict (the same HA-standard mechanism other integrations
        # use for implementation-specific options, e.g. Sonos/cast) —
        # `media_id` is ignored in that case since the text to synthesize
        # comes from `extra.wyoming.text` instead. Everyone else
        # (`tts.speak`, existing automations) keeps calling this with a
        # plain rendered-clip URL exactly as before.
        wyoming = (kwargs.get("extra") or {}).get("wyoming")
        if wyoming:
            await self.coordinator.client.async_announce_wyoming(
                self.node_id,
                host=wyoming["host"],
                text=wyoming["text"],
                port=wyoming.get("port", 10200),
                voice=wyoming.get("voice"),
            )
        else:
            await self.coordinator.client.async_announce(self.node_id, media_id)
