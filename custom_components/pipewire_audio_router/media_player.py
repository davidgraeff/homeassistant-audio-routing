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
from datetime import datetime

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
from homeassistant.util import dt as dt_util

from . import PipewireRouterCoordinator
from .api import AnnouncementGroup, MusicGroup, NowPlaying, RoutingMatrix, RoutingNode
from .const import ATTR_SOURCE, DOMAIN, SERVICE_LINK, SERVICE_UNLINK, SOURCE_NONE

# Must match the bridge-daemon's node-name prefixes exactly — no shared source
# of truth across the Rust/Python boundary, just this comment. The
# (auto-discovered) sendspin *devices* and AirPlay-2 receivers all surface as
# routing-matrix outputs, and each becomes one media_player.
SENDSPIN_DEV_PREFIX = "sendspin-dev-"
AP2_DEV_PREFIX = "ap2-dev-"
# A pw-sink output is a remote PipeWire *host* running the pwrouter-agent, which
# reports and applies that host's master volume/mute.
PWSINK_DEV_PREFIX = "pwsink-dev-"

# All output kinds' prefixes. Used to derive a fallback display name for any
# output regardless of kind.
_OUTPUT_PREFIXES = (SENDSPIN_DEV_PREFIX, AP2_DEV_PREFIX, PWSINK_DEV_PREFIX)

# Host/IP-ish keys that AV-receiver integrations commonly store the receiver
# address under in their config entry (MusicCast, Onkyo/Pioneer, HEOS, …). Used
# to correlate an AirPlay-2 output to an existing HA device by IP.
_HOST_CONF_KEYS = ("host", "ip_address", "address", "ip")

_LOGGER = logging.getLogger(__name__)


def _display_name(node_name: str) -> str:
    """Strips the bridge daemon's node-name prefix and turns
    e.g. "sendspin-dev-voice_pe_kitchen" into "Voice Pe Kitchen"."""
    for prefix in _OUTPUT_PREFIXES:
        if node_name.startswith(prefix):
            return node_name[len(prefix) :].replace("_", " ").title()
    return node_name


def _esphome_hostname(node_name: str) -> str | None:
    """The ESPHome node name (== the device's mDNS hostname with `-`→`_`) that a
    sendspin output's node name is built from: `sendspin-dev-<hostname>` gives
    `<hostname>`, e.g. `home_assistant_voice_093ca8`. `None` for non-sendspin
    outputs (AirPlay-2), which carry no such identity."""
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


def _entry_targets_ip(entry: ConfigEntry, ip: str) -> bool:
    """True if a config entry connects to `ip`, matched against the common
    host/IP keys in its data + options (`_HOST_CONF_KEYS`)."""
    for mapping in (entry.data, entry.options):
        for key in _HOST_CONF_KEYS:
            if str(mapping.get(key) or "") == ip:
                return True
    return False


def _find_ap2_ha_device(hass: HomeAssistant, ip: str | None) -> dr.DeviceEntry | None:
    """Correlate an AirPlay-2 receiver to the Home Assistant device that already
    represents the same physical box, so the `ap2-dev` media_player adopts HA's
    name + area instead of the daemon's slug.

    The adoption rule (kept deliberately small + testable):

    Unlike sendspin — ESPHome speakers matched by the mDNS hostname baked into
    their entity unique_ids — AP2 receivers are third-party AV gear (Yamaha
    MusicCast, Onkyo/Pioneer, a HomeKit/AirPlay device, …), so that trick
    doesn't apply. The one stable identity the daemon exposes for them *without
    a daemon-side change* is the resolved **IP address** (`/api/outputs`→`ip`).
    So: find the HA device whose own integration talks to that same IP —
    either a config entry that stores the IP as its host (`_HOST_CONF_KEYS`), or
    any device whose `configuration_url` points at it. Exactly one match →
    adopt it (the entity links to that device, inheriting its area). Zero, or
    several (ambiguous), → no adoption: the entity stays standalone and the user
    can assign its area in the UI.

    Resolved once at entity creation (like the sendspin path); a receiver whose
    matching HA device only appears later is picked up on the next reload."""
    if not ip:
        return None
    dev_reg = dr.async_get(hass)
    matched: set[str] = set()
    # A config entry (e.g. MusicCast/Onkyo) pointed at this IP → its device(s).
    for entry in hass.config_entries.async_entries():
        if entry.domain == DOMAIN:
            continue
        if _entry_targets_ip(entry, ip):
            for device in dr.async_entries_for_config_entry(dev_reg, entry.entry_id):
                matched.add(device.id)
    # A device that advertises the IP in its configuration URL (some HomeKit /
    # AirPlay devices do), regardless of how its config entry stores the host.
    for device in dev_reg.devices.values():
        if device.configuration_url and ip in str(device.configuration_url):
            matched.add(device.id)

    if len(matched) != 1:
        if len(matched) > 1:
            _LOGGER.warning(
                "AirPlay-2 receiver at %s matched %d Home Assistant devices; not linking (ambiguous)",
                ip,
                len(matched),
            )
        return None
    return dev_reg.async_get(next(iter(matched)))


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


