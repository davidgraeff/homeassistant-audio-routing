# YouTube Music cast receiver

The **Cast button in the YouTube Music phone app** targets the house audio system: the
phone's own transport controls (play/pause/skip/seek/volume) drive playback, and the audio
arrives in the router as an ordinary RTP source that can be routed to any set of outputs.

**Personal install, deliberately not a published feature.** It depends on unofficial
protocols and on `yt-dlp` keeping pace with YouTube's anti-bot measures, it plays from the
owner's signed-in account, and distributing it would be against YouTube's ToS. The
Raspberry Pi role lives in [`firmware/pi-ytmusic/`](../firmware/pi-ytmusic/README.md) and
the Home Assistant deployment in [`ytmusic_receiver/`](../ytmusic_receiver/README.md) — a
**local, never-published add-on**. The audio-router add-on itself knows nothing about
YouTube; from its point of view this is just another RTP source.

If the routing side is unfamiliar, read
[`architecture.md`](../pipewire_audio_router/docs/architecture.md) §3 (*Sources — audio into
the graph*) first; project-level context is in
[`system-architecture.md`](system-architecture.md).

---

## Signal path

```
YTM app on phone
      │  DIAL (SSDP discovery) + Lounge API (control)
      ▼
yt-cast-receiver  (Node service)
      │  JSON IPC  (loadfile / pause / seek / volume / time-pos)
      ▼
mpv --idle          (yt-dlp resolver + decoder + buffer)
      │  PipeWire stream, pinned to →
      ▼
sink  ytm-out  (module-rtp-sink)  ──UDP/RTP──▶  audio-router add-on
                                                      │
                                                      ▼
                                    RTP source, 48 kHz  ("YouTube Music")
                                                      │
                                                      ▼
                                         routing matrix → outputs
```

Two processes — the Node receiver and one long-lived `mpv` — plus one PipeWire module. The
router side needs no code: an RTP source at 48 kHz is the whole integration.

## Why DIAL + Lounge, and not Google Cast

Google Cast *receive* is closed: a sender authenticates the receiver over CASTV2 and
requires a Google-signed device certificate that only exists on real Cast hardware. The one
working exploit replays precomputed signatures extracted from a commercial Android app,
with certificates that expire every 48 hours, and only works against Chrome (whose
Openscreen build skips nonce checking). Phone senders check the nonce.

