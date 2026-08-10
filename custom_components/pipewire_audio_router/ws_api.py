"""Home Assistant WebSocket API behind the routing Lovelace card.

The card (`pipewire_audio_router/frontend/src/card/`) runs on the Home Assistant
origin, so it cannot use the daemon's own REST API + `/api/routing/ws` the way
the add-on's admin UI does: that API lives behind the add-on's ingress path,
needs an ingress session, and isn't reachable at all through HA Cloud or a
reverse proxy. So the integration proxies exactly what the card needs — and
nothing more.

The push side costs nothing: the coordinator already holds the matrix pushed
live from the daemon (`_apply_routing`), so a subscription here is a listener on
it, not a second socket to the Pi.

The payload is deliberately a *reduced* shape (no volumes, meters, now-playing,
`node_id`, or per-output metadata): the card shows inputs, targets and links, so
anything else would be a contract we'd have to keep and never read. That is also
why these types are declared in the card's own `card/types.ts` instead of being
shared with the admin UI's `lib/types.ts`.
"""

from __future__ import annotations

from typing import Any

import voluptuous as vol
from homeassistant.components import websocket_api
from homeassistant.core import HomeAssistant, callback

from .api import PipewireRouterApiError
from .const import DOMAIN

# One prefix for every command, so they're recognizable in the frontend's
# websocket trace and can't collide with another integration's.
WS_SUBSCRIBE = f"{DOMAIN}/subscribe"
WS_LINK = f"{DOMAIN}/link"
WS_UNLINK = f"{DOMAIN}/unlink"
WS_ROUTE_GROUP = f"{DOMAIN}/route_group"
WS_UNROUTE_GROUP = f"{DOMAIN}/unroute_group"

ERR_NO_ENTRY = "no_entry"
ERR_AMBIGUOUS = "ambiguous_entry"
ERR_DAEMON = "daemon_error"


@callback
def async_register(hass: HomeAssistant) -> None:
    """Register every card command. Domain-wide and idempotent — called from
    `async_setup`, i.e. once per Home Assistant start regardless of how many
    daemons are configured."""
    for handler in (
        ws_subscribe,
        ws_link,
        ws_unlink,
        ws_route_group,
        ws_unroute_group,
    ):
        websocket_api.async_register_command(hass, handler)


def _node(node: Any) -> dict[str, Any]:
    """One routing endpoint as the card reads it. `present` is what the card
    grays out — a link to an absent endpoint is kept and reapplied, so it is
    still drawn."""
    return {
        "node_name": node.node_name,
        "display_name": node.display_name,
        "present": node.present,
    }


@callback
def _snapshot(coordinator: Any) -> dict[str, Any]:
    """The card's whole world: inputs, outputs, links, music groups.

    Sent complete every time rather than as a diff — it is a few hundred bytes
    for a house-sized setup, and a diff protocol would be a second source of
    truth about a matrix the daemon already sends whole."""
    matrix = coordinator.routing
    return {
        "sources": [_node(n) for n in matrix.sources],
        "outputs": [_node(n) for n in matrix.outputs],
        "links": [{"source": source, "output": output} for source, output in matrix.links],
        "groups": [
            {"id": group.id, "name": group.name, "members": list(group.members)}
            for group in coordinator.music_groups
        ],
    }


@callback
def _resolve(
    hass: HomeAssistant, connection: websocket_api.ActiveConnection, msg: dict[str, Any]
) -> Any | None:
    """The coordinator this message is about, or `None` after sending the error.

    `entry_id` is optional because the overwhelmingly common case is one daemon:
    the card then needs no configuration at all. With several configured, the
    card must say which — guessing would silently reroute the wrong house."""
    coordinators: dict[str, Any] = hass.data.get(DOMAIN, {})
    entry_id = msg.get("entry_id")
    if entry_id is not None:
        coordinator = coordinators.get(entry_id)
        if coordinator is None:
            connection.send_error(msg["id"], ERR_NO_ENTRY, f"no loaded config entry {entry_id}")
            return None
        return coordinator
    if not coordinators:
        connection.send_error(msg["id"], ERR_NO_ENTRY, "no PipeWire Audio Router config entry is loaded")
        return None
    if len(coordinators) > 1:
        connection.send_error(
            msg["id"],
            ERR_AMBIGUOUS,
            f"{len(coordinators)} routers are configured — set `entry_id` in the card configuration",
        )
        return None
    return next(iter(coordinators.values()))


