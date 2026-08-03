# Sendspin — open items

What is left after the group-churn investigation closed. The narrative, the measurements
and the wrong turns are archived in
[old/sendspin-group-churn-plan.md](old/sendspin-group-churn-plan.md); this file carries
only work that is still outstanding, so it should shrink over time rather than grow.

> **The headline is closed.** "Adding a device silences the whole group for tens of
> seconds" was **our own server dropping the client's clock-sync requests** — a 50 ms
> rate limit plus a single-slot reply channel, against a spec that puts the cadence in the
> client's hands and requires a reply to each. Fixed and confirmed on hardware
> (2026-07-29, `0.3.20260728232459`): **10 exchanges in 10.1–10.6 s, was 40.6–70.7**;
> `received == replied` in every sample; the per-device spread gone. See the archive's
> §4.14.
>
> Not the firmware, not WiFi, not IGMP, not the codec. Four days of hypotheses on the
> wrong side of the wire, because the one counter we trusted incremented *after* the drop
> check and so could not see our own drops.

---

## P1 — act soon

### 1. The satellite1 speaker answers to two names, and the sync group will lose it

A reflash surfaced a pre-existing config problem: `common/satellite1.base.yaml:42` sets
`name_add_mac_suffix: true`, and the untracked `config/satellite1-c4150c.yaml:53` sets
`name: satellite1-c4150c` explicitly — so the MAC suffix now **doubles** it. The device
went from `satellite1-c4150c` to `satellite1-c4150c-c4150c`. (The deleted
`satellite1.yaml` set no explicit name, which is why this never showed before.)

`/api/outputs` currently lists **both** `sendspin-dev-satellite1_c4150c` and
`sendspin-dev-satellite1_c4150c_c4150c` for 192.168.178.99. The stale-mDNS-cached old
entry holds the working connection and is the sync-group member; the new one is unrouted
and burns a phantom idle Opus relay.

**The failure is deferred:** on the next daemon restart the cached entry disappears and
the sync group loses satellite1 until the new name is adopted.

**Fix:** in `config/satellite1-c4150c.yaml`, either drop the explicit `name:` or set
`name_add_mac_suffix: false`; then reflash. One line plus an OTA.

> **Update 2026-08-03.** The deferred failure happened as predicted: the old entry went
> `present=false` while still being *routed*, and satellite1 only worked under the new
> name. An add-on restart has since cleared the duplicate — `/api/outputs` now lists four
> devices, all `present`, all `adopted`. **The config cause is unchanged**, so it returns
> on the next reflash or rename. See item 4 for why the phantom may have mattered for
> more than tidiness.

### 2. `ClientSyncState` silently discards a `client/state` of `"error"`

`submodules/sendspin/src/protocol/messages.rs:~324` defines only `Synchronized` and
`ExternalSource`, with **no `Error` variant and no `#[serde(other)]`**. A `client/state`
carrying `state: "error"` therefore fails to parse and the **whole message** is dropped —
volume, mute and static-delay deltas with it.

Inert today (the pinned firmware never sets it), but it is a **prerequisite for the
firmware upgrade**: sendspin-cpp #67 "report error state when out of sync" adds exactly
that value, so the upgrade's headline observability win would be thrown away silently.

**Fix:** add `Error` plus `#[serde(other)] Unknown`. The codebase already has the idiom —
`PlayerCommandType` in the same file carries `#[serde(other)] Unknown` commented
"forward compatibility". Three lines, worth doing regardless of the upgrade.

### 3. PR #520 is applied to the Satellite1 config but not committed

