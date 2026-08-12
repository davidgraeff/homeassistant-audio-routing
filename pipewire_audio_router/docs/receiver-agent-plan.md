# `pwrouter-agent` — the receiver-side helper for pw-sink targets

**Built and in use** (2026-08-03 onwards). A small Rust helper that runs in the
user's session on a remote PipeWire host, dials *out* to the add-on over a
WebSocket, and takes over both halves of what a pw-sink target needs: **the
receive side** (it loads `module-rtp-session` itself, so there is no hand-written
drop-in) and **a control plane** (master volume, mute, and a duck of the host's
*other* audio). A pw-sink output **is** an agent — no agent, no target.

Code: `pwrouter-agent/` (the helper), `pw-control/` (the bits both ends share) and
`bridge-daemon/src/outputs/pwsink/agent.rs` (the daemon side). This file records
the shape, the reasoning behind each part, and the findings that changed the
design. Section numbers are stable because ~15 source files cite them.

The sending half of this backend is [`pipewire-sink-roadmap.md`](pipewire-sink-roadmap.md).

---

## 1. Why a helper at all

Without one, a pw-sink target is a *virtual* output with **no volume or mute** —
there is no local node to attenuate, the transport carries no control channel, and
PipeWire's RTP modules implement no RTCP feedback either — so the HA
`media_player` had no slider and an announcement could not duck whatever the
remote host was playing on its own. And every target host needed a hand-written
`pipewire.conf.d/` drop-in loading `libpipewire-module-rtp-session`: a
documentation-and-copy-paste step per host that does not survive a reinstall and
cannot be reconciled from the add-on.

### 1.1 Why not native remote control (investigated 2026-08-03)

**PipeWire has no network-reachable control protocol.**
`libpipewire-module-protocol-native` describes itself as *"Native protocol using
unix sockets"* and accepts only a `sockets` argument — so no remote `pw-cli`, no
remote `pipewire-rs` core connection.

The one native remote surface is PipeWire's **PulseAudio server over TCP**
(`module-protocol-pulse`, which implements `module-native-protocol-tcp` and
`module-zeroconf-publish`). It was rejected after reading the implementation and
testing it live on a 1.6.8 host, for two reasons that are worth knowing before
anyone reaches for it again:

- **The pulse cookie is not a preshared key here.** `do_command_auth`
  (`module-protocol-pulse/pulse-server.c:114`) checks only that the cookie blob is
  `NATIVE_COOKIE_LENGTH` bytes and then sets `client->authenticated = true`; the
  bytes are never compared. Verified live: a random 256-byte cookie and the host's
  real `~/.config/pulse/cookie` behave identically. `auth-anonymous` is therefore
  near-meaningless — there is nothing to switch off.
- **Access is per-listen-address, not per-client.** TCP clients default to
  `client.access = "restricted"` (`server.c:511`), which is read-only in practice
  (verified: `pactl get-sink-volume` works, `set-sink-mute` → Access denied).
  Getting control requires `client.access = "unrestricted"` on that address, which
  grants it to *every* client that can reach the port — including microphone
  capture. No per-client credential exists anywhere in the path.

So security would have to live entirely in the transport (firewall/WireGuard). An
SSH-forward variant was rejected too: it can be locked down with a forced command
and `permitopen`, but the credential the user is asked to install still reads as
*shell access to my desktop*, which is no easier to accept than an open
audio-control port.

### 1.2 What the helper buys instead

Least privilege by construction — it accepts a fixed command set and nothing else
— plus three things no remote protocol can do: configure the receive side itself,
ramp a duck locally with no network jitter, and exclude *our own* stream from that
duck.

## 2. Trust model

- **The helper dials out.** Nothing listens on the receiver: no open port, no
  firewall rule, no `unrestricted` access level, nothing for a LAN scan to find.
  One outbound WebSocket to the add-on.
- **Narrow command set.** The wire protocol is an enum (§5), not a passthrough. A
  stolen token buys "change the volume on that host", not audio capture and not a
  shell.
- **Bearer token from an explicit pairing** (§8), stored `0600` in the helper's
  config. Consistent with how HA add-ons authenticate elsewhere; per-message HMAC
  is an available upgrade path, not needed at this blast radius.

## 3. A pw-sink target *is* an agent-managed host