class _SourceMetadataMixin:
    """Now-playing display for an entity whose audio comes from one router source.

    Both the per-output and the music-group `media_player` show what their
    *currently routed source* is playing — title, artist, album, cover art,
    position — from the daemon's per-source metadata (its now_playing.rs, pushed
    on the routing socket). Neither entity *controls* playback: the phone (or
    whatever is feeding the source) does that, so no new feature flags are
    declared. This is display only.

    Subclasses supply `_metadata_source()`; everything below follows from it, so
    an output and a group looking at the same source can never disagree.

    Position is reported with `media_position_updated_at` and Home Assistant
    extrapolates between the daemon's sparse updates. It leads the sound by the
    ingest jitter buffer plus the output's playout latency (order 200–400 ms);
    that drift is accepted rather than corrected.
    """

    coordinator: PipewireRouterCoordinator

    def _metadata_source(self) -> str | None:
        """The source *node name* whose metadata this entity shows."""
        raise NotImplementedError

    def _now_playing(self) -> NowPlaying | None:
        return self.coordinator.now_playing_for(self._metadata_source())

    @property
    def media_content_type(self) -> MediaType | str | None:
        return MediaType.MUSIC if self._now_playing() else None

    @property
    def media_title(self) -> str | None:
        np = self._now_playing()
        return np.title if np else None

    @property
    def media_artist(self) -> str | None:
        np = self._now_playing()
        return np.artist if np else None

    @property
    def media_album_name(self) -> str | None:
        np = self._now_playing()
        return np.album if np else None

    @property
    def media_duration(self) -> int | None:
        np = self._now_playing()
        if not np or np.duration_ms is None:
            return None
        return round(np.duration_ms / 1000)

    @property
    def media_position(self) -> int | None:
        np = self._now_playing()
        if not np or np.position_ms is None:
            return None
        return round(np.position_ms / 1000)

    @property
    def media_position_updated_at(self) -> datetime | None:
        np = self._now_playing()
        if not np or np.position_updated_at is None:
            return None
        return dt_util.utc_from_timestamp(np.position_updated_at / 1000)

    @property
    def media_image_url(self) -> str | None:
        """Cover art, either a producer-supplied URL or the daemon's own bytes.

        A daemon-relative `image_path` is resolved against the daemon's base URL
        here rather than daemon-side, because only this integration knows which
        host it is talking to."""
        np = self._now_playing()
        if not np:
            return None
        if np.image_url:
            return np.image_url
        if np.image_path:
            return f"{self.coordinator.client.base_url}{np.image_path}"
        return None

    @property
    def media_image_remotely_accessible(self) -> bool:
        """Always False, including for a public producer URL.

        Home Assistant then fetches the image server-side and hands the browser
        its own proxy URL — so the daemon's port never has to be reachable from a
        phone on the LAN, an ingress-only setup keeps working, and both artwork
        cases (URL and daemon-served bytes) behave identically."""
        return False


