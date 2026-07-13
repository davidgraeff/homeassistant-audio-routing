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

import voluptuous as vol
from homeassistant.components.media_player import (
    MediaPlayerEntity,
    MediaPlayerEntityFeature,
    MediaPlayerState,
    MediaType,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import config_validation as cv, entity_platform
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import PipewireRouterCoordinator
from .api import MediaPlayerState as ApiMediaPlayerState
from .api import RoutingMatrix
from .const import ATTR_SOURCE, DOMAIN, SERVICE_LINK, SERVICE_UNLINK, SOURCE_NONE

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

    # Dedupe by the stable node *name*, not node_id: a RAOP/sendspin module
    # that reloads comes back with a fresh node_id but the same name, and
    # keying on the id would try to add a second entity sharing the same
    # unique_id (which is name-based below) — a hard error in HA.
    known_node_names: set[str] = set()

    def _add_new_entities() -> None:
        new_entities = [
            PipewireRouterMediaPlayer(coordinator, entry, player.node_name)
            for player in coordinator.data
            if player.node_name not in known_node_names
        ]
        if new_entities:
            known_node_names.update(e.node_name for e in new_entities)
            async_add_entities(new_entities)

    _add_new_entities()
    entry.async_on_unload(coordinator.async_add_listener(_add_new_entities))

    # Low-level routing actions for automations, targeted at an output
    # entity (area/device/entity targeting comes for free). These are the
    # additive escape hatch alongside the exclusive `select_source`.
    platform = entity_platform.async_get_current_platform()
    platform.async_register_entity_service(
        SERVICE_LINK,
        {vol.Required(ATTR_SOURCE): cv.string},
        "async_service_link",
    )
    platform.async_register_entity_service(
        SERVICE_UNLINK,
        {vol.Optional(ATTR_SOURCE): cv.string},
        "async_service_unlink",
    )


class PipewireRouterMediaPlayer(CoordinatorEntity[PipewireRouterCoordinator], MediaPlayerEntity):
    """A single PipeWire output (RAOP or sendspin) exposed as a media_player.

    The output's *input wiring* is modelled as the media_player "source":
    `source_list` is the routable sources the daemon reports, `source` is
    whatever is currently linked in, and `select_source` swaps it (exclusive
    — the previously linked source is unlinked first). This is one source
    per output by design; the `link`/`unlink` services are the additive
    escape hatch for anything more.
    """

    _attr_supported_features = (
        MediaPlayerEntityFeature.VOLUME_SET
        | MediaPlayerEntityFeature.PLAY_MEDIA
        | MediaPlayerEntityFeature.MEDIA_ANNOUNCE
        | MediaPlayerEntityFeature.SELECT_SOURCE
    )
    _attr_has_entity_name = True

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry, node_name: str) -> None:
        super().__init__(coordinator)
        # Identity is the stable node *name*; the ephemeral node_id is
        # re-resolved from each snapshot (see `_current`) so this entity
        # keeps working across a module reload that changes the id.
        self.node_name = node_name
        self._attr_unique_id = f"{entry.entry_id}_{node_name}"
        self._attr_name = _display_name(node_name)

    def _current(self) -> ApiMediaPlayerState | None:
        return next((p for p in self.coordinator.data if p.node_name == self.node_name), None)

    def _matrix(self) -> RoutingMatrix:
        return self.coordinator.routing

    @property
    def _live_node_id(self) -> int | None:
        """This output's node_id in the latest snapshot, or None if it's
        gone. All daemon calls go through this rather than a stored id."""
        current = self._current()
        return current.node_id if current else None

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

    @property
    def source_list(self) -> list[str]:
        return [SOURCE_NONE] + [s.display_name for s in self._matrix().sources]

    @property
    def source(self) -> str | None:
        """The source currently linked into this output, or SOURCE_NONE if
        nothing feeds it. In the exclusive model there's at most one; if the
        graph somehow has several (e.g. wired via the additive `link`
        service), report the first by name so the attribute stays defined."""
        node_id = self._live_node_id
        if node_id is None:
            return None
        matrix = self._matrix()
        linked_source_ids = {src for src, out in matrix.links if out == node_id}
        names = [s.display_name for s in matrix.sources if s.node_id in linked_source_ids]
        return names[0] if names else SOURCE_NONE

    def _resolve_source_id(self, source: str) -> int:
        target = next((s for s in self._matrix().sources if s.display_name == source), None)
        if target is None:
            raise HomeAssistantError(f"unknown source '{source}' for {self.entity_id}")
        return target.node_id

    def _require_node_id(self) -> int:
        node_id = self._live_node_id
        if node_id is None:
            raise HomeAssistantError(f"output '{self.node_name}' is not currently available")
        return node_id

    async def async_set_volume_level(self, volume: float) -> None:
        await self.coordinator.client.async_set_volume(self._require_node_id(), volume)
        await self.coordinator.async_request_refresh()

    async def async_select_source(self, source: str) -> None:
        """Exclusive swap: unlink every source that isn't the requested one,
        then link the requested one (SOURCE_NONE just disconnects)."""
        node_id = self._require_node_id()
        matrix = self._matrix()
        current_source_ids = {src for src, out in matrix.links if out == node_id}

        target_id = None if source == SOURCE_NONE else self._resolve_source_id(source)

        for src_id in current_source_ids:
            if src_id != target_id:
                await self.coordinator.client.async_unlink(src_id, node_id)
        if target_id is not None and target_id not in current_source_ids:
            await self.coordinator.client.async_link(target_id, node_id)

        await self.coordinator.async_request_refresh()

    async def async_service_link(self, source: str) -> None:
        """`pipewire_audio_router.link` — additively connect `source` to
        this output without disturbing any source already linked."""
        await self.coordinator.client.async_link(self._resolve_source_id(source), self._require_node_id())
        await self.coordinator.async_request_refresh()

    async def async_service_unlink(self, source: str | None = None) -> None:
        """`pipewire_audio_router.unlink` — disconnect `source` from this
        output, or every source currently feeding it when `source` is
        omitted."""
        node_id = self._require_node_id()
        if source is not None:
            await self.coordinator.client.async_unlink(self._resolve_source_id(source), node_id)
        else:
            for src_id in {src for src, out in self._matrix().links if out == node_id}:
                await self.coordinator.client.async_unlink(src_id, node_id)
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
        node_id = self._require_node_id()
        wyoming = (kwargs.get("extra") or {}).get("wyoming")
        if wyoming:
            await self.coordinator.client.async_announce_wyoming(
                node_id,
                host=wyoming["host"],
                text=wyoming["text"],
                port=wyoming.get("port", 10200),
                voice=wyoming.get("voice"),
            )
        else:
            await self.coordinator.client.async_announce(node_id, media_id)
