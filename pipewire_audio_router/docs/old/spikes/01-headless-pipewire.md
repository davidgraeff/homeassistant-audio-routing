# Spike 1 result: headless PipeWire + WirePlumber in Docker — PASSED

Confirmed on x86_64 (primary dev loop) and cross-checked under QEMU arm64
emulation via `scripts/build-arm64.sh`.

## What broke initially

Stock Debian bookworm `wireplumber` (0.4.13) exits immediately
(`exit code 70`, log: `disconnected from pipewire`) when started in a
plain container with no `elogind`/`systemd-logind` and no D-Bus system
bus. Root cause: the default `wireplumber.conf` loads a `bluetooth.lua`
component, which pulls in the bluez5 monitor's logind seat-monitoring
integration — unconditionally fatal without a login manager present.

## Fix

`container/etc-wireplumber/wireplumber.conf` overrides the stock config
(placed under `/etc/wireplumber/`, which the search path
prefers over `/usr/share/wireplumber/`) and simply omits the
`bluetooth.lua` component. Matches the architecture decision (Section 5.1
of PLAN.md): this is a pure network-audio router with no local ALSA/BT
hardware, so bluetooth support isn't needed at all — not patched, just
dropped. ALSA's monitor (`main.lua`) is left in; it's a harmless no-op
with no `/dev/snd` bound.

No D-Bus system bus is needed either — `libpipewire-module-rt`'s rtkit
realtime-priority request logs warnings and falls back to non-RT
scheduling when it can't reach a system bus. Not fatal; realtime
scheduling on the real Pi hardware is a separate concern to revisit later
(cap_sys_nice / rtkit-in-container), not a blocker for this spike.

A private per-container D-Bus **session** bus (`dbus-launch`) is still
started in `entrypoint.sh` — some modules probe for it even though we
don't need portal/session integration for a headless router.

## Verified

- `pipewire` and `wireplumber` both start and stay running (checked via
  `kill -0` on their PIDs after a few seconds, and via `docker exec ...
  ps aux` / `pw-cli info 0` against the real container `ENTRYPOINT`, not
  just an ad-hoc shell).
- Virtual nodes can be created (`pw-cli create-node adapter
  "{ factory.name=support.null-audio-sink ... }"`) and linked
  (`pw-link test-source:capture_FL test-sink:playback_FL`), proving the
  actual routing mechanism the whole project depends on works inside the
  container.
- The Dockerfile builds for both `linux/amd64` and `linux/arm64`
  (`docker buildx build --platform linux/arm64`), and the same
  wireplumber fix holds under QEMU arm64 emulation.

## Still open (not blockers, just not yet tested)

- Real jitter/latency measurement (this spike only proved "it runs," not
  "it's fast") — deferred to the Section 7 spike #5 latency test once
  RAOP/sendspin outputs exist.
- Realtime scheduling (rtkit or `cap_sys_nice`) inside the HA add-on's
  actual Docker profile on real Pi hardware — HA add-ons run under a
  specific `apparmor`/capability profile that hasn't been tested yet.
- No physical Pi 4 test performed yet — QEMU emulation confirms the
  config/boot behavior, not real-hardware performance.
