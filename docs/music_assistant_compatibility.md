# Using Music Assistant with this add-on

[Music Assistant](https://music-assistant.io/) and this project do different
jobs — MA knows *what* to play, this knows *where* it goes and when it has to be
exact (the division of labour is in the [root README](../README.md#works-with-music-assistant)).
This page is the practical half: how to connect them, and what to do when it
does not work.

## The seam: MA sends AirPlay, the add-on receives it

The add-on already has an **AirPlay-receive source** — the input a phone or a Mac
casts to. MA's [AirPlay provider](https://www.music-assistant.io/player-support/airplay/)
supports "a very wide range of 3rd party devices such as receivers and smart
speakers" and auto-detects them, so from MA's point of view this input should be
one more AirPlay speaker on the network. Nothing about that path is MA-specific:
it is the same source a phone uses, so the audio arrives as a routable input and
is grouped, delayed, volume-controlled and ducked like any other.

That means the division of labour is not a compromise on either side. MA
resolves the track, decodes it, and manages the queue — the things it is written
for. This add-on receives one stream and takes it from there.

**What this receiver is, exactly**, because two of MA's settings depend on it: an
**AirPlay 1 / RAOP** receiver, advertised over mDNS as `_raop._tcp`. The AP2
receive path in the vendored `shairplay` is behind a Cargo feature this build does
not enable, so nothing here advertises AirPlay-2 capability. Each AirPlay source
gets its own RTSP port, allocated from **5000** upwards in the order sources were
added (`sources/mod.rs`, persisted so it is stable across restarts).

### Setting it up

1. **Give the add-on an AirPlay input.** In the add-on's web UI → *Sources*, add
   an AirPlay source and name it something you will recognise in MA's player
   list (the name is what gets advertised). One AirPlay source is enough — it is
   a receiver, not a per-room thing, and one input can feed any number of
   outputs. Prefer the **first** one you created for MA: it holds port 5000, and
   MA's AirPlay-1 discovery historically only looked there (see below).
2. **Route it.** In the routing matrix, connect that input to the outputs it
   should play on. This is the step that has no equivalent in MA: the same input
   can feed one speaker, a synchronised set of them across protocols, or nothing
   at all until you say so.
3. **In Music Assistant**, the input appears as an AirPlay player. Play to it.
4. **If MA chose AirPlay 2 for it, pin it to AirPlay 1.** MA selects the protocol
   per player and its docs state that "Shairport and AirPlay 2 are currently
   incompatible" — this receiver is from that family and advertises no AP2
   capability, so RAOP is the path that can work. The protocol version is a
   per-player setting in MA (`Automatically select` by default).
5. *(Optional)* Rename or re-route at any time; MA keeps talking to the same
   receiver.

### What MA still owns, and what it does not

| | Owned by |
|---|---|
| Library, playlists, queue, artwork, providers, gapless/crossfade | Music Assistant |
| Which speakers the stream reaches, and their grouping | this add-on's routing matrix |
| Per-output volume, delay, mute | this add-on (and its Home Assistant entities) |
| Ducking under a TTS announcement | this add-on |
| Transport (play/pause/next) | Music Assistant — its player, its queue |

**Expect MA's UI to be where you see what is playing.** MA streams to AirPlay in
*flow mode* by default: one continuous stream with the tracks stitched together
server-side, which is how it gets gapless and crossfade — and, in its own words,
"most players lose metadata display in this mode". This receiver does advertise
metadata support (`md=0,1,2` — text, artwork, progress), so per-track information
may still arrive, but do not count on the add-on's now-playing or the Home
Assistant entity reflecting individual tracks while MA is the sender.

**Build the multi-room group here, not there.** MA's synchronised groups are
per-ecosystem; its cross-provider *Universal Group* explicitly does not play in
sync, because there are no shared timestamps between ecosystems, and it sends
every member the same encoded stream. If you want an AirPlay receiver, an ESPHome
speaker and a PC playing the same music in sync, route one input to all three
here and let MA see a single player.

## Deliberately not built here

Playlists, queues, gapless playback, library browsing, and `PLAY_MEDIA` on the
music-side `media_player` entities. Those are what MA is for. The music entities
this integration exposes are `SELECT_SOURCE` + volume; `PLAY_MEDIA` exists only
on announcement groups, and that is on purpose — an announcement is a short
clip mixed over the top, not a queue.

Implementing music `PLAY_MEDIA` would mean streaming decode, transport
semantics, position reporting and eventually gapless — i.e. becoming a second,
worse player next to a good one. If you want a URL played without MA in the
picture, the honest answer today is: use MA, or cast to the AirPlay input from
whatever already can.

## When it does not work

**MA does not list the AirPlay input at all.** Discovery is mDNS/multicast, so
both have to be on the same L2 network and multicast has to reach MA. The add-on
runs with `host_network: true` for exactly this reason. Check the add-on log for
the AirPlay source coming up, and confirm the name you set is the one you are
looking for.

**MA lists the first AirPlay input but not the others.** Known MA-side
limitation, and the reason the setup steps above say to use the first source: MA's
AirPlay-1 discovery assumed the instance was on port 5000 instead of reading the
port out of the mDNS record, so additional instances on the same host were
invisible — reported against multiple shairport-sync instances, which is exactly
the shape of several AirPlay sources here
([music-assistant/support#4985](https://github.com/music-assistant/support/issues/4985),
closed and labelled *Fix to be Confirmed*). If a second source does not appear,
that is the first thing to check, and a report there with your MA version is the
useful move.

**MA lists it but fails to connect, or connects and no audio arrives.** In order:

1. **Check which protocol MA picked.** If it chose AirPlay 2, that cannot work
   here — MA's own docs say Shairport and AirPlay 2 are currently incompatible,
   and this receiver is from that family with no AP2 capability advertised. Pin
   the player to AirPlay 1 / RAOP in its MA settings.
2. **Get the add-on log around the connection attempt.** The RTSP exchange is
   logged, and which method the sender gave up after is most of the diagnosis.
3. **Open an issue here with that log.** Two outcomes are possible and the log
   distinguishes them: something this receiver should tolerate and does not, or
   something MA's sender does that no receiver can be expected to accept. If it
   is the latter, a report against MA linked to that log is worth more than a
   workaround here.

> **On upstream patches:** the receiver-side interoperability work in this
> project lives as PR-ready branches against
> [`metaneutrons/shairplay-rust`](https://github.com/metaneutrons/shairplay-rust)
> and was driven by *PipeWire's* `module-raop-sink` as the awkward sender, not by
> MA — which ships its own AirPlay sender. Whether MA needs any of it is
> **untested**: no one has yet reported using MA against this receiver either way.
> The exact change and the upstream issue get named here once a real report shows
> one is required; assuming a patch is needed would be guessing.

**Audio arrives but drifts or stutters.** That is a routing/transport question
rather than an MA one — start with
[docs/system-architecture.md](system-architecture.md) and the add-on's own
diagnostics (per-node xrun badges in the routing graph), not with MA's settings.

## Announcements while MA is playing

Nothing to configure on MA's side. A TTS announcement targeted at an output or
an announcement group ducks whatever is playing there, MA included, and restores
it afterwards — MA never learns it happened, and its queue position is
untouched, because the ducking happens in the audio graph rather than in a
player.