YouTube — uniquely among Google's apps — kept its **pre-Cast** path alive: **DIAL**
(SSDP/UPnP) for discovery plus the unofficial **Lounge API** for control. No device
certificates, no crypto to defeat.
[`yt-cast-receiver`](https://github.com/patrickkfkan/yt-cast-receiver) implements both and is
actively maintained ([Volumio's YouTube Cast Receiver
plugin](https://github.com/patrickkfkan/volumio-ytcr) is the reference consumer). Only the
DIAL app name **`YouTube`** is registered — there is no `YouTubeMusic` app, because the
YouTube family shares one DIAL app plus Lounge.

The consequence that shapes everything else: **DIAL/Lounge is a control plane only.** The
phone never sends audio. It sends *"play video id X at position Y"*, and **we** are the
player — we resolve, fetch, decode and pace the stream, and report position and volume back
so the app's UI stays in sync. This is closer to
[`player.rs`](../pipewire_audio_router/bridge-daemon/src/player.rs) than to
[`airplay_source.rs`](../pipewire_audio_router/bridge-daemon/src/airplay_source.rs): there is
no incoming PCM to buffer.

### Design pillars

| Pillar | Choice |
|---|---|
| Discovery + control | DIAL + Lounge via `yt-cast-receiver` (Node ≥ 18) — no Cast, no device certs |
| Player | **`mpv --idle` over JSON IPC** — it already contains `yt-dlp`, the decoder, the buffer and seek |
| Transport to the router | `module-rtp-sink` on its own port, S16LE/**48000**/2 |
| Router side | An ordinary RTP source, `rate = 48000` — **zero daemon changes** |
| Control surface | The phone app. No HA `media_player` for transport; the router's per-output volume still applies to the fan-out |
| Reporting back | Now-playing only, over the daemon's generic `POST /api/now_playing/report` |

**Why a dedicated RTP port rather than mixing into `bt-bridge-rtp`:** each RTP source
carries its own `rate`
([`RtpSourceConfig`](../pipewire_audio_router/bridge-daemon/src/sources_store.rs)). YouTube
audio is natively **48 kHz** (Opus) and the router's graph runs at 48 kHz, so a dedicated
source at 48000 avoids a resample on the sender *and* in the router, while Bluetooth keeps
its 44100. It also makes the two independently routable, which is what you actually want.

**Not in scope:** any YouTube-specific presence in the add-on, its UI or its API;
sample-tight sync with other sources (this is a normal RTP source with a jitter buffer);
casting from the *YouTube* app (it likely works, but it is not a goal); and supporting
anyone else's install.

---

## Two deployments of one receiver

The same application (`firmware/pi-ytmusic/receiver/`) runs in two places, on purpose —
they coexist rather than one replacing the other:

| | Raspberry Pi (Zero 2 W) | HA add-on (`ytmusic_receiver/`) |
|---|---|---|
| install | `scp -r firmware/pi-ytmusic` + `setup_pi_ytmusic.py` | `./scripts/deploy-dev.sh ytmusic` |
| RTP port | 46001 | **46002** (separate router source) |
| DIAL port | 8099 | **8098** (8099 is the router's) |
| JS runtime | private Node 22 tarball (Raspbian ships 20) | distro nodejs 22 |
| cookie jar | `~/.local/state/pi-ytmusic-receiver/cookies.txt` | the add-on's `/data/cookies.txt` |
| provision cookies | `push_cookies.py` | `push_cookies.py --addon` |
| authenticated resolve | ~22 s local, ~7-10 s via remote cipher (both pre-daemon) | **2.6-3.5 s** measured live with the long-lived resolver and the resident worker; a *prefetched* track change reaches audio in **1.1-1.3 s** |

The app directory stays canonical on the Pi side because that role is installed by copying
it wholesale; `scripts/deploy-dev.sh ytmusic` stages a copy into `ytmusic_receiver/receiver/`
before building (Docker cannot `COPY` from outside its build context), and that copy is
gitignored.

**Why the add-on is worth having even though the Pi works:** it halves the cold start (the
JS challenge is CPU-bound), frees the Zero's CPU, and puts the RTP on loopback instead of
the 2.4 GHz radio the Zero shares with Bluetooth. **Why the Pi is worth keeping:** it is
independent of Home Assistant restarts and updates.

The add-on runs `apt`/`npm`/`pip` in its image, so it is **built on the workstation and
pulled**, not built on the device — assembling it under emulation on a Pi 4 is the slow part
even with nothing to compile. Note that a freshly created GHCR package is **private**, and
Supervisor then fails the pull with `401`; the `LABEL org.opencontainers.image.source` in
the Dockerfile links the package to this repo so it inherits the repo's visibility (and so
the prune workflow can use `GITHUB_TOKEN`).

---

## Installing

### Raspberry Pi

[`setup_pi_ytmusic.py`](../firmware/pi-ytmusic/setup_pi_ytmusic.py) is an idempotent,
`--disable`-able configurator in the same style as the Bluetooth role's. It is **additive**:
`setup_pi_bridge.py`'s hard-won BlueZ/WirePlumber configuration (`seat-monitoring`,
`AutoEnable`, `JustWorksRepairing`, `priority.session = 3000`) is never touched. Copy the
whole role directory, because the script installs `receiver/` from next to itself:

```bash
scp -r firmware/pi-ytmusic david@turnerstr-bluetooth.local:~/
ssh david@turnerstr-bluetooth.local \
  '~/pi-ytmusic/setup_pi_ytmusic.py --host 192.168.178.22 --name "Turnerstr Musik"'
```

It installs, in order: packages (pipewire/wireplumber in case this role stands alone, mpv,
nodejs/npm, `python3-venv`, `quickjs`); user lingering; the realtime-scheduling drop-in
(below); the PipeWire drop-in `60-ytmusic-rtp.conf` loading `module-rtp-sink` as the node
**`ytm-out`** (S16LE/48000/2, aimed at `--host:--port`); the private Node 22 tarball; the
yt-dlp venv plus its weekly update timer; the `receiver/` app and its
`ytmusic-receiver.service` user unit; then a verification pass.

Layout on the device:

| Path | What |
|---|---|
| `~/.local/share/pi-ytmusic-receiver/` | the app (`WorkingDirectory` is *not* here — see below) |
| `~/.local/state/pi-ytmusic-receiver/` | state dir: the cookie jar, node-persist's DIAL identity, `yt-dlp-cache/` |
| `~/.local/share/pi-ytmusic-venv/` | yt-dlp + `yt-dlp-ejs` + `yt-dlp-remote-cipher` |
| `~/.local/opt/node22/` | private Node 22 (`v22.23.2`), used as the JS runtime |
| `~/.config/systemd/user/ytmusic-receiver.service` | the receiver (user unit, `WantedBy=default.target`) |
| `~/.config/systemd/user/ytmusic-ytdlp-update.{service,timer}` | weekly resolver refresh |

`--no-service` sets up the audio path alone; `--test-tone` plays a 440 Hz tone through mpv,
pinned exactly the way the service pins it, which proves the audio path *and* the pinning
syntax without any YouTube code in the picture. The two halves stay separable on purpose,
because they fail for entirely different reasons.

The unit's `WorkingDirectory` is the **state dir**, not the app dir: `yt-cast-receiver`'s
`DefaultDataStore` persists via node-persist **relative to the cwd**, and what it persists
includes the DIAL `pid` that keeps the phone recognising the same device.

**No mDNS advert.** The `_pwrouter-btbridge._tcp` advert is the Bluetooth role's discovery
contract with the add-on's Sources tab; this role is invisible to it by design.

### Home Assistant add-on

```bash
./scripts/deploy-dev.sh ytmusic
```

That stages the shared app, builds the image on the workstation, pushes it to GHCR, and has
Supervisor pull it (`image:` is set in `config.yaml`, which makes Supervisor treat a local
add-on as image-based). Options are `device_name`, `rtp_host`/`rtp_port` (default
`127.0.0.1:46002`), `dial_port` (8098), `cipher_url`, `bind_address`, `report_metadata` and
`log_level`.

The container needs `host_network: true` for two independent reasons — DIAL discovery is
SSDP multicast on `239.255.255.250:1900`, so the phone can only find the receiver if it
shares the host's LAN interface; and the RTP goes to the audio-router add-on, which is
loopback on the same host. `SYS_NICE` lets PipeWire's realtime module raise its data loop to
`SCHED_FIFO`. There is deliberately **no ingress panel**: the phone's app is the control
surface.

[`rootfs/run.sh`](../ytmusic_receiver/rootfs/run.sh) generates the `ytm-out` RTP sink from
the options (a static drop-in cannot know the configured host/port), starts a private D-Bus
+ pipewire + wireplumber, kicks off a background `pip install --upgrade` of the resolver
stack, and `exec`s node so it becomes PID 1 and receives Supervisor's `SIGTERM` directly.

### The router side

One-time, by hand in the add-on's Sources tab (or over the API): an RTP source per
deployment —

| | value |
|---|---|
| name | e.g. *YouTube Music* |
| port | `46001` (Pi) / `46002` (add-on) |
| rate | `48000` |
| `source_addr` | `0.0.0.0` (unicast) |
| `ignore_ssrc` | `true` |

`ignore_ssrc = true` is the restart-robust setting: `module-rtp-sink` picks a fresh SSRC per
session, so with `false` the add-on can latch the old SSRC and drop the new stream until the
source is reloaded. Leave `latency_msec` at the default.

---

## Resolution: yt-dlp, the `n` challenge, and prefetch

This, not the Lounge API, is where the maintenance lives.

### A current yt-dlp, from pip

Distro packages go stale in weeks (Debian trixie shipped 2025.04.30 while the workstation
had 2026.06.09), and YouTube breaks extractors far faster than a stable release follows. A
stale resolver presents as *"casting connects fine, then nothing plays"* — the most
confusing failure this system can have. So yt-dlp comes from **pip in its own venv**, never
apt, together with `yt-dlp-ejs` and `yt-dlp-remote-cipher`:

- **Pi:** `~/.local/share/pi-ytmusic-venv`, refreshed by a weekly
  `ytmusic-ytdlp-update.timer` with `Persistent=true`, so a Pi that was off on timer day
  still catches up. It updates without redeploying any of our code, which is the whole
  point of keeping the resolver out of the Node app.
- **Add-on:** baked into `/opt/ytdlp` at build time and refreshed in the background at every
  container start.

The **PyInstaller standalone binary was rejected**: it unpacks itself to `/tmp` on every
invocation, and `yt-dlp --version` alone already costs a measured **~2.3 s** of Python cold
start on a Zero 2 W.

### The `n` signature challenge needs a JavaScript runtime

YouTube makes **authenticated** requests solve an `n` signature challenge, which yt-dlp can
only do with an **external JS runtime** plus the **`yt-dlp-ejs`** solver scripts. Without
one, `yt-dlp` reports `No video formats found!` and *every* track fails — while anonymous
resolution of the same video keeps working, which is what makes this easy to misdiagnose.
The tell is in `yt-dlp -v`:

```
[debug] JS runtimes: none
[debug] [youtube] [jsc] JS Challenge Providers: bun (unavailable), deno (unavailable),
                                                node (unavailable), quickjs (unavailable)
WARNING: n challenge solving failed: Ensure you have a supported JavaScript runtime
         and challenge solver script distribution installed
```

Two non-obvious details decide the design:

- **yt-dlp enables only `deno` by default.** Any other runtime must be turned on explicitly
  with `--js-runtimes`. That single missing flag was the first failure.
- **The usable runtime differs per machine:**

  | | workstation (x86_64) / add-on | Pi (armv7l) |
  |---|---|---|
  | `deno` | available | **no 32-bit ARM build** |
  | `bun` | available | **no 32-bit ARM build** |
  | `node` | 22 → works | Raspbian ships 20.19.2, reported `node-20.19.2 (unsupported)` and refused |
  | `quickjs` | — | Debian package `quickjs` (`/usr/bin/qjs`) → works, but **no JIT** |

  quickjs works but is unusably slow per track, so the Pi installs a **private Node 22
  tarball** (`~/.local/opt/node22`, `v22.23.2`) and passes `node:<path>`; `quickjs` is the
  automatic fallback when that tarball is missing. Measured, authenticated, on the Zero 2 W:

  | resolver path | elapsed |
  |---|---|
  | anonymous (no challenge) | ~6 s |
  | authenticated, node 22 (JIT) | ~22 s |
  | authenticated, quickjs (no JIT) | ~90 s |

The option reaches yt-dlp through `--ytdl-raw-options-append=js-runtimes=…`, never
`--ytdl-raw-options=`, which **replaces** the whole key/value list and would silently drop
the cookies entry.

On a workstation whose distro yt-dlp lacks the solver scripts, `push_cookies.py` passes
**`--remote-components ejs:github`** by default so it fetches the script at runtime and
needs nothing installed. It must be **`ejs:github`, not `ejs:npm`** — both are valid values,
but for the *challenge solver* yt-dlp only downloads from GitHub; with `ejs:npm` it logs
`Remote component challenge solver script (node) was skipped …` and then finds no formats,
a symptom indistinguishable from having no runtime at all even though the runtime is
detected fine. See the [EJS wiki page](https://github.com/yt-dlp/yt-dlp/wiki/EJS). The Pi
and the add-on do **not** use remote components: their venv holds `yt-dlp-ejs` updated
alongside yt-dlp, so they never fetch code at runtime.

Authenticated resolution returns **`251 opus 48000Hz`** — exactly the resample-free format
the 48 kHz path was designed around.

### Remote cipher: the challenge solved off-box

The challenge is CPU-bound, so the biggest win is not doing it here.
[`yt-dlp-remote-cipher`](https://github.com/coletdjnz/yt-dlp-remote-cipher) (by a yt-dlp
maintainer) sends challenges to a [yt-cipher](https://github.com/kikkia/yt-cipher) server —
itself an HTTP wrapper around yt-dlp/ejs, i.e. the same solver, hosted. Measured on the Pi
Zero through the real player path: **6.7-10.3 s** against 23-26 s locally, with a ~6 s
network floor. `YTCR_CIPHER_URL` (the `--cipher-url` flag on the Pi, `cipher_url` in the
add-on options; default `https://cipher.kikkia.dev`) selects the server; `none` disables
remote solving entirely.

Two findings shaped the implementation, both measured:

1. **Passing `--js-runtimes` alongside the plugin silently disables it.** yt-dlp picks one
   JS-challenge provider by preference; the plugin registers none, so every builtin runtime
   outranks it. A deploy that looked wired up logged `[jsc:node] Solving JS challenges using
   node` and took 24-29 s — not one request reached the server. So the two are offered **one
   at a time** (`Resolver#attempts`): attempt 1 has only the remote provider, attempt 2 only
   the local runtime, and the fallback is written explicitly rather than left to yt-dlp.
   This is also why the remote cipher is **not** passed to mpv's own `ytdl_hook`, which
   needs the local runtime as its last-resort path.
2. **The public instance returns a wrong `n` for roughly one request in three.** The URL
   looks fine and only *fetching* it reveals `HTTP 403`. Unverified, such URLs enter the
   prefetch cache and surface as a dead track mid-session, far from the resolve that caused
   them. So every resolved URL is proven with a **two-byte ranged request** (~200 ms) before
   being cached; a rejected URL fails that attempt and escalates to the local solver.
   Verified live: 3 of 4 resolves served remotely in 6.7-10.3 s, the fourth caught at 403
   and re-solved locally in 24 s — no dead tracks either way.

Self-hosting yt-cipher (it ships a `docker-compose`, and its README recommends
`OVERRIDE_SCRIPT_VARIANT=IAS`) would likely fix the 1-in-3 and remove both the shared
10 req/s limit and the third-party egress — which carries the challenge strings and this
host's IP, never cookies.

**The remote cipher is now the slow option, on both boxes.** Its entire value was avoiding a
slow local solve, and the resident worker (below) takes a local solve to **4-48 ms** in steady
state — against 4.4 s for a *successful* remote round trip. Meanwhile the public instance
failed twice in one session on 2026-08-10: the known wrong `n` (`HTTP 403` caught at the
verify step, 4.4 s wasted before the fallback even starts) and an nginx **`504 Gateway
Time-out`**. Both escalated correctly and playback continued, but each costs a whole extra
resolve.

**One line per round trip, not per retry.** The `Solving JS challenges via remote cipher
server` message is the plugin's `note=` on a single `/decrypt_signature` POST, and
`_call_api(player_url, sig_value, n_value)` sends **one request per challenge**. A resolve
of one video needs four — two `sig` specs (two signature lengths, visible in the cache as
`<player>-107.json` and `-111.json`) plus `n` for the progressive URL and for the HLS
manifest, hence the `Downloading m3u8 information` line. So four WAN round trips per track,
where the builtin ejs provider and the resident worker both batch all four into one call:
**15-58 ms against 3.4-4.6 s**.

So set `cipher_url: ""` (add-on) / `--cipher-url none` (Pi). Confirmed live: the resident
worker logs `[memory]` solves at 15-58 ms on the add-on, which is what the remote server
was there to avoid. Turning it off buys three things beyond latency — one fewer internet
dependency, no third-party egress, and **half the resolves per track change**, since the
remote attempt is a complete resolve of its own. That last point stopped being cosmetic
once transient 403 bursts turned out to be the failure mode above: fewer requests per track
is now a correctness argument, not just a speed one.

Two earlier revisions of this section recommended the same change for wrong reasons (first
"a warm yt-dlp makes local solving fast" — it does not, the 15 s was JS parsing; then a
mis-attributed 403). The recommendation only became sound once the *stage* was measured
rather than the total.

**Turning it off is a `config.yaml` change, not a UI one.** An add-on option listed under
`options:` is a *default*, and Supervisor re-applies it whenever the key is missing from
the user's saved options — so clearing the field in the UI removes the key and the default
comes straight back, which looks exactly like the save being ignored. `cipher_url`
therefore has **no default** any more (like `bind_address`, which was always schema-only),
and `run.sh` additionally accepts `none`/`off`/`false` as an explicit disable, matching the
Pi's `--cipher-url none`. A value saved before this change stays in the user's options, so
overwrite it with `none` rather than clearing it.

### PO tokens: the plugin is installed, the server is not

YouTube is rolling out a **GVS PO token** requirement per client — the verbose log carries
`Detected experiment to bind GVS PO Token to video ID for web_safari client` — and yt-dlp
cannot mint one itself. The usual provider is
[`bgutil-ytdlp-pot-provider`](https://github.com/Brainicism/bgutil-ytdlp-pot-provider),
which comes in two halves, and only one of them belongs in this image:

| half | where |
|---|---|
| the **plugin** (pip, pure Python, **116 kB**) | installed in `/opt/ytdlp` and in the Pi's venv, refreshed with yt-dlp |
| the **server** (BotGuard in a JS VM) | **not installed.** `canvas ^3` (native, no armv7 prebuild), `jsdom ^27` and `youtubei.js ^16`: ~150-250 MB of node modules plus a source build, and it is not on npm — `git clone` + `npm ci` + `npx tsc`. Run it wherever suits (its own container is the easy answer) and point `pot_provider_url` / `--pot-url` at it |

**Not needed today, and measured rather than assumed.** With no provider at all, every
client that has audio formats returned a working URL for the video that had been failing —
`default`, `tv`, `web_safari` and `mweb` all 206 — and every *fresh* process produced
playable URLs. The 403s tracked session age, not token absence, so `PO Token Providers:
none` is informational here, not a diagnosis. The switch exists so that turning it on later
is one field rather than a rebuild.

**When unconfigured, PO-token fetching is switched off explicitly** with
`--extractor-args youtube:fetch_pot=never`. Without that the installed plugin defaults to
probing `http://127.0.0.1:4416` and emits a `WARNING: … Error reaching GET
http://127.0.0.1:4416/ping` on **every resolve**. Verified both ways on the add-on: without
the switch, that warning plus `251`; with it, just `251`.

### Prefetch — why track changes are fast and the first track is not

[`resolver.js`](../firmware/pi-ytmusic/receiver/resolver.js) resolves the *next* track to a
direct stream URL while the current one plays; `doPlay` then hands mpv a plain https URL,
bypassing `ytdl_hook` altogether:

| path | measured on the Pi |
|---|---|
| cold — mpv resolves via `ytdl_hook` | **30.7 s** |
| prefetched — direct URL from cache | **0.5 s** |

Deliberately **not** mpv's own playlist prefetch: appending upcoming tracks would let mpv
advance by itself, and the cast session's queue (which the phone's UI mirrors) would drift
from what is actually playing. The queue stays authoritative; only the *resolution* is
pre-done. The next track comes from `player.queue.next ?? player.queue.autoplay`, so it
follows the phone — including its autoplay suggestion.

Details that matter: cached URLs expire on YouTube's own `expire=` stamp (with a 5 min
margin) rather than a guess; `peek()` does not consume the entry, so a retry is still fast;
and if a prefetched URL loads but fails to play, `doPlay` invalidates it and re-runs itself
once through the slow path rather than reporting a dead track to the phone.

The **first** track of a session still pays a cold-ish price, because nothing can be
prefetched before the phone says what to play. What it no longer pays is the *process* half
of that: the resolver daemon's `--prewarm` resolves a throwaway video at startup, so the
first real track meets a warm yt-dlp (see below).

### Why resolution itself cannot just be made fast

Measured, so it does not get re-litigated:

- **yt-dlp's caches do not help.** The reusable artefacts — downloaded player JS
  (`_code_cache`) and derived data (`_player_cache`) — are **instance-level dicts**, alive
  for one `YoutubeDL` object. Disk caching is opt-in per call site
  (`use_disk_cache=False` by default; only `'sigfuncs'` passes `True`, which produces two
  ~450-byte JSONs). The `n`-challenge results are memory-only *and* keyed by the challenge
  string, which is per stream URL. The PO-token cache provider is literally named `memory`.
  And mpv's `ytdl_hook` spawns a fresh yt-dlp per track, so every track discards all of it —
  two consecutive resolves of the same video measured 22 s then 21 s.
- **Process warmth is worth ~3.5 s of a ~20 s authenticated resolve, and no more.** Four
  sequential resolves in one process: 19.5 s, 17.5 s, 15.9 s, then 16.0 s for a *repeat of
  the first*. Repeating the same video is not cheaper — the challenge is keyed per stream
  URL and the GVS PO token is bound per video id. That measurement first led to rejecting a
  resolver daemon outright; it is now built anyway (see below), because ~3.5 s is a third of
  the add-on's resolve and because being warm is what makes prewarming the *first* track
  possible. It is **not** a route to a ~2 s authenticated resolve on its own.
- **Configuration levers are flat.** `--extractor-args youtube:skip=hls,dash`,
  `player_client=tv` and `player_client=web_safari` all came in at 21-25 s, within noise of
  the 22 s baseline.
- **Anonymous-first resolution was considered and rejected** (owner's call, 2026-08-10): it
  is ~6 s instead of ~22 s, but YouTube Music is unlikely to tolerate a signed-out client
  long-term, so the jar is used on every request.

What would actually move it, neither small: resolve on a faster machine (which is what the
add-on deployment and the remote cipher both do), or run the Pi on a 64-bit OS — the Zero
2 W's Cortex-A53 is aarch64-capable, and Raspbian armhf's 32-bit userland is what rules out
deno/bun and modern node in the first place.

**Resolved 2026-08-10.** The premise above was wrong in one respect: the cost was never
spread across the resolve, it was almost entirely **one thing** — re-parsing the player JS
in a fresh `node` on every track. Measured on the add-on host with the JS stage isolated
(`node --permission -` fed exactly what `jsc/_builtin/node.py` feeds it):

| stage of one challenge solve | x86_64 workstation | HA host (aarch64, nice 19) |
|---|---|---|
| raw player + solve — **what happened per track** | 1.05 s | **15.15 s** |
| preprocessed player + solve | 0.33 s | 3.38 s |
| *three* solves in one process, preprocessed | 0.33 s | 2.78 s |
| bare `node` startup floor | 0.10 s | 0.90 s |

So ~11.8 s of the 15.15 s is `preprocessPlayer()` — meriyah parsing 2.8 MB of player JS and
astring regenerating it — ~2.5 s is node parsing the 3.8 MB stdin payload, and the **solving
itself is free**: three challenges cost the same as one. There is no hot loop for a JIT to
warm up; each transform runs once. What remained was to stop re-parsing, which is what the
resident worker below does.

### The long-lived resolver (`ytdlp_daemon.py`)

One yt-dlp process for the life of the service, asked per track over a unix socket, instead
of one process per track. [`ytdlp.js`](../firmware/pi-ytmusic/receiver/ytdlp.js) spawns and
supervises it in exactly the shape `mpv.js` uses for mpv — newline-delimited JSON correlated
by id, the child's stderr forwarded into the receiver's log, respawn on death.

Measured on the Pi Zero 2 W, **anonymous** (2026-08-10, four videos, cold disk cache, both
paths through `nice -n 19 ionice -c 3`):

| | per-track spawn | long-lived daemon |
|---|---|---|
| 1st resolve | 6.3 s | 4.1 s (cold) |
| 2nd | 7.0 s | 3.8 s |
| 3rd | 7.3 s | 3.6 s |
| 4th | 5.6 s | **2.6 s** |

What that removes is the fixed per-process cost — the Python start and `yt_dlp` import, the
~2-3 MB player JS download, the InnerTube client-version probes, the PO token (its cache
provider is `memory`, i.e. per process). What it does **not** touch is the JS challenge, which
turned out to be the dominant term of an authenticated resolve (15 s on the add-on host, all
of it parsing); that is what the resident worker below is for. The two compose: this daemon
makes the *process* cheap, the worker makes the *challenge* cheap, and what remains is the
`/player` round trips.

Five things it has to get right, each with a failure it prevents:

| | why |
|---|---|
| **Modes come from `resolver.js`** | `daemonModes` hands the daemon the *same argv* `#attempts()` would have spawned, so the warm path and the fallback path cannot drift. The daemon turns them into API options with yt-dlp's own `parse_options`, not a translation table. |
| **A per-track spawn is still there** | A daemon that will not start (no importable `yt_dlp`), dies repeatedly, or wedges falls back to spawning — a slow resolver must never become a dead player. Only *transport* failures fall back; a resolve that genuinely failed escalates to the next attempt instead, which is the better move. |
| **One shared cookie jar, reloaded by content** | `--cookies` used to be read per spawn, so a pushed jar took effect on the next song. All modes now share **one** jar object (`YoutubeDL.cookiejar` is a `cached_property`, so assigning it before the first request replaces it wholesale), and it is re-read in place when the file's **content hash** changes — never on mtime, and never as a rebuild. Rotated cookies are flushed back at most every 5 min and on `SIGTERM`. See below for the bug that shaped this. |
| **Warm state is discarded after 2 consecutive failures** | yt-dlp caches *negative* results too. Observed while building this: after one failed solve, later resolves on the same instance stopped reporting `[jsc:node] Solving JS challenges using node` and warned "No supported JavaScript runtime could be found" instead — a provider marked unavailable for the life of the process. A fresh process per track papered over that by construction. |
| **`--prewarm`** | One throwaway resolve at startup, while nothing is playing, so the first real track finds the caches populated. This is the one thing warmth does for the *first* track of a session, which prefetch cannot help by definition. |

### The resident JS-challenge worker (`jsc_worker.js` + `jsc_resident.py`)

One `node` kept alive for the life of the daemon, holding the `{n, sig}` solver **closures**
per player version. Measured on the Pi Zero 2 W (armv7, node 22), which is the box where the
old behaviour hurt most — 17.9 s per track:

| what the solve had to do | cost |
|---|---|
| `[player]` first sight of a player version — full preprocess | **17.9 s** |
| `[preprocessed]` fresh worker, player already on disk — a `Function()` compile | **3.7 s** |
| `[memory]` steady state — a function call | **4-48 ms** |

Every solve logs which of the three it was, because "is the worker actually being used?" is
the only question a latency report about this needs answered:

```
[ytdlpd] jsc: solved 1 challenge(s) for 854a788e-main in 17908ms [player]
[ytdlpd] jsc: solved 2 challenge(s) for 854a788e-main in 7ms [memory]
```

How it fits together:

| | |
|---|---|
| **The worker** | `node --permission -e <source>` — the source is passed on the command line and never imported, so the process can keep yt-dlp's sandbox (no filesystem, no network) while evaluating untrusted player JS. It evaluates the ejs lib+core once, then caches `{n, sig}` per player id, bounded to 4 players. |
| **The provider** | A real yt-dlp `JsChallengeProvider` (`ResidentNodeJCP`), registered **in-process** by the daemon rather than as a plugin package — the worker has to be owned by something long-lived, and that is the daemon. Providers are instantiated per `YoutubeDL`, so the worker is a module singleton and `close()` is deliberately a no-op; letting yt-dlp's close hook kill it would discard every cached player. |
| **Opt-in by extractor-arg** | `youtubejsc-residentnode:enable=1`, injected by the daemon into every mode that is *not* the remote-cipher attempt. Not decoration: `resolver.js` offers exactly one challenge provider per attempt, and a provider registered unconditionally would outrank the remote cipher in the attempt meant to use it — the failure already documented in `#attempts`. |
| **Failure is free** | yt-dlp's director tries providers in preference order and moves to the next one on *any* exception, so a dead worker degrades to the builtin `node` provider by construction. Verified by breaking the worker on purpose: `Error solving n challenge request using "resident node" provider: …` followed by `[jsc:node] Solving JS challenges using node`. |
| **Two consumers, one cached file** | The preprocessed player is written to yt-dlp's own disk cache under exactly the key the builtin ejs provider reads, and `_ENABLE_PREPROCESSED_PLAYER_CACHE` — which upstream leaves off — is switched on. So the fallback path reads what the worker computed and vice versa, and a worker restart costs 3.7 s instead of 17.9 s. |
| **Version-scoped cache** | The upstream key is the player URL alone. The section is suffixed with the solver version (`challenge-solver-0.8.0`), because `run.sh` upgrades `yt-dlp-ejs` at every boot and a new solver paired with a player preprocessed by the old one is the most plausible reason that flag is off upstream. A wrong answer would still be caught by `#verify`'s two-byte ranged request. |

**A player rotation costs one track.** YouTube served `854a788e` and then `341562bc` within
an hour of testing; a new player id has nothing cached anywhere, so the first resolve after a
rotation pays the full preprocess once. `--prewarm` absorbs it at startup, not mid-session.

Switches: `YTCR_JSC_RESIDENT=0` disables the worker (back to yt-dlp's own `node` provider),
`YTCR_YTDLP_PREPROCESSED_CACHE=0` disables the disk cache. The worker only runs where
`YTCR_JS_RUNTIME` is node-based — a box configured for `quickjs` keeps yt-dlp's path
untouched, since the worker speaks node's flags.

Two bugs this shook out, both found by running it rather than reading it: the worker's own
`let jsc` collided with the `var jsc` the core script declares in global scope (node refuses
the whole init with *"Identifier 'jsc' has already been declared"*, so the variable name is
load-bearing — it is now `entry`); and `YoutubeDL.extract_info` returns **None** rather than
raising when yt-dlp handled a failure non-fatally, which surfaced as
`AttributeError: 'NoneType' object has no attribute 'get'` instead of a diagnosis.

#### What a play logs, and how to read it

Three lines per track, because "it took 20 seconds" was unattributable for most of this
work — `resolver.js` timed the resolve, and everything after it was dark:

```
[resolver] resolved <id> in 3.4s via local node (daemon)
[MpvPlayer] <id> loaded after 4.6s (resolve 4.1s, mpv 0.5s)
[MpvPlayer] <id> audio flowing after 6.2s
```

`loaded` is mpv's `file-loaded`; `audio flowing` is mpv's own `playback-restart`, the closest
event to sound actually starting. What sits **outside** these numbers, in order: the phone's
DIAL + Lounge handshake before `Player.play()` arrives (the app's time, not ours), and after
`playback-restart` the RTP hop plus the router's buffering — about 0.6 s, per the latency
budget in the Bluetooth-bridge notes.

This immediately settled one question: in a 13.1 s start, mpv accounted for **1.8 s** and the
resolve for 11.2 s, of which two thirds was a rejected URL and a second resolve. mpv, the
`--audio-buffer=1`, and the mid-stream seek were never the problem. A prefetched track change
reads `resolve 0.0s`, `mpv 1.1s`, audio at 1.2 s.

#### The 403 at the verify step: unexplained, mitigated (2026-08-10)

**Symptom:** a resolve succeeds, and the resolved URL then answers `HTTP 403` to a two-byte
ranged GET. `#verify` catches it, the attempt fails, and without recovery `doPlay` falls
through to mpv re-resolving from scratch (~17 s).

**It arrives in windows.** The same video, from the same code, on the same box: three
consecutive resolves through one daemon all 403 while a fresh `yt-dlp` CLI interleaved
between them gets 206 — and ten minutes later *every* configuration, daemon included, gets
206. Any experiment that does not interleave its control is therefore worthless, which
invalidated several rounds of this investigation.

**What has been ruled out**, each by A/B on the hardware, most of it twice:

| candidate | verdict |
|---|---|
| the resident worker | worker ON vs OFF, both 206 in a good window; and its `n` output is **bit-identical** to a fresh `jsc()` call across repeated solves, rebuilt closures and fresh `vm` contexts |
| the preprocessed-player disk cache | ON vs OFF, both 206; and a fresh process inheriting the live cache dir is fine |
| `youtube-sigfuncs` poisoning | cached specs match freshly computed ones byte for byte |
| `fetch_pot=never` | the failing video resolved to a working URL under `never`, `auto`, `auto` with plugins disabled, and `always` |
| IP binding | the URL's `ip=` matches this host's current IPv6 source address exactly, and still 403s |
| the cookie jar | a copy works; and mpv's own resolution, using the *same* jar file, works |
| session age / `YoutubeDL` age | a session seconds old still 403s; a 4-minute-old one sometimes does not |
| process age, "only the first solve works" | a **brand-new** daemon 403'd on its very first resolve while a CLI seconds later succeeded |
| `_pot_memory_cache` (module-level, per video) | cleared on every rebuild; the next resolve still 403'd |

**What is left:** within a bad window, the daemon's `extract_info` loses where a CLI wins,
and no configuration difference explains it. The leading suspect is server-side — YouTube
deciding a given request needs an attestation this deployment cannot provide (`PO Token
Providers: none`, plus `Detected experiment to bind GVS PO Token to video ID` in the verbose
log, on videos that carry ads). Naming it needs the InnerTube request bodies from a daemon
and a CLI captured *inside* a bad window and diffed; that is the next experiment, and it
needs a window to happen while somebody is watching.

**Ads are the leading mechanism, and the ladder follows from it.** Ad enforcement is
reported **per client** (`Detected a 15s ad skippable after 5s for web_safari`), and the
correlation with failures is suggestive: every URL ever rejected here belonged to an
ad-carrying video, and neither zero-ad video has ever failed. Not conclusive — two
ad-carrying videos have never failed either, on a couple of samples each — but it points
somewhere cheap to act:

| ads | history |
|---|---|
| 2-3 markers | `H7079Sq3dAg`, `dQw4w9WgXcQ`, `kJQP7kiw5Fk`, `LCt92oOexsg`, `ZCbMzDclnEM` — all 403-prone |
| 0 markers | `jNQXAC9IVRw`, `aqz-KE-bpKQ` — never failed |
| 2-3 markers | `QK2oBpEBMs0`, `UnAHtKGgkQ8` — never failed (few samples) |

**Mitigation, which is what actually ships.** Cheapest first, and each rung gated by
`#verify` so a URL that will not play never reaches mpv:

1. **`#verify` retries once after 1.5 s** — a transient rejection costs 1.5 s, not a resolve.
2. **A client ladder.** The primary attempt keeps the default clients (itag 251: opus,
   audio-only, 48 kHz — the resample-free format). If its URL will not play, the same video
   is resolved through `web_embedded`, then `web_safari`, then `mweb` (`YTCR_CLIENT_LADDER`,
   `none` to disable). Ordered by format, measured **with cookies**: `web_embedded` serves the
   same `251 | opus | 136 kbps` as the default, `web_safari` drops to HLS (96) and `mweb` to
   muxed 360p (18, i.e. video bytes fetched and discarded). `android_vr` looked good in an
   anonymous sweep and offers **no formats** once authenticated — which is why the ladder was
   ranked from an authenticated measurement. Each rung's
   `YoutubeDL` is built lazily, so unused rungs cost nothing, and **prewarm only warms the
   primary**.
3. **Then a restart**, once the *whole* ladder has produced unplayable URLs — never on the
   first failure, which would kill the daemon mid-ladder and push the remaining rungs onto
   the per-track spawn path. `resolver.js` waits for the replacement and retries the primary
   attempt only.
4. **mpv's own resolution** stays the last resort: a different code path in a fresh process.

Measured with *every* verify forced to fail — the worst case, which the hypothesis says
should be rare: **13.5 s** for ladder + restart + retry, against 27 s for a real session
before any of this existed.

**Confirmed live (2026-08-10):** two tracks in one session were rescued by the ladder —
`local` produced a URL that answered 403, `client-web_safari` produced one that played
(itag 96). Steady-state track changes in the same session ran at **1.1-1.3 s** to audio on
prefetched URLs, and the per-solve closure rebuild cost **121-182 ms**.

Two things that log exposed, both now fixed:

- **Duplicate `--extractor-args youtube:…` silently replace each other.** Every ladder rung
  had lost `fetch_pot=never` to its own `player_client=…`, and was warning about the
  unreachable PO-token server on every resolve. All `youtube:` settings are now composed
  into a single `youtube:a=1;b=2` value; other prefixes (`youtubejsc-*`, `youtubepot-*`) are
  separate keys and stay separate options.
- **A 403 no longer earns the verify retry.** It is a verdict, not a hiccup — the retry never
  once turned into a 200 — and it was costing 1.7 s of every rescued track. Timeouts, resets
  and 5xx still get one retry; 4xx goes straight to the next rung.

**And a 403 now diagnoses itself.** When every attempt has failed, the daemon sweeps the
video across ten `player_client` values in fresh `YoutubeDL` instances, fetches two bytes of
each resulting URL, and writes a JSON report to `/data/403-diagnostics/` — itag, HTTP status,
ad markers, and the server-side URL parameters (`expire`, `ip`, `ei`, `svpuc`, `cps`;
deliberately never `n`, `sig`, `pot` or cookies). Throttled to one per 15 minutes, run in the
background, ~16 s for ten clients anonymously.

This exists because these rejections arrive in **windows** that cannot be reconstructed
afterwards: the same video 403s three times through the daemon while an interleaved CLI gets
206, and ten minutes later every configuration succeeds. Every hypothesis tested outside a
window measured nothing, and several were. The report answers, unattended, the one question
that matters during a window — *was there a client that could have played this?* — and the
answer tunes the ladder.

**One thing was changed on the strength of a since-falsified theory, and is kept anyway:**
the worker now rebuilds the solver closures per solve instead of caching them, because that
is what the ejs core does (`getFromPrepared` inside `main`) and the cost is a lazy
`Function()` compile — 13 ms on x86_64, and logged per solve as `[rebuilt build Xms]`. It
matches upstream semantics; it is not a fix for the 403.

#### The cookie-jar reload that ate the warmth (fixed 2026-08-10, same day)

**Symptom**, from a live add-on log at a song change:

```
[ytdlpd] mode remote: cookie jar changed on disk — reloading
[ytdlpd] DrORE5_qTFg via remote in 4.4s (cold)
[resolver] remote cipher failed: stream URL rejected with HTTP 403 — falling back to local node
[ytdlpd] mode local: cookie jar changed on disk — reloading
[ytdlpd] DrORE5_qTFg via local in 12.2s (cold)
```

Both modes cold, on a warm daemon, for a total of ~17 s — *worse* than the per-track spawn
it replaced.

**Cause.** The first version gave each mode its own `YoutubeDL` (hence its own jar) and
treated any **mtime** change as "the operator pushed a new jar", discarding that mode's warm
caches. But nearly every write to that file is *ours*: `--cookies FILE` makes yt-dlp dump the
jar back on `close()`, the daemon saves it periodically, and mpv's `ytdl_hook` writes it on
the fallback path — so with two modes sharing one file, each mode's cookie rotation looked
like an external push to the other and the two took turns going cold. Compounding it, a
rebuild of the *fallback* mode is exactly when you can least afford one: the 403 above is the
public cipher instance's known 1-in-3 wrong `n`, so the 12.2 s cold resolve landed on top of
a 4.4 s wasted one.

**Fix**, both halves needed: track the jar's **content hash** against what we last read *or
wrote*, so our own writes are invisible and only a real push registers; and share **one jar
object** across modes, reloaded in place — cookies live in the jar, not in
`_code_cache`/`_player_cache`, so a push has no business invalidating the expensive state.
Verified: `touch` alone changes nothing, an appended cookie produces one
`cookie jar was replaced on disk — reloaded N cookies, keeping warm resolver state` and every
mode stays warm. Sharing the jar also removes a hazard that predates the daemon — two
`YoutubeDL`s rotating the same Google session against each other is what `push_cookies.py`
warns about at length.

#### Every logged line is attributable

A related trap, from the same session: an `nginx 504` error page — padding comments and all —
appeared in the receiver's journal as a dozen **bare** lines, with nothing identifying who
had written them. Two causes, both fixed:

- the daemon's `log()` prefixed only the *first* line of a multi-line message, so
  continuation lines arrived untagged;
- `ytdlp.js` passed the daemon's stderr through verbatim, trusting it to tag itself — which
  cannot hold for text written by a process yt-dlp spawned (the JS runtime for the `n`
  challenge inherits that stderr) or by a Python traceback.

So `log()` now prefixes every line, yt-dlp's own messages are flattened to one line and
capped at 500 characters (they can carry a whole HTTP response), and any untagged stderr line
is forwarded as `[ytdlpd:raw]`. If that blob recurs, the log now says where it came from.

`YTCR_YTDLP_CACHE_DIR` pins yt-dlp's on-disk cache (extracted signature functions, the
downloaded ejs solver scripts) somewhere persistent — `/data/yt-dlp-cache` in the add-on,
the state dir on the Pi. It is the only part of a cold resolve that survives a restart, and
yt-dlp's default (`~/.cache/yt-dlp`) is neither persistent nor reliably writable in the
container.

Environment: `YTCR_YTDLP_DAEMON=0` disables it (back to a spawn per track),
`YTCR_YTDLP_PREWARM=0` skips the startup resolve, `YTCR_YTDLP_PYTHON` overrides the
interpreter (default: `python3` next to `YTCR_YTDL_PATH`, i.e. the venv's own),
`YTCR_YTDLP_SOCKET` overrides the socket path. The receiver's startup line says which path
is in use — `resolver: long-lived yt-dlp` or `resolver: yt-dlp per track` — and every resolve
logs whether it was served warm:

```
[resolver] resolved <id> in 2.6s via local node (daemon)
[resolver] resolved <id> in 4.1s via local node (daemon, cold)
```


#### Resolver traps already handled

Small things that cost real time to find, so that none of them is "tidied up" later:

- **Closing a listening socket does not wake a blocked `accept()`** on Linux. The daemon's
  escalation path logged "exiting" and then sat there until the next connection arrived —
  and `SIGTERM` had the same defect. `serve()` therefore polls with a 1 s timeout instead of
  trusting the close.
- **Waiting for a restart needs its own flag.** `waitUntilRunning()` first returned
  immediately after an escalation, because the child had not exited yet, so `running` was
  still true and the caller fell through to the per-track spawn it was trying to avoid.
  `#restartPending` is set when the restart is *requested* and cleared only when the
  replacement answers `ping`.
- **`MAX_WARM_AGE_S` (300 s) is not what it looks like.** A mode's `YoutubeDL` is rebuilt on
  that interval, but *not* because age causes the 403s — a session seconds old fails too.
  It is kept because a rebuild is the cheapest recurring moment to also clear the
  process-wide PO-token cache, and it costs about a second (measured: 3.4 s for a stale
  resolve against 4.3 s for a rebuilt one, the resident worker and both disk caches
  surviving).
- **A unix socket path is capped at ~108 bytes**, and `bind()` reports that as a bare
  `OSError: AF_UNIX path too long` naming nothing. The daemon checks the length itself and
  says which path it meant.
- **The daemon files have to be deployed explicitly.** `scripts/deploy-dev.sh` stages
  `receiver/*.py` alongside the `*.js` and *fails* if `ytdlp_daemon.py`, `jsc_resident.py` or
  `jsc_worker.js` is missing, and the Dockerfile `COPY`s `receiver/*.py`. Without that the
  image still plays — silently falling back to one `yt-dlp` per track and a 15 s challenge,
  which is exactly the kind of regression a deploy step should not be able to ship. The Pi
  installer copies every plain file in `receiver/`, so it needs no list.
- **`setup_pi_ytmusic.py` verifies the venv's own Python can `import yt_dlp`** and prints
  `long-lived resolver usable: YES/NO`. That import is what the daemon needs, and a failure
  is otherwise invisible: the receiver logs one warning and goes back to per-track spawns.

---

## Cookies

Needed for Premium/ad-free playback and, increasingly, to resolve at all. The jar is a
**live, rotating credential, not a config file**: `yt-dlp --cookies FILE` reads from *and
dumps the jar back into* FILE. Three consequences:

- It lives in the **state dir** and must stay **writable** by the service, or rotated
  cookies are lost. The verification checks and reports that.
- **Provision, don't sync.** Re-pushing an older export over a rotated jar rolls the session
  back and can invalidate it, so
  [`push_cookies.py`](../firmware/pi-ytmusic/push_cookies.py) refuses to overwrite a newer
  jar without `--force`.
- **Export from a dedicated browser session, then close it without logging out and never use
  it again.** If the same session stays live in the everyday browser *and* on the receiver,
  the two rotate against each other and Google invalidates both — usually within hours.
  This is yt-dlp's own documented advice and it is the difference between "works for months"
  and "broke by tomorrow".

`push_cookies.py` runs **on the workstation** (that is where a browser and a login exist)
and does: extract (`--from-browser`, via yt-dlp so Firefox's sqlite *and* Chromium's
keyring-encrypted store both work) or take an existing `--file`; **filter to Google/YouTube
domains only** — a browser jar otherwise ships every site you have ever visited; report the
login cookies and their expiry (never the values); install mode `0600` atomically; then
prove it resolves **on the target with the target's yt-dlp**.

```bash
firmware/pi-ytmusic/push_cookies.py --from-browser firefox:ytm     # extract + push + verify
firmware/pi-ytmusic/push_cookies.py --file ~/cookies.txt --inspect # look, don't push
firmware/pi-ytmusic/push_cookies.py --check                        # is the jar still good?
firmware/pi-ytmusic/push_cookies.py --from-browser firefox --addon # target the HA add-on
```

`--addon` goes through the HA host over SSH into the container's `/data/cookies.txt`
(Supervisor's add-on config is not on a mapped share), and `--addon --check` proves
resolution there.

Cookie expiry **is** inspectable (column 5 of the Netscape format, a Unix timestamp) and the
tool prints it — but treat it as a **lower bound only**: Google invalidates server-side
whenever it likes, and the `__Secure-1PSIDTS`/`-3PSIDTS` companions rotate hourly, so their
expiry says nothing about the session's life. The only real test is resolving a video, which
is why `--check` exists.

No restart is needed after a push: mpv passes `--cookies` to yt-dlp per track, so the next
song picks up the new jar. With no jar present the receiver logs a loud warning and resolves
anonymously rather than failing.

### Making the dedicated session (Firefox, measured on this workstation)

```bash
firefox -CreateProfile ytm
firefox -no-remote -P ytm     # log in to music.youtube.com, then QUIT Firefox
firmware/pi-ytmusic/push_cookies.py --from-browser firefox:ytm
# ...and never open -P ytm again
```

Two ways this fails silently:

- Extracting with `--cookies-from-browser firefox:<dir>` where `<dir>` is a **profile
  directory that exists on disk but is not registered in `profiles.ini`** yields a jar with
  ~10 cookies and *no* login cookies — a stale, signed-out store, reported without error
  because yt-dlp did find a cookie database. Pass the profile **name**, not a directory.
- A **Private Window does not work**: Firefox keeps private-browsing cookies in memory only,
  never in `cookies.sqlite`, so there is nothing to read.

### Diagnosing a jar

`--inspect` names every cookie it found, recognises the signed-out set explicitly, and
rejects a **half-authenticated** jar: yt-dlp builds YouTube's `SAPISIDHASH` header from a
SID-family **and** an APISID-family cookie, so a jar carrying only one family authenticates
nothing while looking superficially fine.

Three failure modes need opposite responses, and the tool spells them out:

| symptom | actually means |
|---|---|
| cookie/expiry errors | make a **new** export — never re-push the old jar |
| bot check, or no formats found | the cookies are fine; **yt-dlp is stale** (or the JS runtime is missing) |
| "Video unavailable" | neither — change the probe video |

That last one is not hypothetical: yt-dlp's own test video `BaW_jenozKc` has been taken
down, so every liveness check failed for a reason with nothing to do with cookies. The probe
is now `jNQXAC9IVRw` ("Me at the zoo"), overridable with `--probe-url`.

Two bugs in the tool itself, worth knowing because they look like cookie problems:
`--cookies FILE` **loads before it dumps**, so a freshly created *empty* temp file fails with
`does not look like a Netscape format cookies file` before the browser is ever read (the file
is now seeded with the `# Netscape HTTP Cookie File` magic header, which is also how it gets
to be `0600` before any cookie lands in it).

**No PO-token provider, deliberately.** Whether the current anti-bot regime needs one for an
authenticated session on this account is unestablished, and standing up `bgutil`-style
infrastructure before knowing would be guessing. The order is: provision the jar, cast, and
only if resolution fails with *bot-check* errors (rather than cookie errors) go looking —
`push_cookies.py --check`'s output says which of the two you are looking at.

---

## The receiver app

[`firmware/pi-ytmusic/receiver/`](../firmware/pi-ytmusic/receiver/) — ESM modules, no build
step (`yt-cast-receiver` ships JS + typings, so plain Node runs it):

| File | Role |
|---|---|
| `index.js` | Config from env, DIAL bind-address selection, receiver wiring, `SIGTERM` shutdown |
| `player.js` | `MpvPlayer extends Player` — the nine abstract methods, plus `end-file` → advance queue |
| `mpv.js` | JSON-IPC client: spawns one long-lived `mpv --idle`, request/reply by `request_id`, re-emits mpv events, forwards mpv's stderr into the log, respawns mpv if it dies |
| `resolver.js` | Pre-resolves the next track to a direct URL; remote-cipher-first with a local fallback |
| `ytdlp.js` | Client for the long-lived resolver: spawns it, JSON over a unix socket, respawns it, falls back to a per-track spawn |
| `ytdlp_daemon.py` | The long-lived resolver itself — one `YoutubeDL` per mode, kept warm across tracks |
| `jsc_resident.py` | A yt-dlp JS-challenge provider backed by a resident `node`, plus the preprocessed-player disk cache |
| `jsc_worker.js` | That resident `node`: holds the ejs solver closures per player version (run via `node -e`, never imported) |
| `priority.js` | The `nice`/`ionice` prefix shared by both resolve paths |
| `metadata.js` | Now-playing reporting to the add-on's API |
| `singleton.js` | Single-instance lock, so a second mpv can never join `ytm-out` |

Everything is configured by environment variable, so the systemd unit (or `run.sh`) is the
single place that describes an install: `YTCR_DEVICE_NAME`, `YTCR_SCREEN_NAME`,
`YTCR_DIAL_PORT`, `YTCR_BIND_ADDRESS`, `YTCR_AUDIO_DEVICE`, `YTCR_YTDL_PATH`,
`YTCR_JS_RUNTIME`, `YTCR_COOKIES`, `YTCR_CIPHER_URL`, `YTCR_ADDON_HOST`/`_API_PORT`/
`YTCR_RTP_PORT`, `YTCR_LOG_LEVEL`.

Pinned to `yt-cast-receiver` **^2.1.0**. A local git checkout of master says 2.1.1, which is
*unpublished* — `npm install` fails with `ETARGET`. npm's latest, 2.1.0, carries everything
used here (`dial.bindToAddresses`, all nine `Player` abstract methods,
`Constants.CLIENTS`).

### The `Player` → mpv mapping

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

Behaviours that are load-bearing:

- **`doPlay` must not resolve early.** The base class flips to PLAYING when it resolves, and
  that state is what the phone renders — so it waits for mpv's `file-loaded`, not for
  `loadfile`'s acknowledgement, which only means "queued". The timeout is generous because
  an authenticated resolve is genuinely slow; a 60 s cap reported every track as failed
  while yt-dlp was still working. mpv 0.40 also moved `loadfile`'s argument positions, which
  presents as a track that never starts.
- **`end-file` fires for stops too.** Only `reason: eof` (or `error`) may advance the queue;
  `stop`/`quit` are our own doing. A tracked flag plus the reason check keeps a `doStop()`
  from skipping a track.
- **One long-lived mpv**, `--idle=yes`, never respawned per track — both the RAM and the
  latency argument on a 512 MB box.
- **Hold the device open *between tracks*, not forever.** `--gapless-audio=yes` keeps the
  audio output open across items; an option that streams silence indefinitely would put
  ~1.5 Mbit/s of silence on the radio around the clock, because `ytm-out` transmits whenever
  it has a client.
- **Never `--no-terminal`.** It discards every mpv diagnostic, so a `yt-dlp` failure becomes
  indistinguishable from a hang. Only terminal *input* is disabled (`--input-terminal=no`).
- **Volume** goes to mpv, so the phone's slider feels right. The router's per-output volume
  stays the fan-out control — the two compose, and that is intended.
- **Position drift is expected and accepted.** `time-pos` is the position of the audio mpv
  has *decoded*, so the phone's progress bar leads the sound by the RTP jitter buffer plus
  output latency and mpv's own 1 s output buffer. Not worth correcting.

### Only one mpv may ever hold the sink

A sink *mixes* its clients, so two mpv processes on `ytm-out` produce one RTP stream
carrying both, which presents as stutter or garble and looks like a fault in the audio path.
This happened for real — a dev harness importing `mpv.js` ran alongside the service. Three
mechanisms now prevent it:

- [`singleton.js`](../firmware/pi-ytmusic/receiver/singleton.js) takes a Linux
  **abstract-namespace** unix socket in `mpv.start()`, *before* the spawn, so anything
  importing `mpv.js` inherits the guard. Abstract because it is the one lock that cannot go
  stale — the kernel drops the name when the last fd closes, including on `SIGKILL`, so
  there is no "is this pidfile real?" dance. The holder answers a connection with its pid, so
  the loser logs *who* holds it. The lock is scoped to the **socket path**, not the program,
  so two deliberately separate instances with their own sink still work.
- `mpv.js` **probes** the IPC socket instead of unlinking it unconditionally: mpv's own
  `--input-ipc-server` bind is a kernel-enforced mutex, and throwing that away is what let a
  second mpv take over the path mid-playback. A live socket is a hard stop; only an
  unlistened one is removed. A `connect()` that neither succeeds nor fails counts as live —
  refusing to start is recoverable, deleting a live socket is not.
- The top-level error handler stops mpv before exiting. It previously called
  `process.exit(1)` without `mpv.stop()`, orphaning an mpv still attached to the sink with
  nothing left to control it — which `Restart=always` then stacked another one on top of
  every few seconds.

Losing the race is not fatal: the service retries every `RestartSec`, cheaply (nothing is
spawned), and recovers by itself when whatever held the lock exits. `AlreadyRunningError` is
reported as one line rather than a stack trace, because it is an operator mistake, not a
crash.

### Now-playing metadata

[`metadata.js`](../firmware/pi-ytmusic/receiver/metadata.js) posts to the daemon's
source-generic `POST /api/now_playing/report`
([api-reference.md](api-reference.md#post-apinow_playingreport)), keyed by the **RTP port**
that identifies which source this is — exactly the way the Bluetooth bridge reports. It is
off unless an add-on host is configured, because this role must stay able to run as a pure
RTP sender that talks to nobody.

**The metadata does not come from the phone.** The Lounge plane carries video ids and
positions only (a `Video` is `{id, client, context}`, and the only `thumbnail` fields in the
library are Google-account avatars). So the title comes from mpv's `media-title` (which
`ytdl_hook` fills in from yt-dlp), and the artwork from the deterministic
`https://i.ytimg.com/vi/<id>/hqdefault.jpg` — no extra request. `media-title` is one
combined string, so artist/album stay empty: splitting them needs a second resolve per
track, which is out of scope until it annoys.

Position is refreshed every 5 s; the daemon throttles publishing and HA extrapolates in
between, so anything faster is pure radio traffic.

---

## Playback stability on the Zero 2 W

Three host-side causes of stutter, diagnosed by measuring the box (2026-08-10). None was in
the receiver's logic and none was visible in any config file.

**The audio path had no realtime scheduling at all.** Every `data-loop.0` thread of
`pipewire` and `wireplumber` was `SCHED_OTHER` at nice 0 (`ps -Lo cls,rtprio,comm` — a
`ps -eo comm` grep misses them, the thread comm is `data-loop.0`), so the RTP sender could
be preempted by any ordinary work. The distro's own mechanism does **not** fix this here:
`/etc/security/limits.d/25-pw-rlimits.conf` grants `@pipewire` `rtprio 95`, but limits.d is
applied by `pam_limits` and this box has **no `/etc/pam.d/systemd-user`** — so
`user@1000.service`, the manager that starts PipeWire, never passes through PAM and its
`LimitRTPRIO` was 0. (RTKit is running, is PipeWire's documented fallback, and did not grant
RT either.) Fixed by a system drop-in on `user@<uid>.service` setting `LimitRTPRIO=95`
(`ensure_realtime_scheduling()`), which needs a **reboot** — the limit is inherited when the
user manager starts. The verification asserts the live scheduling class.

**The prefetch resolve competed with the player, by design.** Prefetch fires right after a
track starts, so the spike lands *mid-song*: `yt-dlp` at ~26 % of a core plus the JS runtime
it spawns at **>100 % CPU and ~88 MB RSS**, for ~20 s, on a 4-core 1 GHz box with ~80 MB
free and a measured 708k pages swapped out. `resolver.js` now exec's the resolve through
`nice -n 19 ionice -c 3`, which the JS runtime **inherits** — the only way to reach that
grandchild, since `--js-runtimes` takes a bare binary path.

**`CPUWeight` must stay at 100.** `CPUWeight=50` was meant to keep a resolve from starving
RTP egress, but a cgroup weight applies to the whole service and **mpv lives in that
service** — so it throttled the player alongside the offender while changing nothing about
their relative priority. Priority is the resolver's problem; realtime is PipeWire's.

mpv is also tuned for this box: `--audio-buffer=1` (up from the 0.2 s default) is the knob
that absorbs a scheduling hiccup before the sink runs dry, and `--demuxer-max-bytes=32MiB`
bounds the demuxer cache (mpv's default is 150 MiB, enough for one long track to push a
512 MB box into swap). Note what is *not* worth touching: `--demuxer-readahead-secs` looks
like the underrun knob at 1 s, but `--cache-secs` defaults to 3600000 and overrides it
whenever the cache is on, so mpv already reads as far ahead as the network allows.

Two host-side factors were ruled *in* but are not this role's to fix: the add-on host running
`frigate-beta` at ~104 % CPU with load ~5 on 4 cores (the same state as the 2026-07-26
sendspin-stutter incident), and `brcmf_sdio_bus_rxctl: resumed on timeout` in `dmesg` — the
WiFi SDIO bus stalling on the radio this Pi also uses for Bluetooth.

---

## Operating and verifying

The Pi is reached as **`ssh david@turnerstr-bluetooth.local`** (key + passwordless sudo from
david-pc; `192.168.178.78`, Raspbian Trixie, armv7l, WirePlumber 0.5.8). The add-on host is
**`192.168.178.22`**, and its API is directly queryable on port 8099 without going through
ingress.

`setup_pi_ytmusic.py`'s own verification covers the sink, monitor cross-talk, realtime
scheduling, the cookie jar's presence and writability, an actual **resolve** (not just a
version print), the service, and the DIAL surface on the LAN address. Everything below is
silent — no audio needed:

```bash
# graph-level, on the Pi (XDG_RUNTIME_DIR is required for PipeWire CLI tools over SSH)
export XDG_RUNTIME_DIR=/run/user/$(id -u)
pw-cli ls Node | grep -E 'ytm-out|rtp-bridge'   # both roles' sinks present
pw-link -il | grep -A2 bt-bridge-capture        # must show rtp-bridge:monitor_*, NOT ytm-out
mpv --audio-device=help | grep -i pipewire      # confirm the device string
cat /sys/class/net/wlan0/statistics/tx_packets  # egress proof; tcpdump under-reports (TX offload)

# the receiver's log — plain `journalctl --user` fails here ("No journal files were
# found"): the appliance user is not in systemd-journal, but user-unit records do
# reach the system journal
sudo journalctl _SYSTEMD_USER_UNIT=ytmusic-receiver.service -f

# from any other host: ONE answer, with the Pi's LAN LOCATION
curl -s http://192.168.178.78:8099/ytcr/ssdp/device-desc.xml

# what the router thinks exists, and what is wired to what
curl -s http://192.168.178.22:8099/api/sources
curl -s http://192.168.178.22:8099/api/routing     # includes the `links` array
```

A connect logs `sender connected: … — YouTube Music`; each track logs `play <videoId>`;
mpv's own failures (i.e. `yt-dlp` breakage) surface there too. **The Pi's journal rotates
faster than a play session** (`--since -6h` returned nothing after one), so raise
`SystemMaxUse` before trying to correlate a stutter with a resolve.

⚠️ **`--test-tone` can be audible in the house.** The Bluetooth source is routed to four
outputs (Dusche + three Voice satellites), so anything that leaks into `bt-bridge-rtp` plays
in the house. Check `/api/routing` before making sound, and prefer the scripted cross-talk
assertion, which is silent for exactly this reason.

### Two things not to "tidy up"

- **`priority.session` on `ytm-out` must stay negative.** A sink also exposes a *monitor*
  source, and the Bluetooth role's loopback capture deliberately follows the **default
  source** — so `ytm-out`'s monitor is a candidate for it. `rtp-bridge` leaves
  `priority.session` unset, which ranks as **0**, so any *positive* value makes this monitor
  win: measured on the hardware, `100` bound the capture to `ytm-out:monitor_*` (and the test
  tone came out of all four speakers `bt-bridge-rtp` feeds), `-100` put it back on
  `rtp-bridge:monitor_*`. A tie at 0 would be non-deterministic, so stay negative.
- **Never hot-restart PipeWire while a phone is connected over Bluetooth.** It orphans the
  A2DP transport on this hardware and only a clean boot recovers it. `restart_services()`
  checks for a `bluez_input.*` node and *skips* the restart if a phone is connected — the
  drop-in on disk is the durable part, so applying it can wait for the next boot.
  `--force-restart` overrides.

### Systemd/unit traps already handled

- **`Environment=` values must be quoted.** systemd splits unquoted values on whitespace,
  so `Environment=YTCR_DEVICE_NAME=Turnerstr Musik` delivered just `Turnerstr` — which is
  what the phone's Cast menu would have shown.
- **Don't probe the DIAL server on loopback**, and not immediately. It binds one address on
  purpose, so `127.0.0.1` is refused even when healthy; and it opens the port only after
  mpv's IPC socket is up, which is several seconds on a Zero 2 W. The check probes the LAN
  address with a 30 s retry.
- **Deploy the whole app directory, not a file list.** A hardcoded copy list crash-looped the
  service the first time a module was added.
- In the add-on, JSON booleans from `/data/options.json` print as `True`/`False` in Python,
  which no shell comparison against `true` matches — that silently disabled metadata
  reporting on the first deploy despite the option defaulting to `true`.

---

## When casting stops working

Walk this list **before** debugging any of our own code. It is also the canary: if casting
breaks right after a phone-app update, the problem is most likely not ours.

**The Lounge log is not evidence of LAN discovery.** The receiver opens its *own* outbound
Lounge channels — one per app — and Google's servers keep them alive with `noop`. A log
showing both `(YouTube)` and `(YouTube Music)` channels idling proves only that the receiver
registered its screen; no sender has necessarily found anything. Likewise, **manual pairing
("Link with TV code") tests nothing about DIAL**: it is entirely outbound and works
off-network by design. A working TV code plus a missing Cast icon is therefore not a
contradiction.

**Establish what the LAN should show before blaming the receiver.** An `M-SEARCH` for
`urn:dial-multiscreen-org:service:dial:1` enumerates every DIAL responder in reach — a stock
Chromecast answers on port 8008 — which separates "our receiver is not advertising" from
"this app/network is not discovering anything".

**Verify the server surface directly** rather than inferring it from the app: `GET` the
device description and confirm the `Application-URL` header, then `GET <prefix>/apps/YouTube`
and expect `200` with `<state>running</state>`.

**Bind the DIAL server to one address on a multi-homed host.** A host with `docker0`
alongside its LAN interface answers every `M-SEARCH` **twice with the same USN** — once with
the LAN `LOCATION`, once with `http://172.17.0.1:8099/...`. A sender that keeps the docker0
answer cannot fetch the device description and drops the device silently, with nothing
logged on our side. `pickBindAddress()` in `index.js` picks a LAN address and warns when
there is more than one candidate; `YTCR_BIND_ADDRESS` (`--bind-address`, `bind_address`)
overrides. Also check for a second SSDP stack on UDP 1900 — a desktop `dleyna-renderer`
counts.

**`device.name` and `screenName` are two distinct strings on purpose.** Per the library,
`name` is shown when the device is found via **DIAL** and `screenName` when found via
**manual pairing** — so the picker itself tells you which path found it, for free.
(`friendlyName` would otherwise default to the hostname, which is easy to scan past.)

**Multicast has to reach the phone.** DIAL discovery is SSDP multicast and needs phone and
receiver on the same L2 segment; a guest VLAN, client isolation, or a mesh node that drops
multicast hides the receiver even when the code is perfect. This is the strongest argument
for running on the single-homed Pi, on the same WiFi as the phone.

### The queue-never-advances bug (fixed 2026-08-10)

**Symptom:** a song finishes and everything goes quiet. Reported on the add-on, but
the Pi had it too — same code.

**Cause, ours:** `doPlay` set an `#expectingStop` flag to swallow the *outgoing*
file's `end-file`, which `loadfile ... replace` produces. When mpv is **idle** there is
no outgoing file, so nothing consumed the flag — and it then swallowed the *current*
song's genuine `eof` instead. Every track played from idle (the first of a session, and
every one after a missed advance) ended in silence.

**Why the flag was unnecessary at all**, measured on mpv 0.40: replacing a playing file
emits `end-file reason=stop`, and an explicit `stop` emits `reason=stop` too. The
handler already filtered on `reason`, so the flag added nothing but the bug. It is
deleted rather than guarded. (`end-file` also carries `playlist_entry_id` if a future
change ever needs to correlate events to a specific load.)

**How it was found**, because the method generalises: the production log showed
`Player.play()` and resolves but *never* `track ended — advancing queue`. Two probes
inside the add-on container settled it — the first proved mpv does emit
`end-file reason=eof` there (so mpv was not at fault), the second drove the real
`MpvPlayer` from idle through a 19 s track and caught the event arriving with the
handler declining to act. Verified by re-running that probe against the fixed player:
`track ended — advancing queue` → `Player.next()`.

### Failure modes, in expected order

1. **`yt-dlp` vs YouTube** — cookies expire, extractors break, token requirements change.
   Frequent, usually fixed by updating `yt-dlp` (which the weekly timer does by itself).
2. **Lounge API drift** — mitigated by `yt-cast-receiver` being maintained for exactly this;
   our exposure is a dependency bump.
3. **Google drops DIAL from the YTM picker** — terminal for this approach, no mitigation.
4. **Zero 2 W resource pressure** — resolution spikes and radio contention, addressed by
   prefetch, resolver priority, realtime scheduling and one long-lived mpv.

---

## Verified, and still open

**Proven end to end on real hardware (2026-08-10):** the YTM app discovers the receiver over
DIAL and drives the full transport set; a live cast reached the player with real
`Player.play()` calls carrying track ids and positions; the Pi answers `M-SEARCH` once with
the correct LAN `LOCATION`; authenticated resolution returns `251 opus 48000Hz`; mpv played a
real `music.youtube.com` URL into `ytm-out` and pushed **4046 RTP packets in 10 s** to the
add-on (anonymous, pre-cookies: 2035 in the same window); a 5 s test tone produced +2166
`wlan0` TX packets; the cross-talk assertion passes
(`bt-bridge-capture → rtp-bridge:monitor_FL/FR`). Track changes cost **0.5 s** via prefetch
instead of 22-30 s.

**Not yet confirmed:**

- **The Pi Zero has not been redeployed** with the long-lived resolver or the resident worker.
  Every stage is measured there (6.3-7.3 s → 2.6-4.1 s for the process; 17.9 s → 4-48 ms for
  the challenge) but the two have never run together on that box, and it is the one where the
  memory budget is tight — a rebuild there costs a player re-download on a 424 MB machine.
- **How often the client ladder is needed**, and whether `web_embedded` (which serves opus)
  rescues as reliably as `web_safari` (which drops to HLS) did. One live session had two
  rescues; that is not a rate.
- **What the default client is actually being denied** when it 403s. A
  `/data/403-diagnostics/` report from a live window is the artefact that would answer it; none
  has been produced yet, because the ladder has recovered every failure so far.
- Whether `--prewarm` is a *net* win on the Zero 2 W specifically. It spends a full cold
  resolve at boot (nice 19, ionice idle, ~88 MB for the JS runtime on a 424 MB box) to make
  the first track of the first session fast. On the add-on this is free; on the Zero, if it
  ever collides with something, `YTCR_YTDLP_PREWARM=0`.
- A full album start-to-finish with **no gap** at every track change.
- Fan-out to two or more outputs in sync, with **announce/duck** behaving exactly as it does
  for any other RTP source (nothing special should be needed — worth confirming rather than
  assuming).
- Phone leaves the LAN mid-playback and comes back: session recovers or ends cleanly, with no
  wedged mpv.
- **Radio contention:** stream Bluetooth and trigger a YTM track change in the same window.
  One BCM43xx radio serves both BT and WiFi, so a prefetch download during BT playback is the
  plausible mechanism if BT dropouts ever correlate with track changes.
- **Memory:** Node + mpv + PipeWire + BlueZ on 512 MB — watch RSS and confirm it does not
  swap; SD-card wear is already a known cost of this box.
- Whether mpv's `ytdl_hook` copes with YTM's authenticated/Premium streams as smoothly as
  plain YouTube, or whether that path needs explicit format selection.
- Whether the current anti-bot regime is satisfiable with cookies alone on this box (see
  *No PO-token provider* above).

---
