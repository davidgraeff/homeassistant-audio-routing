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

### 2. `ClientSyncState` silently discarded a `client/state` of `"error"` ✅

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

> **Upstream check, 2026-08-13 — this is now a dated fuse, and it is the missing signal
> for item 6b.** Read from the sendspin-cpp sources at each tag:
>
> | version | `SendspinClientState::ERROR` | who ships it |
> |---|---|---|
> | v0.6.1 | enum + `to_cstr` → `"error"` exist, **never set** | ESPHome **2026.7.x** (current stable, 2026.7.4) |
> | v0.7.0 – v0.7.2 | **set on unexpected loss of sync** (`sync_task.cpp`: `enqueue_state_update(SendspinClientState::ERROR)`) | ESPHome **dev**, pinning `sendspin-cpp 0.7.2` |
>
> The trigger is a *hard sync* outside an expected alignment: playback off by more than
> `HARD_SYNC_THRESHOLD_US = 5000` (5 ms) when not priming or post-seek — "e.g. buffer
> underrun". Reported once, then `SYNCHRONIZED` again when it re-aligns.
>
> **That is exactly the receiver-side stutter indicator item 6b had to bisect by ear**, and
> today our parser would drop the whole message containing it — volume, mute and static
> delay with it. So this is no longer "worth doing regardless": it is the thing that turns
> the 180 ms guess into a measurement, and it lands the moment ESPHome cuts a release off
> dev. Do it before the upgrade, not after.

> **Done 2026-08-13 — and it is now a diagnosis, not just a parse.**
>
> * **Submodule** (`protocol/messages.rs`): `ClientSyncState` gains `Error` and
>   `#[serde(other)] Unknown`. Two tests pin what the fallback is *for* — an `error` and an
>   invented `"buffering"` both parse **and the message's volume / mute / static delay
>   survive**, which is what was being thrown away.
> * **Daemon** (`outputs/sendspin/server.rs`): `record_sync_state` records the verdict per
>   device — `out_of_sync` plus a `sync_error_count` of distinct episodes — on both the
>   dial-out and inbound paths, clearing the claim (not the history) on disconnect. A
>   transition into error logs a `WARN` naming the running send-ahead **and the knob to
>   change**; recovery logs the episode count. Repeats are absorbed: `client/state` arrives
>   with every volume tick, so only a change logs, counts or pushes a UI frame.
> * **API/UI**: `sendspin_out_of_sync` / `sendspin_sync_errors` on `GET /api/outputs`, and a
>   badge on the Outputs page — red **out of sync** while it is happening, amber
>   **"N dropouts"** once it has recovered. Silence when the device has never reported one,
>   since on `0.6.x` firmware that is indistinguishable from healthy.
>
> **Deliberately no automatic recovery.** A `stream/clear` or an automatic lead bump in
> response would be a feedback loop against a device that cannot keep up at the configured
> lead: every attempt costs audio, so a speaker underrunning every few seconds would be held
> broken by its own recovery. The lead is a site decision (item 6b); this reports, counts,
> and names the number to change. Contrast the AP2 PTP watchdog, which *does* act — there
> the fault is binary and a reconnect provably fixes it.
>
> **Inert until the firmware moves.** The speakers here run pre-`sendspin-cpp` firmware and
> ESPHome stable pins 0.6.1, which never sets the state, so nothing will light up until the
> ladder in item 9 is climbed. That is the point: the machinery is in place *before* the
> upgrade, so the upgrade's one observability win isn't silently dropped on arrival.

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

**Not the group lead — but the first version of this reasoning was wrong, so here is the
corrected form.** The hypothesis was that firmware reporting neither `min_buffer_ms` nor
`required_lead_time_ms` (item 7, F8) gets assumed at the codec minimum, and that the old
firmware needed more than the 320 ms it was given. Raising the effective lead 320 → 900 ms
was confounded by a restart.

