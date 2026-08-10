# YouTube Music cast receiver on the Pi bridge — plan

Make the **Cast button in the YouTube Music phone app** target the house audio system,
with the phone's own transport controls (play/pause/skip/seek/volume) driving playback —
by running a **DIAL + Lounge** receiver on the existing [Raspberry Pi
bridge](../firmware/pi-bridge/README.md) and feeding its audio into the router over
the RTP transport that is already there.

**Scope: personal install only.** This is deliberately *not* an add-on feature. It
depends on unofficial protocols and on `yt-dlp` keeping up with YouTube's anti-bot
measures, so it needs continuous maintenance and is against YouTube's ToS to ship. It
lives in `firmware/`, is separately installable, and the add-on stays unaware of it —
from the router's point of view this is just another RTP source.

Read the add-on's
[`architecture.md`](../pipewire_audio_router/docs/architecture.md) §3 (*Sources — audio
into the graph*) first if the routing side is unfamiliar; project-level context is in
[`system-architecture.md`](system-architecture.md). Design rationale that outlives this
plan graduates into [`decisions.md`](decisions.md), next to the existing *Raspberry Pi
Bluetooth → RTP bridge* entry.

---

## Status

**WP0 PASSED, 2026-08-10.** The YouTube Music app casts to a `yt-cast-receiver` DIAL
receiver, and the demo `FakePlayer` receives the full transport control set — the
interactive UI shows the exact track info and playback position the app is driving. The
premise this whole plan rests on is confirmed on the real sender.

See the WP0 findings below before re-running the spike after any future breakage — they
were measured on this LAN, and are easy to re-trip.

**WP1 + WP2 + WP3 DONE and deployed 2026-08-10.** The receiver is live on the Pi as
`ytmusic-receiver.service`, its DIAL surface answers correctly, and the resolver is a
self-updating yt-dlp 2026.07.04 + yt-dlp-ejs in a venv, proven to resolve `251 opus 48000Hz`
and to carry real YouTube audio into the add-on over RTP; install/verify recipes are in
[`firmware/pi-ytmusic/README.md`](../firmware/pi-ytmusic/README.md).

**The cookie jar is provisioned** (login cookies valid to 2027-09-14, `0600`, writable),
**authenticated resolution works on the Pi**, and **a live cast from the phone reached the
player** — the Lounge log shows real `Player.play()` calls with track ids and positions, so
the DIAL/Lounge half is proven end to end on the real sender.

Three defects surfaced only under that live cast, all fixed: mpv 0.40's `loadfile` argument
positions, `--no-terminal` discarding every mpv diagnostic, and a deploy step that copied a
hardcoded file list (so a newly added module crash-looped the service). Track changes now
cost **0.5 s** via prefetch instead of 22–30 s.

