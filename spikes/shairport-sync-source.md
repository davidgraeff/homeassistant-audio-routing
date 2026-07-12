# Spike: shairport-sync as the AirPlay-receive source — PASSED

Per PLAN.md Section 5.2 ("run `shairport-sync` in receiver mode... each
instance registers its own PipeWire source node"). Requested as a
standalone spike before building it into Phase 2, since it was never
validated in any earlier spike.

## Packaging: no native PipeWire backend — routes via ALSA instead, and that's fine

Ubuntu 26.04's `shairport-sync` package (4.3.7-1build1) is built with
`libdaemon-OpenSSL-Avahi-ALSA-jack-pa-dummy-stdout-pipe-soxr-convolution-metadata-mqtt-dbus-mpris`
— no dedicated PipeWire backend, only ALSA/JACK/PulseAudio. This project's
image already makes this a non-issue: `pipewire-alsa` registers a
`pipewire` ALSA PCM device *and* overrides the system's `default` ALSA
device to route through it
(`/etc/alsa/conf.d/50-pipewire.conf`, `99-pipewire-default.conf`,
confirmed by reading both files directly). So `shairport-sync` with its
default ALSA backend and zero special config lands in the PipeWire graph
automatically, as an ordinary `Stream/Output/Audio` node
(`alsa_playback.shairport-sync`) — no shairport-sync-side PipeWire
awareness needed at all.

## Two real requirements, neither obvious from the package alone

1. **Hard-requires successful mDNS advertisement to start at all** — not
   a soft warning. With no `avahi-daemon` running, it logs a fatal error
   and exits ("Could not establish mDNS advertisement!", "emergency
   exit"). Needed: a real **D-Bus system bus** (not just the private
   session bus this project's other containers get by with) plus
   `avahi-daemon --daemonize --no-drop-root` actually running. Confirmed
   once both were present: `avahi-browse -r _raop._tcp` from the real
   host sees the service, with a full valid TXT record
   (`et=0,1` — supports none/RSA, consistent with a classic-AirPlay-1-only
   build; no `AirPlay2` in the version banner).
2. **PipeWire node only exists while a session is actively playing** —
   same lazy-activation pattern already seen for RAOP sinks (spike 2) and
   RTP (spike 3b). Idle, there's no node at all; nothing to link to until
   a real client connects.

## Real client used: `cliraop` (bundled with Music Assistant's airplay provider)

Reused `music-assistant-server/.../airplay/bin/cliraop-linux-x86_64`
rather than writing a new RAOP sender — a real, working classic-AirPlay-1
client already sitting in this workspace.

**Bug found in `cliraop`, not in shairport-sync:** by default, `cliraop`
unconditionally probes `POST /auth-setup` before anything else,
regardless of what the receiver actually advertises. Confirmed via a raw
manual RTSP `OPTIONS` probe that shairport-sync's `Public:` header lists
only `ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS,
GET_PARAMETER, SET_PARAMETER` — no `POST` at all. shairport-sync
correctly rejects the unadvertised method (`RTSP/1.0 500`), but `cliraop`
doesn't handle that response and **segfaults** (exit 139) instead of
falling back to a plain `ANNOUNCE`. Fixed with `-et 0` (cliraop's own
flag for "not an MFi/AirPort-Express device," which skips the auth-setup
probe entirely) — with it, `cliraop` connects, streams, and exits cleanly
(exit 0).

## Verified rigorously: real signal, not just "a node existed"

`support.null-audio-sink` test sink created, `cliraop` run against
`shairport-sync`, its resulting stream linked into the test sink, `pw-record`
capture analyzed with `ffmpeg`'s `astats` filter (not just byte counts —
matches this project's standing bar since spike 3b):

```
Peak level dB: -55.34
RMS level dB: -78.76
```

Non-silent, genuinely varying signal (flat factor 0.0), reproduced
identically across multiple runs.

**Real timing gotcha, fixed in the persisted test:** `cliraop` feeds the
receiver's buffer well ahead of real-time and exits within ~1-2 seconds
regardless of clip length — it does not block for the nominal playback
duration. The resulting PipeWire node's window to link into is short and
not predictably timed. A single fixed `sleep 0.8` before attempting
`pw-link` **raced it and failed intermittently** (`failed to link ports:
No such file or directory`, despite `pw-link -o`/`-i` showing correct
port names moments before/after — the node had already been torn down
between the listing and the link attempt). Fixed with a tight retry loop
(up to 40 attempts, 50ms apart) instead of guessing a delay — same
"poll until ready" pattern already used elsewhere in this project (e.g.
spike 2's "waiting for sink node" loop), just needed here for a
*disappearing* target instead of an *appearing* one.

## What this means for Phase 2 (Section 5.2 wiring)

- The mechanism is proven: `shairport-sync` genuinely gets phone/PC
  AirPlay audio into this project's PipeWire graph, unmodified, via the
  ALSA path already wired up in the image.
- Phase 2's real implementation needs `avahi-daemon` + a D-Bus **system**
  bus running in the production container (not just the private session
  bus `entrypoint.sh` currently sets up) — a real change to
  `container/entrypoint.sh`/the add-on's `run.sh`, not just a spike-only
  detail.
- The "node only exists while playing" behavior is fine for the real
  daemon: unlike this spike's throwaway manual linking, the bridge daemon
  will hold a live PipeWire registry listener (already built in Phase 1,
  `pw_thread.rs`) and can link/react the moment the node actually
  appears — no guessing delays needed in the real implementation either.
- Multiple concurrent AirPlay sources (one shairport-sync instance per
  expected simultaneous sender, e.g. "phone" and "PC" as separate named
  services) is a real design point to settle in Phase 2, not resolved by
  this spike, which only tested one instance.

## Files added

- `tests/shairport_sync_spike.Dockerfile` — layers `shairport-sync` on
  the real `pw-audio-router` image.
- `tests/test_shairport_sync_source.sh` — fully automated: builds the
  image, starts avahi+dbus+pipewire+wireplumber+shairport-sync, sends a
  real AirPlay stream via `cliraop`, links it into a test sink with the
  retry loop above, verifies real signal via `ffmpeg astats`. Reuses
  Music Assistant's bundled `cliraop` binary rather than vendoring a
  duplicate copy (path overridable via `CLIRAOP_PATH`).