@websocket_api.websocket_command(
    {
        vol.Required("type"): WS_SUBSCRIBE,
        vol.Optional("entry_id"): str,
    }
)
@callback
def ws_subscribe(
    hass: HomeAssistant, connection: websocket_api.ActiveConnection, msg: dict[str, Any]
) -> None:
    """Stream the routing snapshot: one immediately, then one per change."""
    coordinator = _resolve(hass, connection, msg)
    if coordinator is None:
        return

    last: dict[str, Any] | None = None

    @callback
    def _push() -> None:
        # The coordinator notifies its listeners on *every* poll as well as on
        # every pushed matrix, and a poll usually changes nothing the card can
        # see. Comparing against the last payload is what keeps an idle house
        # from waking every open dashboard every few seconds.
        nonlocal last
        snapshot = _snapshot(coordinator)
        if snapshot == last:
            return
        last = snapshot
        connection.send_message(websocket_api.event_message(msg["id"], snapshot))

    connection.subscriptions[msg["id"]] = coordinator.async_add_listener(_push)
    connection.send_result(msg["id"])
    _push()


async def _call(
    connection: websocket_api.ActiveConnection, msg: dict[str, Any], coro: Any
) -> None:
    """Await a daemon call and answer the message.

    No optimistic result: the answer means the daemon accepted it, and the new
    matrix arrives on the subscription a moment later over the daemon's own
    push socket. So the card never has to guess what the graph became."""
    try:
        await coro
    except PipewireRouterApiError as err:
        connection.send_error(msg["id"], ERR_DAEMON, str(err))
        return
    connection.send_result(msg["id"])


@websocket_api.websocket_command(
    {
        vol.Required("type"): WS_LINK,
        vol.Required("source"): str,
        vol.Required("output"): str,
        vol.Optional("entry_id"): str,
    }
)
@websocket_api.async_response
async def ws_link(
    hass: HomeAssistant, connection: websocket_api.ActiveConnection, msg: dict[str, Any]
) -> None:
    """Route one source into one output (additive — an output may mix several)."""
    coordinator = _resolve(hass, connection, msg)
    if coordinator is None:
        return
    await _call(connection, msg, coordinator.client.async_link(msg["source"], msg["output"]))


@websocket_api.websocket_command(
    {
        vol.Required("type"): WS_UNLINK,
        vol.Required("source"): str,
        vol.Required("output"): str,
        vol.Optional("entry_id"): str,
    }
)
@websocket_api.async_response
async def ws_unlink(
    hass: HomeAssistant, connection: websocket_api.ActiveConnection, msg: dict[str, Any]
) -> None:
    """Drop one source→output route."""
    coordinator = _resolve(hass, connection, msg)
    if coordinator is None:
        return
    await _call(connection, msg, coordinator.client.async_unlink(msg["source"], msg["output"]))


@websocket_api.websocket_command(
    {
        vol.Required("type"): WS_ROUTE_GROUP,
        vol.Required("group_id"): str,
        vol.Required("source"): str,
        vol.Optional("entry_id"): str,
    }
)
@websocket_api.async_response
async def ws_route_group(
    hass: HomeAssistant, connection: websocket_api.ActiveConnection, msg: dict[str, Any]
) -> None:
    """Put a whole music group on one source.

    This is the group's *exclusive* route — the same reconciling call the group's
    `media_player.select_source` makes: every member onto that source, every
    other source dropped. Which is why the card can't create a group whose
    speakers disagree about what they play."""
    coordinator = _resolve(hass, connection, msg)
    if coordinator is None:
        return
    await _call(
        connection,
        msg,
        coordinator.client.async_route_music_group(msg["group_id"], msg["source"]),
    )


@websocket_api.websocket_command(
    {
        vol.Required("type"): WS_UNROUTE_GROUP,
        vol.Required("group_id"): str,
        vol.Optional("entry_id"): str,
    }
)
@websocket_api.async_response
async def ws_unroute_group(
    hass: HomeAssistant, connection: websocket_api.ActiveConnection, msg: dict[str, Any]
) -> None:
    """Silence a whole music group (drop every member's routes)."""
    coordinator = _resolve(hass, connection, msg)
    if coordinator is None:
        return
    await _call(connection, msg, coordinator.client.async_unroute_music_group(msg["group_id"]))
