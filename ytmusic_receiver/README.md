# YouTube Music Cast Receiver (add-on)

Makes the **Cast button in the YouTube Music app** target the
[PipeWire Audio Router](../pipewire_audio_router/README.md), by running a DIAL + Lounge
receiver next to it and feeding its audio in as an RTP source.

```
YouTube Music app --DIAL/Lounge--> node /app/index.js
    --JSON IPC--> mpv --> ytm-out (module-rtp-sink)
    --UDP/RTP:46002--> audio router --> routing matrix --> speakers
```

**A local, deliberately never-published add-on.** It depends on unofficial protocols and on
`yt-dlp` keeping pace with YouTube, and it plays from the owner's signed-in account. How it
works, why it is shaped this way, and every measured gotcha behind it:
[`docs/ytmusic-receiver.md`](../docs/ytmusic-receiver.md).

The receiver application is **shared** with the Raspberry Pi deployment and lives at
[`firmware/pi-ytmusic/receiver/`](../firmware/pi-ytmusic/README.md), which stays canonical —
that role is installed by `scp -r firmware/pi-ytmusic`, so it has to be self-contained.
`scripts/deploy-dev.sh ytmusic` stages a copy into `./receiver/` (gitignored) before
building, because Docker cannot `COPY` from outside its build context.

## Install

```bash
./scripts/deploy-dev.sh ytmusic
```

The image is **built on the workstation and pulled**, not built on the device: there is no
compiler involved, but assembling it still means `apt` + `npm install` + a pip venv, and
doing that under emulation on a Pi 4 is the slow part. `image:` is set in `config.yaml`,
which is what makes Supervisor treat this local add-on as image-based.

Then, in the audio router, add an **RTP source** with `port 46002`, `rate 48000`,
`source_addr 0.0.0.0` and `ignore_ssrc true`, and route it wherever the music should play.
Without it the audio has nowhere to go and metadata reports come back `404`.

Finally, provision the cookie jar from a workstation with a browser:

```bash
firmware/pi-ytmusic/push_cookies.py --from-browser firefox:ytm --addon
firmware/pi-ytmusic/push_cookies.py --addon --check
```

With no jar the receiver resolves anonymously (which YouTube Music is unlikely to tolerate
for long, and which means no Premium/ad-free playback). The jar is a live, rotating
credential — read the rules in
[`docs/ytmusic-receiver.md`](../docs/ytmusic-receiver.md#cookies) before pushing one,
especially the one about exporting from a browser session you then never use again.

## Options

| Option | Default | What |
|---|---|---|
| `device_name` | `Musik (Home Assistant)` | Name shown in the app's Cast menu |
| `rtp_host` / `rtp_port` | `127.0.0.1` / `46002` | Where the audio is sent. Loopback when the router runs on this host. Not 46000/46001 — those belong to the Bluetooth bridge and the Pi receiver |
| `dial_port` | `8098` | DIAL/SSDP HTTP port. Not 8099 — that is the audio router's |
| `cipher_url` | `https://cipher.kikkia.dev` | yt-cipher server that solves YouTube's JS challenges off-box. Empty = always solve locally |
| `bind_address` | *(auto)* | Pin the DIAL server to one local IP; only needed on a multi-homed host, where answering SSDP on the wrong address makes senders silently drop the device |
| `report_metadata` | `true` | Report now-playing (title/artwork/position) to the router's API |
| `log_level` | `info` | |

`host_network: true` is required twice over: DIAL discovery is SSDP multicast, so the phone
can only find the receiver if it shares the host's LAN interface, and the RTP target is the
router add-on on the same host. `SYS_NICE` lets PipeWire raise its data loop to `SCHED_FIFO`
so a resolve cannot starve playback. There is no ingress panel on purpose — the phone's app
*is* the control surface.

## What runs inside

[`rootfs/run.sh`](rootfs/run.sh) generates the `ytm-out` RTP sink from the options (a static
drop-in cannot know the configured host/port), starts a private D-Bus + `pipewire` +
`wireplumber`, refreshes `yt-dlp`/`yt-dlp-ejs`/`yt-dlp-remote-cipher` in the background (the
image bakes them in, but YouTube breaks extractors on its own schedule), and `exec`s node so
it becomes PID 1 and gets Supervisor's `SIGTERM` directly — its own handler stops the cast
session and mpv cleanly.

The JS runtime here is **node 22** (Ubuntu 26.04's), which yt-dlp accepts and which is ~4×
faster at the `n` challenge than quickjs; quickjs is installed as a fallback only, and is
what the armv7 Pi has to use.

The cookie jar and the DIAL identity live in `/data`, so the phone keeps recognising the
same device across restarts.

## Logs

```bash
ha addons logs local_ytmusic_receiver
```

A connect logs `sender connected: … — YouTube Music`, each track logs `play <videoId>`, and
mpv's own failures (i.e. `yt-dlp` breakage — the most likely cause of "casting connects but
nothing plays") surface there too.