It was then recorded here that "the lead was returned to an effective 320 ms with no
restart and audio kept playing", and that this ruled the lead out. **That premise was
false.** `GET /api/sync/settings` reported `effective 320 ms` while the running relay was
still logging `lead 870..892 ms` — see item 6. Lowering the configured lead never
re-armed anything, so the *running* lead never went back.

The conclusion survives on better evidence: the running lead was **~880 ms in both the
playing state and the silent state** (measured before and after the 2026-08-03 17:2x
reproduction below), so it does not discriminate between them. F8 is still not implicated —
but note that no test has yet run these devices at a genuinely low lead.

**Reproduction (2026-08-03 17:2x) — the most useful fact so far.** Reconnecting the
"Kitchen Zone" group from the *Music Now 2* input to *Bluetooth Bridge* and back, via the
routing graph, reproduces it on demand. That is a **source switch**, which tears the sync
group down and rebuilds it, so all members re-dial. Measured immediately afterwards, with
music playing:

- **every device receives real audio at an identical ~19 kB/s** on the wire (093ca8
  19,194 · 096287 19,229 · 0966f3 19,128 · satellite1 19,112 B/s). Not silence — music.
  So the daemon delivers to the silent devices exactly as it does to the working one.
- the graph is correct: `airplay-in → sync-grp-… → bridge-sendspin-capture ×4`, one
  anchor, fed by the current source.
- the connections are ~5 min old with `replied == recv` and a healthy ~0.77/s cadence, i.e.
  the re-dial and the clock sync both worked.

**Confirmed workaround, and it sharpens the diagnosis considerably.** Nudging a device's
static delay — a *per-device* re-dial (§4.10) — brings it back, verified on 093ca8 and then
on all affected devices across a repeat of the reproduction. So:

| operation | outcome |
|---|---|
| group rebuild (source switch) — every member re-dials into a **fresh** `SharedTimeline` | **silent** |
| per-device re-dial — one member re-joins the **established** timeline | **plays** |
| full add-on restart | plays |

Both middle rows re-dial the device, so the re-dial is not what matters. **The variable is
the timeline**: devices that join a newly-created `SharedTimeline` fail to render, devices
that join one already running are fine. That is where to look next, and it is also exactly
what `stream/clear` (item 5) addresses — it re-anchors without re-dialling, so it isolates
"needs re-anchoring" from "needs a new connection" in one test. It was not deployed when
this was reproduced; deploy it before the next attempt.

**So the cause is state that a restart clears but a reconnect does not.** Two candidates,
neither confirmed:

- **The phantom routed member** (item 1). The group had a *routed but permanently absent*
  member, `sendspin-dev-satellite1_c4150c`. Worth checking whether an absent-but-routed
  member can wedge the arming of its groupmates — it is the one anomaly that was present
  throughout the fault and gone afterwards. Against it: satellite1 under its *new* name
  played fine the whole time, and no mechanism is known that would spare one member and
  starve three.
- **Ordinary stale per-device state** in a connection 5.7 days old, cleared by the fresh
  connect (fresh time filter, fresh `stream/start`).

> **Ruled out: the "two `bridge-sendspin-capture` streams".** Flagged twice as a possible
> leaked capture from an earlier generation, which would have matched the
> restart-fixes-it/re-dial-does-not asymmetry perfectly. It is not a leak: all three
> consumers of `sendspin_capture` (the sendspin relay, the AP2 sender, the pw-sink sender)
> hard-coded the *same* node name, so a graph with AP2 receivers routed showed two
> identically-named captures — one sendspin, one AP2. Fixed by naming each after its owner
> (`bridge-sendspin-capture` / `bridge-ap2-capture` / `bridge-pwsink-capture`), so the
> graph no longer invites this mistake.

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

### 5. `stream/clear` — now exposed ✅

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
was the only option.

