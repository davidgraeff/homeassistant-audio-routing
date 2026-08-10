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
| authenticated resolve | ~22 s local, ~7-10 s via remote cipher | faster: the Pi 4 is ~3.3× per core |

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
| `~/.local/state/pi-ytmusic-receiver/` | state dir: the cookie jar and node-persist's DIAL identity |
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

The **first** track of a session still pays full price, because nothing can be prefetched
before the phone says what to play. That is accepted.

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
- **A long-lived resolver process was built, measured and rejected.** Four sequential
  resolves in one process: 19.5 s, 17.5 s, 15.9 s, then 16.0 s for a *repeat of the first*.
  Only ~3.5 s of a ~20 s cost is process warmth, and repeating the same video is not
  cheaper — the challenge is keyed per stream URL and the GVS PO token is bound per video id.
  A daemon to save 3.5 s was not worth a service.
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
