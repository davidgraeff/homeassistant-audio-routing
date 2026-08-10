# YouTube Music cast receiver → RTP (Pi role)

Makes the **Cast button in the YouTube Music phone app** target the house audio system, by
running a DIAL + Lounge receiver on the same Raspberry Pi as the
[Bluetooth → RTP bridge](../pi-bridge/README.md) and feeding its audio into the
[PipeWire audio router add-on](../../pipewire_audio_router/README.md) as a second RTP
source.

```
YouTube Music app --DIAL/Lounge--> receiver/ (Node)
    --JSON IPC--> mpv --> ytm-out (module-rtp-sink)
    --UDP/RTP:46001--> add-on --> routing matrix --> speakers
```

**Personal install, not an add-on feature.** It depends on unofficial protocols and on
`yt-dlp` keeping pace with YouTube, so it needs ongoing maintenance and is against
YouTube's ToS to distribute. Full rationale, work packages and hard-won gotchas:
[`docs/pi-ytmusic-receiver-plan.md`](../../docs/pi-ytmusic-receiver-plan.md).

Google Cast itself is **not** used — a Cast receiver needs a Google-signed device
certificate. DIAL + the Lounge API is YouTube's surviving pre-Cast path and needs no such
thing.

## Install

Copy the **whole directory** (the setup script installs `receiver/` from next to itself):

```bash
scp -r firmware/pi-ytmusic david@turnerstr-bluetooth.local:~/
ssh david@turnerstr-bluetooth.local \
  '~/pi-ytmusic/setup_pi_ytmusic.py --host 192.168.178.22 --name "Turnerstr Musik"'
```

- `--host` (required): the add-on / HA host the RTP is sent to.
- `--name`: the name shown in the app's Cast menu.
- `--port` (default `46001`): must match the add-on's RTP source for this role.
- `--bind-address`: pin the DIAL server to one local IP. Needed on a multi-homed host —
  answering SSDP on the wrong address makes senders silently drop the device.
- `--no-service`: set up the audio path only (no cast receiver).
- `--test-tone`: play a 440 Hz tone into the sink. **Read the warning below first.**
- `--disable`: remove this role (leaves the Bluetooth bridge alone).

Idempotent — safe to re-run. Run it **as the appliance user**, not root.

On the add-on, add an RTP source with `port 46001`, `rate 48000`, `source_addr 0.0.0.0`,
and route it wherever you want the music.

## Cookies (for Premium / ad-free, and increasingly to resolve at all)

The jar is a **live, rotating credential**: `yt-dlp --cookies FILE` reads from *and writes
back to* FILE. Provision it from a workstation with a browser:

```bash
./push_cookies.py --from-browser firefox:ytm     # extract, filter, push, verify
./push_cookies.py --file ~/cookies.txt --inspect # what's in it + expiry, no push
./push_cookies.py --check                        # is the jar on the Pi still good?
```

**Export from a dedicated or private browser session, then close it without logging out and
never use that session again.** If the same login stays live in your everyday browser *and*
on the Pi, the two rotate against each other and Google invalidates both — usually within
hours. This single rule is the difference between a jar that lasts months and one that dies
tomorrow.

The tool only ships Google/YouTube cookies (a browser jar otherwise contains every site you
have visited), installs mode `0600`, never prints cookie values, and refuses to overwrite a
newer jar without `--force` — re-pushing an old export over a rotated one can invalidate the
session. No restart is needed: cookies are read per track.

If your workstation's yt-dlp lacks the `yt-dlp-ejs` solver scripts (most distro packages
do), resolution fails on YouTube's JS challenge — *not* on your cookies. The tool therefore
passes `--remote-components ejs:github` by default, which fetches the script at runtime and
needs nothing installed. It must be **`ejs:github`**: `ejs:npm` is a valid option value but
yt-dlp downloads the challenge solver only from GitHub, and with `ejs:npm` it skips the
download and then finds no formats — a symptom indistinguishable from having no JS runtime.
Use `--ytdlp <venv>/bin/yt-dlp --remote-components none` if you would rather install the
package than fetch at runtime.

