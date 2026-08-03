# `pwrouter-agent` — receiver-side helper for pw-sink targets

Status: **implemented** (2026-08-03) — P1-P3 built, control plane verified live
(§14.2); deferrals listed in §13. Supersedes the static receiver drop-in described
in [pipewire-sink-roadmap.md](pipewire-sink-roadmap.md) §10.

Code: `pwrouter-agent/` (the helper) and `bridge-daemon/src/pwsink_agent.rs` (the
daemon side).

## 1. Why

The pw-sink output type streams to a remote PipeWire host and works, but two
things are unsatisfying:

* **No volume/mute.** A pw-sink target is a *virtual* output with no local node
  to attenuate, and the transport (RTP + AppleMIDI) carries no control channel —
  PipeWire's RTP modules implement no RTCP feedback either (roadmap §4). So the
  HA `media_player` has no slider, and an announcement cannot duck whatever the
  remote host is playing on its own.
* **Static config on the receiver.** Every target host needs a hand-written
  `pipewire.conf.d/` drop-in loading `libpipewire-module-rtp-session`. That is a
  documentation-and-copy-paste step per host, it does not survive a re-install,
  and it is impossible to reconcile from the add-on.

### 1.1 Why not native remote control (investigated 2026-08-03)

PipeWire has no network-reachable control protocol:
`libpipewire-module-protocol-native` describes itself as *"Native protocol using
unix sockets"* and accepts only a `sockets` argument, so there is no remote
`pw-cli`, and no remote `pipewire-rs` core connection.

The one native remote surface is PipeWire's **PulseAudio server over TCP**
(`module-protocol-pulse` implements `module-native-protocol-tcp` and
`module-zeroconf-publish`). It was rejected after reading the implementation and
testing it live on a 1.6.8 host:

* **The pulse cookie is not a preshared key here.** `do_command_auth`
  (`module-protocol-pulse/pulse-server.c:114`) checks only that the cookie blob
  is `NATIVE_COOKIE_LENGTH` bytes and then sets `client->authenticated = true`;
  the bytes are never compared. Verified live: a random 256-byte cookie and the
  host's real `~/.config/pulse/cookie` behave identically. `auth-anonymous` is
  therefore near-meaningless — there is nothing to switch off.
* **Access is per-listen-address, not per-client.** TCP clients default to
  `client.access = "restricted"` (`module-protocol-pulse/server.c:511`), which is
  read-only in practice (verified: `pactl get-sink-volume` works,
  `set-sink-mute` → Access denied). Getting control requires
  `client.access = "unrestricted"` on that address, which grants it to *every*
  client that can reach the port — including microphone capture. No per-client
  credential exists anywhere in the path.

So security would have to live in the transport (firewall/WireGuard), and an
SSH-forward variant was rejected too: it can be locked down with a forced
command and `permitopen`, but the credential a user is asked to install still
reads as *shell access to my desktop*, which is no easier to accept than an open
audio-control port.

### 1.2 What the helper buys instead

A helper on the receiver is **least privilege by construction** — it accepts a
fixed command set (volume, mute, later duck) and nothing else — and it can do
things no remote protocol can: configure the receive side itself, ramp a duck
locally with no network jitter, and exclude our own stream from a duck.

## 2. Trust model

* **The helper dials out.** Nothing listens on the receiver: no open port, no
  firewall rule, no `unrestricted` access level, nothing for a LAN scan to find.
  One outbound WebSocket to the add-on.
* **Narrow command set.** The wire protocol is an enum (§5), not a passthrough.
  A stolen token buys "change the volume on that host", not audio capture and not
  a shell.
* **Bearer token from an explicit pairing** (§8), stored `0600` in the helper's
  config. Consistent with how HA add-ons authenticate elsewhere; upgrade path to
  per-message HMAC exists but is not in v1 given the blast radius.

## 3. Mandatory, not optional

