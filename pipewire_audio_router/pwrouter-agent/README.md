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

The easy way: open the add-on's **Outputs** page → *Explain receiver hosts*, and
download the binary for your architecture. The add-on serves the build that matches
it, so there is nothing to compile and no third-party download. Those binaries need
glibc 2.34+ (Ubuntu 22.04 LTS, Fedora 35, Debian 12 or newer) and PipeWire 0.3.

To build it yourself instead — older distro, or you are working on the agent —
requires PipeWire (any 1.x) and a Rust toolchain plus `libpipewire-0.3` headers
(`pipewire-devel` on Fedora, `libpipewire-0.3-dev` on Debian/Ubuntu).

```sh
cargo build --release
install -Dm755 target/release/pwrouter-agent ~/.local/bin/pwrouter-agent
~/.local/bin/pwrouter-agent autostart enable   # writes the systemd user unit
systemctl --user start pwrouter-agent
journalctl --user -u pwrouter-agent -f
```

The unit file is **built into the binary**, so there is nothing to download or
copy: `autostart enable` writes
`~/.config/systemd/user/pwrouter-agent.service`, points its `ExecStart` at the
binary you just ran it from, and enables it for the next login. `autostart
disable` removes it again, and `autostart` on its own says which it currently is.
Neither starts or stops a running agent — that is `systemctl --user start|stop
pwrouter-agent`, on purpose: an agent that installs its own unit is usually
already running, and two agents in one session would fight over its volume. The
same switch sits in the tray menu under **Autostart**.

Re-running `autostart enable` also refreshes an older unit file, which is how a
host picks up changes to its hardening after an upgrade.

On start the agent finds the add-on over mDNS and asks to pair. The log prints a
short code, minted once per run — so it stays the same across reconnects, and a
restart is how you ask for a new one:

```
WARN not paired yet — pairing code for this host: 4F2A9C
```

On a desktop you do not have to read the journal for it: the agent also raises a
notification with the code and puts a status icon in the tray whose menu keeps
showing it, along with the add-on it found, the sink your audio lands in and
whether an announcement is currently turning other audio down. The menu is
read-only — the add-on drives this machine, not the tray.

Both need the desktop to provide them, and neither is required: with no
notification server the notification is skipped, and the tray icon needs a
[StatusNotifierItem] host — KDE, Xfce, Cinnamon, MATE and most WM bars have one
built in, GNOME needs its [AppIndicator extension]. A headless server has neither
and behaves exactly as before. To find out what a given session supports, without
involving the add-on at all:

```sh
pwrouter-agent spike-desktop            # shows both for a fake code; Ctrl-C to stop
RUST_LOG=debug pwrouter-agent spike-desktop   # ... and says why either is missing
```

[StatusNotifierItem]: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/
[AppIndicator extension]: https://extensions.gnome.org/extension/615/appindicator-support/

The host then shows up under **Discovered devices** on the add-on's Outputs page,
like any other speaker, with that same code on its card. Compare the two and press
**Pair**: that adds it as an output *and* stores the token, `0600`, in
`~/.config/pwrouter-agent/config.json`. The same thing over the API, if you prefer:

```sh
curl -s http://<addon-host>:8099/api/outputs/discovered   # find its node_name
curl -s -X POST http://<addon-host>:8099/api/outputs/pwsink-dev-<host>_<user>/adopt
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

All three commands work without a paired add-on:

```sh
# Does this session show a tray icon and a notification? (See "Pair" above.)
pwrouter-agent spike-desktop

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
pwrouter-agent autostart disable        # removes and disables the unit
systemctl --user stop pwrouter-agent
rm ~/.local/bin/pwrouter-agent
rm -r ~/.config/pwrouter-agent          # forgets the pairing token
```

Unpairing is done in the add-on, on the output's card (**Unpair**, or
`POST /api/outputs/<node_name>/unpair`): it revokes the token and clears the
host's routing, group membership and Home Assistant player. A running agent whose
token was revoked does not give up — it drops the dead token and goes back to
asking, so the host reappears under Discovered devices and letting it back in is
one click. Stopping it as above is what makes it stay gone.