class PipewireRouterMediaPlayer(CoordinatorEntity[PipewireRouterCoordinator], _SourceMetadataMixin, MediaPlayerEntity):
    """A single PipeWire output (sendspin or AirPlay-2) exposed as a
    media_player.

    The output's *input wiring* is modelled as the media_player "source":
    `source_list` is the routable sources the daemon reports, `source` is
    whatever is currently linked in, and `select_source` swaps it (exclusive
    — the previously linked source is unlinked first). This is one source
    per output by design; the `link`/`unlink` services are the additive
    escape hatch for anything more.

    Behaviour is keyed off the node-name *prefix*, not a per-kind subclass.
    Both output kinds are *virtual* (no PipeWire node): state is derived from
    routing, and volume is carried in-band per device — sendspin through its
    per-device store, AirPlay-2 through the daemon's AP2 control plane
    (`/api/ap2/volume`|`/api/ap2/mute`, which additionally backs VOLUME_MUTE).
    """

    _attr_has_entity_name = True

    def __init__(
        self, hass: HomeAssistant, coordinator: PipewireRouterCoordinator, entry: ConfigEntry, node_name: str
    ) -> None:
        super().__init__(coordinator)
        # Identity is the stable node *name* (the daemon's ephemeral node_id is
        # never used for these virtual outputs), so this entity keeps working
        # across a module reload that changes the id.
        self.node_name = node_name
        self._attr_unique_id = f"{entry.entry_id}_out_{node_name}"

        # Feature set by kind. Every (virtual) output kind gets source selection
        # + volume. AirPlay-2 volume/mute go through the daemon's AP2 control
        # plane (PUT /api/ap2/volume|mute), so AP2 additionally advertises
        # VOLUME_MUTE. Per-device announce for a virtual output is a separate,
        # still-pending piece of work, so neither kind advertises PLAY_MEDIA
        # /MEDIA_ANNOUNCE — announcements fan out via the announcement group.
        features = MediaPlayerEntityFeature.SELECT_SOURCE | MediaPlayerEntityFeature.VOLUME_SET
        if self._is_ap2 or self._is_pwsink:
            features |= MediaPlayerEntityFeature.VOLUME_MUTE
        self._attr_supported_features = features

        # Correlate a virtual output to its Home Assistant device once, at
        # creation, so the entity links to that device and takes HA's name +
        # area. Two correlation rules by kind: sendspin matches the ESPHome
        # device by mDNS hostname; AirPlay-2 matches a third-party AV device by
        # the receiver's IP (`_find_ap2_ha_device`). Unmatched outputs fall back
        # to the daemon-derived display name with no device link.
        # Resolved here — matching devices are typically registered at HA start,
        # before this integration sets up — so one that only appears later is
        # picked up on the next reload.
        if self._is_ap2:
            meta = coordinator.outputs_meta.get(node_name)
            self._ha_device = _find_ap2_ha_device(hass, meta.ip if meta else None)
        elif self._is_sendspin:
            self._ha_device = _find_ha_device(hass, node_name)
        else:
            self._ha_device = None

        if self._ha_device is None:
            self._attr_name = _display_name(node_name)
        else:
            # has_entity_name + a name → "<device name> <name>", e.g.
            # "Home Assistant Voice Badezimmer Audio Routing". The suffix keeps
            # this routing output distinct from the speaker's own built-in
            # media_player ("<device name> Media Player") on the same device,
            # while still leading with the HA device name.
            self._attr_name = "Audio Routing"

    @property
    def _is_sendspin(self) -> bool:
        return self.node_name.startswith(SENDSPIN_DEV_PREFIX)

    @property
    def _is_ap2(self) -> bool:
        return self.node_name.startswith(AP2_DEV_PREFIX)

    @property
    def _is_pwsink(self) -> bool:
        """A remote PipeWire host with a paired pwrouter-agent. Its volume/mute is
        the *host's master out*, applied by the agent — so unlike sendspin/AP2 the
        slider moves the whole machine, which is what lets an announcement duck
        music the router isn't playing."""
        return self.node_name.startswith(PWSINK_DEV_PREFIX)

    @property
    def _is_virtual(self) -> bool:
        """A virtual output (sendspin or AirPlay-2) has no PipeWire node: it
        never appears in the polled media_players feed, so its state comes from
        routing rather than the feed."""
        return self._is_sendspin or self._is_ap2 or self._is_pwsink

    @property
    def _virtual_prefix(self) -> str | None:
        """The node-name prefix of this output's virtual kind — used to group
        same-kind co-routed devices."""
        if self._is_sendspin:
            return SENDSPIN_DEV_PREFIX
        if self._is_ap2:
            return AP2_DEV_PREFIX
        if self._is_pwsink:
            return PWSINK_DEV_PREFIX
        return None

    @property
    def device_info(self) -> DeviceInfo | None:
        """Merge this media_player into the matched Home Assistant device, so it
        is grouped under the real speaker and inherits its name + area. `None`
        when unmatched (kept standalone). We reuse the device's own existing
        identity — its full-MAC connection(s) if it has any (sendspin's ESPHome
        device, some AV gear), else its integration identifiers (MusicCast /
        HomeKit devices keyed that way) — never reconstruct one, which is what
        makes HA merge this entity into that exact device."""
        if self._ha_device is None:
            return None
        mac_connections = {
            (conn_type, conn_value)
            for conn_type, conn_value in self._ha_device.connections
            if conn_type == dr.CONNECTION_NETWORK_MAC
        }
        if mac_connections:
            return DeviceInfo(connections=mac_connections)
        if self._ha_device.identifiers:
            return DeviceInfo(identifiers=set(self._ha_device.identifiers))
        return None

    def _output(self) -> RoutingNode | None:
        """This output's routing-matrix entry (the authoritative existence +
        present/offline signal for sendspin and AirPlay-2 alike)."""
        return next((o for o in self._matrix().outputs if o.node_name == self.node_name), None)

    def _matrix(self) -> RoutingMatrix:
        return self.coordinator.routing

    @property
    def available(self) -> bool:
        # Present in the live graph right now. A configured-but-offline output
        # stays in the matrix with present=False (shown unavailable); one that
        # leaves the matrix entirely loses its entity (see _reconcile_entities).
        output = self._output()
        return output is not None and output.present

    @property
    def state(self) -> MediaPlayerState | None:
        # Virtual (sendspin / AirPlay-2): derive from routing — playing if a
        # present source is linked into this output, else idle.
        if self._is_virtual and self.available:
            present_sources = {s.node_name for s in self._matrix().sources if s.present}
            linked = any(out == self.node_name and src in present_sources for src, out in self._matrix().links)
            return MediaPlayerState.PLAYING if linked else MediaPlayerState.IDLE
        return None

    @property
    def volume_level(self) -> float | None:
        # AirPlay-2 volume comes from the daemon's per-device store, surfaced on
        # `/api/outputs` as `ap2_volume` (0.0–1.0). It's device-authoritative
        # when known, but genuinely unknown until the receiver reports one or the
        # user sets one — in which case the daemon omits it and we honestly report
        # None rather than fabricating full scale.
        if self._is_ap2:
            meta = self.coordinator.outputs_meta.get(self.node_name)
            return meta.ap2_volume if meta else None
        # Sendspin volume is carried in-band (no PipeWire node volume); it comes
        # from the daemon's per-device store (0-100), default full scale.
        if self._is_sendspin:
            return self.coordinator.sendspin_volumes.get(self.node_name, 100) / 100
        # pw-sink: the host's own master volume, read back from its agent. `None`
        # while no agent is connected — the value belongs to that desktop, so
        # there is nothing honest to show when we cannot see it.
        if self._is_pwsink:
            meta = self.coordinator.outputs_meta.get(self.node_name)
            return meta.pwsink_volume if meta else None
        return None

    @property
    def is_volume_muted(self) -> bool | None:
        # Only AirPlay-2 advertises VOLUME_MUTE; its mute flag is carried on
        # `/api/outputs` as `ap2_muted` (defaults to False daemon-side).
        if self._is_ap2:
            meta = self.coordinator.outputs_meta.get(self.node_name)
            return meta.ap2_muted if meta else None
        if self._is_pwsink:
            meta = self.coordinator.outputs_meta.get(self.node_name)
            return meta.pwsink_muted if meta else None
        return None

    @property
    def extra_state_attributes(self) -> dict[str, object] | None:
        """For a virtual device that shares its exact source-set with other
        devices of the same kind, expose the synchronized group it's part of
        (the daemon forms these automatically — sync_group.rs). Absent for a
        lone/ungrouped device. Reported under a kind-specific key
        (`sendspin_group_members` / `airplay2_group_members`)."""
        if not self._is_virtual:
            return None
        members = self._group_members()
        if len(members) < 2:
            return None
        key = "airplay2_group_members" if self._is_ap2 else "sendspin_group_members"
        return {key: [_display_name(n) for n in members]}

    def _group_members(self) -> list[str]:
        """Node names of the same-kind virtual devices sharing this device's
        exact (non-empty) set of routed sources — i.e. its synchronized group.
        Grouping is within one protocol (sendspin with sendspin, AP2 with AP2):
        a mixed-protocol group is a separate acoustic-alignment concern the
        daemon doesn't form automatically."""
        prefix = self._virtual_prefix
        if prefix is None:
            return []
        matrix = self._matrix()

        def source_key(output_name: str) -> tuple[str, ...]:
            return tuple(sorted(src for src, out in matrix.links if out == output_name))

        mine = source_key(self.node_name)
        if not mine:
            return []
        return sorted(
            o.node_name
            for o in matrix.outputs
            if o.node_name.startswith(prefix) and source_key(o.node_name) == mine
        )

    @property
    def source_list(self) -> list[str]:
        return [SOURCE_NONE] + [s.display_name for s in self._matrix().sources]

    def _linked_source_names(self) -> list[str]:
        """The sources linked into this output, as *node names*, in matrix order.

        The one place this output resolves "what feeds me": `source` renders it for
        display and `_metadata_source` reads metadata for it, so the label and the
        now-playing card can never disagree about which source they mean."""
        matrix = self._matrix()
        linked = {src for src, out in matrix.links if out == self.node_name}
        return [s.node_name for s in matrix.sources if s.node_name in linked]

    @property
    def source(self) -> str | None:
        """The source routed into this output, or SOURCE_NONE if none. Read
        from the persisted intent (by stable name), so it stays correct even
        when this output is momentarily offline. Exclusive model → at most one;
        report the first by name if several were wired additively."""
        matrix = self._matrix()
        names = self._linked_source_names()
        by_name = {s.node_name: s.display_name for s in matrix.sources}
        return by_name[names[0]] if names else SOURCE_NONE

    def _metadata_source(self) -> str | None:
        """Show the metadata of the source feeding this output — the same first-of
        several rule `source` uses."""
        names = self._linked_source_names()
        return names[0] if names else None

    def _resolve_source_name(self, source: str) -> str:
        target = next((s for s in self._matrix().sources if s.display_name == source), None)
        if target is None:
            raise HomeAssistantError(f"unknown source '{source}' for {self.entity_id}")
        return target.node_name

    async def async_set_volume_level(self, volume: float) -> None:
        if self._is_ap2:
            # 0.0–1.0 pushed in-band to the receiver via the AP2 control plane
            # (no PipeWire node volume for a virtual AP2 output).
            await self.coordinator.client.async_set_ap2_volume(self.node_name, volume)
        elif self._is_sendspin:
            # 0.0–1.0 → 0–100, sent in-band to the device (no PipeWire node vol).
            await self.coordinator.client.async_set_sendspin_volume(self.node_name, round(volume * 100))
        elif self._is_pwsink:
            # Applied by the host's agent to its own master out (device Route).
            await self.coordinator.client.async_set_pwsink_volume(self.node_name, volume)
        else:
            raise HomeAssistantError(f"volume is not supported for {self.entity_id}")
        await self.coordinator.async_request_refresh()

    async def async_mute_volume(self, mute: bool) -> None:
        if self._is_ap2:
            await self.coordinator.client.async_set_ap2_mute(self.node_name, mute)
        elif self._is_pwsink:
            await self.coordinator.client.async_set_pwsink_mute(self.node_name, mute)
        else:
            # Only AirPlay-2 and pw-sink advertise VOLUME_MUTE; guard so a direct
            # service call on another kind fails loudly instead of no-op-ing.
            raise HomeAssistantError(f"mute is not supported for {self.entity_id}")
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


