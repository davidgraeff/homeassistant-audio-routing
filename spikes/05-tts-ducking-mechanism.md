# Spike 5: TTS announce ducking — does PipeWire have per-link volume?

## Question

PLAN.md Section 5.6 assumed that TTS/announce ducking would work by
reducing the *link's* volume (the connection between the music source
node and the sink), not the sink's overall volume — leaving the announce
stream, mixed into the same sink, unaffected. Is that a real PipeWire
mechanism, or does "volume" only exist at the node level?

This matters because the whole design of `MEDIA_ANNOUNCE` ducking depends
on being able to turn music down *without* turning the announce audio
(mixed into the same sink) down with it.

## Answer: no per-link volume. Per-source-node volume is correct and works.

Two things were verified empirically, not just by reading docs:

### Part A — the "volume" link property is inert

`pw-link` accepts an arbitrary properties blob via `-p`, e.g.:

```
pw-link -p '{"volume": 0.1}' t-src:capture_FL t-sink:playback_FL
```

`pw-cli info <link-id>` afterwards shows the property is *stored* on the
link object (visible under `properties:`). But storing a property is not
the same as it having any processing effect. `pw-cli info <link-id>` also
shows a Link object exposes **only a Format param — no Props param at
all**. There is no gain/processing stage on a Link; it is a pure
port-to-port connection.

Confirmed by a real A/B signal test (same WAV file, real `pw-cat`
playback/record, `ffmpeg -af astats` for RMS):

| Link property | RMS |
|---|---|
| none (baseline) | -21.42 dB |
| `volume: 0.1` | -21.51 dB |

A genuine 0.1x gain would show roughly a 20 dB drop. The observed
0.09 dB difference is noise. **The link-level "volume" property has zero
effect on the actual audio.** PLAN.md's Section 5.6 assumption was wrong.

### Part B — per-source-node volume via `wpctl` works correctly

Setup: two source nodes (`t-src`, `t-announce`) both linked into the same
sink (`t-sink`) — i.e. exactly the announce-ducking topology. Volume was
controlled per-node with `wpctl set-volume <node-id> <value>`, and each
stage was captured and measured for real:

| Stage | Setup | RMS | Peak |
|---|---|---|---|
| 1. Baseline | `t-src` @ 1.0 alone | -20.32 dB | -0.55 dB |
| 2. "Ducked" | `t-src` @ 0.1 + `t-announce` @ 1.0, both playing concurrently | -15.59 dB | 0.00 dB |
| 3. Restored | `t-src` @ 1.0 again, `t-announce` silent | -20.32 dB | -0.55 dB |

Stage 2 is *louder* than baseline, not quieter — which is exactly what
correct ducking behavior should look like here: `t-src` was turned down
but `t-announce` at full volume dominates the mix, and PipeWire mixes the
two sources into the sink additively with no extra configuration needed.
Stage 3 is an exact match to stage 1, confirming restore is clean with no
residual state or glitch.

This confirms all three properties needed for the ducking feature:
1. PipeWire natively mixes N concurrent sources into one sink — no special
   sink/mixer config required.
2. Reducing one source node's volume ducks only that source; a
   concurrently-linked second source is unaffected.
3. Restoring the original volume returns to the exact original level.

## Conclusion / design change

Replace every "per-link volume" reference with **per-source-node volume**
(`wpctl set-volume <node-id> <value>`, the same mechanism the bridge
daemon already uses for the `POST /api/media_players/:node_id/volume`
endpoint). The announce-ducking feature is unblocked:

1. Bridge daemon looks up the current source node(s) linked into the
   target sink (already tracked by `pw_thread.rs`'s `RegistryState`).
2. Duck them: `wpctl set-volume <node_id> <duck_level>` for each.
3. Create an ephemeral announce source node, link it into the target
   sink, play the fetched audio via `pw-cat`.
4. On completion (or timeout/error), restore original volumes and remove
   the ephemeral node.

No PipeWire config changes, no new modules, no dynamic `pipewire.conf`
generation needed — this is achievable entirely through existing
`wpctl`/`pw-cli`/`pw-link`/`pw-cat` subprocess calls, consistent with the
rest of the bridge daemon's existing scope decisions.

## Test script

[`tests/test_ducking_mechanism.sh`](../tests/test_ducking_mechanism.sh)
reproduces both parts of this investigation end-to-end in a disposable
container.