A pw-sink target **is** an agent-managed host: no agent, no target. Discovery
therefore changes from "browse `_pipewire-audio._udp`" (which detects a
*statically configured* receiver) to "an agent has paired and is connected",
which is a stronger and more honest signal — it also finally answers roadmap §4's
"configured vs. connected" question, and makes the liveness poll in
`pw_sink_liveness.rs` a second-order detail rather than the only signal.

Consequences, accepted:

* Existing hosts stop working until their agent is installed. There is no
  migration path and none is wanted (same "start clean" stance as the output
  adoption gate).
* `pw_target_discovery.rs` loses its role as the source of truth for targets; it
  survives at most as a diagnostic ("host advertises a session but has no
  agent → tell the user to install it").
* The static drop-in is **removed** from every host, including the author's own
  Fedora machines (§9.3).

## 4. Architecture

```
  add-on (bridge-daemon)                        receiver host (user session)
  ┌───────────────────────────┐                 ┌──────────────────────────────┐
  │ pwsink_agent.rs           │   WS  (agent    │ pwrouter-agent               │
  │  · pairing + token store  │◄── dials out ───┤  · own pw_context (client)    │
  │  · per-host command chan  │   token auth)   │  · loads module-rtp-session   │
  │  · volume/mute registry   │                 │    into that context  ← §7    │
  │  · keepalive deadline     │                 │  · walks link → target sink   │
  └─────────┬─────────────────┘                 │  · channelVolumes get/set     │
            │ audio (unchanged)                 │  · restores state on exit     │
            └── AppleMIDI/RTP ─────────────────►└──────────────────────────────┘
```

The audio path does not change: `pwsink_server.rs` keeps advertising one
`pwrouter-<slug>` session per target and streaming L16 to it. The agent only adds
a control plane — and takes over creating the receive side.

**Why a client-side module load works** (spike S1): the agent runs its own
`pw_context` and loads `libpipewire-module-rtp-session` into it, exactly as
`bridge-daemon` already loads `rtp-sink`/`rtp-source` into its own context
(`pw_module.rs` + `pw_thread.rs`). The nodes a module creates in a client context
are ordinary graph nodes, so WirePlumber routes the receive stream to the default
sink just as the drop-in does — with the module's lifetime bound to the agent
process instead of to the PipeWire daemon's config.

## 5. Wire protocol (v1)

JSON over WS, daemon→agent commands and agent→daemon events. Versioned with a
`hello` handshake so an older agent against a newer daemon degrades to the
commands it knows.

| Direction | Message | Notes |
|---|---|---|
| agent→daemon | `hello { protocol, agent_version, machine_id, hostname, user, token? }` | no token = a pair request; the daemon derives identity (`machine_id:user`), label and node name |
| daemon→agent | `pair_pending { code }` | logged by the agent so the approver can match it |
| daemon→agent | `paired { token }` / `denied { reason }` | approval mints the token; a denial ends the agent's retry loop |
| daemon→agent | `welcome { session_name, ifname?, jitter_ms?, keepalive_secs }` | become the receiver for that session (§7); also arms the §9.2 deadline |
| daemon→agent | `release` | stop receiving (target removed) — stays paired |
| daemon→agent | `set_volume { volume }` | cubic 0.0-1.0, HA's `volume_level` contract |
| daemon→agent | `set_mute { muted }` | |
| daemon→agent | `duck { depth, ramp_ms }` / `unduck { ramp_ms }` | *foreign* streams only |
| daemon→agent | `ping` / agent→daemon `pong` | keepalive |
| agent→daemon | `state { volume, muted, sink_name, receiving, ducked }` | pushed on *any* change, including local ones |
| agent→daemon | `foreign_session { session }` | another router is also received here (§7.1) |

### 5.1 Parameters, never a module-args string

`welcome` carries *parameters* (session name, optional interface, optional jitter
buffer); the agent builds the `rtp-session` argument string itself. Passing the
args through would hand whatever is on the other end of the socket the ability to
reconfigure the host's audio arbitrarily — which is precisely the property that
disqualified the PulseAudio TCP route in §1.1, so it must not reappear here.

Master volume is the sink our stream feeds (§6), so `set_volume` is deliberately
*not* parameterised by node — the agent decides which node that is, and the
daemon never learns the remote graph.

## 6. Master volume: which node, and which param

Do **not** read `default.audio.sink` metadata (pipewire-rs 0.10 *does* have a
typed Metadata proxy, so this is a choice, not a workaround): the default sink is
not necessarily the sink our stream landed in — the user can move it, and on a
multi-sink host they will. Follow the graph from the node the agent itself created
instead:

1. the receive stream node — identified by `rtp.session` (§7.1), not by name;
2. find the `Link` global whose `link.output.node` is that id;
3. its `link.input.node` is the sink our audio actually lands in — authoritative
   by construction, and it tracks the user moving our stream to another device.

**S2 verified** the walk end-to-end on a live host (`pwsink-in` id 122 →
`alsa_output.pci-…analog-stereo` id 90).

### 6.1 Node `Props` is the wrong lever for a device sink

`bridge-daemon/src/volume.rs` writes `channelVolumes` on a node's SPA `Props`,
which is right for the daemon's *virtual* sinks — but the S2 spike showed it is
**not** the master volume of a real device sink:

* `wpctl`/the desktop applet read `Route.props.channelVolumes` on the **Device**
  object (verified: device 84, route index 3, device 4 → `0.045` = `0.36` cubic,
  exactly what `wpctl get-volume 90` reported);
* an agent write to node 90's `Props.channelVolumes` (`0.001`) was simultaneously
  visible in `pw-dump` *and* ignored by `wpctl`, with the node's `softVolumes`
  staying `[1.0, 1.0]` — i.e. the write did not become applied gain either;
* and it is **transient**: a later read showed the node back at `0.368`, i.e.
  tracking the device volume (`wpctl` `0.36`), so WirePlumber mirrors the device
  route onto the node and reverts anything the agent writes there.

So **S2b**: master volume/mute must set `SPA_PARAM_Route` on the device
(`index` + `device` from the node's `card.profile.device` / `device.id` props,
carrying `channelVolumes` and `mute`). That is also how mute arrives for free,
and it is the value the user's own UI displays — which the §9.4 "never fight the
user" requirement depends on.

Cubic↔linear (`V³`, matching wpctl and HA's `volume_level`) is unchanged and
stays shared. P1 factors it into a small **`pw-control`** crate used by daemon and
agent. It must live *outside* `bridge-daemon/` — a path dep inside the workspace
root directory silently becomes a workspace member (the load-bearing
`[workspace] exclude` comment in `bridge-daemon/Cargo.toml`).

Node `Props` keeps one job: **per-stream duck** (P3) of *foreign* playback
streams, where it is the correct and appropriately invisible lever — it does not
disturb the user's device slider.

## 7. Receiver configuration moves into the agent

The agent reproduces what the drop-in did, from `welcome.receiver_args`. Recorded
reference (the drop-in this replaces, `90-pwsink-receiver.conf`):

```
libpipewire-module-rtp-session
  local.ifname   = <LAN iface>        # was hardcoded enp5s0 → auto-detect
  sess.media     = audio
  audio.format   = S16BE              # applemidi_sender byte-swaps to L16 BE
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

Three improvements over the static version:

* **`local.ifname` auto-detected** (the iface holding the route to the daemon)
  rather than hardcoded per host.
* **Session scoping** — see §7.1; the mechanism is agent-side, because the module
  offers none.
* **Lifetime.** Unload on disconnect/exit → no stale receiver, and the module
  reloads with fresh args when the daemon's session parameters change (no user
  edit, no PipeWire restart). Verified in S1: killing the agent removed every node
  the module had created.

### 7.1 Session scoping is the agent's job (S1b, answered)

Stock `module-rtp-session` cannot be scoped, and cannot be pointed at a peer:

* discovery compatibility is decided by **service type plus the TXT format
  fields** (`subtype`/`format`/`rate`/`channels`) in `make_service()` — the
  session *name* is never compared, so every router session on the LAN with our
  format is connected to;
* `sess.name` is the receiver's *own* advertised name, not a filter;
* `destination.ip`/`destination.port` are set internally from a resolved mDNS
  service before `make_session()` — there is no static-peer configuration.

That is the deferred cross-talk problem in `pwsink_server.rs` ("Deferred:
multi-target routing scoping"), and it cannot be fixed with module arguments.
What the agent *can* use: `make_session()` stamps every session's streams with
`rtp.session = <session name>`, so the agent identifies its own session's nodes
exactly and can tear down the links of a foreign router's session (or mute them).
Scoping therefore becomes an agent behaviour, with no PipeWire patch.

**Caveat found in S2:** `rtp.session` is *not* in the registry `global` event's
reduced property set — the agent must bind the node and read its info props, and
must bind **inside** the registry callback, because globals are emitted once per
proxy (a listener registered after the first roundtrip sees nothing).

### 7.2 The send-side twin (open defect, found in S1)

`make_session()` creates **two** streams per session — a send stream (INPUT
direction) and the receive stream (OUTPUT) — and both inherit `stream.props`. The
drop-in's `media.class = "Stream/Output/Audio"` (needed so the receive stream is
audible rather than merely recordable) is therefore also applied to the send half,
which logs

```
pw_stream_connect(): media.class Stream/Output/Audio does not expect Input stream direction
spa.audioadapter: unmatched follower format / no matching params
```

and leaves a broken node in the graph that WirePlumber even links to the sink
(observed: `53:66 -> 90:99` alongside the real `65:118 -> 90:98`). Harmless to
audio — we never send on this path — but it is graph litter and log spam, and it
is why a host shows *two* `pwsink-in` nodes.

Options for the agent (decide in P2): tear down the bogus send-side link and
ignore the node; or drop the `media.class` override and have the agent create the
link to the target sink itself, which removes the need for `node.autoconnect` and
gives explicit control over *which* sink receives us (§11 P4 wants that anyway).
Worth an upstream report either way.

## 8. Pairing and discovery

1. Daemon advertises `_pwrouter-ctl._tcp` over the shared storm-safe mDNS daemon
   (`discovery_supervisor.rs`, LAN-restricted per the mDNS-storm fix).
2. `pwrouter-agent pair` browses for it, connects, sends `hello`.
3. The add-on UI shows *"david-local wants to pair — Approve"*; approving mints a
   token which the agent stores in `~/.config/pwrouter-agent/config.json` (0600).
4. Thereafter the agent reconnects on its own with the token, backing off on
   failure. Manual override (`--daemon host:port`) for routed/non-mDNS setups.

## 9. Safety rails

The failure mode to design against is *"the user's desktop is silently stuck at
20 % or muted"*.

1. **Restore on exit**, including SIGTERM/SIGINT: original volume/mute restored,
   module unloaded.
2. **Keepalive deadline in the agent**: no `ping` within N seconds → auto-unduck
   and restore. A dead add-on, a rebooted Pi or a yanked cable cannot leave the
   host ducked. (The *agent* enforces it; a daemon-side timer cannot help when
   the daemon is the thing that died.)
3. **State file** for the crash case: pre-duck volumes are written before the
   first attenuation and restored on next start if the file is stale.
### 9.4 Never fight the user

Volume changes made *locally* (pavucontrol, volume keys) are pushed as `state`
events, so HA follows the host rather than overwriting it — the agent is not the
owner of the value, just a controller of it. This is why §6.1 matters: if the
agent wrote a lever the user's own UI cannot see, the two would silently diverge.

### 9.3 Removing the static drop-ins

`~/.config/pipewire/pipewire.conf.d/90-pwsink-receiver.conf` is deleted on the
author's Fedora hosts once S1 proves the dynamic path (its content is recorded in
§7 above, which is the only copy needed). **Not touched:**
`60-bt-bridge-rtp-source.conf` — that is the ESP32 BT-bridge *input* into the
desktop, a different feature; folding it into the agent is a separate decision.

## 10. Packaging

* One binary per arch (x86_64, aarch64), dynamically linked against
  `libpipewire-0.3.so` — present by definition on any target host.
* **systemd user unit** (`~/.config/systemd/user/pwrouter-agent.service`), since
  it needs the user's session PipeWire. `systemctl --user enable --now`.
* Served **from the add-on's own frontend** ("download the helper for this
  host"), so there is no third-party fetch to trust and the version always
  matches the daemon.

## 11. Phases

| Phase | Content | Exit criterion |
|---|---|---|
| **S1** (spike) | agent loads `rtp-session` in its own client context | ✅ done — module loads, session discovered, receive stream auto-linked to the default sink, all nodes vanish on exit (§14) |
| **S1b** (spike) | session-name scoping for `rtp-session` | ✅ answered — impossible in the module, done agent-side via `rtp.session` (§7.1) |
| **S2** (spike) | link-walk to the target sink + volume get/set | ✅ walk + read verified; ❗ write must use the device `Route` param, not node `Props` (§6.1) |
| **S2b** (spike) | `SPA_PARAM_Route` set on the Device (volume + mute), read-back observed by `wpctl` | `wpctl get-volume` reflects an agent-set value; local changes read back |
| **P1** | agent (config, pairing, reconnect, restore rails); `pwsink_agent.rs`; master volume/mute through to the HA `media_player` | ✅ built; control plane verified live (§14.2). `pw-control` extraction deferred (§13) |
| **P2** | receiver config owned by the agent; targets sourced from paired agents (§3); drop-in deleted; systemd unit | ✅ built; adoption gate verified live. Serving the binary from the add-on frontend deferred (needs a cross-arch build stage) |
| **P3** | per-stream duck of *foreign* streams with local ramp, wired to the announce path alongside `overlay_mixer` | ✅ built (`duck_output`/`unduck_output` from `announce.rs`); not yet heard on real audio |
| **P4** | host-scoped extras: report sinks (target a *named* sink), report xruns into the profiler badges | deferred |

Note P3's two ducks coexist: our own music in the stream is ducked by
`overlay_mixer` on the daemon side as today; the agent only touches *other*
applications' streams on that sink, never `pwsink-in`.

## 12. Files

| File | Role |
|---|---|
| `pwrouter-agent/` (new crate, own workspace root) | the helper: config, WS client, module load, volume. Sibling of `bridge-daemon/`, deliberately **not** copied into the add-on image by `Dockerfile` (P2 adds a build stage so the frontend can serve it) |
| `pwrouter-agent/src/receiver.rs` | the `rtp-session` args replacing the drop-in (§7), with tests |
| `pwrouter-agent/src/volume.rs` | graph snapshot + stream→sink walk + `channelVolumes` (S2b adds `Route`) |
| `pwrouter-agent/src/pw_module.rs` | copy of the daemon's module-load FFI until `pw-control` exists |
| `pw-control/` (new crate, P1) | `channelVolumes` get/set + cubic scale, shared with the daemon |
| `bridge-daemon/src/pwsink_agent.rs` (new) | daemon side: WS endpoint, token store, per-host command channel, keepalive |
| `bridge-daemon/src/volume.rs` | source of the shared volume code (moves to `pw-control`) |
| `bridge-daemon/src/pw_module.rs` | the client-context module-load FFI the agent reuses |
| `bridge-daemon/src/pw_target_discovery.rs` | demoted to diagnostic (§3) |
| `bridge-daemon/src/pwsink_server.rs` | unchanged audio path; gains session-scoping once S1b lands |
| `custom_components/pipewire_audio_router/media_player.py` | `VOLUME_SET`/`VOLUME_MUTE` for pw-sink outputs with a connected agent |

## 13. Decisions and deferrals

Answered 2026-08-03:

1. **Session-name filtering** — impossible in the module; done agent-side via
   `rtp.session` (§7.1). *Deferred:* actively tearing down a foreign router's
   links. The agent ignores foreign sessions and logs them; cross-talk with two
   routers on one LAN therefore still exists, and choosing teardown-vs-mute stays
   open (§7.2 shares that decision).
2. **Multi-user hosts** — **one agent per logged-in session.** The config lives in
   the user's `~/.config`, and the pairing identity is *machine + user*, so two
   users on one host pair as two independent targets. A system-wide agent is not
   built.
3. **BT-bridge RTP source** — **not** the agent's business; that drop-in was a test
   fixture. The agent owns only the pw-sink receive side.
4. **Sleep/resume** — to be determined empirically on the author's own host during
   normal use; no self-inflicted suspend cycles. The agent already reloads the
   receiver module on every reconnect, which is the expected remedy if a resumed
   session comes back mute.

Still deferred (open design questions, deliberately not implemented):

* the send-twin cleanup / self-created link (§7.2) — includes "which sink" control;
* extracting the shared `pw-control` crate: `Dockerfile` copies only
  `bridge-daemon/` into the build context, so a path dep one level up needs a
  build-context change to that and to `scripts/build-daemon.sh`. Until then the
  agent carries a marked copy of the pod/volume code (same convention as
  `pw_module.rs`);
* §11 P4 host-scoped extras (named-sink targeting, xrun reporting).

## 14. Spike results

Run 2026-08-03 against `david-local` (PipeWire 1.6.8) with the add-on advertising
`pwrouter-david_local` at `192.168.178.22:6200`. Spike code:
`pwrouter-agent/` (`spike-receiver`, `spike-volume`).

### S1 — receiver with no config file: **works**

```
loading libpipewire-module-rtp-session with args:
  { local.ifname = "enp5s0" sess.media = "audio" audio.format = "S16BE" … }
mod.rtp-session: create session: pwrouter-david_local 192.168.178.22:6200 _pipewire-audio._udp
node  id=53  name=pwsink-in-spike  class=Stream/Output/Audio     (send twin, §7.2)
node  id=65  name=pwsink-in-spike  class=Stream/Output/Audio     (receive stream)
link  id=133 65:118 -> 90:98                                    (auto-routed to the default sink)
link  id=134 65:132 -> 90:99
```

* A **client-context** module load behaves like a daemon-context one: the nodes are
  ordinary graph nodes and WirePlumber auto-routes them. No drop-in, no PipeWire
  restart, no root.
* Process exit (SIGTERM) removed every `pwsink-in-spike` node — module lifetime is
  process lifetime, which is what makes §9's rails workable.
* Two warnings to carry forward: the send-twin defect (§7.2), and
  `sess.latency.msec 100 should be an integer multiple of rtp.ptime 6.458` — the
  receiver jitter buffer should be set to a multiple of ptime rather than left at
  the default.
* Not yet verified: *audible* audio end-to-end, because the host was still running
  the static receiver in parallel. Re-verify after the next PipeWire restart, when
  the removed drop-in (§9.3) actually stops being loaded.

### S1b — session scoping: **not possible in the module**, agent-side instead

Source-read of `module-rtp-session.c` (1.6.8) plus the observed behaviour: see
§7.1. The usable hook is the per-session `rtp.session` property.

### S2 — master volume: walk **works**, write lever **was wrong**

```
receive stream: id=122 name=pwsink-in rtp.session=pwrouter-david_local
target sink:    id=90 name=alsa_output.pci-0000_0a_00.4.analog-stereo class=Audio/Sink
volume:         0.446 (cubic, = wpctl/HA scale)
```

* Stream→sink walk and `rtp.session` identification both verified.
* Node-`Props` writes were visible in `pw-dump` but invisible to `wpctl`, and
  `softVolumes` stayed `1.0` → the device `Route` param is the real master volume
  (§6.1). Follow-up: **S2b**.
* Registry `global` props are a reduced set; `rtp.session` needs a bound node, and
  the bind must happen inside the registry callback (§7.1 caveat).

### S2b — the device `Route` lever: **works**

```
lever:          device Route (index=3, device=4, 2ch)
volume:         0.368 (cubic)  [unmuted]
set volume to   0.250  →  wpctl get-volume @DEFAULT_AUDIO_SINK@ = 0.25
restored 0.370  →  wpctl = 0.37
```

`wpctl` now agrees with the agent exactly, which the node-`Props` path never did
(§6.1). Mute rides along in the same pod.

## 14.2 Control plane, verified live (2026-08-03)

A daemon built from this tree on `127.0.0.1:8099` plus the real agent, both on the
author's desktop:

* **pairing** — tokenless `Hello` → pending row with code `F89CFE`, the *same*
  code in the agent's log and in `GET /api/agents`; `POST /api/agents/approve`
  minted a token, the agent persisted it `0600` and reconnected with it, and was
  welcomed as `pwsink-dev-david_local_david`;
* **receiver ownership** — on `Welcome` the agent loaded `rtp-session` for its own
  session name; the host went from 2 `pwsink-in` nodes (the stale in-daemon module)
  to 4, and back to 2 on `SIGTERM`. Module lifetime really is process lifetime;
* **cross-talk detection** — the agent noticed the *other* session on the host
  (`pwrouter-david_local`, from the production add-on) and reported it without
  touching it, exactly as §7.1 specifies. This also proves the bound-node prop
  reading works on the service path, not just in the spike;
* **commands** — `PUT /api/pwsink/volume|mute` reached the agent (it logged the
  attempt and why it could not apply it: this host was not receiving *its* session,
  only the production one), and an unknown host returns `503` rather than pretending;
* **adoption gate** — the paired host appears under `/api/outputs/discovered` as
  `discovered`, and only after `adopt` in `/api/outputs`, with
  `present: true` while the agent is connected and `present: false` after it exits.
  Volume/mute are omitted while unknown rather than fabricated;
* **HA** — `media_player` gains `VOLUME_SET`/`VOLUME_MUTE` for `pwsink-dev-*`,
  reads the host-reported level, and routes both through `/api/pwsink/*`
  (3 new tests; 29 pass in `test_media_player.py`).

### Not yet verified live

The **join** between the two verified halves: an agent controlling the master
volume of the sink *its own* session lands in. It needs a daemon that actually
advertises a session to this agent, i.e. a deployed add-on — the local test daemon
had no routing. Both halves are proven separately with the same lever code
(§14/S2b), and the graph-tracking half of the service path is proven by the
cross-talk detection above, so what remains untested is `pw_thread`'s
`target_sink()` resolution on a live session. First thing to check after deploying.

Also unverified: an audible duck of foreign streams (P3) and the sleep/resume
behaviour (§13.4), both of which need normal day-to-day use rather than a test rig.

### Host state after the spikes

* `~/.config/pipewire/pipewire.conf.d/90-pwsink-receiver.conf` **removed** (§9.3).
  The running PipeWire still has the module loaded from before the removal, so
  pw-sink audio to this host keeps working until the next PipeWire restart or
  login — after which the agent is the only receiver path.
* `60-bt-bridge-rtp-source.conf` deliberately untouched.
* Node 90's `Props.channelVolumes` was restored to its pre-spike value, and has
  since re-synced to the device volume by itself (§6.1) — the user-visible device
  volume was never written.
* A `module-native-protocol-tcp` loaded on loopback for the §1.1 pulse-auth test
  was unloaded again.
* The §14.2 test rig is gone: the local daemon and agent processes were stopped,
  their `/data` lived in a scratch directory, and the desktop's volume is back at
  the 0.37 the *user* set. The agent is **not** installed as a service on this host
  yet (no systemd unit enabled).
