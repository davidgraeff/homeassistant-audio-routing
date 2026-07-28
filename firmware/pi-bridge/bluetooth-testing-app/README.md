# Bluetooth testing app

A small web console for the [Raspberry Pi Bluetooth → RTP bridge](../README.md).
Open it in a browser on the LAN and you can, live:

- **See the incoming Bluetooth audio** as a waveform envelope of the last 15 s,
  with a peak meter in dBFS;
- **See digital silence called out explicitly** — a red wash on the waveform, a
  streak timer, a silent-duty percentage and a log of completed episodes with
  durations;
- **Switch the A2DP codec** (with the honest caveats in *Codecs* below);
- **See the RTP sender's PipeWire state** — the phone → loopback → `rtp-bridge`
  chain hop by hop, the A2DP transport state, the Bluetooth byte rate, and the
  Pi's own count of UDP datagrams actually transmitted.

It exists because of the 2026-07-28 dropout investigation
([rtp-input-dropouts-plan.md](../../pipewire_audio_router/docs/rtp-input-dropouts-plan.md)),
which came down to a distinction no packet counter can make: the phone was
streaming **full-bitrate aptX whose decoded content was pure digital silence**.
Every layer looked healthy — packets flowing, sockets draining, transport
`active` — and only looking at the *sample values* showed the fault. This app puts
that measurement permanently on a screen, next to the byte rate, so the two can be
read together.

## Requirements

Nothing to install. Python 3.10+ stdlib only — no pip, no venv, no build step, no
CDN. It shells out to tools the bridge already has:

| Tool | Used for | If missing |
|---|---|---|
| `pw-record`, `pw-dump` | audio capture, graph state | the app is not useful; both ship with PipeWire |
| `wpctl` | codec profile switching (lever 1) | that lever is shown as unavailable |
| `busctl` | A2DP transport state | transport row reads "none" |
| `bluetoothctl` | connected devices, reconnect | those controls are disabled |
| `hciconfig` | Bluetooth byte rate | rate reads "hciconfig unavailable" |
| `hcitool` | link quality | quality is omitted |

Every reader degrades to "unknown" rather than taking the page down.

## Run

```bash
# on the Pi, as the bridge user (the one whose PipeWire session owns the graph)
cd ~/bluetooth-testing-app        # wherever you copied it
python3 app.py                    # then open http://<pi>:8080/
```

Options: `--host` (default `0.0.0.0`), `--port` (default `8080`), `--target`
(a PipeWire node name; defaults to the connected phone), `--verbose`.

Copy it over with:

```bash
scp -r firmware/pi-bridge/bluetooth-testing-app david@turnerstr-bluetooth.local:~/
```

To leave it running across reboots, a user unit is enough — it must run in the
**same** user session as PipeWire, so `--user`, not a system unit:

```ini
# ~/.config/systemd/user/bt-testing-app.service
[Unit]
Description=Bluetooth bridge testing console
After=pipewire.service
[Service]
ExecStart=/usr/bin/python3 %h/bluetooth-testing-app/app.py
Restart=on-failure
[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload && systemctl --user enable --now bt-testing-app
```

> It binds `0.0.0.0` with **no authentication** — it can switch codecs and
> disconnect Bluetooth devices. Fine on a trusted home LAN; don't expose it.

## What the waveform means

The capture is `s16` stereo @ 48 kHz — the same format the bridge transmits — so
what you see is directly comparable to what lands on the RTP wire. It is reduced
to one `(peak, rms)` pair per 20 ms block: 50 points/s, 750 points for the 15 s
window. The filled envelope is peak, the thin white line is RMS.

**Digital silence is `peak == 0`: every sample in the block exactly zero.** That is
categorically different from "quiet", and the distinction is the whole point:

| What you see | What it means |
|---|---|
| waveform + `A2DP in` at full rate | healthy |
| **silence + `A2DP in` at full rate** | the sender is transmitting silence, or the decoder is producing zeros — the fault is upstream of this bridge |
| silence + `A2DP in` at ~0 | the phone stopped sending, or the link dropped |
| silence + transport not `active` | A2DP was suspended |
| waveform present but `RTP out` ~0 | the bridge is receiving but not transmitting — look at the chain panel |

The episode log is the same measurement that produced the dropout table in the
investigation, just live: durations there ran 10–261 s at a 26 % silent duty.

## Codecs

**The phone picks the codec, not the Pi.** The Pi is an A2DP *sink*; in A2DP the
*source* chooses from the capabilities the sink advertises. So there is no "use SBC
now" command, and the app offers the two levers that do exist:

**1 · Switch the device profile** (`wpctl set-profile`). Some PipeWire builds expose
each codec as its own profile on the BlueZ device. Where they do, this renegotiates
in place — no restart, no reconnect. The app *discovers* whether this box offers it
and greys the control out when it doesn't, rather than pretending.

> **Measured on this bridge (2026-07-28): lever 1 does not exist here.** The phone's
> device advertises only `off` and `audio-gateway` — no per-codec profiles — so
> lever 2 below is the only way to change the codec on this Pi.

**2 · Allowed codec set.** `bluez5.codecs` is an **allow-list**, so the UI is a
row of checkboxes — tick what the Pi may advertise and the phone can only choose
from those.

It writes `~/.config/wireplumber/wireplumber.conf.d/99-bt-testing-codec.conf`:

```
monitor.bluez.properties = {
  bluez5.codecs = [ ldac aac sbc_xq sbc ]
}
```

**WirePlumber's** config, not PipeWire's — verified on the bridge (WP 0.5.8), where
`scripts/monitors/bluez.lua` does
`Conf.get_section_as_properties("monitor.bluez.properties")`. `bluez5.codecs` in
`pipewire.conf.d` is silently ignored. The `99-` prefix outranks
`setup_pi_bridge.py`'s `51-bt-rtp-bridge.conf`, and it sets a different section, so
the two merge rather than clobber.

