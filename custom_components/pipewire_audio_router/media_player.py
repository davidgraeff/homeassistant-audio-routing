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
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import config_validation as cv, entity_platform, entity_registry as er
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import PipewireRouterCoordinator
from .api import MediaPlayerState as ApiMediaPlayerState
from .api import RoutingMatrix, RoutingNode
from .const import ATTR_SOURCE, DOMAIN, SERVICE_LINK, SERVICE_UNLINK, SOURCE_NONE

# Must match the bridge-daemon's node-name prefixes exactly — no shared source
# of truth across the Rust/Python boundary, just this comment. RAOP outputs and
# the (auto-discovered) sendspin *devices* both surface as routing-matrix
# outputs, and each becomes one media_player.
RAOP_NODE_PREFIX = "raop-out-"
SENDSPIN_DEV_PREFIX = "sendspin-dev-"


def _display_name(node_name: str) -> str:
    """Strips the bridge daemon's node-name prefix and turns
    e.g. "sendspin-dev-voice_pe_kitchen" into "Voice Pe Kitchen"."""
    for prefix in (RAOP_NODE_PREFIX, SENDSPIN_DEV_PREFIX):
        if node_name.startswith(prefix):
            return node_name[len(prefix) :].replace("_", " ").title()
    return node_name


def _output_node_names(coordinator: PipewireRouterCoordinator) -> list[str]:
    """The node names of every routing-matrix output — the set that should
    have a media_player. Drives both creation and removal, so an output that
    leaves the matrix (a discovered device that's gone) loses its entity, while
    a configured-but-offline one stays (present=False → unavailable)."""
    return [o.node_name for o in coordinator.routing.outputs]


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    coordinator: PipewireRouterCoordinator = hass.data[DOMAIN][entry.entry_id]

    # One entity per routing-matrix output, keyed by the stable node *name*
    # (a reloaded module returns a fresh node_id but the same name). Entities
    # are added when an output appears and removed when it leaves the matrix.
    entities: dict[str, PipewireRouterMediaPlayer] = {}

    @callback
    def _reconcile_entities() -> None:
        desired = set(_output_node_names(coordinator))

        new = [
            PipewireRouterMediaPlayer(coordinator, entry, name)
            for name in desired
            if name not in entities
        ]
        for ent in new:
            entities[ent.node_name] = ent
        if new:
            async_add_entities(new)

        gone = [name for name in entities if name not in desired]
        if gone:
            registry = er.async_get(hass)
            removed = [entities.pop(name) for name in gone]

            async def _remove() -> None:
                for ent in removed:
                    if ent.entity_id and registry.async_get(ent.entity_id):
                        registry.async_remove(ent.entity_id)
                    await ent.async_remove(force_remove=True)

            hass.async_create_task(_remove())

    _reconcile_entities()
    entry.async_on_unload(coordinator.async_add_listener(_reconcile_entities))

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

    @property
    def _is_sendspin(self) -> bool:
        return self.node_name.startswith(SENDSPIN_DEV_PREFIX)

    def _current(self) -> ApiMediaPlayerState | None:
        """This output's entry in the polled /api/media_players feed (RAOP
        only — sendspin devices are virtual and never appear there)."""
        return next((p for p in self.coordinator.data if p.node_name == self.node_name), None)

    def _output(self) -> RoutingNode | None:
        """This output's routing-matrix entry (the authoritative existence +
        present/offline signal for both RAOP and sendspin)."""
        return next((o for o in self._matrix().outputs if o.node_name == self.node_name), None)

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
        # Present in the live graph right now. A configured-but-offline output
        # stays in the matrix with present=False (shown unavailable); one that
        # leaves the matrix entirely loses its entity (see _reconcile_entities).
        output = self._output()
        return output is not None and output.present

    @property
    def state(self) -> MediaPlayerState | None:
        # RAOP: authoritative playing/idle from the media_players feed.
        current = self._current()
        if current is not None:
            return MediaPlayerState.PLAYING if current.state == "playing" else MediaPlayerState.IDLE
        # Sendspin (virtual, not in the feed): derive from routing — playing if
        # a present source is linked into it, else idle.
        if self._is_sendspin and self.available:
            present_sources = {s.node_name for s in self._matrix().sources if s.present}
            linked = any(out == self.node_name and src in present_sources for src, out in self._matrix().links)
            return MediaPlayerState.PLAYING if linked else MediaPlayerState.IDLE
        return None

    @property
    def volume_level(self) -> float | None:
        # Sendspin volume is carried in-band (no PipeWire node volume); it comes
        # from the daemon's per-device store (0-100), default full scale.
        if self._is_sendspin:
            return self.coordinator.sendspin_volumes.get(self.node_name, 100) / 100
        current = self._current()
        return current.volume if current else None

    @property
    def extra_state_attributes(self) -> dict[str, object] | None:
        """For a sendspin device that shares its exact source-set with other
        sendspin devices, expose the synchronized group it's part of (the
        daemon forms these automatically — sendspin_group.rs). Absent for a
        lone/ungrouped device and for RAOP outputs."""
        if not self._is_sendspin:
            return None
        members = self._group_members()
        if len(members) < 2:
            return None
        return {"sendspin_group_members": [_display_name(n) for n in members]}

    def _group_members(self) -> list[str]:
        """Node names of the sendspin devices sharing this device's exact
        (non-empty) set of routed sources — i.e. its synchronized group."""
        matrix = self._matrix()

        def source_key(output_name: str) -> tuple[str, ...]:
            return tuple(sorted(src for src, out in matrix.links if out == output_name))

        mine = source_key(self.node_name)
        if not mine:
            return []
        return sorted(
            o.node_name
            for o in matrix.outputs
            if o.node_name.startswith(SENDSPIN_DEV_PREFIX) and source_key(o.node_name) == mine
        )

    @property
    def source_list(self) -> list[str]:
        return [SOURCE_NONE] + [s.display_name for s in self._matrix().sources]

    @property
    def source(self) -> str | None:
        """The source routed into this output, or SOURCE_NONE if none. Read
        from the persisted intent (by stable name), so it stays correct even
        when this output is momentarily offline. Exclusive model → at most one;
        report the first by name if several were wired additively."""
        matrix = self._matrix()
        linked_sources = {src for src, out in matrix.links if out == self.node_name}
        names = [s.display_name for s in matrix.sources if s.node_name in linked_sources]
        return names[0] if names else SOURCE_NONE

    def _resolve_source_name(self, source: str) -> str:
        target = next((s for s in self._matrix().sources if s.display_name == source), None)
        if target is None:
            raise HomeAssistantError(f"unknown source '{source}' for {self.entity_id}")
        return target.node_name

    def _require_node_id(self) -> int:
        node_id = self._live_node_id
        if node_id is None:
            raise HomeAssistantError(f"output '{self.node_name}' is not currently available")
        return node_id

    async def async_set_volume_level(self, volume: float) -> None:
        if self._is_sendspin:
            # 0.0–1.0 → 0–100, sent in-band to the device (no PipeWire node vol).
            await self.coordinator.client.async_set_sendspin_volume(self.node_name, round(volume * 100))
        else:
            await self.coordinator.client.async_set_volume(self._require_node_id(), volume)
        await self.coordinator.async_request_refresh()

    async def async_select_source(self, source: str) -> None:
        """Exclusive swap: unlink every source that isn't the requested one,
        then link the requested one (SOURCE_NONE just disconnects). Routes by
        stable name, so it works — and persists — even if the output is offline
        (it's applied when the device returns)."""
        matrix = self._matrix()
        current_sources = {src for src, out in matrix.links if out == self.node_name}

        target = None if source == SOURCE_NONE else self._resolve_source_name(source)

        for src in current_sources:
            if src != target:
                await self.coordinator.client.async_unlink(src, self.node_name)
        if target is not None and target not in current_sources:
            await self.coordinator.client.async_link(target, self.node_name)

        await self.coordinator.async_request_refresh()

    async def async_service_link(self, source: str) -> None:
        """`pipewire_audio_router.link` — additively connect `source` to
        this output without disturbing any source already linked."""
        await self.coordinator.client.async_link(self._resolve_source_name(source), self.node_name)
        await self.coordinator.async_request_refresh()

    async def async_service_unlink(self, source: str | None = None) -> None:
        """`pipewire_audio_router.unlink` — disconnect `source` from this
        output, or every source currently routed to it when `source` is
        omitted."""
        if source is not None:
            await self.coordinator.client.async_unlink(self._resolve_source_name(source), self.node_name)
        else:
            for src in {src for src, out in self._matrix().links if out == self.node_name}:
                await self.coordinator.client.async_unlink(src, self.node_name)
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