`yt-dlp` on the Pi comes from pip in `~/.local/share/pi-ytmusic-venv` — never apt (Debian's
is over a year stale) — together with **`yt-dlp-ejs`**, and updates weekly via
`ytmusic-ytdlp-update.timer`. Both are required: YouTube makes authenticated requests solve
an `n` signature challenge, which needs the solver scripts **and** a JS runtime. yt-dlp
enables only `deno` by default, and on armv7l the choice is narrow: deno and bun have no
32-bit ARM builds, and Raspbian's node 20 is rejected as `(unsupported)`. This role therefore
uses **quickjs** (`/usr/bin/qjs`, Debian package) via `YTCR_JS_RUNTIME`, while
`push_cookies.py` uses node on the workstation.

Only **authenticated** requests hit that challenge, so an anonymous test proves nothing about
it — the verification deliberately probes with the cookie jar when one is present and says so
in its output. Missing any of that looks like
"casting connects, nothing plays" — `setup_pi_ytmusic.py` therefore *resolves a video* as
part of its verification rather than just printing a version:

```bash
systemctl --user start ytmusic-ytdlp-update.service   # update now
~/.local/share/pi-ytmusic-venv/bin/yt-dlp --version
```

## Verifying

The setup script's own output checks the sink, cross-talk, the service and the DIAL
surface. Everything below is silent — no audio needed:

```bash
# on the Pi (XDG_RUNTIME_DIR is required for PipeWire CLI tools over SSH)
export XDG_RUNTIME_DIR=/run/user/$(id -u)
pw-cli ls Node | grep -E 'ytm-out|rtp-bridge'
pw-link -il | grep -A2 bt-bridge-capture     # must be rtp-bridge:monitor_*, NOT ytm-out

# the receiver's log (plain `journalctl --user` does not work here — the appliance
# user is not in the systemd-journal group)
sudo journalctl _SYSTEMD_USER_UNIT=ytmusic-receiver.service -f

# from any other host: the DIAL advert should be ONE answer with the Pi's LAN LOCATION
curl -s http://192.168.178.78:8099/ytcr/ssdp/device-desc.xml
```

⚠️ **`--test-tone` can be audible in the house.** The Bluetooth source is routed to real
speakers, and a monitor-routing mistake sends this role's audio there too. The scripted
cross-talk check exists so you can verify that *silently*; check `/api/routing` on the
add-on before making sound.

## Layout

| Path | What |
|---|---|
| `setup_pi_ytmusic.py` | The whole configurator: packages, PipeWire drop-in, app install, systemd units, verification |
| `push_cookies.py` | Runs on a **workstation**: extract/filter/push the cookie jar and prove it resolves on the Pi |
| `receiver/index.js` | Entry point: env config, DIAL bind-address choice, receiver wiring |
| `receiver/player.js` | `Player` implementation mapping cast commands to mpv |
| `receiver/mpv.js` | mpv JSON-IPC client (one long-lived `mpv --idle`) |
| `receiver/resolver.js` | Pre-resolves the next track to a direct URL (30.7 s → 0.5 s per track change) |
| `receiver/metadata.js` | Now-playing reporting to the add-on |
| `receiver/metadata.js` | Reports the playing track to the add-on (title from mpv, artwork from the video id) |

Installed on the Pi to `~/.local/share/pi-ytmusic-receiver` with state (including the DIAL
identity) in `~/.local/state/pi-ytmusic-receiver`.

## Two things not to "tidy up"

- **`priority.session` on `ytm-out` must stay negative.** The Bluetooth role's loopback
  capture follows the *default source*, and `rtp-bridge` leaves `priority.session` unset
  (= 0) — so any positive value makes this sink's monitor win, and YouTube Music is then
  also forwarded into `bt-bridge-rtp`, out of every speaker that source feeds.
- **Never hot-restart PipeWire while a phone is connected** over Bluetooth: it orphans the
  A2DP transport until a reboot. The script refuses to; `--force-restart` overrides.
