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

import logging

import voluptuous as vol
from homeassistant.components import media_source
from homeassistant.components.media_player import (
    MediaPlayerEntity,
    MediaPlayerEntityFeature,
    MediaPlayerState,
    MediaType,
    async_process_play_media_url,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import (
    config_validation as cv,
    device_registry as dr,
    entity_platform,
    entity_registry as er,
)
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import PipewireRouterCoordinator
from .api import AnnouncementGroup, MusicGroup
from .api import MediaPlayerState as ApiMediaPlayerState
from .api import RoutingMatrix, RoutingNode
from .const import ATTR_SOURCE, DOMAIN, SERVICE_LINK, SERVICE_UNLINK, SOURCE_NONE

# Must match the bridge-daemon's node-name prefixes exactly — no shared source
# of truth across the Rust/Python boundary, just this comment. RAOP outputs and
# the (auto-discovered) sendspin *devices* both surface as routing-matrix
# outputs, and each becomes one media_player.
RAOP_NODE_PREFIX = "raop-out-"
SENDSPIN_DEV_PREFIX = "sendspin-dev-"

_LOGGER = logging.getLogger(__name__)


def _display_name(node_name: str) -> str:
    """Strips the bridge daemon's node-name prefix and turns
    e.g. "sendspin-dev-voice_pe_kitchen" into "Voice Pe Kitchen"."""
    for prefix in (RAOP_NODE_PREFIX, SENDSPIN_DEV_PREFIX):
        if node_name.startswith(prefix):
            return node_name[len(prefix) :].replace("_", " ").title()
    return node_name


def _esphome_hostname(node_name: str) -> str | None:
    """The ESPHome node name (== the device's mDNS hostname with `-`→`_`) that a
    sendspin output's node name is built from: `sendspin-dev-<hostname>` gives
    `<hostname>`, e.g. `home_assistant_voice_093ca8`. `None` for non-sendspin
    outputs (RAOP), which carry no such identity."""
    if node_name.startswith(SENDSPIN_DEV_PREFIX):
        return node_name[len(SENDSPIN_DEV_PREFIX) :]
    return None


def _find_ha_device(hass: HomeAssistant, node_name: str) -> dr.DeviceEntry | None:
    """Correlate a sendspin output to the Home Assistant device that represents
    the same physical speaker, so the media_player can adopt HA's name and area
    instead of the cryptic daemon-side hostname.

    The only identity the daemon transmits for a sendspin device is its mDNS
    hostname (e.g. `home-assistant-voice-093ca8`) — the full MAC is not
    advertised. But that *full hostname* string appears verbatim in the ESPHome
    integration's entity ids on the real device (e.g. the firmware `update`
    entity's unique_id `<mac>-update-home_assistant_voice_093ca8`), and that
    device carries the speaker's genuine full-MAC connection. So we match on the
    whole hostname (not a truncated MAC), resolve it to that device, and later
    link via the device's real connections.

    Returns the matched device, or `None` if there's no match — or, defensively,
    if more than one distinct device matches (ambiguous → don't guess)."""
    hostname = _esphome_hostname(node_name)
    if not hostname:
        return None

    ent_reg = er.async_get(hass)
    device_ids = {
        entity.device_id
        for entity in ent_reg.entities.values()
        # Skip our own entities (ours embeds the hostname too) and anything not
        # tied to a device; then require the full hostname as a substring of the
        # ESPHome-assigned unique_id / entity_id.
        if entity.platform != DOMAIN
        and entity.device_id is not None
        and (hostname in (entity.unique_id or "") or hostname in entity.entity_id)
    }

    if len(device_ids) != 1:
        if len(device_ids) > 1:
            _LOGGER.warning(
                "sendspin output %s matched %d Home Assistant devices by hostname %r; "
                "not linking (ambiguous)",
                node_name,
                len(device_ids),
                hostname,
            )
        return None
    return dr.async_get(hass).async_get(next(iter(device_ids)))


def _output_node_names(coordinator: PipewireRouterCoordinator) -> list[str]:
    """The node names of every routing-matrix output — the set that should
    have a media_player. Drives both creation and removal, so an output that
    leaves the matrix (a discovered device that's gone) loses its entity, while
    a configured-but-offline one stays (present=False → unavailable)."""
    return [o.node_name for o in coordinator.routing.outputs]


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    coordinator: PipewireRouterCoordinator = hass.data[DOMAIN][entry.entry_id]

    # Entities are created one per music group + one per announcement group, and
    # (only when the daemon's `expose_outputs_as_media_players` toggle is on) one
    # per output. Keyed by a namespaced string ("mg:<id>", "ag:<id>",
    # "out:<node>") so the three kinds never collide, and each is added/removed as
    # its group/output/toggle appears or leaves.
    entities: dict[str, CoordinatorEntity] = {}

    @callback
    def _reconcile_entities() -> None:
        desired: dict[str, CoordinatorEntity] = {}
        for g in coordinator.music_groups:
            key = f"mg:{g.id}"
            if key not in entities:
                desired[key] = MusicGroupMediaPlayer(coordinator, entry, g.id)
        for g in coordinator.announcement_groups:
            key = f"ag:{g.id}"
            if key not in entities:
                desired[key] = AnnouncementGroupMediaPlayer(coordinator, entry, g.id)
        if coordinator.expose_outputs:
            for name in _output_node_names(coordinator):
                key = f"out:{name}"
                if key not in entities:
                    desired[key] = PipewireRouterMediaPlayer(hass, coordinator, entry, name)

        new = list(desired.values())
        for key, ent in desired.items():
            entities[key] = ent
        if new:
            async_add_entities(new)

        # Anything no longer backed by a group/output (or per-output turned off).
        live_keys = (
            {f"mg:{g.id}" for g in coordinator.music_groups}
            | {f"ag:{g.id}" for g in coordinator.announcement_groups}
            | ({f"out:{n}" for n in _output_node_names(coordinator)} if coordinator.expose_outputs else set())
        )
        gone = [key for key in entities if key not in live_keys]
        if gone:
            registry = er.async_get(hass)
            removed = [entities.pop(key) for key in gone]

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

    def __init__(
        self, hass: HomeAssistant, coordinator: PipewireRouterCoordinator, entry: ConfigEntry, node_name: str
    ) -> None:
        super().__init__(coordinator)
        # Identity is the stable node *name*; the ephemeral node_id is
        # re-resolved from each snapshot (see `_current`) so this entity
        # keeps working across a module reload that changes the id.
        self.node_name = node_name
        self._attr_unique_id = f"{entry.entry_id}_out_{node_name}"

        # Correlate a sendspin output to its Home Assistant device (by mDNS
        # hostname → the ESPHome device) once, at creation. When matched, the
        # entity links to that device and takes HA's name + area; unmatched
        # (RAOP, or an unknown device), it falls back to the daemon's derived
        # display name with no device. Resolved here — the ESPHome device is
        # registered at HA start, before this integration sets up — so a device
        # that only appears later is picked up on the next reload.
        self._ha_device = _find_ha_device(hass, node_name)
        if self._ha_device is None:
            self._attr_name = _display_name(node_name)
        else:
            # has_entity_name + a name → "<device name> <name>", e.g.
            # "Home Assistant Voice Badezimmer Audio Routing". The suffix keeps
            # this routing output distinct from the speaker's own built-in
            # assist media_player ("<device name> Media Player") on the same
            # device, while still leading with the HA device name.
            self._attr_name = "Audio Routing"

    @property
    def _is_sendspin(self) -> bool:
        return self.node_name.startswith(SENDSPIN_DEV_PREFIX)

    @property
    def device_info(self) -> DeviceInfo | None:
        """Link to the matched Home Assistant device by its genuine full-MAC
        connection(s), so this media_player is grouped under the real speaker
        and inherits its name + area. `None` when unmatched (kept standalone).
        We reuse the device's existing connections (never reconstruct a MAC),
        which is what merges this entity into that exact device."""
        if self._ha_device is None:
            return None
        mac_connections = {
            (conn_type, conn_value)
            for conn_type, conn_value in self._ha_device.connections
            if conn_type == dr.CONNECTION_NETWORK_MAC
        }
        if not mac_connections:
            return None
        return DeviceInfo(connections=mac_connections)

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
        extra = kwargs.get("extra") or {}
        # Deliberately at INFO so the "did HA ever route audio to us?" question
        # is answerable from the normal log without turning on debug — this is
        # the first thing to check when TTS/announce "does nothing".
        _LOGGER.info(
            "play_media on %s (node=%s): media_type=%s announce=%s media_id=%r extra=%r",
            self.entity_id,
            self.node_name,
            media_type,
            kwargs.get("announce"),
            media_id,
            extra,
        )
        node_id = self._require_node_id()
        wyoming = extra.get("wyoming")
        try:
            if wyoming:
                await self.coordinator.client.async_announce_wyoming(
                    node_id,
                    host=wyoming["host"],
                    text=wyoming["text"],
                    port=wyoming.get("port", 10200),
                    voice=wyoming.get("voice"),
                )
            else:
                # tts.speak / the media browser hand us a `media-source://` URI,
                # not a URL — resolve it to a concrete one, then make it
                # absolute (async_process_play_media_url) so the bridge daemon,
                # which runs on a different host, can actually fetch it. A plain
                # URL passes through both steps unchanged.
                if media_source.is_media_source_id(media_id):
                    resolved = await media_source.async_resolve_media(
                        self.hass, media_id, self.entity_id
                    )
                    _LOGGER.debug("resolved media-source %r -> %r", media_id, resolved.url)
                    media_id = resolved.url
                media_id = async_process_play_media_url(self.hass, media_id)
                _LOGGER.info("announcing %r into %s (node=%s)", media_id, self.entity_id, node_id)
                await self.coordinator.client.async_announce(node_id, media_id)
        except Exception as err:
            # Surface the failure both to the caller (service error) and the log,
            # so a broken announce is never silent.
            _LOGGER.error("play_media on %s failed: %s", self.entity_id, err)
            raise


class MusicGroupMediaPlayer(CoordinatorEntity[PipewireRouterCoordinator], MediaPlayerEntity):
    """A named music group as a media_player: pick the source the whole group
    plays (`select_source`, routing all members together) and set a group master
    volume (applied to every member). One entity per music group."""

    _attr_supported_features = MediaPlayerEntityFeature.VOLUME_SET | MediaPlayerEntityFeature.SELECT_SOURCE
    _attr_has_entity_name = False

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry, group_id: str) -> None:
        super().__init__(coordinator)
        self._group_id = group_id
        self._attr_unique_id = f"{entry.entry_id}_mg_{group_id}"

    def _group(self) -> MusicGroup | None:
        return next((g for g in self.coordinator.music_groups if g.id == self._group_id), None)

    def _matrix(self) -> RoutingMatrix:
        return self.coordinator.routing

    @property
    def available(self) -> bool:
        return self._group() is not None

    @property
    def name(self) -> str | None:
        g = self._group()
        return g.name if g else None

    @property
    def source_list(self) -> list[str]:
        return [SOURCE_NONE] + [s.display_name for s in self._matrix().sources]

    def _member_sources(self) -> set[str]:
        """Source node names currently linked to any member of this group."""
        g = self._group()
        if not g:
            return set()
        members = set(g.members)
        return {src for src, out in self._matrix().links if out in members}

    @property
    def source(self) -> str | None:
        srcs = self._member_sources()
        names = [s.display_name for s in self._matrix().sources if s.node_name in srcs]
        return names[0] if names else SOURCE_NONE

    @property
    def state(self) -> MediaPlayerState | None:
        present_sources = {s.node_name for s in self._matrix().sources if s.present}
        return MediaPlayerState.PLAYING if self._member_sources() & present_sources else MediaPlayerState.IDLE

    @property
    def volume_level(self) -> float | None:
        """Group master = the mean of the members' individual volumes."""
        g = self._group()
        if not g or not g.members:
            return None
        levels: list[float] = []
        for name in g.members:
            if name.startswith(SENDSPIN_DEV_PREFIX):
                levels.append(self.coordinator.sendspin_volumes.get(name, 100) / 100)
            else:
                current = next((p for p in self.coordinator.data if p.node_name == name), None)
                if current is not None and current.volume is not None:
                    levels.append(current.volume)
        return sum(levels) / len(levels) if levels else None

    def _resolve_source_name(self, source: str) -> str:
        target = next((s for s in self._matrix().sources if s.display_name == source), None)
        if target is None:
            raise HomeAssistantError(f"unknown source '{source}' for {self.entity_id}")
        return target.node_name

    async def async_select_source(self, source: str) -> None:
        if source == SOURCE_NONE:
            await self.coordinator.client.async_unroute_music_group(self._group_id)
        else:
            await self.coordinator.client.async_route_music_group(self._group_id, self._resolve_source_name(source))
        await self.coordinator.async_request_refresh()

    async def async_set_volume_level(self, volume: float) -> None:
        """Apply the master volume to every member (in-band for sendspin, node
        volume for RAOP)."""
        g = self._group()
        if not g:
            return
        for name in g.members:
            if name.startswith(SENDSPIN_DEV_PREFIX):
                await self.coordinator.client.async_set_sendspin_volume(name, round(volume * 100))
            else:
                current = next((p for p in self.coordinator.data if p.node_name == name), None)
                if current is not None:
                    await self.coordinator.client.async_set_volume(current.node_id, volume)
        await self.coordinator.async_request_refresh()