> **Done 2026-08-03.** `POST /api/sendspin/clear` sends the frame to one device, and the
> web UI offers it as **Resync** on each connected sendspin output. Deliberately *not*
> the library's `Group::clear_stream` helper: that also calls `timeline.reset()`, and our
> per-device groups share one `SharedTimeline`, so it would re-anchor every member of the
> group to fix one of them. Sending the frame alone leaves the groupmates untouched.
>
> A disconnected device reports `ok: false` with a reason rather than pretending: there is
> nothing to clear, and its next stream starts fresh anyway. Unlike volume/mute/delay a
> clear stores no desired state, which is pinned by
> `outputs::sendspin::volume::tests::clearing_a_disconnected_device_is_reported_not_stored`.
>
> This is the first thing to try next time item 4 recurs — and it leaves the evidence
> intact, which an add-on restart did not.

### 6. `group_lead_effective_ms` did not report the lead actually in use ✅

`GET /api/sync/settings` reported `configured 0 / floor 320 / effective 320 ms` while the
running relay logged `lead 870..892 ms` — the value left over from an earlier experiment
that raised it to 900.

That is §4.6's high-water mark behaving as designed: the send-ahead only ever rises, so
*lowering* the configured lead re-arms nothing, and `SharedTimeline::send_ahead_us` is
fixed at construction (builder-only, no setter — see the closed §4.15 note). It even
survived the group rebuild at the source switch in item 4.

The design is defensible; **the reporting is not.** The API presents
`group_lead_effective_ms` as what is in force, and it silently was not. That directly cost
this investigation a wrong conclusion — item 4's original "the lead was returned to 320
and audio kept playing" was built on it.

> **Done 2026-08-12.** It happened again — `effective 130 ms` reported while the relay
> logged `lead 899..921 ms` — and this time it cost a *user* the tuning loop, not just an
> investigation. Two fixes:
>
> * **`group_lead_running_ms`** (plus `group_lead_running`, per group with its anchor name)
>   from `GroupReconciler::running_sendspin_leads`, and the Settings page says "streaming
>   right now at X ms, not Y ms" whenever they differ, in amber.
> * **The knob is two-way.** A `PUT /api/sync/settings` calls `rearm_lead`, so a group
>   whose requirement differs from its running send-ahead restarts — which is the only way
>   *down* other than restarting the add-on. The high-water rule still governs everything
>   automatic (`sendspin_config_changed`); the re-arm is a separate, explicitly-requested
>   decision (`sendspin_lead_rearm`), because "a member left, so the lead could drop" and
>   "the user asked for less latency" deserve opposite answers. The response says how many
>   groups will restart.

### 6b. The smallest lead these speakers play at: **180 ms** ✅ (bisected 2026-08-13)

