# pwrouter-agent

The receiver-side helper for the add-on's **pw-sink** outputs. Install it on any
Linux host you want the router to stream to; the host then appears in the add-on
as an output whose volume and mute Home Assistant can drive, and whose *other*
audio (Spotify, a browser, a game) can be ducked while an announcement plays.

Design and rationale: [`../docs/receiver-agent-plan.md`](../docs/receiver-agent-plan.md).

## What it does — and what it deliberately cannot do

* It **dials out** to the add-on over one WebSocket. Nothing listens on your
  machine: no open port, no firewall rule, no PulseAudio TCP server.
* It accepts a **fixed command set** — set volume, set mute, duck, unduck, become
  the receiver for a session, stop being one. That is the whole protocol. The
  add-on cannot ask it to run a command, read a file, or reconfigure your audio;
  even the receive module's arguments are built here, from parameters, never
  passed in as a string.
* It **configures the receive side itself**, so there is no
  `pipewire.conf.d/` drop-in to write or maintain. The module it loads lives and
  dies with the process: stop the agent and the nodes disappear.
* It **restores your volumes** on unduck, on disconnect, when the add-on stops
  answering, and on exit. A dead add-on cannot leave your desktop attenuated.
* It never *owns* your volume: when you change it locally, that change is pushed
  back so Home Assistant follows the machine rather than overwriting it.

## Install

Requires PipeWire (any 1.x) and, to build, a Rust toolchain plus
`libpipewire-0.3` headers (`pipewire-devel` on Fedora,
`libpipewire-0.3-dev` on Debian/Ubuntu).

```sh
cargo build --release
install -Dm755 target/release/pwrouter-agent ~/.local/bin/pwrouter-agent
install -Dm644 pwrouter-agent.service ~/.config/systemd/user/pwrouter-agent.service
systemctl --user daemon-reload
systemctl --user enable --now pwrouter-agent
journalctl --user -u pwrouter-agent -f
```

On first start the agent finds the add-on over mDNS and requests pairing. The log
prints a short code:

```
WARN waiting for approval in the add-on UI — pairing code: 4F2A9C
```

Approve it in the add-on (the same code is shown there, so you can tell two
requests apart), and the agent reconnects with a token stored `0600` in
`~/.config/pwrouter-agent/config.json`. Until the add-on ships the approval UI,
approve over its API:

```sh
curl -s http://<addon-host>:8099/api/agents            # find your identity
curl -s -X POST http://<addon-host>:8099/api/agents/approve \
     -H 'content-type: application/json' \
     -d '{"identity":"<machine-id>:<user>"}'
```

If mDNS does not reach the add-on (routed VLANs, a container without host
networking), point the agent at it directly — the address is remembered:

```sh
pwrouter-agent run --daemon 192.168.1.20:8099
```

## Two users on one machine

Pairing identity is *machine id + user*, and the config lives in each user's
`~/.config`. Two logged-in users therefore pair as two independent outputs, and
neither can drive the other's audio. Run one agent per session.

## Diagnosing a host by hand

Both commands work without a paired add-on:

```sh
# Load the receive module in the foreground and watch the nodes/links it creates.
pwrouter-agent spike-receiver --ifname enp5s0

# Show which sink the router's stream lands in, which lever controls it, and its
# level; optionally set one.
pwrouter-agent spike-volume
pwrouter-agent spike-volume --set 0.4
```

`spike-volume` prints the lever it found — `device Route` for a real sound card
(what `wpctl` and your volume applet use) or `node Props` for a virtual sink. This
distinction matters: writing the node's volume on a *device* sink is invisible to
your desktop and gets reverted by WirePlumber, which is why the agent uses the
route.

## Uninstall

```sh
systemctl --user disable --now pwrouter-agent
rm ~/.local/bin/pwrouter-agent ~/.config/systemd/user/pwrouter-agent.service
rm -r ~/.config/pwrouter-agent          # forgets the pairing token
```

Remove the pairing in the add-on too (`POST /api/agents/forget`), which revokes
the token.