Discovery therefore changed from "browse `_pipewire-audio._udp`" — which detects a
*statically configured* receiver — to "an agent has paired and is connected",
which is both a stronger and a more honest signal, and which finally answers the
"configured vs. connected" question this backend could not previously answer.

Consequences, accepted:

- Existing hosts stopped working until their agent was installed. No migration
  path, and none wanted (the same "start clean" stance as the output adoption
  gate).
- `outputs/pwsink/discovery.rs` lost its role as the source of truth. It survives
  as a **diagnostic only** — its browse still logs "discovered pw-sink target",
  nothing reads its registry, and it is not even in `AppState` any more.

> **This half was missed the first time round, and it made every pw-sink output
> silent** (found live 2026-08-05: paired, adopted, routed — and the agent still
> reporting `receiving: false`). `routing::sync_group::compute_desired` and
> `routing::build_matrix` still built their pw-sink half from that registry, which
> keys a host `pwsink-dev-<host>`, while a pairing — and therefore every routing
> link, adoption verdict and HA entity — carries `pwsink-dev-<host>_<user>`. So
> `source_set_of` was asked about a name no link could hold: no member, no
> `pwsink_server`, no advertised session, and an agent waiting forever for one. The
> visible tell of two sources of truth was the matrix showing the host as
> `present: false` while `/api/outputs` showed it connected.
>
> Both now read `Agents::connected_targets()`. **The trap to remember:** an agent
> host *also* advertises `_pipewire-audio._udp` (its own receive session, under its
> bare hostname), so the registry looks plausibly populated while being useless as
> a key. `compute_desired` has a test now — it had none, which is why nothing
> caught this.

The static drop-in is **removed** from every host, including the author's own
Fedora machines (§9.3).

## 4. Architecture

```
  add-on (bridge-daemon)                        receiver host (user session)
  ┌───────────────────────────┐                 ┌──────────────────────────────┐
  │ outputs/pwsink/agent.rs   │   WS  (agent    │ pwrouter-agent               │
  │  · pairing + token store  │◄── dials out ───┤  · own pw_context (client)    │
  │  · per-host command chan  │   token auth)   │  · loads module-rtp-session   │
  │  · volume/mute registry   │                 │    into that context  ← §7    │
  │  · keepalive deadline     │                 │  · walks link → target sink   │
  └─────────┬─────────────────┘                 │  · channelVolumes get/set     │
            │ audio (unchanged)                 │  · restores state on exit     │
            └── AppleMIDI/RTP ─────────────────►└──────────────────────────────┘
```

The audio path does not change: `outputs/pwsink/server.rs` keeps advertising one
`pwrouter-<slug>` session per target and streaming L16 to it. The agent adds a
control plane — and takes over creating the receive side.

**Why a client-side module load works** (spike S1, §14): the agent runs its own
`pw_context` and loads `libpipewire-module-rtp-session` into it, exactly as
`bridge-daemon` loads `rtp-sink`/`rtp-source` into its own (`pw-control`'s module
FFI + `pw/thread.rs`). The nodes a module creates in a client context are ordinary
graph nodes, so WirePlumber routes the receive stream just as the drop-in did —
with the module's lifetime bound to the agent process instead of to the PipeWire
daemon's config. Verified: killing the agent removed every node the module had
created.

## 5. Wire protocol (v1)

JSON over WS, versioned with a `hello` handshake so an older agent against a newer
daemon degrades to the commands it knows (`pwrouter-agent/src/proto.rs`,
`PROTOCOL_VERSION`).

| Direction | Message | Notes |
|---|---|---|
| agent→daemon | `hello { protocol, agent_version, machine_id, hostname, user, token? }` | no token = a pair request; the daemon derives identity (`machine_id:user`), label and node name |
| daemon→agent | `pair_pending { code }` | shown by the agent so the approver can match it |
| daemon→agent | `paired { token }` / `denied { reason }` | approval mints the token |
| daemon→agent | `welcome { session_name, ifname?, jitter_ms?, keepalive_secs }` | become the receiver for that session (§7); also arms the §9.2 deadline |
| daemon→agent | `release` | stop receiving (target removed) — stays paired |
| daemon→agent | `set_volume { volume }` | cubic 0.0–1.0, HA's `volume_level` contract |
| daemon→agent | `set_mute { muted }` | |
| daemon→agent | `duck { depth, ramp_ms }` / `unduck { ramp_ms }` | *foreign* streams only |
| daemon→agent | `ping` / agent→daemon `pong` | keepalive |
| agent→daemon | `state { volume, muted, sink_name, receiving, ducked }` | pushed on *any* change, including local ones |
| agent→daemon | `foreign_session { session }` | another router is also received here (§7.1) |

