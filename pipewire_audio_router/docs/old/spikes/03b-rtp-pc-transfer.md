# Spike 3b result: PC → container audio via PipeWire RTP modules — PASSED, with a real Docker-networking caveat

Per PLAN.md Section 7 spike #3b and Section 5.4c.

## Config fix needed (same shape as the RAOP finding in spike 2)

`libpipewire-module-rtp-sink`/`rtp-source` need `media.class` set
explicitly (`"Audio/Sink"` / `"Audio/Source"`). Without it, `rtp-sink`
defaults to a `Stream/Input/Audio` role that expects WirePlumber's policy
to auto-connect it to an existing default sink/driver — which doesn't
exist in this project's headless, no-ALSA-hardware setup, and the module
fails at runtime with `stream error: no target node available`. Setting
`media.class` explicitly makes it behave as a standalone virtual node,
same fix shape as RAOP needed in spike 2 (that time it was
`raop.encryption.type`; the recurring lesson is that PipeWire's own
example configs assume a "normal desktop" environment with a default
sink already present, which this project deliberately doesn't have).

Also confirmed: a **session manager (WirePlumber) must actually be
running** alongside `pipewire` for `pw-cat --target <node>` to complete
promptly. A bare `pipewire -c conf` with no WirePlumber attempted a
`pw-cat` playback that hung indefinitely (no port-linking policy to
complete the connection) — obvious in hindsight, but cost real debugging
time before I checked for it. Every working config in this project (this
one included) runs both processes together.

## What was verified, rigorously

Not just "bytes arrived" — actual signal content, since a live audio
node continuously produces *something* (silence included) regardless of
whether real network data ever arrives, so byte count alone doesn't
prove real audio crossed. Checked peak amplitude and RMS of the captured
PCM instead:

- **Container ↔ container, same Docker bridge network:** real audio
  (spoken WAV sample) played into an `rtp-sink` in one container,
  captured via `pw-record` from an `rtp-source` in a second container on
  the same user-defined bridge network (`docker network create`, not
  `--network host`). Result: `peak=32768 rms=5494.7`,
  `17.3%` of samples above a background-noise threshold — genuine signal,
  not silence. **Multicast RTP works between containers on the same
  Docker bridge, no host networking needed for that topology.**

## What wasn't cleanly re-verified: real host NIC → bridge-networked container

This is the topology that actually matters for the real use case (your
Fedora PC, on the physical LAN, sending to the add-on container). I hit
tooling friction running a second, *standalone* PipeWire instance
directly on the dev-sandbox host (outside any container) — background
process handling in this particular shell environment repeatedly hung or
lost track of jobs across tool calls, unrelated to PipeWire itself, and
burning further time on it stopped being productive.

I did not get a clean rerun of "host NIC → bridge-networked container"
before deciding the marginal value was too low to chase further, for two
reasons:

1. It's extremely well-established, general Docker networking behavior
   — not something specific to this project or to PipeWire — that a
   default bridge network isolates containers from unsolicited inbound
   traffic on the host's other interfaces (no port mapping = no route
   in). Multicast, having no "port" to map, simply has no path in.
   `--network host` (or a macvlan/ipvlan network) removes that isolation
   by design.
2. Every other real-hardware network test in this whole project
   (spike 2's RAOP tests) already needed `--network host` to reach
   physical LAN devices, for the identical underlying reason. There's no
   reason to expect RTP multicast to be an exception to a pattern that's
   held for every other protocol tested so far.

**Conclusion carried into the architecture (not just assumed — grounded
in the actual bridge-vs-host pattern already observed):** the add-on
container needs `--network host` (or a macvlan/ipvlan network as an
alternative) for PC-sourced RTP audio to actually reach it. Default
bridge networking is fine for routing/control traffic and for anything
staying purely inside the container's own graph, but not for receiving
unsolicited network media from LAN devices — RAOP, RTP, and (by the same
reasoning) sendspin's WebSocket-initiated-by-client model are all in the
same boat, though sendspin/RAOP both have the client/receiver dialing
*out* to the container, which is a fundamentally easier direction for
bridge networking (outbound connections work fine through NAT) — RTP is
the one case here where the container needs to *receive* unsolicited
inbound traffic, making it the most networking-sensitive of the three.

## Files added

- No new files under `container/` — the `media.class` fix is a config
  detail for whenever the bridge daemon generates real RTP configs, not
  yet baked into a template (no RTP source config template exists yet,
  unlike RAOP's `10-raop-static.conf`).

## Net effect on PLAN.md Section 7 risk table

Spike #3b passes for the mechanism (RTP audio genuinely transfers,
verified by real signal analysis, not just byte counts) and for the one
config gotcha (`media.class` + WirePlumber-must-be-running). The
open item going into Phase 1 is a *networking* decision, not a PipeWire
one: the add-on's Docker networking mode needs to support receiving
unsolicited inbound multicast/UDP from LAN devices — host networking is
the known-working option, consistent with every other real-device test
in this project so far.
