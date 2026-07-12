# Running these tests

Real tests against actual Home Assistant internals (config flow machinery,
`DataUpdateCoordinator`, entity platform forwarding, the state machine,
service-call dispatch) via
[`pytest-homeassistant-custom-component`](https://pypi.org/project/pytest-homeassistant-custom-component/) —
only the network layer (`PipewireRouterApiClient`, i.e. calls to the
bridge daemon) is mocked, not HA itself.

From the repo root:

```
pip install pytest-homeassistant-custom-component homeassistant
python3 -m pytest custom_components/pipewire_audio_router/tests/ -p pytest_homeassistant_custom_component
```

`pytest.ini` at the repo root sets `asyncio_mode = auto`, required for the
`async def test_...` functions here to run at all.
