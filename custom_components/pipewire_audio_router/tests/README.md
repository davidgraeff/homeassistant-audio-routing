# Running these tests

Real tests against actual Home Assistant internals (config flow machinery,
`DataUpdateCoordinator`, entity platform forwarding, the state machine,
service-call dispatch) via
[`pytest-homeassistant-custom-component`](https://pypi.org/project/pytest-homeassistant-custom-component/) —
only the network layer (`PipewireRouterApiClient`, i.e. calls to the
bridge daemon) is mocked, not HA itself.

From the repo root:

```
pip install pytest-homeassistant-custom-component homeassistant home-assistant-frontend
python3 -m pytest custom_components/pipewire_audio_router/tests/ -p pytest_homeassistant_custom_component
```

`home-assistant-frontend` is needed even though nothing here tests the UI: the
integration depends on the `frontend` component (it serves the dashboard card),
and that component imports `hass_frontend` when it sets up — without the wheel,
every test that loads a config entry fails with
`Setup failed for dependencies: ['frontend']`. `scripts/test-custom-components.sh`
installs the version this HA pins, and is the easier way to run all of this.

`pytest.ini` at the repo root sets `asyncio_mode = auto`, required for the
`async def test_...` functions here to run at all.

## What each file covers

- `test_config_flow.py` — the host/port config flow (success, cannot-connect,
  duplicate-abort).
- `test_media_player.py` — one `media_player` per output: state/volume,
  announce (URL), `select_source`/`link`/`unlink`, and the live
  routing WebSocket.
- `test_voice_duck.py` — voice-assistant ducking: satellite state → area →
  which outputs are ducked (both scopes), the satellite's own output included,
  entity-area override, two rooms at once, release on idle/unavailable/switch-off
  and on entry unload, and the AP2-by-IP path with per-output entities off.
- `test_card_ws_api.py` — the dashboard card's WebSocket API: the snapshot shape,
  pushes on change (and *no* push when a poll changed nothing), the four routing
  commands, daemon errors surfaced to the card, and the one-vs-several-routers
  `entry_id` rules.
- `test_rtp_source.py` — the Bluetooth-bridge RTP `switch`/`number`: entities
  reflect daemon state, enable/disable, live vs. remembered port changes, and
  the API client's `/api/sources` calls (parsing + `ok`-flag errors).