Excluding a codec means leaving it unticked, so the *aptX-suspect* experiment is
one click: **exclude aptX** unticks `aptx` and `aptx_hd`, leaving LDAC/AAC/SBC-XQ/SBC
for the phone to fall back to. Two guards, because getting this wrong looks exactly
like the bug being investigated:

- **Ticking nothing is refused.** An empty allow-list would leave the phone unable
  to negotiate anything, i.e. silence.
- **Ticking everything writes no restriction** rather than pinning a list that
  happens to be the full set, so "allow all" really returns to free negotiation.

### Applying it safely

WirePlumber reads config only at startup, so the drop-in needs a restart — and
restarting WirePlumber *with a phone connected* orphans the bridge's loopback and
wedges the audio path (see [../README.md](../README.md), "Don't restart
PipeWire/WirePlumber while a phone is connected"). **Apply & renegotiate** therefore
runs the one ordering that is safe:

1. write the drop-in;
2. **disconnect** the phone;
3. restart WirePlumber — with nothing connected, so there is no loopback to orphan;
4. **reconnect** the phone, which renegotiates against the narrowed set.

If the disconnect fails it stops *before* the restart rather than pressing on into
the hazard. If the reconnect fails it says so and tells you to reconnect from the
phone, instead of reporting success while audio is down. *Write file only* skips
steps 2–4 and leaves the change for the next reconnect.

Deleting the drop-in by hand is equivalent to **allow all** (still needs a restart).

## HTTP API

The page is a client of this; `curl` works just as well.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/` | the page |
| `GET` | `/api/state` | one JSON snapshot: `audio`, `graph`, `counters` |
| `GET` | `/api/stream?since=N` | SSE, 5 Hz; each frame carries only envelope points after `N` |
| `POST` | `/api/capture` | `{"target": "<node>"}` — point the waveform elsewhere |
| `POST` | `/api/codec/profile` | `{"device_id": N, "profile_index": N}` — lever 1 |
| `POST` | `/api/codec/pin` | `{"codecs": ["aac","sbc"]}` — write the allow-list only; `[]` means no restriction. A bare `{"codec": "sbc"}` is accepted as a one-element set |
| `POST` | `/api/codec/apply` | same body — write **and** renegotiate (disconnect → restart WirePlumber → reconnect). Returns a `steps` array |
| `POST` | `/api/reconnect` | `{"address": "AA:BB:.."}` — force renegotiation |

Server-Sent Events rather than a WebSocket because the live feed is one-directional,
so it needs no framing library and stays inside the stdlib. The first frame
backfills the whole ring (~9 KB), subsequent frames are ~1.4 KB — about 7 kB/s.

## Tests

```bash
python3 -m unittest discover -s tests -v     # 51 tests, no Pi or PipeWire needed
```

Everything that parses external output or makes a decision is a pure function
tested against captured real-world samples — `hciconfig` counters, `/proc/net/snmp`
columns, `busctl` output, `pw-dump` graphs, codec-name matching, the silence
episode logic, the allow-list round-trip including the exclude-aptX case and both
guards, and the apply sequence's ordering and both of its failure paths (with the
service restart injected, so tests never restart a real WirePlumber — an earlier
smoke test that let it through restarted the developer's audio stack). That split
exists because this tool targets a box that is not always reachable.

## Verification status

Honest about what has and has not been exercised on real hardware.

**Verified on the bridge itself** (2026-07-28, read-only — no config was changed):
the app runs there, serves the page, auto-picks the connected phone, and captures
its live audio (peaks 12k–17k, `silent_duty` 0 %). The chain panel resolves
correctly (`bt-bridge-capture` ← `bluez_input.64_B5_…`, all three hops `running`,
`capture_bound_to_a2dp` true). Its counters agree with the hand measurements from
the investigation: **A2DP in 44.8 kB/s**, **RTP out 154.3 datagrams/s**, transport
`active` (codec 255 = aptX), link quality 215/255.

**Two bugs only real hardware exposed**, both fixed and now covered by regression
tests whose fixtures mirror the real formats:

- `pw-dump` gives **integer node IDs**, not names, on `link.output.node` /
  `link.input.node`. The parser compared them to names, so no link ever matched and
  the chain panel always claimed nothing was connected — while the unit fixture made
  the same mistake, so the tests happily agreed with the bug.
- BlueZ puts the MediaTransport object at **either** `/dev_…/fdN` *or*
  `/dev_…/sepN/fdN` — both were observed on this one bridge, hours apart. Matching
  only the first shape made the transport row read "none" on a healthy link.

Also verified on the developer box: SSE streaming and its delta/cursor logic
(750-point backfill then ~1.4 kB frames), `pw-record` capture end to end,
peak/RMS, ring trimming, target switching, graceful degradation when
`hciconfig`/`hcitool` are absent, and the allow-list write/read-back against a
throwaway `HOME` including both guards. **54 unit tests pass.**

**Not yet verified** — needs a run with a phone connected *and* a deliberate config
change, so it was left for you:

- That narrowing `bluez5.codecs` actually makes the phone renegotiate to AAC/SBC.
  The location and mechanism are confirmed from `bluez.lua`; the end-to-end effect
  is not. The codec pill shows the negotiated codec, so **exclude aptX → Apply &
  renegotiate** and watch it change (or not).
- The `Apply & renegotiate` sequence against real `bluetoothctl`/`systemctl`. Its
  ordering and both failure paths are unit-tested; the real calls have not run.
- Lever 1 is confirmed **absent** on this Pi, so its UI path is untested by
  construction — it will simply stay greyed out.