class MusicGroupMediaPlayer(CoordinatorEntity[PipewireRouterCoordinator], _SourceMetadataMixin, MediaPlayerEntity):
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

    def _linked_source_names(self) -> list[str]:
        """Member-linked sources as *node names*, in matrix order — the single
        resolution `source` and `_metadata_source` both read."""
        srcs = self._member_sources()
        return [s.node_name for s in self._matrix().sources if s.node_name in srcs]

    @property
    def source(self) -> str | None:
        names = self._linked_source_names()
        by_name = {s.node_name: s.display_name for s in self._matrix().sources}
        return by_name[names[0]] if names else SOURCE_NONE

    def _metadata_source(self) -> str | None:
        """A group shows its source's metadata — the same source its `source`
        property names, so the chip label and the media card always agree."""
        names = self._linked_source_names()
        return names[0] if names else None

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
            elif name.startswith(AP2_DEV_PREFIX):
                meta = self.coordinator.outputs_meta.get(name)
                if meta is not None and meta.ap2_volume is not None:
                    levels.append(meta.ap2_volume)
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
        """Apply the master volume to every member, in-band per device: sendspin
        through its per-device store, AirPlay-2 through the AP2 control plane."""
        g = self._group()
        if not g:
            return
        for name in g.members:
            if name.startswith(SENDSPIN_DEV_PREFIX):
                await self.coordinator.client.async_set_sendspin_volume(name, round(volume * 100))
            elif name.startswith(AP2_DEV_PREFIX):
                await self.coordinator.client.async_set_ap2_volume(name, volume)
            elif name.startswith(PWSINK_DEV_PREFIX):
                await self.coordinator.client.async_set_pwsink_volume(name, volume)
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