### 5.1 Parameters, never a module-args string

`welcome` carries *parameters* (session name, optional interface, optional jitter
buffer); the agent builds the `rtp-session` argument string itself. Passing the
args through would hand whatever is on the other end of the socket the ability to
reconfigure the host's audio arbitrarily — precisely the property that
disqualified the PulseAudio TCP route in §1.1, so it must not reappear here.

The jitter buffer is **always sent** and is the daemon's per-output playout-delay
setting (`routing::sync_settings::pwsink_jitter`, default 100 ms): the daemon
decides what a target runs at, so the figure the UI shows and the figure the
receiver uses cannot disagree. Changing it re-sends `welcome`, and since the agent
reloads its receiver on every one by design (§13.4), retuning needed no new
message type and works against already-deployed agents.

Master volume is the sink our stream feeds (§6), so `set_volume` is deliberately
*not* parameterised by node — the agent decides which node that is, and the daemon
never learns the remote graph.

## 6. Master volume: which node, and which param

Do **not** read `default.audio.sink` metadata (pipewire-rs 0.10 *does* have a
typed Metadata proxy, so this is a choice, not a workaround): the default sink is
not necessarily the sink our stream landed in — the user can move it, and on a
multi-sink host they will. Follow the graph from the node the agent itself created:

1. the receive stream node — identified by `rtp.session` (§7.1), not by name;
2. the `Link` global whose `link.output.node` is that id;
3. its `link.input.node` is the sink our audio actually lands in — authoritative
   by construction, and it tracks the user moving our stream elsewhere.

Verified end to end on a live host (`pwsink-in` id 122 →
`alsa_output.pci-…analog-stereo` id 90).

### 6.1 Node `Props` is the wrong lever for a device sink

Writing `channelVolumes` on a node's SPA `Props` is right for the daemon's own
*virtual* sinks, and it is **not** the master volume of a real device sink. Three
observations from the spike:

- `wpctl` and the desktop applet read `Route.props.channelVolumes` on the
  **Device** object (verified: device 84, route index 3, device 4 → `0.045` =
  `0.36` cubic, exactly what `wpctl get-volume 90` reported);
- an agent write to node 90's `Props.channelVolumes` (`0.001`) was simultaneously
  visible in `pw-dump` *and* ignored by `wpctl`, with the node's `softVolumes`
  staying `[1.0, 1.0]` — so the write did not become applied gain either;
- and it is **transient**: a later read showed the node back at `0.368`, tracking
  the device volume, because WirePlumber mirrors the device route onto the node and
  reverts anything written there.

So master volume and mute set **`SPA_PARAM_Route` on the Device** (`index` +
`device` from the node's `card.profile.device` / `device.id` props, carrying
`channelVolumes` and `mute`). Mute arrives for free in the same pod, and it is the
value the user's own UI displays — which §9.4 depends on. Confirmed working:
setting 0.250 made `wpctl get-volume @DEFAULT_AUDIO_SINK@` read 0.25, and
restoring 0.370 read back 0.37. Node `Props` keeps exactly one job: **per-stream
duck of foreign playback streams**, where it is the correct and appropriately
invisible lever.