**Anonymous-first resolution was considered and rejected** (owner's call, 2026-08-10): it is
~6 s instead of ~22 s, but YouTube Music is unlikely to tolerate a signed-out client
long-term, so the jar stays in use on every request.

**WP1 detail** —
[`firmware/pi-ytmusic/setup_pi_ytmusic.py`](../firmware/pi-ytmusic/setup_pi_ytmusic.py) is
live on the Pi. `ytm-out` loads, mpv plays into it, RTP egress confirmed, and the add-on's
*Youtube Music* source (`rtp-in-youtube-music`, port 46001, 48 kHz) receives it. Nothing
about the daemon or the add-on changed. Three corrections came out of building it: no null
sink, a **negative** `priority.session`, and a guard against hot-restarting PipeWire — see
WP1.

---

## Two deployments of one receiver

The same application (`firmware/pi-ytmusic/receiver/`) runs in two places, on
purpose — they coexist rather than one replacing the other:

| | Raspberry Pi (Zero 2 W) | HA add-on (`ytmusic_receiver/`) |
|---|---|---|
| install | `scp -r firmware/pi-ytmusic` + `setup_pi_ytmusic.py` | `./scripts/deploy-dev.sh ytmusic` |
| RTP port | 46001 | **46002** (separate router source) |
| DIAL port | 8099 | **8098** (8099 is the router's) |
| JS runtime | private node 22 tarball (Raspbian ships 20) | distro nodejs 22 |
| cookie jar | `~/.local/state/pi-ytmusic-receiver/cookies.txt` | the add-on's `/data/cookies.txt` |
| provision cookies | `push_cookies.py` | `push_cookies.py --addon` |
| authenticated resolve | ~7-9 s (remote cipher) | faster: the Pi 4 is ~3.3x per core |

The app directory stays canonical on the Pi side because that role is installed by
copying it wholesale; `deploy-dev.sh` stages a copy into the add-on before building
(Docker cannot `COPY` from outside its build context).

**Why the add-on is worth having even though the Pi works:** it halves the cold
start (the challenge is CPU-bound JS), frees the Zero's CPU, and moves the RTP onto
loopback instead of the 2.4 GHz radio the Zero shares with Bluetooth. **Why the Pi
is worth keeping:** it is independent of Home Assistant restarts and updates.

The add-on runs `apt`/`npm`/`pip` in its image, so it is **built on the workstation
and pulled**, not built on the device — assembling it under emulation on a Pi 4 is
the slow part even with nothing to compile. Note that a freshly created GHCR package
is **private**, and Supervisor then fails the pull with `401`; the
`LABEL org.opencontainers.image.source` in the Dockerfile links the package to this
repo so it inherits the repo's visibility (and so the prune workflow can use
`GITHUB_TOKEN`).

---

## 1. Why this shape, and why it isn't Chromecast

Google Cast receive is **closed**: a sender authenticates the receiver over CASTV2 and
requires a Google-signed device certificate that only exists on real Cast hardware. The
one working exploit replays precomputed signatures extracted from a commercial Android
app, with certificates that expire every 48 hours, and only against Chrome (whose
Openscreen build skips nonce checking). Phone senders check the nonce. That door is shut,
and it would not be worth walking through if it were open.

YouTube — uniquely among Google's apps — kept its **pre-Cast** path alive: **DIAL**
(SSDP/UPnP) for discovery plus the unofficial **Lounge API** for control. No device
certificates, no crypto to defeat. [`yt-cast-receiver`](https://github.com/patrickkfkan/yt-cast-receiver)
implements both and is actively maintained ([Volumio's YouTube Cast Receiver
plugin](https://github.com/patrickkfkan/volumio-ytcr) is the reference consumer).

The consequence that shapes everything below: **DIAL/Lounge is a control plane only.**
The phone never sends audio. It sends *"play video id X at position Y"*, and **we** are
the player — we resolve, fetch, decode and pace the stream ourselves, and report position
and volume back so the app's UI stays in sync. This is closer to
[`player.rs`](../pipewire_audio_router/bridge-daemon/src/player.rs) than to
[`airplay_source.rs`](../pipewire_audio_router/bridge-daemon/src/airplay_source.rs): there is no incoming PCM to
buffer.

**Design pillars**

| Pillar | Choice |
|---|---|
| Discovery + control | DIAL + Lounge via `yt-cast-receiver` (Node ≥ 18) — no Cast, no device certs |
| Player | **`mpv` in `--idle` mode over JSON IPC** — it already contains `yt-dlp`, the decoder, the buffer, seek and playlist prefetch |
| Transport to the router | A **second** `module-rtp-sink` on the Pi, own port (**46001**), S16LE/**48000**/2 |
| Router side | An ordinary RTP source, `rate = 48000` — **zero daemon changes** |
| Host | The existing Pi bridge box, as an **additive sibling role** — the BT bridge config is not touched |
| Control surface | The phone app. No HA `media_player` for transport; the router's own per-output volume still applies for the fan-out |

**Why a second RTP port rather than mixing into `bt-bridge-rtp`:** the multi-source
refactor made extra RTP sources free, and each carries its own `rate`
([`RtpSourceConfig`](../pipewire_audio_router/bridge-daemon/src/sources_store.rs)). YouTube audio is natively
**48 kHz** (Opus), and the router's graph runs at 48 kHz — so a dedicated source at 48000
avoids a resample on the Pi *and* in the router, while Bluetooth keeps its 44100. It also
makes the two independently routable, which is what you actually want.

**Non-goals**

- Any presence in the add-on, its UI, or its API.
- Sample-tight sync with other sources (this is a normal RTP source with a jitter buffer).
- Casting from the *YouTube* app (it will likely work for free; it is not the goal and
  not an acceptance criterion).
- Supporting anyone else's install. No provisioning story beyond "runs on my Pi".

---

## 2. Signal path

```
YTM app on phone
      │  DIAL (SSDP discovery) + Lounge API (control)
      ▼
yt-cast-receiver  (Node service, systemd)
      │  JSON IPC  (loadfile / pause / seek / volume / time-pos)
      ▼
mpv --idle          (yt-dlp resolver + decoder + buffer, built in)
      │  PipeWire stream, pinned to →
      ▼
sink  ytm-out  (module-rtp-sink)  ──UDP:46001──▶  add-on
                                                      │
                                                      ▼
                              RTP source "YouTube Music" @48000
                                                      │
                                                      ▼
                                         routing matrix → outputs
```

Two processes, both new, both on the Pi; nothing else changes.

---

## WP0 — Go/no-go spike ✅ **done 2026-08-10**

The question was: **does the YouTube Music app's cast picker list a DIAL receiver at all?**
The pre-spike evidence was secondhand (the Volumio plugin claims YTM support and can
distinguish YTM senders via `Constants.CLIENTS.YTMUSIC`), and Google has been narrowing
this path for years.

**Answer: yes.** `yt-cast-receiver`'s demo, run on a LAN box with a stub player, is
discovered by the YTM app and receives the whole transport set — track info and playback
position track the app exactly. Proceed with the plan as written.

### Findings worth keeping (this section is the re-run checklist)

If casting ever stops working, walk this list **before** debugging any of our own code.

**The Lounge log is not evidence of LAN discovery.** The receiver opens its *own* outbound
Lounge channels — one per app — and Google's servers keep them alive with `noop`. So a log
showing both `(YouTube)` and `(YouTube Music)` channels idling proves only that the
receiver registered its screen; no sender has necessarily found anything. Likewise,
**manual pairing ("Link with TV code") tests nothing about DIAL**: it is entirely
outbound and works off-network by design. A working TV code plus a missing Cast icon is
therefore not a contradiction — and it is not evidence about YTM either.

**Establish what the LAN should show before blaming the receiver.** An `M-SEARCH` for
`urn:dial-multiscreen-org:service:dial:1` enumerates every DIAL responder in reach — a
stock Chromecast answers on port 8008 — so it separates "our receiver is not advertising"
from "this app/network is not discovering anything".

**Bind the DIAL server to one address on a multi-homed host.** The spike box had `docker0`
alongside its LAN interface and answered every `M-SEARCH` **twice with the same USN** —
once with the LAN `LOCATION`, once with `http://172.17.0.1:8099/...`. A sender that keeps
the docker0 answer cannot fetch the device description and drops the device silently, with
nothing logged on our side. `DialOptions` has the knob:

```js
new YouTubeCastReceiver(player, {
  dial: { bindToAddresses: ['<lan-ip>'] },        // or bindToInterfaces: ['<iface>']
  device: { name: 'ROUTER-DIAL', screenName: 'ROUTER-CODE' }
})
```

This is the strongest argument for running the service **on the Pi**: single-homed, on the
same WiFi as the phone, so neither the double-advert nor a wired↔wireless multicast hop
can happen. (Also check for a second SSDP stack on UDP 1900 — a desktop `dleyna-renderer`
was bound there too, including on the docker address.)

**Set `device.name` *and* `screenName`, to different strings.** `friendlyName` defaults to
the hostname, which is easy to scan straight past in a picker. Better, per the library
docs, `name` is shown when the device is found via **DIAL** and `screenName` when found via
**manual pairing** — so two distinguishable strings tell you *which path* found it, for
free.

**Verify the server surface directly** rather than inferring it from the app. `GET` the
device description and confirm the `Application-URL` header; `GET
<prefix>/apps/YouTube` and expect `200` with `<state>running</state>`. Note only the app
name **`YouTube`** is registered — `createDelegate({'YouTube': app}, …)` is hardcoded, and
there is no `YouTubeMusic` app. That is correct: the YouTube family shares one DIAL app
plus Lounge.

**Traps not hit, still true.** DIAL discovery is SSDP multicast and needs the phone and
receiver on the same L2 segment; a guest VLAN, client isolation, or a mesh node that drops
multicast hides the receiver even when the code is perfect.

---

## WP1 — RTP egress role on the Pi — ✅ **deployed 2026-08-10**

Delivered as [`firmware/pi-ytmusic/setup_pi_ytmusic.py`](../firmware/pi-ytmusic/setup_pi_ytmusic.py)
— same idempotent, `--disable`-able style as the Bluetooth role. Additive only:
`setup_pi_bridge.py` is a pile of hard-won fixes (bluez `seat-monitoring`, `AutoEnable`,
`JustWorksRepairing`, `priority.session = 3000`) and is never touched.

- A PipeWire drop-in (`60-ytmusic-rtp.conf`) loading `module-rtp-sink` as the node
  **`ytm-out`**, pointed at the add-on host on `--port` (default **46001**),
  **S16LE / 48000 / 2**.
- **No mDNS advert.** The `_pwrouter-btbridge._tcp` advert is the BT role's discovery
  contract with the add-on's Sources tab; this role is invisible to it by design.
- `--test-tone` plays a 440 Hz tone through mpv, pinned exactly the way WP2 will pin it —
  so it proves the audio path *and* the pinning syntax in one step.

Router side, one-time by hand: add an RTP source named *YouTube Music*, `port = 46001`,
`rate = 48000`, `source_addr = 0.0.0.0` (unicast), `ignore_ssrc = true`, and leave
`latency_msec` at the default until WP4 says otherwise.

**Correction to the original sketch: no null sink.** This section first called for a null
sink `ytm-out` *plus* an `module-rtp-sink`. That is redundant — `module-rtp-sink` already
*is* an `Audio/Sink` node and mpv is an ordinary playback client, so it plays straight into
it. (The BT role needs `module-loopback` only because its input is a *capture* node,
`bluez_input.*`.) A null sink plus a permanent loopback would also keep the graph driven
around the clock, transmitting silence at **~1.5 Mbit/s** over the radio this Pi shares
with Bluetooth — while the BT role measured *zero packets while idle*, a property worth
keeping. So there is no `node.pause-on-idle = false` either: idle here means no client,
which means nothing is sent, which is correct.

**Three integration hazards, all handled in the script:**

1. **mpv must be pinned to `ytm-out` explicitly.** Confirmed on the device:
   `--audio-device=pipewire/ytm-out` is a valid device string (mpv lists it as
   `'pipewire/ytm-out' (YouTube Music to RTP (sender))`), and a 5 s tone through it
   produced real RTP egress. `PIPEWIRE_PROPS={ target.object = ytm-out }` is the fallback.

2. **Cross-talk into the Bluetooth source — this one actually fired.** A sink also exposes
   a *monitor* source, and the BT role's loopback capture deliberately follows the
   **default source**, so `ytm-out`'s monitor is a new candidate for it. The first deploy
   set `priority.session = 100`, and the capture promptly bound to `ytm-out:monitor_*` —
   so the test tone was forwarded into `bt-bridge-rtp` *as well* and came out of all four
   speakers that source is routed to.

   **`rtp-bridge` leaves `priority.session` unset, which ranks as 0** — so any *positive*
   value makes this monitor win. The fix is a **negative** value; measured on the hardware:
   `100` → capture bound to `ytm-out:monitor_*`; `-100` → back to `rtp-bridge:monitor_*`
   (that sink's own idle, driverless monitor, which sends nothing). A tie at 0 would be
   non-deterministic, so stay negative. `verify()` now **asserts** this and needs no audio
   to do it — deliberately, because the BT source is routed to real speakers and a leaking
   test tone is audible in the house.

3. **Never hot-restart PipeWire under a live phone.** It orphans the A2DP transport on this
   hardware and only a clean boot recovers it (a BT-role bring-up lesson). `restart_services()`
   checks for a `bluez_input.*` node and *skips* the restart if a phone is connected — the
   drop-in on disk is the durable part, so applying it can wait for the next boot.
   `--force-restart` overrides.

### Deploying and testing on the device

The Pi is reached as **`ssh david@turnerstr-bluetooth.local`** (key + passwordless sudo
from david-pc; the box is `192.168.178.78`, Raspbian Trixie, armv7l, WirePlumber 0.5.8).
The add-on host is **`192.168.178.22`**, and its API is directly queryable on port 8099
without going through ingress — useful for checking state before touching anything:

```bash
# deploy / re-deploy (idempotent)
scp firmware/pi-ytmusic/setup_pi_ytmusic.py david@turnerstr-bluetooth.local:~/
ssh david@turnerstr-bluetooth.local '~/setup_pi_ytmusic.py --host 192.168.178.22'

# what the router thinks exists, and what is wired to what
curl -s http://192.168.178.22:8099/api/sources
curl -s http://192.168.178.22:8099/api/routing     # ← includes the `links` array
```

**Check `/api/routing` BEFORE playing any test audio.** The Bluetooth source is routed to
four outputs (Dusche + three Voice satellites), so anything that leaks into
`bt-bridge-rtp` plays in the house. The scripted cross-talk assertion is silent for exactly
this reason; prefer it to `--test-tone` unless you *want* sound.

Graph-level checks on the Pi (`XDG_RUNTIME_DIR=/run/user/$(id -u)` is required for all
PipeWire CLI tools over SSH):

```bash
pw-cli ls Node | grep -E 'ytm-out|rtp-bridge'   # both roles' sinks present
pw-link -il | grep -A2 bt-bridge-capture        # must show rtp-bridge:monitor_*, NOT ytm-out
mpv --audio-device=help | grep -i pipewire      # confirm the device string
cat /sys/class/net/wlan0/statistics/tx_packets  # egress proof; tcpdump under-reports (TX offload)
```

**Verified 2026-08-10:** `ytm-out` present; `pipewire/ytm-out` is a valid mpv device;
5 s tone → +2166 wlan0 TX packets; cross-talk assertion **PASS**
(`bt-bridge-capture → rtp-bridge:monitor_FL/FR`).

**One open item on the router side:** the *Youtube Music* source is configured with
`ignore_ssrc = false` ("only one client"). `module-rtp-sink` picks a fresh SSRC per
session, so after a PipeWire restart on the Pi the add-on may latch the old SSRC and drop
the new stream until the source is reloaded. `true` is the restart-robust setting unless the
single-sender guarantee is wanted deliberately. Getting audio end-to-end *without* any YouTube code in the path is the point of
this WP.

---

## WP2 — The receiver service — ✅ **deployed 2026-08-10, awaiting a live cast**

Built as [`firmware/pi-ytmusic/receiver/`](../firmware/pi-ytmusic/receiver/) — three small
ESM modules, no build step (`yt-cast-receiver` ships JS + typings, so plain Node runs it):

| File | Role |
|---|---|
| `mpv.js` | JSON-IPC client: spawns one long-lived `mpv --idle`, request/reply by `request_id`, re-emits mpv events, forwards mpv's stderr into the log, respawns mpv if it dies |
| `player.js` | `MpvPlayer extends Player` — the nine abstract methods, plus `end-file` → advance queue |
| `index.js` | Config from env, DIAL bind-address selection, receiver wiring, `SIGTERM` shutdown |
| `singleton.js` | Single-instance lock, so a second mpv can never join `ytm-out` (see below) |

**Only one mpv may ever hold the sink — enforced, 2026-08-10.** A sink *mixes* its
clients, so two mpv processes on `ytm-out` produce one RTP stream carrying both, which
presents as stutter or garble and looks like a fault in the audio path. This happened for
real: a dev harness importing `mpv.js` ran alongside the service. Three things allowed it,
all now fixed:

- `mpv.js` **unlinked the IPC socket unconditionally** before spawning, to clear a stale
  one. That threw away a free guarantee — mpv's own `--input-ipc-server` bind is a
  kernel-enforced mutex — and it is what let a second mpv take the path over while the
  first was still playing. It now *probes* the socket: a live one is a hard stop, only an
  unlistened one is removed. (A `connect()` that neither succeeds nor fails is treated as
  live: refusing to start is recoverable, deleting a live socket is not.)
- The DIAL port bind in `index.js` did reject a duplicate *receiver*, but only **after**
  `mpv.start()`, and the top-level `catch` then called `process.exit(1)` **without
  `mpv.stop()`** — orphaning an mpv still attached to the sink, with nothing left to
  control it, which `Restart=always` would stack another one on top of every few seconds.
  The handler now stops mpv, and reports `AlreadyRunningError` as one line rather than a
  stack trace.
- The real guard is [`singleton.js`](../firmware/pi-ytmusic/receiver/singleton.js): a
  Linux **abstract-namespace** unix socket, taken in `mpv.start()` *before* the spawn, so
  anything importing `mpv.js` inherits it. Abstract because it is the one lock that cannot
  go stale — the kernel drops the name when the last fd closes, including on `SIGKILL`, so
  there is no "is this pidfile real?" dance. The holder answers a connection with its pid,
  so the loser logs *who* holds it. The lock is scoped to the **socket path**, not the
  program, so two deliberately separate instances with their own sink still work.

Losing the race is not fatal to the service: it retries every `RestartSec`, cheaply
(nothing is spawned), and recovers by itself when whatever held the lock exits.

Installed by the same script as WP1, as `ytmusic-receiver.service` (user unit,
`WantedBy=default.target`, `Restart=always`, `CPUWeight=50` so a `yt-dlp` spike cannot
starve RTP egress). `WorkingDirectory` is a stable state dir because
`DefaultDataStore` persists via node-persist **relative to the cwd** — and what it
persists includes the DIAL `pid` that keeps the phone recognising the same device.

**Verified on the device (no phone, no sound):** service active, `NRestarts=0`, one
long-lived mpv, DIAL device description served with `friendlyName='Turnerstr Musik'`,
`Application-URL` present, `/ytcr/apps/YouTube` → `200`, and — checked from another host —
the Pi answers `M-SEARCH` **once**, with the correct LAN `LOCATION`. The WP0 multi-homing
trap does not arise here: the Pi is single-homed and the bind is explicit.

**Pinned to `yt-cast-receiver` ^2.1.0, not 2.1.1.** A local git checkout of master says
2.1.1, which is *unpublished* — `npm install` fails with `ETARGET`. npm's latest is 2.1.0,
which was checked to carry everything used here (`dial.bindToAddresses`, all nine `Player`
abstract methods, `Constants.CLIENTS`).

**Two bugs this deploy found, both fixed:**

- **`Environment=` must be quoted in the unit.** systemd splits unquoted values on
  whitespace, so `Environment=YTCR_DEVICE_NAME=Turnerstr Musik` delivered just
  `Turnerstr` — which is what the phone's Cast menu would have shown.
- **Don't probe the DIAL server on loopback**, and don't probe it immediately. It binds one
  address on purpose, so `127.0.0.1` is refused even when healthy; and it opens the port
  only after mpv's IPC socket is up, which is several seconds on a Zero 2 W. `verify()` now
  probes the LAN address with a 30 s retry.

**Reading the log:** `journalctl --user` fails on this box (`No journal files were found`)
because the appliance user is not in `systemd-journal`. User-unit records do reach the
system journal, so:

```bash
sudo journalctl _SYSTEMD_USER_UNIT=ytmusic-receiver.service -f
```

A connect logs `sender connected: … — YouTube Music`; each track logs `play <videoId>`;
mpv's own failures (i.e. `yt-dlp` breakage, the WP3 subject) surface there too.

### Prefetch ✅ **done 2026-08-10** — `resolver.js`

Measured, then built. Resolution is genuinely slow here (see WP3a), so
[`resolver.js`](../firmware/pi-ytmusic/receiver/resolver.js) resolves the *next* track to a
direct stream URL while the current one plays; `doPlay` then hands mpv a plain https URL,
which bypasses `ytdl_hook` altogether:

| path | measured on the Pi |
|---|---|
| cold — mpv resolves via `ytdl_hook` | **30.7 s** |
| prefetched — direct URL from cache | **0.5 s** |

Deliberately **not** mpv's own playlist prefetch: appending upcoming tracks would let mpv
advance by itself and the cast session's queue (which the phone's UI mirrors) would drift
from what is actually playing. The queue stays authoritative; only the *resolution* is
pre-done. The next track comes from `player.queue.next ?? player.queue.autoplay`, so it
follows the phone — including its autoplay suggestion.

Details that matter: cached URLs expire on YouTube's own `expire=` stamp (with a 5 min
margin) rather than a guess; `peek()` does not consume the entry so a retry is still fast;
and if a prefetched URL loads but fails to play, `doPlay` invalidates it and re-runs itself
once through the slow path rather than reporting a dead track to the phone.

**Not fixed by this:** the *first* track of a session still pays the full ~22–30 s, because
nothing can be prefetched before the phone says what to play. See WP3a for why, and for the
one option that would fix it too.

**Next: a live cast from the phone** — the one thing that cannot be tested from here.
Cast from YTM, then check: the device appears as *Turnerstr Musik*; play/pause/skip/seek
and the volume slider all act; the app's progress bar tracks; audio comes out of whatever
`rtp-in-youtube-music` is routed to. Then WP4's list.

### The Player → mpv mapping

Nearly 1:1, which is the whole reason for choosing mpv:

| `Player` method | mpv IPC |
|---|---|
| `doPlay(video, position)` | `loadfile <url> replace` + `start=<position>` |
| `doPause()` / `doResume()` | `set_property pause true/false` |
| `doStop()` | `stop` |
| `doSeek(position)` | `seek <position> absolute` |
| `doSetVolume({level, muted})` | `set_property volume` + `set_property mute` |
| `doGetVolume()` | `get_property volume` / `mute` |
| `doGetPosition()` | `get_property time-pos` |
| `doGetDuration()` | `get_property duration` |

Notes that matter:

- **Let mpv resolve.** Hand it the watch URL and let its `ytdl_hook` invoke `yt-dlp`.
  Shelling out to `yt-dlp` per track from Node re-adds Python interpreter + extractor
  startup — *seconds* on a Zero 2 W — in front of every track change.
- **Prefetch is deferred** (see above) — every track transition currently pays a fresh
  resolve. Measure before adding it.
- **`doPlay` must not resolve `true` early.** The base class flips to PLAYING when it
  resolves, and that state is what the phone renders — so it waits for mpv's `file-loaded`
  (60 s cap) rather than for `loadfile`'s acknowledgement, which only means "queued".
- **`end-file` fires for stops too.** Only `reason: eof` (or `error`) may advance the
  queue; `stop`/`quit` are our own doing. A tracked flag plus the reason check keeps a
  `doStop()` from skipping a track.
- **One long-lived mpv**, `--idle=yes`, never respawned per track — this is both the RAM
  and the latency argument on a 512 MB box.
- **Hold the device open *between tracks*, not forever.** Rely on mpv's gapless playback
  (which keeps the audio output open across playlist items) rather than an option that
  streams silence indefinitely: `ytm-out` transmits whenever it has a client, so a
  permanently-open device means ~1.5 Mbit/s of silence on the radio around the clock (see
  WP1). A cold start costs one jitter-buffer fill; inter-track gaps are what matter.
- **Volume** goes to mpv, so the phone's slider feels right. The router's per-output
  volume stays the fan-out control — the two compose, and that is intended.
- **Position drift is expected and accepted.** The phone's progress bar leads the sound by
  the RTP jitter buffer plus output latency (order 200–400 ms). Do not try to correct it.
- Persist `yt-cast-receiver`'s `pid` in a writable state dir so the phone keeps
  recognising the same device.

---

## WP3 — Resolver and account hardening — ✅ **deployed 2026-08-10** (jar is the owner's step)

This, not the Lounge API, is where the maintenance lives — and it turned out to be **two
independent problems**, only one of which is about cookies.

### 3a. A JavaScript runtime for the `n` challenge (the real blocker)

The first authenticated resolution attempt failed with `No video formats found!` — while the
*same* video resolved fine anonymously minutes earlier. Neither of the obvious suspects was
right: it was not the player client (`tv`, `mweb`, `web_safari`, `tv_embedded` all failed
identically) and not the yt-dlp version (latest behaved the same). Verbose output had it:

```
[debug] [youtube] Found YouTube account cookies      <- the jar was fine
[debug] JS runtimes: none
[debug] [youtube] [jsc] JS Challenge Providers: bun (unavailable), deno (unavailable),
                                                node (unavailable), quickjs (unavailable)
WARNING: n challenge solving failed: Ensure you have a supported JavaScript runtime
         and challenge solver script distribution installed
[debug] Detected experiment to bind GVS PO Token to video ID for web_safari client
[debug] ... YouTube is forcing SABR streaming for this client
```

YouTube now makes authenticated requests solve an **`n` signature challenge**, which yt-dlp
can only do with an **external JS runtime** plus the **`yt-dlp-ejs`** solver scripts. Two
non-obvious details decide the design:

- **yt-dlp enables only `deno` by default.** Any other runtime must be turned on explicitly
  with `--js-runtimes`. That single missing flag was the first failure.
- **The usable runtime differs per machine**, which is the awkward part:

  | | workstation (x86_64) | Pi (armv7l) |
  |---|---|---|
  | `deno` | available | **no 32-bit ARM build** |
  | `bun` | available | **no 32-bit ARM build** |
  | `node` | 22 → works | Raspbian ships **20.19.2**, which yt-dlp reports as `node-20.19.2 (unsupported)` and refuses |
  | `quickjs` | — | Debian package `quickjs` (`/usr/bin/qjs`) → **works** |

  So it is `node` on the desktop and **`quickjs`** on the Pi (`YTCR_JS_RUNTIME`, and
  `LOCAL_JS_RUNTIME`/`REMOTE_JS_RUNTIME` in `push_cookies.py`). Worth revisiting if node
  ever reaches ≥ 23.5 here, since yt-dlp prefers node over quickjs.

**Why it costs this much *per track*, and why caching does not save us.** The expensive part
is executing YouTube's player JavaScript to solve the challenge — not downloading it: quickjs
and node differ by ~68 s on identical input, which is engine speed, not network. yt-dlp does
cache, but not in a way that helps here:

- The reusable artefacts — the downloaded player JS (`_code_cache`) and derived data
  (`_player_cache`) — are **instance-level dicts**, alive only for one `YoutubeDL` object.
- Disk caching is **opt-in per call site** (`use_disk_cache=False` by default). In this
  version only `'sigfuncs'` passes `True`; on the Pi that produces two ~450-byte JSONs in
  `~/.cache/yt-dlp/youtube-sigfuncs/`, which is evidently not the bottleneck.
- The `n`-challenge results (`'n'`) are **memory-only** — and keyed by the challenge string
  itself, which is *per stream URL*, so persisting them would not help a different video
  anyway.
- The PO-token cache provider is literally named `memory` (`MemoryLRUPCP`).

And mpv's `ytdl_hook` spawns a **fresh yt-dlp process per track**, so every track discards
all of it. Two consecutive resolves of the *same* video measured 22 s then 21 s — no
improvement, which is the empirical confirmation.

**A long-lived resolver process was proposed, measured, and REJECTED.** The idea was to hold
one `YoutubeDL` instance so its in-process caches survive between tracks. Measured on the Pi,
four sequential resolves in a single process:

| # | video | elapsed |
|---|---|---|
| 1 | jNQXAC9IVRw | 19.5 s |
| 2 | UnAHtKGgkQ8 | 17.5 s |
| 3 | GS5OhxdJUUg | 15.9 s |
| 4 | jNQXAC9IVRw *(repeat of #1)* | **16.0 s** |

Only ~3.5 s of a ~20 s cost, and **repeating the same video is not cheaper** — so process
warmth is not what makes this slow. The `n` challenge is keyed per stream URL and the GVS PO
token is bound per video id, so each new track does the expensive JS work regardless. A
daemon would have added a service to save 3.5 s. Not built.

Configuration levers were tested too and are all flat: `--extractor-args
youtube:skip=hls,dash` (fewer formats to challenge), `player_client=tv`, and
`player_client=web_safari` all came in at 21-25 s, i.e. within noise of the 22 s baseline.

**So prefetch is the whole answer for track changes, and the cold start of a session is
simply the price.** What would actually move it, neither of them small:

- **Resolve on a faster machine.** The challenge is CPU-bound JS: a 64-bit host with a
  faster core should be several times quicker. Awkward here because the obvious candidate
  runs Home Assistant OS, where arbitrary services are not welcome.
- **Run the Pi on a 64-bit OS.** The Zero 2 W's Cortex-A53 is aarch64-capable; Raspbian armhf
  is a 32-bit userland, which also rules out deno/bun and modern node. Reinstalling the OS of
  a box that also does Bluetooth bridging and camera duty is its own project.

**The trap that hid this, and now can't:** only *authenticated* requests hit the challenge.
An anonymous probe resolves happily on a box whose runtime yt-dlp rejects — so the first
version of `verify()` printed `resolves a video: YES` on the Pi while every real track would
have failed. It now probes **with the cookie jar whenever one exists**, labels the result
`(authenticated)` or `(anonymous)`, and when anonymous says outright that it has *not* proved
the runtime works. `(unsupported)` in `yt-dlp -v` output is the tell.

Verified on both machines with cookies: resolution returns **`251 opus 48000Hz`** — exactly
the resample-free format WP1's 48 kHz path was designed around. So the receiver passes
`js-runtimes=<runtime>` to yt-dlp (via
`--ytdl-raw-options-append`, *not* `--ytdl-raw-options=`, which would replace the whole list
and silently drop the cookies entry), the venv installs `yt-dlp-ejs` alongside yt-dlp, and
the weekly timer updates both.

`verify()` no longer just prints the yt-dlp version — it **resolves a video** and reports the
format, because "installed" and "works" turned out to be very different states and every
other check passed while nothing could play. On a workstation whose distro yt-dlp lacks the
solver scripts, `push_cookies.py` now passes **`--remote-components ejs:github`** by default
so it fetches the script at runtime and needs nothing installed (`--ytdlp PATH` pointed at a
venv with `yt-dlp-ejs`, plus `--remote-components none`, is the other way).

**`ejs:github`, not `ejs:npm`** — a distinction that cost another round trip. Both are valid
values of the option, but for the *challenge solver* yt-dlp only downloads from GitHub; with
`ejs:npm` it logs `Remote component challenge solver script (node) was skipped … enable the
download with --remote-components ejs:github (recommended)` and then finds no formats. So the
symptom is identical to having no runtime at all, even though the runtime is detected fine
(`JS runtimes: node-22.22.2`, `Solving JS challenges using node`). Verified both ways on the
workstation. See the [EJS wiki page](https://github.com/yt-dlp/yt-dlp/wiki/EJS).

The Pi does **not** use remote components: its venv holds `yt-dlp` and `yt-dlp-ejs` updated
together by the weekly timer, so it never fetches code at runtime, and `verify()`'s resolve
check catches a version mismatch at setup time instead.

**End-to-end proof, 2026-08-10:** mpv on the Pi, with the service's exact arguments —
`quickjs` + the provisioned cookie jar — played a real **`music.youtube.com`** URL into
`ytm-out` and pushed **4046 RTP packets in 10 s** to the add-on (anonymous, pre-cookies: 2035
in the same window). `push_cookies.py --check` passes. Silent in the house, because that
source is not routed to an output yet.

### 3b. Remote cipher — the challenge solved off-box ✅ **deployed 2026-08-10**

The JS challenge is CPU-bound, so the biggest win is not doing it here.
[`yt-dlp-remote-cipher`](https://github.com/coletdjnz/yt-dlp-remote-cipher) (by a yt-dlp
maintainer) sends the challenges to a [yt-cipher](https://github.com/kikkia/yt-cipher)
server — itself "an http api wrapper for yt-dlp/ejs", i.e. the same solver, hosted.
Measured on the Pi Zero, through the real player path:

| path | resolve |
|---|---|
| local node 22 | 23-26 s |
| **remote cipher** | **6.7-10.3 s** |
| network floor | ~6 s |

Two findings that shaped the implementation, both measured:

1. **Passing `--js-runtimes` alongside the plugin silently disables it.** yt-dlp picks one
   JS-challenge provider by preference; the plugin registers none, so every builtin runtime
   outranks it. The first deploy looked wired up but logged `[jsc:node] Solving JS challenges
   using node` and took 24-29 s — not one request reached the server. The two are therefore
   offered **one at a time** (`Resolver#attempts`): attempt 1 has only the remote provider,
   attempt 2 only the local runtime. This also means the fallback had to be written
   explicitly rather than left to yt-dlp.
2. **The public instance returns a wrong `n` for roughly one request in three.** The URL
   looks fine and only *fetching* it reveals `HTTP 403`. Unverified, those URLs enter the
   prefetch cache and surface as a dead track mid-session, far from the resolve that caused
   them. So every resolved URL is now proven with a **two-byte ranged request** before being
   cached (~200 ms); a rejected URL fails that attempt and escalates to the local solver.
   Verified live: 3 of 4 resolves served remotely in 6.7-10.3 s, the fourth caught at 403 and
   re-solved locally in 24 s — no dead tracks either way.

Self-hosting yt-cipher (it ships a `docker-compose`, and its README recommends
`OVERRIDE_SCRIPT_VARIANT=IAS`) would likely fix the 1-in-3 and remove both the shared
10 req/s limit and the third-party egress — the challenge strings and this host's IP, never
cookies. `YTCR_CIPHER_URL` makes that a URL change; `--cipher-url none` disables remote
solving entirely.

### 3c. A current yt-dlp

The Pi's apt `yt-dlp` was **2025.04.30**; the workstation's was 2026.06.09. YouTube breaks
extractors far faster than a Debian stable release follows, and a stale resolver presents
as *"casting connects fine, then nothing plays"* — the most confusing failure this system
can have.

So yt-dlp comes from **pip in its own venv** (`~/.local/share/pi-ytmusic-venv`), never
apt, with a **weekly `ytmusic-ytdlp-update.timer`** (`Persistent=true`, so a Pi that was off
on timer day still catches up). It updates without redeploying any of our code, which is
the whole point of keeping it out of the Node app. Now running **2026.07.04**.

The **PyInstaller standalone binary was rejected**: it unpacks itself to `/tmp` on every
invocation, and `yt-dlp --version` alone already costs a **measured ~2.3 s** of Python cold
start on this Zero 2 W. That measurement is also the strongest argument for eventually
adding the WP2 prefetch — every track transition pays it.

### 3d. Cookies

The jar is a **live, rotating credential, not a config file**. `yt-dlp --cookies FILE`
reads from *and dumps the jar back into* FILE (its own `--help` says so), which drives three
design decisions:

- It lives in the **state dir** and must stay **writable** by the service, or rotated
  cookies are lost. `verify()` checks and reports that.
- **Provision, don't sync.** Re-pushing an older export over a rotated jar rolls the
  session back and can invalidate it, so
  [`push_cookies.py`](../firmware/pi-ytmusic/push_cookies.py) refuses to overwrite an
  existing jar without `--force`.
- **Export from a dedicated/private browser session, then close it without logging out and
  never use it again.** If the same session stays live in the everyday browser *and* on the
  Pi, the two rotate against each other and Google invalidates both — usually within hours.
  This is yt-dlp's own documented advice and it is the difference between "works for
  months" and "broke by tomorrow".

`push_cookies.py` runs **on the workstation** (that is where a browser and a login exist)
and does: extract (`--from-browser`, via yt-dlp so Firefox's sqlite *and* Chromium's
keyring-encrypted store both work) or take an existing `--file`; **filter to Google/YouTube
domains only** — a browser jar otherwise ships every site you have ever visited to the Pi;
report the login cookies and their expiry (never the values); install mode `0600`
atomically over ssh; then prove it resolves **on the Pi with the Pi's yt-dlp**.

Cookie expiry **is** inspectable (column 5 of the Netscape format, a Unix timestamp) and
the tool prints it — but treat it as a **lower bound only**: Google invalidates server-side
whenever it likes, and the `__Secure-1PSIDTS`/`-3PSIDTS` companions rotate hourly, so their
expiry says nothing about the session's life. The only real test is resolving a video,
which is why `--check` exists.

```bash
firmware/pi-ytmusic/push_cookies.py --from-browser firefox:ytm   # extract + push + verify
firmware/pi-ytmusic/push_cookies.py --file ~/cookies.txt --inspect  # look, don't push
firmware/pi-ytmusic/push_cookies.py --check                      # is the jar still good?
```

**Making the dedicated session (measured on this workstation, 2026-08-10).** The obvious
first attempt fails in a way worth writing down: extracting with
`--cookies-from-browser firefox:<dir>` where `<dir>` is a **profile directory that exists on
disk but is not registered in `profiles.ini`** yields a jar with ~10 cookies and *no* login
cookies at all — a stale, signed-out store. `yt-dlp` reports no error, because it did find a
cookie database. Pass the profile **name**, not a directory, and check `profiles.ini` for
what actually exists.

A **Private Window does not work either**: Firefox keeps private-browsing cookies in memory
only, never in `cookies.sqlite`, so there is nothing for yt-dlp to read. The workable form of
"dedicated session" on Firefox is a named profile:

```bash
firefox -CreateProfile ytm
firefox -no-remote -P ytm     # log in to music.youtube.com, then QUIT Firefox
firmware/pi-ytmusic/push_cookies.py --from-browser firefox:ytm
# ...and never open -P ytm again
```

**Two bugs in the tool itself, found the same way and worth knowing about:**

- `--cookies FILE` **loads before it dumps**, so handing yt-dlp a freshly created *empty*
  temp file fails with `does not look like a Netscape format cookies file` — before it ever
  reads the browser. The file has to be seeded with the `# Netscape HTTP Cookie File` magic
  header (which is still worth creating ourselves, so it is `0600` *before* every cookie in
  the browser lands in it).
- **yt-dlp's own test video `BaW_jenozKc` has been taken down.** It was the obvious probe
  and it now returns "Video unavailable", i.e. every liveness check failed for a reason with
  nothing to do with cookies. The probe is now `jNQXAC9IVRw` ("Me at the zoo", the oldest
  video on the platform) and is overridable with `--probe-url`; failures matching
  video-unavailable patterns are reported as *"that is the probe video, not the jar"*.

That third case matters generally, because the three failure modes need opposite responses,
and the tool now spells them out: cookies expired → make a **new** export (never re-push the
old jar); bot-check/missing-formats → the cookies are fine and **yt-dlp is stale**; video
unavailable → neither, change the probe.

`--inspect` now names every cookie it found and recognises the signed-out set explicitly,
so this diagnosis is self-serve. It also rejects a **half-authenticated** jar: yt-dlp builds
YouTube's `SAPISIDHASH` header from a SID-family **and** an APISID-family cookie, so a jar
carrying only one family authenticates nothing while looking superficially fine.

No restart is needed after a push: mpv passes `--cookies` to yt-dlp per track (via
`--ytdl-raw-options`), so the next song picks up the new jar. With no jar present the
receiver logs a loud warning and resolves anonymously rather than failing.

**Not done, and deliberately:** no PO-token provider. The current anti-bot regime may or
may not need one *for an authenticated session on this account*, and standing up
`bgutil`-style infrastructure before establishing that it is required would be guessing.
The order is: provision the jar, cast, and only if resolution fails with bot-check errors
(rather than cookie errors) go looking — `push_cookies.py --check`'s failure output says
which of the two you are looking at.

**Owner's step, not automatable from here:** creating the dedicated login session and
running the push. Those are real account credentials and they should be extracted by the
person who owns them.

---

## WP4 — Validation

- A full album start-to-finish with **no gap at track changes** (prefetch working).
- Skip / seek / pause / volume from the phone, all reflected in the app's UI.
- Fan-out: the YTM source routed to two or more outputs, in sync, with **announce/duck**
  behaving exactly as it does for any other RTP source (nothing special should be needed —
  confirm rather than assume).
- Phone leaves the LAN mid-playback, comes back: session recovers or ends cleanly, and no
  wedged mpv.
- **Radio contention check:** stream Bluetooth and trigger a YTM track change in the same
  window. One BCM43xx radio serves both BT and WiFi, so a prefetch download during BT
  playback is the plausible mechanism if BT dropouts ever correlate with track changes.
- **Memory:** Node + mpv + PipeWire + BlueZ on 512 MB. Watch RSS and confirm it doesn't
  swap — SD-card wear is already a known cost of this box.

### Playback stutter — three causes, all host-side (2026-08-10)

Reported as "sometimes stutters" and diagnosed by measuring the box, not by reading the
code. None of it was in the receiver's logic, and none was visible in any config file.

**The audio path had no realtime scheduling at all.** Every `data-loop.0` thread of
`pipewire` and `wireplumber` was `SCHED_OTHER` at nice 0 (`ps -Lo cls,rtprio,comm` — a
`ps -eo comm` grep misses them, the thread comm is `data-loop.0`), so the RTP sender could
be preempted by any ordinary work. The distro's own mechanism does **not** fix this here:
`/etc/security/limits.d/25-pw-rlimits.conf` grants `@pipewire` `rtprio 95`, but limits.d is
applied by `pam_limits` and this box has **no `/etc/pam.d/systemd-user`** — so
`user@1000.service`, the manager that starts PipeWire, never passes through PAM, and its
`LimitRTPRIO` was 0. (RTKit is running, is PipeWire's documented fallback, and did not
grant RT either.) Fixed by a system drop-in on `user@<uid>.service`
(`ensure_realtime_scheduling()`), which needs a **reboot** — the limit is inherited when
the user manager starts. `verify()` now asserts the live scheduling class, silently.

**The prefetch resolve competed with the player, by design.** `#prefetchUpcoming()` fires
right after a track starts, so the spike lands *mid-song*: `yt-dlp` at ~26 % of a core plus
the JS runtime it spawns for the `n` challenge at **>100 % CPU and ~88 MB RSS**, for ~20 s,
on a 4-core 1 GHz box with ~80 MB free and a measured 708k pages swapped out. Both it and
mpv ran at nice 0. `resolver.js` now exec's the resolve through `nice -n 19 ionice -c 3`,
which the JS runtime **inherits** — the only way to reach that grandchild, since
`--js-runtimes` takes a bare binary path.

**`CPUWeight=50` was counterproductive and is now 100.** It was meant to keep a resolve
from starving RTP egress, but a cgroup weight applies to the whole service and **mpv lives
in that service** — so it throttled the player alongside the offender while changing
nothing about their relative priority. Priority is the resolver's problem (above);
realtime is PipeWire's.

Also raised mpv's `--audio-buffer` from its 0.2 s default to **1 s**, the knob that absorbs
a scheduling hiccup before the sink runs dry, and bounded `--demuxer-max-bytes` to 32 MiB
(mpv's default is 150 MiB, enough for one long track to push this box into swap).
Note what is *not* worth touching: `--demuxer-readahead-secs` looks like the underrun knob
at 1 s, but `--cache-secs` defaults to 3600000 and overrides it whenever the cache is on,
so mpv already reads as far ahead as the network allows.

Two host-side factors were ruled *in* but are not this role's to fix: the add-on host runs
`frigate-beta` at ~104 % CPU with load ~5 on 4 cores (the same state as the 2026-07-26
sendspin-stutter incident), and `dmesg` shows `brcmf_sdio_bus_rxctl: resumed on timeout` —
the WiFi SDIO bus stalling on the radio this Pi also uses for Bluetooth.

**The Pi's journal rotates faster than a play session**, which is why none of this could be
read after the fact (`journalctl _SYSTEMD_USER_UNIT=ytmusic-receiver.service --since -6h`
returned nothing). Raise `SystemMaxUse` before trying to correlate a stutter with a resolve.

---

## Known failure modes, in expected order

1. **`yt-dlp` vs YouTube** — cookies expire, extractors break, token requirements change.
   Frequent, usually fixed by updating `yt-dlp`.
2. **Lounge API drift** — mitigated by `yt-cast-receiver` being maintained for exactly
   this; our exposure is a dependency bump.
3. **Google drops DIAL from the YTM picker** — terminal for this approach, no mitigation.
   The WP0 spike is also the canary: if casting stops working after a phone-app update,
   re-run it before debugging anything of ours.
4. **Zero 2 W resource pressure** — resolution spikes and radio contention, addressed by
   prefetch, `CPUWeight`, and one long-lived mpv.

---

## Open questions

- ~~Does the YTM picker list DIAL devices?~~ **Answered yes, WP0, 2026-08-10.**
- Does `mpv`'s `ytdl_hook` cope with YTM's authenticated/Premium streams as smoothly as
  plain YouTube, or does that path need explicit format selection?
- Is the current anti-bot regime satisfiable with cookies alone on this box?
- ~~Worth reporting the phone-side "now playing" metadata anywhere in HA?~~ **Answered,
  2026-08-10 — yes, but not phone-side and not YouTube-specific.** Two findings decided it.
  First, there *is* no phone-side metadata: the Lounge plane carries only video ids (a
  `Video` is `{id, client, context}`, and the only `thumbnail` fields in the library are
  Google-account avatars), so anything we display we resolve ourselves from mpv/yt-dlp.
  Second, the same gap exists for the Bluetooth bridge (AVRCP metadata on BlueZ's D-Bus,
  unread) and for the add-on's **own** AirPlay source, where vendored shairplay already
  parses DMAP and offers `on_metadata`/`on_coverart`/`on_progress` hooks that
  `airplay_source.rs` leaves as no-ops. So this became a generic, source-agnostic feature of
  the daemon rather than anything belonging to this receiver: see
  [`source-metadata-plan.md`](source-metadata-plan.md). This plan's pillar is unaffected —
  the add-on gains no knowledge of YouTube; the Pi becomes "an RTP source that also reports
  metadata", exactly like the Bluetooth bridge. The work here is that plan's WP4:
  `observe_property` on `media-title`/`duration`/`pause` in
  [`receiver/mpv.js`](../firmware/pi-ytmusic/receiver/mpv.js), `Player.on('state')` for
  status, and artwork derived from the video id as
  `https://i.ytimg.com/vi/<id>/hqdefault.jpg`. Splitting artist/album out of the single
  `media-title` string needs a second resolve and stays out of scope until it annoys.
