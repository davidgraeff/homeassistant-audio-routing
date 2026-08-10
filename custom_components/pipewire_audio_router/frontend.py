"""Serve the routing Lovelace card and load it into the frontend.

The card is a single pre-built ES module committed at `www/pipewire-router-card.js`
(built from `pipewire_audio_router/frontend/` by `npm run build:card`) — HACS
copies this directory verbatim and runs no build step, so the artifact has to be
in the repository.

`add_extra_js_url` makes the module load on every frontend page, which is what
lets `type: custom:pipewire-router-card` work with no manual entry under
Settings → Dashboards → Resources. The alternative — creating a Lovelace
resource programmatically — only works in storage mode and leaves a row behind
that outlives the integration.
"""

from __future__ import annotations

import logging
from pathlib import Path

from homeassistant.components import frontend
from homeassistant.components.http import StaticPathConfig
from homeassistant.core import HomeAssistant

from .const import DOMAIN

_LOGGER = logging.getLogger(__name__)

CARD_FILENAME = "pipewire-router-card.js"
CARD_URL = f"/{DOMAIN}/{CARD_FILENAME}"
CARD_PATH = Path(__file__).parent / "www" / CARD_FILENAME


async def async_register_card(hass: HomeAssistant) -> None:
    """Serve the card at `CARD_URL` and load it on every dashboard.

    Missing artifact is a warning, not a failure: the integration's entities are
    the product, and a source checkout that hasn't run the card build should
    still set up."""
    stat = await hass.async_add_executor_job(_stat_card)
    if stat is None:
        _LOGGER.warning(
            "routing card not found at %s — the `custom:pipewire-router-card` "
            "dashboard card will be unavailable (run `npm run build:card`)",
            CARD_PATH,
        )
        return
    await hass.http.async_register_static_paths(
        [StaticPathConfig(CARD_URL, str(CARD_PATH), cache_headers=True)]
    )
    # Cache-busted by the artifact's own mtime, so an upgraded install serves the
    # new card instead of the browser's copy of the old one — and we don't have
    # to remember to bump a version constant alongside every card change.
    frontend.add_extra_js_url(hass, f"{CARD_URL}?v={stat}")


def _stat_card() -> int | None:
    """The card's mtime as a whole number of seconds, or `None` if it isn't there."""
    try:
        return int(CARD_PATH.stat().st_mtime)
    except OSError:
        return None