> **Result.** `group_lead_ms` **180** + `opus_floor_ms` **40** plays clean on all four
> speakers; below 180 ms of total lead there is audible stutter. Both are now the shipped
> defaults ([`DEFAULT_GROUP_LEAD_MS`](../bridge-daemon/src/routing/sync_settings.rs),
> [`DEFAULT_OPUS_FLOOR_MS`](../bridge-daemon/src/outputs/sendspin/codec.rs)), so a cold start
> begins where the hardware works instead of where the codec's block size happens to land.
>
> **The split is deliberate.** 40 ms is the *codec's* share (decode headroom, waived for
> PCM/FLAC, overridden by a device that reports its own `min_buffer_ms`); 140 ms is the
> *site's* share, which is what the group lead is for. Rolling both into the Opus floor
> would have charged a PCM group for a budget it does not spend.
>
> **What 180 ms buys, roughly** — none of it guaranteed, because 802.11 is CSMA/CA
> best-effort and offers no latency bound at all:
>
> | term | typical | note |
> |---|---|---|
> | Opus block + capture quantum | ~20 + ~21 ms | fixed; nothing is sent before a whole block exists |
> | network, median | 2–5 ms | a healthy 2.4 GHz link, medium load |
> | retransmit chain + airtime contention | 20–100 ms | the dominant term, and the reason the tail matters more than the median |
> | DTIM / power-save wakeup | 0–100 ms | interval × DTIM count, if the speaker sleeps between beacons |
> | MCU decode + task scheduling | 5–20 ms | shared with whatever else the ESP32 is doing (voice, mic) |
> | **off-channel roam scan** | **100–500 ms** | **not covered at 180 ms** — suppressed only from ESPHome 2026.7.0 (esphome#17133); this box runs 2026.4.5, see item 9 |
>
> So 180 ms covers the ordinary tail with little to spare and does *not* cover a scan —
> consistent with "clean, but stutters if I go lower". Expect the figure to move with the
> radio environment (a new neighbouring AP on the same channel, a microwave, a legacy
> 1 Mbit device hogging airtime), and re-bisect against the running value the Settings page
> now reports rather than against the configured one.
>
> **Still worth trying, and would lower it further if it works:** PCM on one speaker (no
> codec floor, no MCU decode in the budget, ~1.5 Mbit/s per device). If PCM plays clean
> markedly lower, the margin is decode-bound and the fix is item 9's firmware ladder rather
> than more buffer.

<details>
<summary>The measurements this came from (kept: they say what the sender was doing, which is what ruled it out)</summary>

Opened by item 6's fix, because the honest answer is now visible instead of hidden. The
send-ahead default has been wrong in **both** directions: 250 ms → 40 ms (2026-08-10,
commit 9f56025, on the note that Opus "plays cleanly at this lead"), and 40 ms is not
enough.

**What was measured on 2026-08-12** (four ESPHome speakers — 3× Voice PE + satellite1 —
Opus over 2.4 GHz, `airplay-in` as the source, `opus_floor_ms` set to 70 on this install):

| running lead | with music | the daemon's own numbers |
|---|---|---|
| **130 ms** (70 floor + satellite1's 60 ms static delay) | every speaker stutters continuously | 500 blocks/10 s, `ts gap 20000..20000 µs`, `lead 97..121 ms`, zero dropped writes |
| **930 ms** (after one static-delay nudge raised the high-water mark) | clean | identical: 500 blocks/10 s, exact gaps, `lead 899..921 ms` |

The sender is therefore **not** the starving party in either state. Both windows are flat,
continuous and on time; the difference is entirely how much margin the *receiver* has for
WiFi retransmits, DTIM wakeups and MCU decode. (Note ESPHome 2026.7.0's "suppress WiFi
roam scanning while playing", esphome#17133 — the box runs 2026.4.5, so these speakers
still roam-scan mid-playback. That is a credible chunk of the margin, and it is in item 9's
version ladder.)

**How the 180 ms above was then found:** by ear, since nothing about a stuttering receiver
is visible from the daemon (blind spot #1 in item 4). Two things made that loop tolerable —
the UI reports the running lead, and the knob goes down as well as up — so the procedure is
worth keeping for the next re-bisect: play music to the whole group, set the lead well
above the last known-good value, Apply, listen, and step down until stutter returns.

A caution for whoever does it: `largest packet 3 B` in the relay log means the Opus encoder
is emitting **silence** frames, i.e. nothing is playing into the anchor. Two 22-minute
windows of that were nearly mistaken for "the stutter reproduced with no music" — the real
music window (`largest packet ~600 B`) started at 21:08:56 and is the only part of that log
which says anything about audio.

</details>

---

## P2 — worth doing, not urgent

### 7. Four inert spec deviations

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

> **F7/F8 upstream check, 2026-08-13 — the firmware never reports either timing field, at
> any version.** `min_buffer_ms` and `required_lead_time_ms` do not appear **anywhere** in
> sendspin-cpp v0.6.1 or v0.7.2 (0 hits across the whole tree), nor in ESPHome's `sendspin`
> component on `dev`. `PlayerRole::Impl::build_state_fields` emits exactly `volume`,
> `muted`, `static_delay_ms` and `supported_commands` — and the spec marks both fields
> **"REQUIRED for players"** (`roles/player/v1.md`), so this is an upstream client gap, not
> a version we can wait for. **F8 therefore stays inert indefinitely**, and the group lead
> stays a site measurement (item 6b) rather than something a speaker can tell us. Filing it
> upstream is the only route to a reported value.
>
> **F7 is bounded and harmless.** The hello *does* carry `buffer_capacity`: ESPHome's
> `buffer_size` defaults to 1,000,000 bytes and the library advertises 80 % of it
> (800 kB) — ≈33 s of Opus, ≈4.2 s of PCM at 48 kHz/16/2. Our `MAX_QUEUED_AUDIO_FRAMES = 32`
> (640 ms of Opus) is far inside that, so the local invention is safe at any lead we would
> plausibly use. Note the spec's own caveat: `buffer_capacity` is a **byte** cap and can cut
> the effective queued *duration* below a requested `min_buffer_ms` — which matters for PCM
> long before it matters for Opus.
>
> Two device-side terms worth knowing for the latency budget, both new since the pinned
> firmware: `extra_startup_silence_ms` (default **50 ms**, silence inserted at stream start
> so the decode pipeline stays ahead of the sink) and ESPHome's `fixed_delay` — the Voice PE
> config sets **480 µs** for its AIC3204 DAC. Neither is an ongoing buffer, so neither
> substitutes for the group lead.

### 8. The device's API log subscription delivers nothing

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

### 9. The firmware pin is four months stale — but the ladder is the cost

`kahrendt/esphome` @ `7a6cf5c` (2026-03-23) from the **closed** WIP PR esphome#14933,
predating `sendspin-cpp` v0.1.0. Upstream ESPHome now ships the component pinning
sendspin-cpp 0.7.0.

**Demoted:** v0.7.0 keeps `burst_size 8`, the 10 s timeout, skip-never-retransmit and the
`is_time_synced()` gate — verified against sources. It does **not** address what turned
out to be the root cause, and in one respect it is worse (`update()` runs only at burst
completion, so worst case is 8 × 10 s).

> **Re-checked 2026-08-13, and there is now one concrete reason to climb the ladder.**
> Current stable ESPHome is **2026.7.4**, still pinning `sendspin-cpp 0.6.1`; `dev` pins
> **0.7.2** (released 2026-08-12), so the version numbers in this item have moved but the
> ladder has not. What changed is the payoff: **0.7.x reports `client/state: "error"` on an
> unexpected loss of sync** (item 2), which is the only receiver-side stutter signal that
> exists — the thing item 6b had to bisect by ear. Still absent at every version:
> `min_buffer_ms` / `required_lead_time_ms` (item 7), so the upgrade buys *detection*, never
> *negotiation*. The other reason remains esphome#17133 (no roam scanning while playing),
> already in 2026.7.0 — one release *below* the 0.7.x pin, so a partial move gets the
> single biggest term in the 180 ms budget without the full ladder.

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

### 10. §4.10's residual single-device gap

A static-delay change now reconnects only its own device (archive §4.10), but that device
still has a **≤3 s redial gap** while the retry pass re-supervises it, and
`server_devices` still lists it during the gap — so `has_live_sender` reads true and an
announcement aimed at it in that window is dropped. Same failure class as the old
group-wide restart, much smaller window, one device. A rapid nudge burst in the alignment
panel can extend it, which is what its warning is for.

### 11. Frontend copy edits were never type-checked

`npm run check` was not run for the §4.10 wording changes in `SettingsTab.svelte` and
`AlignTab.svelte` (the agent's worktree had no `node_modules`). They are comments and
user-visible strings only, no logic — but the check is still owed.

### 12. `.claude/` is untracked and unignored

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
- [rtp-input-dropouts.md](rtp-input-dropouts.md) — the *other* silence, at the
  Bluetooth A2DP boundary, which is a separate fault and still has one open question (is
  it the phone or the Pi's aptX decoder? the SBC test settles it).
- [architecture.md](architecture.md) §4/§5.1 — the anchor + per-device-sender model these
  all operate on.
