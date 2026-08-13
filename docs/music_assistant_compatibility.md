# Using Music Assistant with this add-on

[Music Assistant](https://music-assistant.io/) and this project do different
jobs — MA knows *what* to play, this knows *where* it goes and when it has to be
exact (the division of labour is in the [root README](../README.md#how-this-fits-with-music-assistant)).
This page is the practical half: how to connect them, and what to do when it
does not work.

## The seam: MA sends AirPlay, the add-on receives it

The add-on already has an **AirPlay-receive source** — the input a phone or a Mac
casts to. MA has an AirPlay player provider, so from MA's point of view this
input is simply another AirPlay speaker on the network. Nothing about that path
is MA-specific: it is the same source a phone uses, so the audio arrives as a
routable input and is grouped, delayed, volume-controlled and ducked like any
other.

That means the division of labour is not a compromise on either side. MA
resolves the track, decodes it, and manages the queue — the things it is written
for. This add-on receives one stream and takes it from there.

### Setting it up

1. **Give the add-on an AirPlay input.** In the add-on's web UI → *Sources*, add
   an AirPlay source and name it something you will recognise in MA's player
   list (the name is what MA advertises). One AirPlay source is enough; it is a
   receiver, not a per-room thing.
2. **Route it.** In the routing matrix, connect that input to the outputs it
   should play on. This is the step that has no equivalent in MA: the same input
   can feed one speaker, a synchronised set of them across protocols, or nothing
   at all until you say so.
3. **In Music Assistant**, the input appears as an AirPlay player. Play to it.
4. *(Optional)* Rename or re-route at any time; MA keeps talking to the same
   receiver.

### What MA still owns, and what it does not

| | Owned by |
|---|---|
| Library, playlists, queue, artwork, providers, gapless | Music Assistant |
| Which speakers the stream reaches, and their grouping | this add-on's routing matrix |
| Per-output volume, delay, mute | this add-on (and its Home Assistant entities) |
| Ducking under a TTS announcement | this add-on |
| Transport (play/pause/next) | Music Assistant — its player, its queue |

MA's own multi-room grouping is per-protocol; if you want a group that mixes an
AirPlay receiver with an ESPHome speaker and a PC, build it here and let MA see
one player.

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

**MA does not list the AirPlay input.** Discovery is mDNS/multicast, so both
have to be on the same L2 network and multicast has to reach MA. The add-on runs
with `host_network: true` for exactly this reason. Check the add-on log for the
AirPlay source coming up, and confirm the name you set is the one you are
looking for.

**MA lists it but fails to connect, or connects and no audio arrives.** This is
the interesting case, and it is a *sender-side* question: AirPlay-1 senders vary
in what they expect from a receiver during the RTSP handshake, and this
receiver's AirPlay-1 path has been broadened over time to cope with senders that
are not iOS. If you hit this:

1. Get the add-on log around the connection attempt — the RTSP exchange is
   logged, and which method the sender gave up after is the whole diagnosis.
2. **Open an issue here with that log.** Two outcomes are possible and the log
   distinguishes them: something this receiver should tolerate and does not, or
   something MA's sender does that no receiver can be expected to accept.
3. If it is the latter, the fix belongs upstream of MA — usually a dependency
   bump in MA rather than a change in MA itself. A bug report there, linked to
   the log, is worth more than a workaround here.

> **On upstream patches:** the receiver-side interoperability work in this
> project lives as PR-ready branches against
> [`metaneutrons/shairplay-rust`](https://github.com/metaneutrons/shairplay-rust)
> and was driven by *PipeWire's* `module-raop-sink` as the awkward sender, not by
> MA. Whether any of it is needed for MA specifically is **not yet established**
> — MA ships its own AirPlay sender. This section will name the exact change and
> the upstream issue once a real report shows one is required; until then,
> assuming a patch is needed would be guessing.

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