class AnnouncementGroupMediaPlayer(CoordinatorEntity[PipewireRouterCoordinator], MediaPlayerEntity):
    """A named announcement group as a media_player: `play_media` / TTS fans a
    ducked announcement to the group's targets (priority + duck come from the
    group). One entity per announcement group. Not a music source."""

    _attr_supported_features = MediaPlayerEntityFeature.PLAY_MEDIA | MediaPlayerEntityFeature.MEDIA_ANNOUNCE
    _attr_has_entity_name = False

    def __init__(self, coordinator: PipewireRouterCoordinator, entry: ConfigEntry, group_id: str) -> None:
        super().__init__(coordinator)
        self._group_id = group_id
        self._attr_unique_id = f"{entry.entry_id}_ag_{group_id}"

    def _group(self) -> AnnouncementGroup | None:
        return next((g for g in self.coordinator.announcement_groups if g.id == self._group_id), None)

    @property
    def available(self) -> bool:
        return self._group() is not None

    @property
    def name(self) -> str | None:
        g = self._group()
        return g.name if g else None

    @property
    def state(self) -> MediaPlayerState | None:
        # Announcements are transient overlays; there's no persistent play state.
        return MediaPlayerState.IDLE

    async def async_play_media(self, media_type: MediaType | str, media_id: str, **kwargs) -> None:
        extra = kwargs.get("extra") or {}
        _LOGGER.info(
            "play_media on announcement group %s: media_type=%s media_id=%r extra=%r",
            self.entity_id,
            media_type,
            media_id,
            extra,
        )
        wyoming = extra.get("wyoming")
        try:
            if wyoming:
                await self.coordinator.client.async_announce_group(self._group_id, wyoming=wyoming)
            else:
                if media_source.is_media_source_id(media_id):
                    resolved = await media_source.async_resolve_media(self.hass, media_id, self.entity_id)
                    media_id = resolved.url
                media_id = async_process_play_media_url(self.hass, media_id)
                await self.coordinator.client.async_announce_group(self._group_id, url=media_id)
        except Exception as err:
            _LOGGER.error("announce to group %s failed: %s", self.entity_id, err)
            raise