Cubic↔linear (`V³`, matching wpctl and HA's `volume_level`) is shared with the
daemon through the **`pw-control`** crate (§12). It lives *outside*
`bridge-daemon/` — a path dep inside the workspace root directory silently becomes
a workspace member, which the load-bearing `[workspace] exclude` comment in
`bridge-daemon/Cargo.toml` explains — and the `Dockerfile` copies it to
`/pw-control` so `../pw-control` resolves from `WORKDIR /build`, the same trick the
sendspin submodule uses.

## 7. Receiver configuration lives in the agent

What the drop-in used to do, now built by the agent from `welcome`'s parameters
(recorded because it is the only remaining copy of `90-pwsink-receiver.conf`):

```
libpipewire-module-rtp-session
  local.ifname   = <LAN iface>        # was hardcoded per host → auto-detected
  sess.media     = audio
  audio.format   = S16BE              # the sender byte-swaps to L16 BE
  audio.rate     = 48000
  audio.channels = 2
  audio.position = [ FL FR ]
  stream.props = {
    media.class      = "Stream/Output/Audio"
    node.name        = "pwsink-in"
    node.description = "pw-router sink"
    node.autoconnect = true
  }
```

Four improvements over the static version: **`local.ifname` auto-detected** (the
iface holding the route to the daemon); **session scoping** (§7.1, agent-side
because the module offers none); **a chosen output** (§7.3, where the drop-in could
only land in whatever the host's default sink happened to be); and **lifetime** —
unload on disconnect/exit means no stale receiver, and the module reloads with
fresh args whenever the daemon's session parameters change, with no user edit and
no PipeWire restart.

One warning to carry forward from the spike: `sess.latency.msec 100 should be an
integer multiple of rtp.ptime 6.458`. The jitter buffer wants to be a multiple of
ptime rather than left at the default — which is why the daemon clamps it to whole
packet times (`pipewire-sink-roadmap.md` §1).

### 7.1 Session scoping is the agent's job

Stock `module-rtp-session` cannot be scoped and cannot be pointed at a peer, read
from `module-rtp-session.c` (1.6.8):

- discovery compatibility is decided by **service type plus the TXT format
  fields** (`subtype`/`format`/`rate`/`channels`) in `make_service()` — the session
  *name* is never compared, so every router session on the LAN with our format is
  connected to;
- `sess.name` is the receiver's *own* advertised name, not a filter;
- `destination.ip`/`destination.port` are set internally from a resolved mDNS
  service before `make_session()` — there is no static-peer configuration.

That is the cross-talk problem in `outputs/pwsink/server.rs`, and it cannot be
fixed with module arguments. The usable hook: `make_session()` stamps every
session's streams with `rtp.session = <session name>`, so the agent identifies its
own session's nodes exactly and *could* tear down (or mute) a foreign router's
links. Today it ignores foreign sessions and reports them (`foreign_session`);
choosing teardown-vs-mute is still open (§13).

**Caveat:** `rtp.session` is *not* in the registry `global` event's reduced
property set — the agent must bind the node and read its info props, and must bind
**inside** the registry callback, because globals are emitted once per proxy (a
listener registered after the first roundtrip sees nothing).

### 7.2 The send-side twin (open defect)

`make_session()` creates **two** streams per session — a send stream (INPUT
direction) and the receive stream (OUTPUT) — and both inherit `stream.props`. The
`media.class = "Stream/Output/Audio"` needed so the receive stream is audible
rather than merely recordable is therefore also applied to the send half, which
logs `pw_stream_connect(): media.class Stream/Output/Audio does not expect Input
stream direction` plus an audioadapter format mismatch, and leaves a broken node in
the graph that WirePlumber even links to the sink. Harmless to audio — we never
send on this path — but it is graph litter and log spam, and it is why a host shows
*two* `pwsink-in` nodes.

Two ways out, still undecided (§13): tear down the bogus send-side link and ignore
the node; or drop the `media.class` override and have the agent create the link to
the target sink itself, which removes the need for `node.autoconnect` and gives
explicit control over which sink receives us.

### 7.3 Which output plays it — chosen on the machine, and pinned

Landing in the host's default sink is a poor default for the case this feature
exists for: a machine wired to the speakers in one room, whose desktop default
follows a headset, a dock or an HDMI monitor. So the tray offers a **Play to**
picker over the host's sinks, stored as `config.target_sink` (`node.name`, which
survives reboots and re-plugging where a node id does not). Three decisions worth
keeping:

- **It lives on the machine, not in the add-on.** The add-on decides *what* is
  routed to a host; only the person at that keyboard knows which of its own outputs
  they mean. Same channel as the Autostart switch: the tray reports a `Request`,
  `client::run` (which owns the config file and the PipeWire thread) stores it,
  applies it, and publishes back what was stored — so the menu cannot show a
  setting that was never written.
- **A pin never falls back.** `target.object` alone is only a *preference*: the
  session manager moves the stream to the default sink when the chosen one is
  missing, which is exactly the automatic switch a pin exists to prevent. So it
  always carries `node.dont-reconnect = true`, and an absent target means
  **silence** — audio in the wrong room is worse than no audio. The tray says so
  ("Chosen output … is not available — nothing is played") rather than letting the
  user read it as the add-on being broken.
- **Coming back is not automatic either.** `dont-reconnect` means the stream was
  destroyed rather than moved, so nothing reattaches on its own and the pin would
  decay into permanent silence the first time a USB interface was unplugged.
  `pw::thread::resync_pin` watches for the chosen sink appearing and reloads the
  module then — acting on the *transition* only, because reloading whenever a
  present-but-unattached pin is seen would spin on every graph change if the
  attach keeps failing.

`None` (follow the system default) stays on offer and remains the default: it is
what every host did before the picker and is still right for a laptop. The
`media.class`/send-twin interaction is worth an upstream report either way.

## 8. Pairing and discovery

1. The daemon advertises `_pwrouter-ctl._tcp` on the shared, LAN-restricted mDNS
   daemon (`supervisor.rs`, per the mDNS-storm fix).
2. `pwrouter-agent run` browses for it, connects, and sends `hello` **without** a
   token, carrying a **pairing code the agent mints once per process** and shows at
   startup. The daemon validates its shape (6 uppercase hex, `valid_pair_code`) and
   mints its own if it is missing or malformed — the code is a string a human
   compares across two screens, not an authenticator, so the agent may choose it,
   but a rogue agent must not get to write arbitrary text next to a host's name.
   (Earlier the daemon minted it per hello, which rotated it on every reconnect and
   left the code in the host's journal stale.)
3. **A host that has asked to pair is a discovered output.** The Outputs tab lists
   it under "Discovered devices" with every other kind, its card carrying the code,
   and pairing it is that card's **Add** (labelled *Pair*): `adopt_output` mints the
   token and adopts in one step, because a human ran the agent on that host and a
   human is clicking here. That means the node name has to be settled at `hello`,
   not at approval (`node_name_for_identity`), since the page keys every row by it —
   with a machine-id suffix when two hosts share both hostname and username, who
   would otherwise fold into one card, one routing row and one HA entity.
4. Approving mints a token; the agent stores it `0600` in
   `~/.config/pwrouter-agent/config.json` and reconnects with it, backing off on
   failure. Manual override (`--daemon host:port`) for routed/non-mDNS setups.
5. Paired hosts are ordinary output cards (level, mute, which sink, ducked — all
   host-reported) whose destructive action is **Unpair**: revoke the token, clear
   routing and group membership, un-adopt. One action, because "take this out of my
   outputs" and "stop trusting that machine" are not intentions a user has apart.
6. **No answer from the daemon ever ends an agent.** A denial — revoked token, a
   lost `agents.json`, a protocol bump — makes it drop the token it cannot use and
   keep dialling in, so the host returns as pairable on its own. It used to exit,
   which meant `Restart=always` respawning it every 5 s into the same refusal
   forever, and any add-on reinstall demanding a login on *every* receiver host to
   get the outputs back. Unpairing therefore behaves like un-adopting a speaker that
   is still on the network: it reappears under Discovered, and **Ignore** is how you
   put it away.

**Two wrong turns on the listing, both worth remembering:** polling every 5 s
re-rendered the card on every tick (which read as flickering), and then reacting to
*any* WebSocket frame fetched four times a second, because the matrix frame is also
the meter tick. The listing now does not poll at all — the daemon pokes its change
notifier on every pairing event, which is what pushes the output listings on the
routing WebSocket. (A typed `agents` frame exists too, mirroring `/api/agents`,
kept for diagnostics.)

### 8.1 Showing the code on the desktop, not only in the journal

`journalctl --user -u pwrouter-agent` is the right answer for a server and a poor
one for a desktop: the person who just installed the helper is sitting at the
machine that could simply show them the code. `src/desktop.rs` mirrors it into two
desktop surfaces:

- a **notification** when the daemon reports the request pending, carrying the code
  and the host label. Sent with `replaces_id` and suppressed for a code already
  announced, because an unpaired agent re-hellos on every backoff tick and gets
  `pair_pending` back each time — without both, the screen would flash the same
  banner every few seconds and stack a bubble per reconnect;
- a **status tray icon** (StatusNotifierItem via `ksni` — pure Rust on zbus, so the
  two-arch cross-build and its GLIBC floor are untouched) whose menu keeps the code
  readable afterwards, alongside the daemon address, target sink, level and duck
  state it already reports upstream.

It also carries the two settings that are genuinely local to that machine: **Play
to** (§7.3) and **Autostart**, a two-option radio that installs or removes the
systemd user unit through `autostart.rs` (§10) and reports back what systemd
actually says afterwards rather than what was clicked. The command line has the
same switch (`pwrouter-agent autostart [enable|disable]`) for headless hosts and
for the one case the tray cannot help with: a *sandboxed* agent whose unit predates
`ReadWritePaths=-%h/.config/systemd/user` cannot write the file, so the toggle
fails with a notification pointing at the terminal, where it works.

Three deliberate limits:

1. **Both are optional and neither is authoritative.** No session bus, no
   notification server, or no tray host each degrade to what existed before — the
   log line — and none of them can fail the agent. Which is also why the code still
   goes to the journal at `warn`.
2. **The menu is a display, plus the settings only this machine can answer.** Every
   status row is disabled; the only rows that do anything are Play to, Autostart and
   "show the notification again". No unpair and no quit: those would be a second,
   divergent way to manage a `Restart=always` unit, and a control surface competing
   with the add-on, which is the thing that is supposed to drive this host. The two
   settings are the exception because *nothing else can answer them*.
3. **Legacy icon names** (`audio-speakers`, `dialog-password`) resolve in both
   Breeze and Adwaita (via its `Inherits=AdwaitaLegacy`), whereas the two
   `-symbolic` spellings wanted here are not present in both themes.

Tray support is not universal — KDE, Xfce, Cinnamon, MATE and most WM bars
implement the spec, GNOME needs its AppIndicator extension — so `spike-desktop`
answers "does this session have either?" without involving the add-on, and
`assume_sni_available` is set so a user unit that starts before the shell waits for
the watcher instead of giving up.

## 9. Safety rails

The failure mode to design against is *"the user's desktop is silently stuck at
20 % or muted"*.

1. **Restore on exit**, including SIGTERM/SIGINT: original volume/mute restored,
   module unloaded.
2. **Keepalive deadline in the agent**: no `ping` within N seconds → auto-unduck
   and restore. A dead add-on, a rebooted Pi or a yanked cable cannot leave the host
   ducked. The *agent* enforces it, because a daemon-side timer cannot help when the
   daemon is the thing that died.
3. **State file** for the crash case: pre-duck volumes are written before the first
   attenuation and restored on next start if the file is stale.

### 9.3 Removing the static drop-ins

`~/.config/pipewire/pipewire.conf.d/90-pwsink-receiver.conf` is deleted on the
author's Fedora hosts (its content is recorded in §7, the only copy needed).
**Not touched:** `60-bt-bridge-rtp-source.conf` — that is the ESP32 BT-bridge
*input* into the desktop, a different feature, and folding it into the agent is a
separate decision (§13.3).

### 9.4 Never fight the user

Volume changes made *locally* (pavucontrol, volume keys) are pushed as `state`
events, so HA follows the host rather than overwriting it — the agent is not the
owner of the value, just a controller of it. This is why §6.1 matters: an agent
writing a lever the user's own UI cannot see would silently diverge from it.

## 10. Packaging

- **One binary per arch — x86_64 *and* aarch64**, both built regardless of the
  add-on's own architecture, since the machine on the other end is usually not the
  Pi. Dynamically linked against `libpipewire-0.3.so`: a static build is impossible
  by nature, because the agent *is* a PipeWire client and must use the library the
  host runs.
- Built in the `Dockerfile`'s **`agent` stage** from `rust:1-slim-bookworm`, not
  the trixie base the daemon uses. What reaches the user is not the builder's glibc
  but the symbol versions the binaries reference, and a modern toolchain floors at
  `GLIBC_2.34` — so bookworm buys the widest reach for free while still shipping
  PipeWire 0.3.65 headers (new enough for pipewire-rs 0.10). The stage **asserts**
  that floor with `objdump -T` and fails the build if a binary exceeds it, because
  the in-app help promises concrete minimums: **Ubuntu 22.04 LTS+, Fedora 35+,
  Debian 12+** (measured: both binaries top out at exactly `GLIBC_2.34`).
  Cross-building needed only the *foreign* architecture added to dpkg — adding the
  native one makes apt resolve unqualified package names against the wrong
  architecture.
- Copied to `/app/www/agent/` and **served by the add-on itself**, so the download
  in the help dialog needs no third-party fetch and always matches the daemon.
- **systemd user unit** (`~/.config/systemd/user/pwrouter-agent.service`), since it
  needs the user's session PipeWire. The unit is `include_str!`-ed into the binary
  and installed by `pwrouter-agent autostart enable` (or the tray switch) — which is
  what removed a `curl` of a raw.githubusercontent URL from the install
  instructions: two steps, one of them a download from a *third party*, for a file
  the binary already contains. `ExecStart` is rewritten to the installing binary's
  own path (`%h/…` when it is under the user's home), so the unit can never start a
  different copy than the one that wrote it, and enabling deliberately does **not**
  start: the installer is usually the running agent, and a second one in the same
  session would fight it over the volume. That the agent writes its own unit is also
  the one thing `ProtectHome=read-only` had to be relaxed for
  (`ReadWritePaths=-%h/.config/systemd/user`, `-` because the directory need not
  exist).

## 11. What shipped, in what order

| | Content | Outcome |
|---|---|---|
| **S1** | agent loads `rtp-session` in its own client context | ✅ module loads, session discovered, receive stream auto-linked, all nodes vanish on exit (§4) |
| **S1b** | session-name scoping for `rtp-session` | ✅ answered — impossible in the module, done agent-side via `rtp.session` (§7.1) |
| **S2** | link-walk to the target sink + volume get/set | ✅ walk + read verified; ❗ the write lever was wrong (§6.1) |
| **S2b** | `SPA_PARAM_Route` on the Device (volume + mute) | ✅ `wpctl` agrees with the agent exactly (§6.1) |
| **P1** | agent (config, pairing, reconnect, restore rails), the daemon side, master volume/mute through to the HA `media_player` | ✅ built; control plane verified live (§14.2) |
| **P2** | receiver config owned by the agent; targets sourced from paired agents (§3); drop-in deleted; systemd unit | ✅ built — but "targets sourced from paired agents" was only half done until 2026-08-05, so no pw-sink output could carry audio (§3) |
| **P3** | per-stream duck of *foreign* streams with a local ramp, wired to the announce path alongside `overlay_mixer` | ✅ built (`duck_output`/`unduck_output` from `announce/mod.rs`); not yet heard on real audio |
| **P4** | host-scoped extras: target a *named* sink, report xruns into the profiler badges | deferred |

P3's two ducks coexist: our own music in the stream is ducked by `overlay_mixer` on
the daemon side; the agent only touches *other* applications' streams on that sink,
never `pwsink-in`. The daemon re-asserts an **absolute** depth
(`OverlayMixer::effective_duck` via `announce::sync_agent_duck`) rather than
clearing the host outright, because the agent does no ref-counting and an
announcement finishing must not un-duck a room whose voice assistant is still
talking.

## 12. Files

| File | Role |
|---|---|
| `pwrouter-agent/` (own workspace root) | the helper: config, pairing, WS client, receiver module, volume, duck. Sibling of `bridge-daemon/`, deliberately **not** copied into the add-on image |
| `pwrouter-agent/src/proto.rs` | the agent↔daemon wire protocol (§5) |
| `pwrouter-agent/src/receiver.rs` | the `rtp-session` args replacing the drop-in (§7), with tests |
| `pwrouter-agent/src/pw_thread.rs` | the service path: graph tracking, master lever, duck ramps, `resync_pin` |
| `pwrouter-agent/src/volume.rs` | the diagnostic path (`spike-*`): snapshot, stream→sink walk, lever |
| `pwrouter-agent/src/desktop.rs` | tray icon + pairing notification (§8.1); status is display-only, the settings are Play to and Autostart |
| `pwrouter-agent/src/autostart.rs` | the embedded systemd unit and the enable/disable pair behind both the tray switch and the CLI (§10) |
| `pw-control/` (own workspace root) | shared with the daemon: volume/route pods, the cubic scale, and the `pw_context_load_module` FFI neither `pipewire-rs` wraps nor either side should duplicate |
| `bridge-daemon/src/outputs/pwsink/agent.rs` | daemon side: WS endpoint, token store, per-host command channel, keepalive |
| `bridge-daemon/src/outputs/pwsink/discovery.rs` | diagnostic only, read by nothing (§3) |
| `bridge-daemon/src/outputs/pwsink/server.rs` | the unchanged audio path |
| `frontend/src/components/OutputsTab.svelte` | the pairing UI: a host waiting to pair is a discovered output card carrying the code, and Add/Unpair are its decisions |
| `custom_components/pipewire_audio_router/media_player.py` | `VOLUME_SET`/`VOLUME_MUTE` for pw-sink outputs with a connected agent |

## 13. Decisions and deferrals

1. **Session-name filtering** — impossible in the module; done agent-side via
   `rtp.session` (§7.1). *Deferred:* actively tearing down a foreign router's
   links, so cross-talk with two routers on one LAN still exists and the
   teardown-vs-mute choice stays open (§7.2 shares that decision).
2. **Multi-user hosts — one agent per logged-in session.** The config lives in the
   user's `~/.config` and the pairing identity is *machine + user*, so two users on
   one host pair as two independent targets. A system-wide agent is not built.
3. **The BT-bridge RTP source is not the agent's business** — that drop-in was a
   test fixture. The agent owns only the pw-sink receive side.
4. **Sleep/resume** — to be determined empirically during normal use; no
   self-inflicted suspend cycles. The agent already reloads the receiver module on
   every reconnect, which is the expected remedy if a resumed session comes back
   mute.

Still deferred: the send-twin cleanup / self-created link (§7.2), which includes
"which sink" control; and §11's P4 host-scoped extras.

## 14. What was verified, and what still is not

### 14.1 The spikes (2026-08-03)

Run against a live desktop (PipeWire 1.6.8) with the add-on advertising
`pwrouter-david_local`. The findings are folded into §4, §6.1, §7.1 and §7.2 —
those four sections *are* the results. The one number worth repeating: the receive
stream came up as id 122 with `rtp.session=pwrouter-david_local`, linked to sink id
90 at cubic volume 0.446, and setting 0.250 via the device `Route` made `wpctl`
read 0.25.

### 14.2 Control plane, verified live (2026-08-03)

A daemon built from this tree plus the real agent, both on the author's desktop:

- **pairing** — tokenless `hello` → pending row with code `F89CFE`, the *same* code
  in the agent's log and in `GET /api/agents`; approving minted a token, the agent
  persisted it `0600`, reconnected with it, and was welcomed as
  `pwsink-dev-david_local_david`. (Verified against the then-current
  `POST /api/agents/approve`; approval is now `POST /api/outputs/{node}/adopt` and
  the code comes from the agent — §8, so this half wants re-verifying.)
- **receiver ownership** — on `welcome` the agent loaded `rtp-session` for its own
  session name; the host went from 2 `pwsink-in` nodes (the stale in-daemon module)
  to 4, and back to 2 on SIGTERM. Module lifetime really is process lifetime.
- **cross-talk detection** — the agent noticed the *other* session on the host and
  reported it without touching it, exactly as §7.1 specifies. This also proves the
  bound-node prop reading works on the service path, not just in the spike.
- **commands** — `PUT /api/pwsink/volume|mute` reached the agent (which logged the
  attempt and why it could not apply it: the host was not receiving *its* session,
  only the production one), and an unknown host returns `503` rather than pretending.
- **adoption gate** — the host appears under `/api/outputs/discovered`, and only
  after `adopt` in `/api/outputs`, with `present: true` while the agent is connected
  and `false` after it exits. Volume/mute are omitted while unknown rather than
  fabricated.
- **HA** — `media_player` gains `VOLUME_SET`/`VOLUME_MUTE` for `pwsink-dev-*`,
  reads the host-reported level, and routes both through `/api/pwsink/*`.

### 14.3 Not yet verified live

- **The join between the two verified halves**: an agent controlling the master
  volume of the sink *its own* session lands in. It needs a daemon that actually
  advertises a session to this agent, i.e. a deployed add-on — the local test daemon
  had no routing. Both halves are proven separately with the same lever code, and
  the graph-tracking half of the service path is proven by the cross-talk detection
  above, so what remains untested is `pw_thread`'s `target_sink()` resolution on a
  live session. **First thing to check after deploying.**
- **An audible duck of foreign streams** (P3), and **sleep/resume** (§13.4) — both
  need normal day-to-day use rather than a test rig.