The LWIP/WiFi/SPIRAM sdkconfig restoration from
[Satellite1-ESPHome#520](https://github.com/FutureProofHomes/Satellite1-ESPHome/pull/520)
is applied to `config/common/core_board.yaml` in the working tree, compiled, and flashed
to satellite1. It is **not committed**, and the other three speakers do not have it.

It did **not** change the silence symptom (that was the clock-sync root cause in the banner above, now fixed) — but it is
justified on its own merits: the owner independently reports WiFi drops and unreliable
OTA, and the dropped buffers are a credible cause. Decide: commit and roll to the other
three, or revert.

**Caveat on the evidence:** the measurement that cleared it ran with the phone
disconnected, so the RX path was unloaded (~150 B/s instead of ~26 kB/s). Buffer
starvation is precisely what a *loaded* path causes, so #520 is **under-tested**, not
disproven. Re-measure with the phone connected before drawing a conclusion either way.

### 4. Intermittent: sendspin devices receive audio but render nothing (2026-08-03)

Three of four sendspin devices went silent for both music and announcements, while
satellite1 kept playing. **Cleared by an add-on restart; cause not established.**

**What was measured, and what it rules out.** Per-device bytes on the wire (tcpdump on
port 8928, announce fired mid-capture) showed each "broken" device receiving a full-rate
stream, correctly targeted:

| target | its bytes/s during the clip | the other three |
|---|---|---|
| 093ca8 | 16,041 → 21,507 → 20,289 → 21,227 → 21,301 → 20,419 | ~880 (baseline) |
| 0966f3 | 22,215 → 20,449 → 20,756 → 21,166 → 20,778 → 16,220 | ~880 |
| 096287 | 13,939 → 21,623 → 20,359 → 21,041 → 21,702 → 21,099 | ~880 |

So routing, targeting, the overlay mixer, encoding and delivery were all healthy. The
graph was minimal and correct (`bt-bridge-rtp → sync-grp-… → bridge-sendspin-capture`,
all active), and identical during an announcement — correct, since the sendspin overlay
is mixed in-process with no node involved. Clock sync was healthy too:
`received == replied` with 341k+ exchanges per device.

**Ruled out — the group lead.** The working hypothesis was that firmware which reports
neither `min_buffer_ms` nor `required_lead_time_ms` (item 6, F8) gets assumed at the
codec minimum, and that the old firmware needed more lead than the 320 ms it was given.
Raising the effective lead 320 → 900 ms did not settle it, because the restart
intervened — **but the lead was then returned to an effective 320 ms with no restart and
audio kept playing.** Same lead as when it was silent, now fine. That is good evidence
the lead was never the cause, and F8 is not implicated.

**So the cause was state that a restart cleared.** Two candidates, neither confirmed:

- **The phantom routed member** (item 1). The group had a *routed but permanently absent*
  member, `sendspin-dev-satellite1_c4150c`. Worth checking whether an absent-but-routed
  member can wedge the arming of its groupmates — it is the one anomaly that was present
  throughout the fault and gone afterwards. Against it: satellite1 under its *new* name
  played fine the whole time, and no mechanism is known that would spare one member and
  starve three.
- **Ordinary stale per-device state** in a connection 5.7 days old, cleared by the fresh
  connect (fresh time filter, fresh `stream/start`).

**What would catch it next time.** The gap is that "this device is being sent audio" and
"this device is rendering audio" were indistinguishable from the daemon, which is why
this needed `tcpdump` to investigate at all:

1. **A per-device rendered/receiving indicator in the UI.** Bytes/s per device, or
   audio-vs-silence, is the single highest-value addition — it is exactly the measurement
   above, and it should not require a packet capture.
2. **Announce admission is now logged** (2026-08-03, deployed in
   `0.3.20260803170132`): one `USER ACTION: announce -> N target(s) [...]` line per call
   with admission, on-demand sessions and skipped targets. Previously an 8 h window
   contained zero announce lines. The next occurrence is diagnosable from the log.
3. **Surface `present=false` but routed** as a stale phantom with a "forget" action, so
   candidate 1 above can be excluded in one glance rather than by reading the API.
4. **Expose `stream/clear`** — see item 5 — so recovery does not need an add-on restart.

A caution for whoever picks this up: `tone: true` is the *calibration* pattern, an
alternating 8 ms click once per second, so roughly one block in fifty carries audio. It
is easy to read as "nothing played" — I did, mid-investigation, and briefly concluded the
daemon was delivering nothing. Use `test: true` (the committed speech clip) to ask whether
a device makes sound at all.

### 5. `stream/clear` exists in the protocol and the daemon never sends it

The submodule implements the spec's reset primitive — `Group::clear_stream`: *"Ask every
member to discard buffered-but-unplayed audio without ending the stream, and reset the
timeline anchor to match"* (`broadcast_stream_clear`, `queue_stream_clear`). There are
**no references to it anywhere in `bridge-daemon/src/`** and no API for it.

That is the natural per-device "resync" action, and its absence is why recovering item 4
needed an add-on restart. What exists today is a **per-device reconnect**, which §4.10
made surgical — nudge one device's static delay and only that device drops and re-dials
(fresh connection, fresh clock filter, fresh `stream/start`), with the log confirming
"the other 3 member(s) keep streaming":

```bash
curl -X PUT localhost:8099/api/sendspin/delay -H 'Content-Type: application/json' \
  -d '{"node_name":"sendspin-dev-<slug>","delay_ms":1}'   # then put it back to 0
```

That is heavier than `stream/clear` (a full reconnect rather than a buffer flush) but it
is available now. A `POST /api/sendspin/clear` plus a UI button would be a small addition
and the right tool for a wedged device.

---

## P2 — worth doing, not urgent

### 6. Four inert spec deviations

From the 2026-07-29 audit against [Sendspin/spec](https://github.com/Sendspin/spec)
(HEAD `aa752f6`). Each was checked against the *pinned firmware* rather than assumed, and
all are currently inert — correctness debt, not live bugs. Full detail in the archive's
§4.13.

| | Deviation | Becomes live when |
|---|---|---|
| F3 | `server_transmitted` missing from `stream/start` / `stream/end` / `stream/clear`; the spec makes it non-optional and `roles/player/v1.md` defines the lead budget against it | a client validates the field, or measures its lead the spec's way |
| F5 | we add a client to `ready` for *any* `client/state`, so an `external_source` device is streamed to anyway and never sent `stream/end` (spec: MUST NOT) | a device is taken over by a local source |
| F7 | `buffer_capacity` is parsed and never read; our `MAX_QUEUED_AUDIO_FRAMES = 32` is a local invention where the spec supplies a negotiated number | a device advertises a smaller capacity than we assume |
| F8 | `required_lead_time_ms` excluded from the send-ahead — correct against spec HEAD, a violation against the era spec | the firmware starts reporting it (i.e. on upgrade) |

### 7. The device's API log subscription delivers nothing

`aioesphomeapi-logs` against satellite1 handshakes fine and then produces **zero** lines —
across a full stream stop/start, across a reboot that should have printed a boot banner,
and with no reply to a `dump_config` sent at `LOG_LEVEL_VERY_VERBOSE`. The device runs
`logger: level: debug`, so a `ESP_LOGW` should pass.

Two things known: `--strip-ansi-escapes` exists only in the `~/.local/bin` install, not
the venv's (it errors out there), which accounted for part of an earlier "empty log"; and
even with the flag removed the subscription still delivers nothing, for a reason not yet
determined.

**Until this works, an empty ESPHome log is not evidence of anything.** Fixing it is the
prerequisite for any future device-side debugging.

### 8. The firmware pin is four months stale — but the ladder is the cost

`kahrendt/esphome` @ `7a6cf5c` (2026-03-23) from the **closed** WIP PR esphome#14933,
predating `sendspin-cpp` v0.1.0. Upstream ESPHome now ships the component pinning
sendspin-cpp 0.7.0.

**Demoted:** v0.7.0 keeps `burst_size 8`, the 10 s timeout, skip-never-retransmit and the
`is_time_synced()` gate — verified against sources. It does **not** address what turned
out to be the root cause, and in one respect it is worse (`update()` runs only at burst
completion, so worst case is 8 × 10 s).

The real cost is the version ladder: the component shipped in ESPHome **2026.5.0**,
**2026.7.0 pins sendspin-cpp 0.6.1**, and **0.7.0 is only on `dev`** (≥2026.8). The box
runs **2026.4.5**, so reaching 0.7.0 means moving the *entire* Satellite1 config forward —
XMOS flasher, `fusb302b`, `tas2780`, `satellite1_radar`, the `http_request` pin and a
local TAS2780 boot patch.

Two things make a partial move attractive: **esphome#17133 "Suppress WiFi roam scanning
while playing" is already in 2026.7.0**, a release *earlier* than the 0.7.0 pin; and the
YAML mostly shrinks (the whole `external_components:` block and `http_request:` go away,
and `const` / `media_source` / `speaker_source` already exist natively in 2026.4.5 — so
the fork pin is currently overriding two components for no reason). Do item 2 first.

---

## P3 — known rough edges, accepted for now

### 9. §4.10's residual single-device gap

A static-delay change now reconnects only its own device (archive §4.10), but that device
still has a **≤3 s redial gap** while the retry pass re-supervises it, and
`server_devices` still lists it during the gap — so `has_live_sender` reads true and an
announcement aimed at it in that window is dropped. Same failure class as the old
group-wide restart, much smaller window, one device. A rapid nudge burst in the alignment
panel can extend it, which is what its warning is for.

### 10. Frontend copy edits were never type-checked

`npm run check` was not run for the §4.10 wording changes in `SettingsTab.svelte` and
`AlignTab.svelte` (the agent's worktree had no `node_modules`). They are comments and
user-visible strings only, no logic — but the check is still owed.

### 11. `.claude/` is untracked and unignored

Agent worktrees land in `.claude/worktrees/`. Add it to `.gitignore` before someone
commits one by accident.

---

## Closed, with the reasoning worth keeping

### §4.15 "stop reconnecting for a config change at all" — **superseded, measured**

The spec supports it: `stream/start` on a live stream *"updates the stream configuration
without clearing buffers"*, so nothing requires dropping the WebSocket to change codec or
lead. The plan was to do exactly that.

**It is no longer worth the risk, and here is the number.** Measured after the §4.14 fix,
a codec change — the remaining group-wide restart trigger — costs **208 ms** daemon-side
(`stopping sendspin server` 23:49:28.605 → all four `starting its stream` 23:49:28.813),
and the device-side resync is now near-instant because the opening 8-message burst
completes in milliseconds instead of timing out. So §4.15 would save roughly **0.2 s plus
one buffer refill**, down from the 20–30 s it was designed to save.

Against that, implementing it needs two things that are not cheap:

1. **The relay task must become codec-reconfigurable or restartable while connections
   persist.** It captures `relay_codec`, `codec_delay_us` and the reblocker's block size
   as locals at spawn, and owns `groups`, `client_to_node`, the mixer and the timeline.
2. **`SharedTimeline::send_ahead_us` has no setter** — it is builder-only
   (`with_send_ahead_us`) and read per `stamp()` call, so a lead change needs a submodule
   API addition. Worse, changing the lead mid-stream *is* a timeline discontinuity: raise
   it and the existing anchor looks too close, so `stamp()` re-anchors forward and leaves
   a gap. That is a player hard-sync event — the exact glitch class just eliminated.

`set_config` (codec/format) *is* already interior-mutable, so the codec half is the
tractable one if this is ever revisited. Revisit only if latency of a config change starts
mattering; otherwise the 208 ms is a fine price for not reintroducing a discontinuity.

One thing checked and found already correct: the reconciler compares the **resolved**
codec (`resolve_codec`), not the requested one, so setting a device from `auto` to the
codec it already resolved to does *not* trigger a restart.

---

## Where the history is

- [old/sendspin-group-churn-plan.md](old/sendspin-group-churn-plan.md) — the whole
  investigation: §2b–§2d the measurements, §3 the hypotheses and how each died, §4.1–§4.8
  the fixes that shipped, §4.14 the root cause, and the corrections to conclusions that
  turned out to be wrong (§4.9-A finding 1 especially — "I grepped for the consumers and
  found none" is not the same as "there is no gate").
- [rtp-input-dropouts-plan.md](rtp-input-dropouts-plan.md) — the *other* silence, at the
  Bluetooth A2DP boundary, which is a separate fault and still has one open question (is
  it the phone or the Pi's aptX decoder? the SBC test settles it).
- [architecture.md](architecture.md) §4/§5.1 — the anchor + per-device-sender model these
  all operate on.
