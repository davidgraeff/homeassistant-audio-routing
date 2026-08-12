# Microphone-assisted speaker alignment

**Goal.** Replace the by-ear alignment wizard's *judgement* step with a
measurement. The user opens the add-on UI on a phone, grants microphone access,
and the daemon computes each group member's render offset in milliseconds from the
mic signal, writes the delays, and verifies the result — instead of the user
dragging a slider until two clicks sound like one.

**What stays.** The existing session model in `align/calibrate.rs` — server-owned
playback, volume snapshot/restore, safety timeout, the two-tone click track — is
the right shape and is reused wholesale. This plan adds a measurement path beside
the by-ear path, not instead of it: by-ear remains the fallback when the phone has
no usable microphone (§4.1) or the room defeats the estimator (§5.5).

---

## 0. Status

**Viable and largely built; one feature left in the daemon; never run against real
speakers.** The microphone path is proven on hardware, the DSP is measured, the
orchestration runs end to end in tests, and the UI renders it. What remains in the daemon
is **parallel excitation** (W7); **multi-position chaining** (W12) is built, which
unblocks W6c (the chaining UI) and W8b.

Verified 2026-08-11: **450 daemon tests passing / 0 failing**, `cargo clippy --all-targets`
at exactly its 6 pre-existing warnings (2 `derivable_impls`, 4 `chunks_exact` — none from
this work), frontend `npm run check` **160 files / 0 errors / 0 warnings**,
`frontend/dist` untouched.

**§14 is the authoritative work-package list** — remaining first, then in flight, then
done. This section does not repeat it.

### What each work package taught the plan

The corrections matter more than the code, because most of them contradicted a section
written earlier:

| From | Correction |
|---|---|
| W1 | §4.2 — the constraint read-back cannot hard-refuse on absence (Safari reports nothing); §12 — Vite inlines small `?url` assets as data URLs, which `addModule()` will not load |
| W2 | §5.2 — the obvious filter shape is wrong; §5.3 — a per-period validity gate was missing; **§5.6 — a blind spot no refusal rule can cover** |
| W3 | **§10.2 — the transitivity check as specified was arithmetically vacuous** (the most important correction here); §5.3 — the ambiguity margin was overstated and drift needs a fit; §8 — a missing `PROPOSED` state and a budget optimistic by ~1 min |
| W4 | §7.1 — the learning phase does **not** parallelise in Stage 1 and the crosstalk matrix is not free; the SNR target moved 15 → 25 dB |
| W10 | §12.3.1 — **forming a group costs a reconnect wave**, twice per step |
| W11 | §12.3 — a reservation cannot literally participate in `occupancy()`, and the queued case was missed |
| W13 | **§2.4.1 — the sendspin knob is an advance, not a delay**; §1.1.2 — the *write-back* is the precision bottleneck, not the estimator |
| W17 | §12.3.2 — silencing is a per-**output** capability, not a kind property |
| W18 | §7/§12.2 understated it: an AP2 member with an unknown level was not merely level-fixed during a session, it was **silent** |
| W20 | §7's table was wrong that a pw-sink member has "none in this path" — the **level** is a per-output capability too. And the two capabilities are not one: a host can report a level with **no mute** (a virtual sink's node `Props`), and unlike the mute the level has **no fallback**, because the relay has no gain |
| W12 | **§1.2's "comparability across steps" does not apply to a chain** — what crosses a position boundary is a delay in milliseconds, not a phase, so only the position *in flight* is voided by a reconnect; §1.1's "subtract the global minimum" is not expressible with mixed polarities and had to become the §2.4.2 solver; §1.1 did not say where the aligned set *is* when two overlaps disagree (it is their mean, and half the disagreement is the joint's error) |

### Decisions taken

| Decision | Where | Why |
|---|---|---|
| Alignment does **not** change the wire codec | §2.3.1 | The codec is part of the latency chain, so calibrating under PCM and running under Opus measures a lead that does not exist |
| Real writes are **deferred to the end**; provisional delays live in the relay | §1.1.1 | A write costs a device reconnect; a per-step chain would spend its wall clock waiting for speakers to return |
| The sendspin knob is an **advance** | §2.4.1 | Settled in `Sendspin/sendspin-cpp`, the client the devices run |
| **Hold the union once**, scope positions by audibility | §12.3.1 | Forming costs a reconnect wave; audibility is free |
| Drive **AP2 volume** for the session | §7, W18 | Otherwise the level phase is a silent no-op for AP2 members |
| Mute **and level** are per-**output** capabilities, resolved in one place and re-resolved every position | §12.3.2, W17/W20 | An agent can drop mid-walk, so two members of one kind differ; the level then has no fallback and must be *reported*, not skipped |
| Calibration levels are **session-owned, not persisted** | §12.2, W19 | Survives a reload; a stored level is a good seed and a bad promise, since it depends on where the phone is |
| The **merged-peak check is dropped**, not deferred | §10.3 | Expressible, but strictly less sensitive than the residual check beside it |
| **W9 is not built yet, and will never be a user-facing toggle** | §5.6.1 | W22 can *quantify* whether §5.6 bites using data the current code already produces; and choosing between two DSP front-ends is a harder judgement than the one this feature exists to remove |

### What is not yet trustworthy

1. **§5.6 is the top technical risk and no amount of code closes it.** An early reflection
   inside the analysis window biases a speaker 1–2 ms and passes *every* check, including
   §10.2's — whose tolerance must absorb loudspeaker crossover split and codec band-split
   too. Only W9 fixes it properly.
2. **The write-back, not the estimator, is the precision floor** (§1.1.2): integer-ms
   knobs round by up to 0.5 ms per member, and pw-sink's 15 ms playout floor means a
   sub-15 ms correction cannot be expressed at all.
3. **The deferred-write scheme rests on an unmeasured equivalence** (§1.1.1): relay delay
   ≡ device delay. W21 is one reconnect, spent early, that turns it into a fact.
4. **Never run against real speakers** (W22). Formation cost, mute-settling time, real
   reconnect duration and whether the amplitude-stability gate survives a real room are
   all unmeasured. Every figure in this document comes from unit tests, synthetic signals
   or the single W0 mic spike.

**Document precedence.** The design was revised on 2026-08-11 for the real target
scenario — a large apartment where no single position hears every speaker. That added the
three-mode wizard on the **Outputs** page (§1, §12.1), multi-position chaining (§1.1) and
the exclusivity requirements (§12.3). Earlier sections written against a single-group,
source-card framing are **superseded wherever the two disagree**.

---

## 1. What "aligned" means, and the three modes

A single microphone in a single place measures **electrical delay + acoustic
propagation delay**, and cannot separate them. At ~3 ms per metre of path
difference, a mic 2 m from speaker A and 5 m from speaker B reads a 9 ms offset
that is entirely real and entirely geometric. This is not a DSP problem to solve;
it is a product decision about what the feature promises.

Three modes, one estimator, different orchestration. The user picks on the wizard's
first page (§12.1):

| Mode | User does | Aligns | Good for |
|---|---|---|---|
| **Multi-position** (default) | Aligns a locally-audible set, then repositions and aligns the next set through an overlap (§1.1) | Each region at *its* listening spot, regions made mutually consistent through the overlaps | A large apartment — the case where no single position hears everything |
| **Near field** | Walks to each speaker in turn, holds the phone at it | The *wire* — path difference collapses below ~1 ms, so it is right everywhere | Whole-house coherence: correct while walking around, not at N specific spots |
| **Manual** | Today's by-ear wizard | Whatever the ear decides | Fallback when the mic is unusable (§4.1) or the estimator refuses (§5.5) |

A single-position group is just multi-position with one step, so it needs no mode of
its own.

Near field has a bonus property: with the mic stream **open while the user walks**,
the analysis grid is just `frame_index mod period` on a continuous capture (§3), so it
survives the walk; a **closure measurement** (revisit the first speaker at the end)
separates accumulated clock drift from real offsets. It removes chaining entirely —
one reference frame, no overlaps, no error accumulation — at the cost of walking to
every speaker.

### 1.1 Multi-position chaining

Maintain a set of already-aligned speakers with their applied delays. Each subsequent
step selects a locally-audible set **plus overlap members** from the aligned set.

At the new position, measure everything including the overlaps — an overlap's arrival
already includes its applied delay. Target = the **latest** arrival among the step's
members. Each new member's delay follows from that target. If a *new* member arrives
later than the overlap, the overlap must gain Δ — and because **a common delay added
to an already-aligned set preserves that set's internal alignment**, Δ is applied to
*every* member of the aligned set, not just the overlap. That is the whole trick, and
it is what makes the chain work.

So the overlap's delay is the step's floor: new members are placed relative to it, and
it only ever moves *up*, dragging its whole history with it.

**Renormalise globally at the end.** Every step can only add delay, so the floor
ratchets upward across an apartment. After the last step, subtract the global minimum
from every speaker: a common shift, so all relative alignment survives, and the floor
returns to zero. Without it you accumulate latency for nothing and risk crossing the
send-ahead high-water mark (§9.2), which costs a group-wide reconfiguration rather
than a per-device reconnect.

**Use two overlaps, not one.** With a single overlap the chain has no redundancy at
its most dangerous point: that one measurement is applied as a common shift to the
entire aligned set *and* anchors everything downstream, so a §5.6 reflection bias on
it propagates to the whole apartment undetected. Two overlaps have a *known*
relationship from the previous step, so measuring both gives an independent estimate
of the step's error — spatial redundancy, which is far stronger than §10.2's cross-band
check. Disagreement beyond tolerance must refuse the step rather than poison the chain.

Expectation-setting on that tolerance: two overlaps will **not** measure as identical
at the new position, because the previous step aligned them at the *previous* position
and their path difference here is different. What is checkable is that the disagreement
stays inside plausible geometry, so the tolerance is a few ms — still enough to catch
the failures that matter.

**What chaining does and does not guarantee.** Each step inherits the reference frame
of the position it was measured from. A, B, X coincide at P1; X, Y, Z coincide at P2.
A and Y are only *indirectly* related, so in the doorway between the two rooms they
are approximate. That is inherent to one microphone, and given the premise that no
single position hears everything it is the right trade. Users who want corridor
coherence want near field.

### 1.1.1 Provisional delays live in the relay; real writes happen once, at the end

**Decided 2026-08-11.** A real delay write costs a device reconnect — tens of seconds
(§2.3) — so a chain that writes per step would spend most of its wall clock waiting for
speakers to come back. Instead the chain applies its delays **provisionally, in the
daemon**, and writes the real knobs **once** after the last step.

The mechanism is a **per-device delay line**, not per-device tone synthesis. Every
per-device relay already calls `mix_into(node_name, block, &mut buf)`
(`sendspin_server.rs:824`, `ap2_server.rs:408`, `pwsink_server.rs:220`); a per-device
ring buffer read at an offset of *d* samples emits older content against the unchanged
timestamp schedule, so the device renders it later — exactly a delay.

Why this shape rather than the §6.3 synthesis:

- **Transport-agnostic.** A delay line needs no presentation timestamp and no timeline
  anchor — it only buffers — so it works for sendspin, AP2 and pw-sink alike. This is
  the thing §6.3 could not do.
- **Sample-accurate despite block-granular relays.** Reading a ring at an offset is
  indifferent to block boundaries, so it is not quantised to Opus's 20 ms frames.
- **Nothing is persisted.** A daemon restart mid-session drops the provisional delays
  and leaves the user's stored config untouched — safer than per-step writes.
- **Renormalisation becomes free.** Between positions it is just an offset change, so
  §1.1's write → renormalise → write collapses to "adjust, verify, walk on".

**The assumption this rests on, and how to retire it.** Relay-side delay *d* and
device-side static delay *d* must produce the same audible shift. They are applied at
different points — upstream buffering vs. the device's own renderer — and a per-transport
constant difference would make every position's verified alignment wrong by that constant
once the real write lands. §10's verification would catch it, but only after the whole
apartment had been walked.

So **measure the equivalence once, early**: on the first group, apply *N* ms to one
speaker as a relay offset and measure the shift; then apply the same *N* ms as a real
device delay and measure again. One reconnect, spent at the start, turns the scheme's
foundation from an assumption into a measured fact — and if the two disagree, the
difference is a per-transport constant to correct for.

### 1.1.3 What the equivalence experiment can actually measure (W21)

Three corrections to §1.1.1/§1.1.2, from building it:

1. **"A per-transport constant to correct for" is not measurable — and mostly is not the
   problem.** §1.1.2 item 3's two-post-reconnect difference cancels any constant,
   including exactly the one §1.1.1 hoped to find. What *is* measurable is the **scale**
   and the **sign**, and those are the ones that matter: a constant is a common shift
   within a kind, and a common shift is free (§2.4.2), whereas a gain error *g* leaves
   every member wrong by `(g − 1)·d_i`, which nothing absorbs. A constant *between* kinds
   does matter and nothing measurable on one speaker can see it.
2. **Two reconnects are not enough either.** They are tens of seconds apart, and a 100 ppm
   phone clock creeps ~6 ms across one — against a step §9.2 caps at 20 ms. So each arm is
   **bracketed** (baseline → changed → baseline) and the device arm costs **three** writes.
   Without the bracket the experiment reports a ~30 % scale error that does not exist.
3. **ε is a number, not an argument.** The part of reconnect variation the bracket cannot
   separate from drift becomes a floor under the reported resolution, so the claim degrades
   honestly when two reconnects land differently.

The step is **20 ms = exactly one Opus frame**, which leaves the content-to-frame phase
untouched and so nulls §1.1.2 item 2's MDCT confound *for this measurement* — at the cost
of saying nothing about item 2 for an arbitrary delay.

Two incidental findings worth keeping: **AP2's render delay is applied live** by
`set_output_latency`, contradicting §2.4 and `sync_settings.rs:19`'s "on next reconnect";
and `set_sendspin_delay_handler` reconnects **unconditionally**, which is what makes a
`from → from` write a usable symmetric baseline.

**Consequence for §2.3.** If the equivalence holds, "tens of seconds per delay change"
governs only the final write, not the iteration loop. The open-loop design does not
change (the estimator still returns milliseconds directly, so there is nothing to
search for), but a failed verification becomes cheap to retry.

### 1.1.2 What the delay line can and cannot stand in for (W13 findings)

**Confirmed unaffected, checked against the code rather than assumed:** shifting content
against a fixed timestamp schedule does **not** touch the send-ahead lead or any jitter
buffer. The line emits exactly as many bytes as it consumes, one block per block, at the
same instant. `SharedTimeline::stamp` derives its stamp from block *length* plus the
clock and is called once per block *before* the per-device fan-out; `applemidi_sender`
advances its RTP timestamp by `frames_per_pkt`; the AP2 sender stamps whatever it is fed.
None of them can observe that the content is older.

Four ways relay-side and device-side delay still differ:

1. **The sign may be inverted for sendspin — see §2.4.1.** This is the big one and it is
   not a property of the delay line; the line is a true delay, and the knob it stands in
   for may be an advance.
2. **Codec frame phase — inherent, not calibratable.** The line sits *upstream* of
   `sendspin_codec::Reblocker`, so a delay that is not a whole multiple of the codec frame
   (20 ms for Opus) moves a transient to a different position inside the MDCT window,
   whereas the device knob shifts rendering with the content-to-frame phase untouched.
   The decoded audio is still delayed by exactly *d*, but Opus's smearing is
   window-position dependent, so the measured *peak position* can move by a fraction of a
   frame the device knob would never produce. This is the concrete mechanism behind
   §2.3.1's "sub-millisecond peak position through Opus is unverified". sendspin-with-Opus
   only; PCM is exact and FLAC is lossless.
3. **The device arm of the experiment needs two reconnects, not one.** §1.1.1 budgeted
   one, which is not enough: writing the knob forces a reconnect, and that restarts the
   stream, clock sync and buffer fill, any of which can shift that device's offset by ε.
   "Relay *d*, no reconnect" versus "device *d*, after reconnect" measures *d ± Δ ± ε* and
   cannot separate the two. The device arm must be a **difference of two post-reconnect
   measurements**: write 0 → reconnect → measure, then write *N* → reconnect → measure.
4. **What gets written is not what was applied, and this is where sub-ms alignment dies.**
   The line is sample-accurate (20.8 µs at 48 kHz); the knobs are integer milliseconds,
   so the write-back rounds by up to 0.5 ms per member. Worse, pw-sink's playout delay has
   a hard floor of three packet times (`PWSINK_JITTER_MIN_MS` = 15 ms), so a sub-15 ms
   pw-sink delay **cannot be written at all**. Independent of any equivalence question,
   the write-back — not the estimator — is the precision bottleneck.

And one operational asymmetry: only the **real** write feeds `required_send_ahead_us`, so
only it can cross the group's high-water mark (§9.2). The provisional delays never do,
which means the walk cannot feel the mark approaching. Check it before writing, not after.

### 1.1.4 Five things this section did not say, from building it (W12)

1. **Where the aligned set "arrives" at a new position is ambiguous, and the mean is the
   answer.** §1.1 says to measure two overlaps and refuse on disagreement but never says
   what to do when they disagree *within* tolerance — which is the normal case. Their
   arrivals bracket the aligned set, so the anchor is their **mean**, and **half the
   disagreement is that joint's error**, reported per step and summed across the chain
   (`ChainError`). One overlap gives no such estimate at all, so a chain containing one
   single-overlap step reports **no total** rather than a total with a hole in it.
2. **The tolerance is 8 ms, and what it is checking is not precision.** What two overlaps
   read apart at a new position is a *difference of differences* — how their relative
   geometry changed between the two spots — which is real and unbounded by anything in
   the capture. 8 ms ≈ 2.7 m of that at §1's 3 ms/m; the failures it exists to catch (an
   overlap that was never aligned, a wrapped phase, a §5.6 reflection lock at +5 ms, a
   speaker that was moved) are 5 ms to hundreds. A 1–2 ms per-speaker bias sails through
   it, exactly as it sails through §10.2.
3. **Refusing the *step* is not refusing the run.** Everything already aligned is still
   good and still carries its provisional delays, so a disagreement (or a failed
   transitivity/repeatability at that position) parks the chain with the reason and lets
   the user stand there and try again. Only the run's *bindings* — session, capture, delay
   line, cancellation — are fatal. Losing an apartment's chain to one bad joint would be
   the wrong trade.
4. **"Subtract the global minimum" is not expressible.** With mixed polarities the
   minimum is not a delay anyone can subtract (§2.4.2), so the renormalisation *is* the
   interval solver: feed it `max(p) − pᵢ` as each member's arrival and its own target
   choice becomes the free common shift — which it then picks to keep the largest knob
   smallest (§9.2). A sendspin-only chain still lands on its earliest member at advance 0.
5. **The chain's state has to be what the line is *applying*, not what the step solved
   for.** The line and the knobs are both whole milliseconds; a model carrying the exact
   ideal would disagree with reality by up to 0.5 ms per step, and that error would land
   in the alignment between the aligned set and every position after it. Rounding when
   the delay is applied keeps the two identical, and the error stops accumulating because
   every later position measures its overlaps *through* the line.

### 1.2 The reference frame is one continuous microphone capture

This is the rule both modes' overlap requirements follow from, and it is worth stating
once rather than rediscovering per mode.

**Everything measured within a single continuous capture is comparable**, because the
analysis grid is just `frame_index mod period` on that stream (§3) — no clock sync with
anything. **Nothing is comparable across captures:** a mic reconnect restarts
`align_mic`'s frame counter *and* a new session restarts the click player, so both the
grid origin and the content phase move by unknown amounts. W3 already handles this
inside a run (`grid_epoch`, which discards a set and restarts once); this is the same
fact one level up.

Consequences:

- Multi-position needs an overlap **per step**, because each step is its own position
  but also — if the capture was interrupted — its own frame.
- Near field needs **no overlap within a walk**, however long: keep the mic stream open
  while walking between floors and the whole house is one frame, with the closure
  measurement absorbing drift. It needs an overlap only to link **separate sessions**.
- A near-field overlap is *better* than a multi-position one. Measured at arm's length,
  the bridge is a clean wire-delay comparison; a multi-position overlap is measured
  across a room, so its Δ carries a path-difference ambiguity that can only be bounded
  (§1.1). Cost: one extra stop, ideally a speaker reachable from both walks.

So the advice to users is: **prefer one continuous session for everything that should
be coherent**, and split only when you must.

**Correction from W12: a chain does not depend on capture continuity across positions,
and saying it does costs the user a re-walk.** The first bullet above is the right rule
stated slightly too strongly. What crosses a position boundary in a chain is a
**provisional delay in milliseconds**, not a phase, and every position re-measures its
overlaps in its own frame — so a capture that reconnects *between* two positions costs
nothing, which is what this section's own parenthetical ("if the capture was interrupted —
its own frame") is pointing at. What is genuinely not survivable is a reconnect *inside*
one position, and that voids **that position only**: its readings are discarded, the user
stands there again, and everything aligned earlier is untouched. Each step therefore
records its own `grid_epoch`, no two steps' observations are ever compared, and the
honest bound on a joint is the overlap disagreement (§1.1.4) rather than the capture's
continuity. Voiding the whole chain instead would send someone round the apartment again
for nothing.

Two practical costs this section originally glossed over:

- **The session had a 15-minute deadline that a whole-house walk would hit.** The safety
  watchdog was a one-shot timer armed at `start` and never re-armed, so a long walk died
  mid-walk as a lost session — making the advice above impossible to honour. It is now an
  **idle** timeout: each arrival re-solos its speaker, so a walk refreshes it without the
  measurement code knowing the watchdog exists. A genuinely abandoned session still tears
  down.
- **A mid-walk mic reconnect costs the *user* a re-walk**, not the daemon a loop. The
  `grid_epoch` machinery this section points at restarts an internal set; here the same
  event sends someone round the apartment again. Budget it as a user-visible failure and
  allow more restarts than the internal path does, because each one is expensive to the
  person, not the process.

---

## 2. What exists today, and the corrections building it forced

### 2.1 The session

`align/calibrate.rs` owns one alignment session at a time:

- `click_wav()` (`align/calibrate.rs:60`) builds a 2 s S16LE/44.1k stereo loop: an 8 ms
  Hann-enveloped 3000 Hz burst ("A") at t=0, an 8 ms 1500 Hz burst ("B") at
  t=1.0 s, amplitude 0.5.
- `start()` (`align/calibrate.rs:213`) resolves the group, snapshots sendspin volumes,
  and loops the WAV into the group's **sync anchor** via
  `pw::player::play_loop_to_target(anchor, …)` (`pw/player.rs:79`).
- `apply_audibility()` (`align/calibrate.rs:319`) solos the reference + target and mutes
  everyone else — sendspin via the protocol mute (`sendspin_volume.rs:293`, a live
  transient push, no reconnect), AP2 via `ap2_volume::set_muted`.
- `arm_timeout()` tears the session down after 15 min so a closed tab cannot leave
  the room muted with a click looping.

Members are `Sendspin | Airplay2` only (`align/calibrate.rs:92`); pw-sink outputs are
not modelled.

### 2.2 Correction: both speakers play both tones

The A/B alternation is not a per-speaker label. One WAV goes into one anchor, and
`apply_audibility` only gates *audibility*, so reference and target both emit the
3 kHz A burst and the 1.5 kHz B burst. A/B exists so that a target slipped by a
whole click lands its A on the reference's B, making the error audible — it
disambiguates the ±2 s window for the *ear*.

Consequence: there is no "overlap of two different frequencies" to detect today.
Any per-speaker frequency labelling is new work (§6.2).

### 2.3 Correction: a delay write costs tens of seconds, not 5

`set_sendspin_delay_handler` (`api.rs:3117`) persists the delay, pushes it live,
and then — unless `sendspin_delay_live` is on, which it is not by default
(`settings_store.rs:52`) — calls `force_device_reconnect` for that one device,
because current ESPHome firmware reads the static delay only at stream start.
Hardware-confirmed: a reconnecting sendspin device takes **tens of seconds** to
render again regardless of how the previous session ended. AP2 render delay is the
same story — `sync_settings.rs:19` notes it takes on the next membership/rate
reconnect.

This is the single most important constraint on the orchestration, and it has a
clean answer: **the estimator returns milliseconds directly, so there is no search
loop.** Measure everything, write once, verify once (§8). Do not build a
hill-climber; at ~30 s per iteration it would be unusable, and it is unnecessary.

### 2.3.1 Decided: alignment does **not** change the wire codec

A session must measure through whatever wire codec the group actually runs. This was
considered and rejected on 2026-08-11, and the reason is a correctness one rather
than a preference:

**The codec is part of the latency chain, not a neutral transport.** Both
`codec_delay_us` and the send-ahead floor are codec-dependent
(`sendspin_codec.rs:337` derives Opus's floor from `opus_floor_ms`). Calibrating
under PCM and then running under Opus would measure a lead that does not exist in
normal operation, so the offsets would not transfer — the substitution would quietly
invalidate its own results. The codec's constant contribution to delay is something
you *want* folded into the measured offset.

It follows that verifying one's chosen codec actually works is a precondition the
user owns, not something alignment papers over.

**What this costs, recorded honestly.** A lossy transform codec is entitled to smear
an 8 ms transient: Opus's MDCT window spreads it, and per-band bit allocation differs
between the 1.5 kHz and 3 kHz channels. A *constant* per-codec delay is harmless
(it is calibrated out), but a *frequency-dependent* one lands directly on §10.2's
cross-band transitivity check, whose tolerance must therefore absorb codec band-split
on top of loudspeaker crossover split. Another reason that check is weaker than it
looks. Measured detectability through Opus is not the problem — 65 dB and 70 dB peak
SNR on the two channels in a real room — but sub-millisecond peak *position* through
Opus is still unverified.

### 2.3.2 A wedged member is a real state, and a reconnect clears it

Hardware-observed 2026-08-11: a sendspin device can wedge such that a stream renders
**intermittently** — the click track audibly breaking up — while the daemon side is
provably clean (relay logs showed 50.0 blocks/s, a constant 20000 µs timestamp gap,
and zero drops throughout). Switching codec cleared it; so did switching back to Opus
and nudging the static delay, because that forces the device to reconnect. So the
cure is a reconnect, and the fault is past the last point the daemon can observe.

Consequence for the gate: intermittency must be its own diagnosis. Caught by the
amplitude-spread check instead, it is reported as "hold the phone still" and sends
the user after entirely the wrong thing — see `GateReason::Intermittent`, which fires
only after the tone has been *heard and then lost twice*, so a mute settling is not
mislabelled as a broken stream.

### 2.4 The knobs

| Member kind | Delay knob | API | Applies |
|---|---|---|---|
| sendspin | static delay, 0–5000 ms, integer ms | `PUT /api/sendspin/delay` | on device reconnect (or live if `sendspin_delay_live`) |
| AP2 | render delay, ms | `PUT /api/outputs/{node}/latency` | on next reconnect |
| pw-sink | playout delay, ms, floored at 3 packet times | `PUT /api/outputs/{node}/latency` | on next reconnect |

### 2.4.1 RESOLVED: the sendspin knob is an **advance**, and the solver must model knob *ranges*

This plan said "all three are additive only — you can delay a speaker, never advance
it", and therefore that the reference must be the *latest*-arriving member (§9.1). For
AP2 render delay and pw-sink playout delay that is right. **For sendspin it appears to
be exactly backwards**, and the solver in `align/measure.rs` currently assumes the
wrong sign.

Three independent readings, all pointing the same way:

- The reference client subtracts it: `server_to_local_instant_with_latency` does
  `instant.checked_sub(output_latency_micros)`
  (`submodules/sendspin/src/sync/clock.rs:397`), moving the target instant **earlier**.
- Its comment says so outright: *"Emit each sample `delay` earlier so downstream
  (amp/speaker) latency lands it on time"* (`synced_player.rs:919`) — i.e. it is an
  **output-latency compensation**, not a delay.
- Our own daemon agrees: `required_send_ahead_us` adds each member's `static_delay_ms`
  to the group lead *"because the device plays that much earlier"*
  (`sendspin_server.rs:520`).

If that is how the ESPHome firmware behaves, then for sendspin you can only ever
**advance** a speaker (0–5000 ms), never delay it, so:

- a sendspin-only group's reference is the **earliest**-arriving member, not the latest;
- a mixed group holds knobs of **both signs** at once, and the solver has to know which
  is which per member kind;
- the write-back stays expressible — `advance_i = max_j(d_j) − d_i` reproduces the same
  relative geometry, since a common shift is free — but the numbers are **inverted, not
  offset by a constant**, so this is not a calibration, it is a sign bug.

**Settled 2026-08-11 in the client the devices actually run.** The ESPHome speakers use
`Sendspin/sendspin-cpp`, and `src/sync_task.cpp:593` computes the target playback time as

```cpp
client_timestamp = get_client_time(entry->timestamp)
                 - get_effective_static_delay_ms() * US_PER_MS
                 - config.fixed_delay_us;
```

The static delay is **subtracted** from the target instant, so a larger value plays
**earlier**. Five independent confirmations now agree — the C++ firmware client, the Rust
reference client, its comment, the protocol field doc, and our own send-ahead
calculation. Asking by ear was not viable, which is the point of this feature.

### 2.4.2 What that means for the solver: knob *ranges*, not a reference member

"Reference = latest arrival, add delay to everyone else" is wrong in both directions. Each
member's knob has a **polarity and a range**, so the right model is a feasible-interval
intersection, not a chosen reference.

For member *i*, let τ_i be its measured arrival with its **current** knob value:

| Kind | Knob | Arrival as a function of the knob | Achievable arrivals |
|---|---|---|---|
| sendspin | advance `a ∈ [0, 5000]` ms | `τ_i + a_i − a` | `[τ_i + a_i − 5000, τ_i + a_i]` |
| AP2 / pw-sink | delay `d ∈ [D_min, D_max]` | `τ_i − d_i + d` | `[τ_i − d_i + D_min, τ_i − d_i + D_max]` |

`D_min` is 0 for AP2 but **15 ms** for pw-sink (`PWSINK_JITTER_MIN_MS`, three packet
times), so a pw-sink member cannot be placed arbitrarily early.

The common target **T** must lie in the intersection of every member's interval.

- **Empty intersection ⇒ refuse**, naming the two members whose ranges do not overlap.
  This is a real case in a mixed group: sendspin can only move *earlier* from where it
  sits and AP2/pw-sink only *later*, so a group whose sendspin member is already the
  earliest and whose AP2 member is the latest is only alignable using whatever headroom
  their current values provide.
- **Otherwise pick the T in the intersection that minimises the largest knob value.**
  Both polarities cost latency — an AP2 delay directly, and a sendspin advance because
  `required_send_ahead_us` adds it to the group lead — so this generalises §9.2's "keep
  the smallest applied delay as small as possible" while keeping the high-water mark low.
- For a **sendspin-only** group this reduces to `T = min_i(τ_i + a_i)`: the member with the
  earliest *intrinsic* (knob-zero) arrival gets advance 0, and every other member is
  advanced to meet it. So the reference is the **earliest** member — exactly inverted from
  what §9.1 says.

**New consequence for §9.2:** the send-ahead high-water mark is now driven by *advances*
as well as delays, so the check must consider both.

---

## 3. The measurement principle

**Everything is measured as a relative time within one continuous microphone
stream.** The daemon never needs to know when a burst was emitted in the phone's
time base, and the phone's clock never needs to be synchronised with anything.
This deletes an entire class of hard problems: WebSocket jitter, browser timestamp
resolution, NTP, PTP, network delay.

Why it works: every member renders the *same content* from the *same anchor
stream*. Member *i* renders content-frame *N* at
`T_content(N) + delay_i + path_i`. Differencing two members eliminates
`T_content(N)` entirely, leaving exactly `(delay_i − delay_j) + (path_i − path_j)`
— the thing we want, plus the geometry term §1 is about. This holds across
transports (sendspin, AP2, pw-sink) without any of them sharing a clock, because
the phase reference lives in the *audio content*, not in a clock.

Requirements on the mic stream:

- **Gapless and monotonic.** Count frames; never derive time from wall-clock
  timestamps or `AudioContext.currentTime`.
- **Drop detection.** A sequence number per chunk; a gap invalidates the window in
  progress rather than silently shifting every subsequent measurement.
- **Continuity across a measurement set.** For sweet-spot mode, all members are
  measured within one loop pass, so drift is irrelevant. For sequential soloing
  (§6.1) and near-field walking, keep the socket open and track loop phase
  continuously so drift is observable and removable.

---

## 4. Microphone ingest

### 4.1 Permission and secure context — checked

The HA ingress view renders the add-on UI as:

```html
<iframe title=${this._addon.name} src=${this._addon.ingress_url} @load=…>
```

(verified in the frontend bundle on the live instance,
`hass_frontend/frontend_latest/12563.*.js`). There is **no `allow=` attribute and
no `sandbox=` attribute**. That turns out to be fine, not fatal: `ingress_url` is
a same-origin path (`/api/hassio_ingress/<token>/`), and the Permissions-Policy
default allowlist for `microphone` is `self`, which covers same-origin iframes. So
`getUserMedia` needs no `allow="microphone"` and nothing sandbox-blocks it.

**The real gate is secure context.** On `http://homeassistant.local:8123`,
`navigator.mediaDevices` does not exist — plain HTTP on a LAN hostname is not a
secure context, and only `localhost` is exempt. This instance has no `ssl_*` in
`configuration.yaml`'s `http:` block but does run `core_duckdns` +
`core_nginx_proxy`, so the HTTPS path exists and **that** is the URL this feature
must be used and tested on.

Product consequence: "open Home Assistant over HTTPS" becomes a documented
precondition, and the UI must detect the insecure case and say so plainly rather
than failing with a permission error. There is no workaround.

Still untested from a shell, and needing a real device: a phone actually granting
the permission, and iOS/Android browser behaviour inside that same-origin iframe.
**Do this before building anything else.**

### 4.2 Capture constraints — non-negotiable

```js
getUserMedia({ audio: {
  echoCancellation: false,   // AEC would cancel exactly the signal being measured
  autoGainControl:  false,   // AGC makes envelope peaks meaningless
  noiseSuppression: false,   // NS is a non-linear time-varying filter
  channelCount: 1,
}})
```

Echo cancellation is the dangerous one: it is *designed* to remove loudspeaker
sound from a mic signal, and it adapts over seconds, so a session that starts fine
degrades as it converges.

Read back `track.getSettings()` — but the check **cannot be a hard refusal on
absence**, which is what this plan originally said. Safari omits
`echoCancellation` / `autoGainControl` / `noiseSuppression` from `getSettings()`
entirely, so refusing when they are unreported would rule out every iPhone, and
§4.1 makes the phone the target device. So:

- an explicit `true` → refuse, and explain *why* AEC destroys the measurement;
- unreported → proceed, but surface it as a visible caveat in the UI.

That leaves a real hole — an iPhone silently applying AEC looks identical to an
iPhone not reporting. §13 tracks it; the practical detector is behavioural rather
than declarative: AEC converges over seconds, so a burst amplitude that decays
monotonically across a measurement is the signature.

### 4.3 Transport

`AudioWorkletNode` → 128-frame blocks → batch to ~20 ms → `Int16Array` → binary
WebSocket frame with a 4-byte sequence number. At 48 kHz mono that is ~96 kB/s,
which is nothing on a LAN. Handle a 44.1 kHz `AudioContext` (iOS) as well as
48 kHz; carry the rate in a JSON hello frame.

New endpoint `GET /api/align/mic/ws`, upgraded with `axum`'s ws feature —
`routing::routing_ws` (`routing.rs:738`) and `pwsink_agent::agent_ws` are the
in-repo precedents for the handler shape. One socket at a time; a second
connection is rejected. Closing the socket does **not** tear down the session (the
user may be switching modes), but the 15 min safety timeout still applies.

The server also sends **one text frame back** before any audio flows —
`{"type":"ready",…}` or `{"type":"error","reason":…}` followed by a close. Without
it, the "another capture is already connected" rejection reaches the browser only
as an opaque close and the client cannot tell refusal from a dead socket.

**The socket is deliberately not bound to an alignment session.** Binding was in
the original sketch, but the mic check has independent value *before* a session
exists — it is how the user finds out their phone works at all — and the binding
belongs at the orchestration layer (W3), where the loop-phase gate lives anyway.

Back-pressure policy: if the send queue stalls, the client drops blocks and bumps
the sequence number so the server sees a gap and discards the window. Never buffer
to catch up — a stale window is worse than a missing one.

---

## 5. The estimator

### 5.1 Signal

Start with the existing click track. An 8 ms Hann-enveloped burst has ~250 Hz
null-to-null main-lobe width and gives ~0.3–1 ms peak-position accuracy at
reasonable SNR — comfortably matching the 1 ms granularity of the delay knobs.

If real rooms turn out to smear the peaks too much, the upgrade is a 200–300 ms
log chirp (200 Hz → 8 kHz) inside the same 2 s loop, correlated with a matched
filter: ~25 dB more processing gain against reverb and noise. Keep the audible
clicks in the loop alongside it so the by-ear path and the user's mental model
still work. This needs an FFT (`rustfft`, small pure-Rust dep, overlap-save) —
which is exactly why it is the *second* implementation, not the first.

### 5.2 Detection — v1 needs no new dependencies

Per channel: a complex bandpass at the channel's centre frequency → magnitude
envelope → peak pick → parabolic interpolation on the envelope maximum for
sub-sample resolution. Two channels for the A/B click track, N for the parallel
mode (§6.2). O(n) with a handful of multiplies per sample; runs comfortably on a
tokio task off the RT threads.

**The filter shape matters more than this plan originally admitted.** A single
boxcar/Goertzel integrator matched to the burst length is the obvious choice and it
does not work well: the correlation peak it produces is quartically flat, and the
negative-frequency image leaves a ripple at 2·f_c whose crests become local maxima,
so the picked peak hops between crests. Measured: ±0.5 ms per-period spread and a
0.15 ms bias at 30 dB SNR.

The fix costs one extra running sum and no dependency — **two cascaded boxcars of
L/2, i.e. a triangular analysis window**, whose stopband is the boxcar's squared.
Measured improvement: ±0.04 ms spread, 0.013 ms bias — about 20×. Use a
linear-phase integrator rather than a resonator or one-pole, for the same reason
§6.2 cares about group delay: a frequency-dependent delay in the *analysis* filter
becomes a per-channel bias, which is exactly what must not happen.

### 5.3 Phase, epoch and ambiguity

Every arrival is reduced to a **phase within the 2 s pattern**. The A/B frequency
labelling identifies *which* burst was detected, so *arrival identification* is
unambiguous over the full 2 s.

**This section overstated the margin.** Identification is unambiguous; the measured
*spread* still wraps at ±1 s, and nothing distinguishes a wrap from a small offset.
A group misaligned by more than a large fraction of the period must therefore be
refused rather than silently mismeasured — the implementation refuses beyond
0.4 × pattern (800 ms) and says to rough the group in by ear first. AP2 members
carrying 800 ms render delays (`sync_settings.rs:465` uses exactly that as its test
value) sit right at that boundary, so this is not theoretical.

Two further corrections from building it: **§6.1's alternation is not sufficient for
drift.** It reduces the error but leaves a residual, and removing that needs a pooled
common-drift slope fit, which needs **≥2 measurement passes** — so two passes are
structural, not padding, and a single-pass run must carry an explicit "no drift
correction applied" warning.

**Near field is the exception, and it is not a loophole.** Its closure measurement
(§1.2) gives *one* member — the anchor — a second reading, and that is all a slope needs.
Fed to the same pooled fit, the anchor is the only member with two points, so the slope
reduces exactly to `closure_error / walk_span` and every single-reading member's offset
becomes `phase − slope · time`. The closure measurement **is** the drift fit rather than
a second mechanism beside it, so a near-field walk is one pass **plus** closure and must
*not* carry the no-drift-correction warning.

Average over several loop periods. Noise averages down as √N; reverb does not (it
is deterministic for a fixed mic position), which is why the chirp upgrade exists.

**A per-period validity gate is required and was missing from this plan.** The
period grid origin is arbitrary, so a burst can straddle a period boundary and that
period then contributes a pure-noise phase to the fit. Periods below an SNR gate
are therefore excluded from the fit but **still counted in the reported SNR** — so a
hopeless capture is refused as low-SNR rather than quietly disappearing as "too few
periods", which would tell the user nothing about what went wrong.

### 5.4 Output

The estimator returns, per member: phase in ms, a peak SNR, the ratio of the
largest to the second-largest peak in its channel, drift in ppm, and a **standard
error** from the spread across loops. Everything downstream consumes the
uncertainty, not just the point estimate.

The straight-line fit's intercept needs an **origin, shared across channels** — the
abscissa is centred on the aggregation window. Per-channel origins would make the
intercepts incomparable and inflate the standard error with pointless
extrapolation.

### 5.4.1 Measured accuracy (W2, synthetic signals)

Recorded so later work argues from numbers rather than impressions. Delta error on
a 17.3 ms injected offset:

| Condition | Result |
|---|---|
| Noiseless, 48 kHz / 44.1 kHz | +0.005 ms / +0.012 ms |
| 30 → 5 dB broadband SNR | 0.014 → 0.012 ms |
| 0 dB broadband SNR | 0.065 ms (still accepted) |
| −3 dB and below | refused |
| +100 ppm clock drift | drift reported as +100.0 ppm; delta still 0.02 ms |
| Reverb train, +7…+71 ms at −4…−19 dB | direct arrival found, 0.14 ms, accepted |

Broadband SNR here is burst peak / noise RMS; the *reported* peak SNR runs ~17 dB
higher. The cliff from "0.15 ms" to "meaningless" spans about **3 dB**, and the
15 dB peak-SNR threshold sits on the safe side of it. `std_error_ms` is the
sharpest discriminator across that cliff (0.06 ms → 51 ms).

**Noise is not the limiting factor.** A phone in a quiet room has 20+ dB more SNR
than this needs. §5.6 is the limit.

### 5.5 Refusal rules

The estimator must be willing to say no. Refuse — and fall back to by-ear — when:

- peak SNR below ~15 dB in any channel after the learning phase has done its best;
- second-peak ratio too close to 1 (ambiguous which arrival is the direct sound);
- standard error across loops above ~1 ms (user moving, drifting, or a bad room);
- any clipped block in the window (§7);
- a sequence gap in the window.

Writing a delay from a bad measurement is much worse than not writing one: it
silently degrades a system the user previously had aligned by ear.

### 5.6 The blind spot the refusal rules cannot cover

**None of the §5.5 checks can detect an early reflection that lands inside the
analysis window.** It merges with the direct arrival into one peak and pulls it. To
every verdict the result looks excellent: the peak is single and strong
(second-peak ratio > 10), and because the reflection is deterministic the *same
wrong answer* arrives every period, so the standard error is under 0.01 ms. The
estimator is confidently wrong.

Measured with a 0.9× reflection on one channel: −1.72 ms at +1 ms, +0.89 at +2 ms,
−0.93 at +3 ms, −0.40 at +5 ms, ~0 at +8 ms. With the reflection *louder* than the
direct sound (1.4× at +5 ms) the estimator locks onto the reflection and reports
+5.2 ms as accepted. Beyond the window, a reflection becomes a separate peak and is
correctly refused as ambiguous — so the exposure is bounded by the guard distance,
not unbounded.

This is codified as a **passing test** (`an_early_reflection_biases_silently`) with
asserted bounds, so a future change that fixes or worsens it will be noticed.

Three consequences, all load-bearing:

- It is the real argument for **W9** (chirp + matched filter) — but W9 is **gated on
  evidence, not on argument**: see §5.6.1. W9 resolves the
  direct arrival from an early reflection instead of merging them. W9 was framed as
  a noise-driven upgrade; it is not — §5.4.1 shows noise is a non-problem. W9 is a
  *reflection* fix, and whether it is needed depends on what W3 sees in real rooms.
- It makes plan §10's transitivity check non-negotiable — but **that check is far
  weaker than it first appears.** Transitivity as literally specified turned out to
  be arithmetically vacuous here (§10.2 explains why), and the frequency-band form
  that replaced it must carry a 3 ms tolerance to avoid firing on loudspeaker
  crossover differences. So a pass does **not** prove this blind spot was avoided;
  1–2 ms of per-speaker bias sails through every check in the design. This is the
  single strongest argument for W9.
- Near-field mode (§1) is a partial mitigation for free: at arm's length the direct
  sound dominates any reflection by far more than the 0.9× that produced these
  numbers.

### 5.6.1 Decided: W9 waits for evidence, and will never be a toggle

**Decided 2026-08-12.** W9 (chirp + matched filter) is the only proper fix for the blind
spot above, and it is nonetheless **not built yet**. Three reasons, in order of weight.

**A toggle would ask the user the one question this whole feature exists to remove.**
Choosing between two DSP front-ends is a strictly harder judgement than "did that speaker
move earlier or later", which is already beyond the ear — that is why we measure. A "use
chirp" checkbox hands someone a decision they have no instrument to make, and no way to
know afterwards whether they chose right.

**The need can be measured with what already ships.** §10.2's cross-band transitivity
check computes, per member, `split_i = phase_B(i) − phase_A(i) − 1000 ms` — the split
between the 1.5 kHz and 3 kHz arrivals. That *is* the reflection signature: a merged early
reflection biases the two bands differently, a clean direct arrival does not. So **W22
should read the distribution of `split_i` across a real run.** Splits clustering near zero
say reflections are not biasing much and W9 buys little; splits clustering at 1–3 ms say
they are. That is a far cheaper experiment than implementing a chirp, and it is available
the moment this is deployed.

**The cost is not the FFT, it is the second calibration.** Every threshold here was derived
against the 8 ms Hann burst: `MIN_PEAK_SNR_DB`, the second-peak ratio, the ~12 ms guard
distance, §10.2's 3 ms tolerance, §1.2's closure rate bound, §1.1's 8 ms overlap tolerance.
A chirp changes peak shape and guard distance, so all of them need re-deriving — and
several are currently justified by *measured* numbers rather than argument. Add `rustfft`
and FFT correlation on a 4-core Pi with a documented history of CPU starvation, and
"optionally enabled" stops being a flag and becomes a second configuration of the
estimator, validated against synthetic signals just like the first — which is still itself
unvalidated against a real room.

**If W22's splits say it bites**, the shape is an **automatic escalation**, not a
preference: measure with the click track, and when the evidence points at a reflection (a
persistently non-zero cross-band split, or a marginal second-peak ratio) switch *that
member* to the chirp, re-measure, and report that it did so and why. The daemon decides,
because the daemon is the only party holding the numbers — the same principle as the
per-output silence and level channels (§12.3.2). A narrower first step is legitimate too: a
chirp offered as an on-demand **diagnostic** on one member ("why is this speaker
uncertain?"), outside the default path, which needs no recalibration of the run.

**The argument on the other side, recorded rather than buried:** if the bias does bite,
not having W9 ready means another development cycle before anyone can align an apartment
properly. That is a real cost. It loses to the risk of calibrating a second signal path on
the same unvalidated foundation as the first.

---

## 6. Excitation: sequential vs parallel

### 6.1 Stage 1 — sequential solo-alternation

Keep the shared anchor stream exactly as it is. Alternate the solo between members
every loop period using the existing live `set_mute`, and measure each member's
phase in its own window.

- **Zero new audio plumbing.** Reuses `play_loop_to_target` and
  `apply_audibility` as-is.
- **Works for every member kind**, because the phase reference is carried in the
  content (§3) and is therefore transport-agnostic.
- Alternating (rather than doing all of A then all of B) makes mic-clock drift
  average out instead of accumulating.
- Mute is not sample-precise — it lands within the stream's send-ahead window —
  so leave one loop period of guard between the switch and the first accepted
  window. That guard is the same "wait for a confirmed tone sequence" gate that
  covers reconnects and settling; implement it once as *re-acquire loop-phase lock
  with stable amplitude before accepting a window*.
- Cost: ~8–10 s per member per pass. Linear in N.

This is the stage that proves the estimator against real rooms and real phone
microphones, which is where the actual risk lives.

### 6.2 Stage 2 — parallel, frequency-division with per-device gating

Give every member its own burst frequency and measure all N in a single loop pass.
Measurement time becomes roughly **independent of N**: a handful of loops, 10–20 s
for the whole group, versus 10 s × N.

The mechanism that makes this work across transports is not per-device tone
*synthesis* — see §6.3 for why that does not generalise — but **per-device
filtering of shared content**:

1. The anchor stream carries a 2 s loop containing all N bursts, each at its own
   frequency, **time-staggered** into its own slot.
2. Each per-device relay applies a narrow **bandpass at that device's assigned
   frequency**, so each speaker emits only its own burst.
3. The mic separates the channels by frequency; the assigned slot is a
   cross-check on identity, not the identification mechanism.

Why filtering rather than synthesis: a filter needs **no timing information at
all**. It does not need to know the content position, a frame index, or a
presentation timestamp — which is precisely what the AP2 and pw-sink relays cannot
provide (§6.3). The injection point already exists in all three:
`overlay_mixer::mix_into(node_name, …)` is called per device in
`sendspin_server.rs:824`, `ap2_server.rs:408` and `pwsink_server.rs:220`. A
sibling `cal_gate(node_name, block, &mut buf)` slots in beside it.

Design notes:

- **Filter delay is a differential bias.** A biquad's group delay depends on its
  centre frequency and Q, so different channels are delayed differently. Two
  options: a linear-phase FIR (constant delay for all channels, exact by
  construction, but ~129 taps × 48 kHz × 2 ch per device is real work on a relay
  thread that is already tight on a 4-core Pi), or a 2–3 biquad cascade (~15
  MAC/sample, cheap) with **analytic group-delay compensation** per channel. Take
  the biquad cascade; the delay is known at design time to well under 0.1 ms, and
  it is a constant, not a measurement error. Verify it in a unit test by filtering
  a synthetic burst and checking the peak shift against the analytic value.
- **Time-staggering is free and worth doing** even though the bandpass makes it
  unnecessary for identification: only one speaker is energised at a time, so the
  room's reverb floor stays low. With simultaneous bursts, every speaker's reverb
  tail lands in every channel and the floor rises with N — that, not band
  crowding, is the real scaling limit. With a 2 s loop and N slots, a 6-speaker
  group gets 333 ms slots, tolerating ±166 ms of slippage before slot confusion,
  and the frequency label resolves it even beyond that.
- **Frequency assignment must avoid harmonic collisions.** A burst driven hard
  produces 2nd/3rd harmonic distortion in the speaker; if that lands on another
  channel's centre frequency it creates a stable spurious peak at the wrong time,
  which is much more dangerous than noise. The clean solution for N ≤ 4 is to keep
  the whole set inside one octave, e.g. **2000 / 2500 / 3050 / 3700 Hz** — spacings
  ≥ 500 Hz, and 2×2000 = 4000 sits 300 Hz above the highest channel. For N > 4 the
  set has to be found by a small constrained search (no member within ±300 Hz of
  2× or 3× another, ≥500 Hz spacing, inside the phone-mic + small-speaker usable
  band of roughly 800 Hz–6 kHz). Do not hand-wave a table: **the learning phase
  measures actual per-channel crosstalk for free** (§7), so validate the
  assignment empirically at runtime and reassign or fall back to Stage 1 if a
  channel pair fails.
- **Something must still pump the graph.** The relays only run when capture
  delivers blocks, so `play_loop_to_target` keeps running — it is what feeds the
  sink. The per-device gate shapes that content; it does not replace the source.

### 6.3 Why per-device synthesis does not generalise (recorded so it is not retried)

The tempting alternative is to synthesise each device's tone as a pure function of
a shared timeline position — sample-aligned by construction, no filter cost, best
possible SNR. It works, but **only inside the sendspin per-device relay**, which
has exactly what is needed: `let ts = timeline.stamp(len)` is computed once per
block and fanned identically to every member (`sendspin_server.rs:818`), so
`tone(ts, device_i)` is coincident across sendspin members by construction.

The other two transports have no such anchor:

- `ap2_server.rs`'s relay (`ap2_server.rs:386`) just fans PCM chunks to each
  `LiveFrameSender`; there is no presentation timestamp in the loop, and it runs
  from its **own** capture (`spawn_with_rate("ap2", …)`, `ap2_server.rs:371`) with
  an independent frame origin.
- `pwsink_server.rs` is the same shape.

Using each relay's local monotonic "now" instead would inject that relay's
scheduling jitter (~one quantum, 10–20 ms) straight into the measurement — far
worse than the sub-millisecond target.

So: synthesis is an optional optimisation for **all-sendspin groups**; the
filtering approach in §6.2 is the one that covers mixed groups. If both are built,
pick per group by member composition. If only one is built, build §6.2.

---

## 7. The level-learning phase

Needed, and for a sharper reason than "until the mic can hear it". The constraint
is **two-sided**:

- Clipping is broadband, so one clipped mic block corrupts **every** channel, not
  just the loud speaker's. With AGC off (§4.2) nothing manages headroom, so the
  *sum* of all speakers must stay under the mic's ceiling.
- Each *individual* channel needs margin over the noise + reverb floor in its own
  band. **Target 25 dB, not the estimator's 15 dB refusal threshold** — this plan
  originally said 15, which is wrong: §5.4.1's cliff is only ~3 dB wide and 15 dB
  sits *on* its safe edge, leaving zero margin for the floor moving between the
  learning pass and the measurement. There is a second reason in §7.1.

A near speaker can easily be 20 dB hotter at the mic than a far one, so this is a
per-speaker gain solve, not a global volume.

### 7.1 Two claims in this section were wrong

**"The learning phase parallelises too" does not hold in Stage 1.** Under the shared
click track every speaker emits *both* bursts (§2.2), so the measurement channels
are shared and a per-member SNR cannot be attributed from an all-play round at all.
Stage 1 must ramp **sequentially**, one solo per member; only Stage 2's per-speaker
frequency assignment makes a parallel ramp possible. The solver therefore carries two
ramp modes, and the parallel one refuses duplicate channel labels at construction
rather than returning an unattributable answer.

**The crosstalk matrix is not a free by-product of a parallel ramp.** The estimator
returns one peak per channel per window, so leakage landing inside another member's
slot cannot be separated out of an all-play round. In Stage 2 the matrix costs **N
extra solo rounds after convergence**. It *is* genuinely free in Stage 1, where the
ramp is already solo-per-member.

**And the matrix's dynamic range is bounded by the driven channel's SNR** — leakage
below the noise floor reads as ~0 dB. So the SNR target must exceed the crosstalk
threshold in magnitude or the verdict is unprovable: a 25 dB target against a −20 dB
"usable" bound is what makes it mean anything. That is the second reason the target
is 25 and not 15.

Round bounds, both enforced: parallel `6 + N` (6 ramp rounds plus one solo per member
for the matrix), sequential `2N + 4`.

Knob availability differs by member kind and the session's member model does not
currently reflect it (`align/calibrate.rs:92` knows only Sendspin and Airplay2):

| Kind | Level knob | Note |
|---|---|---|
| sendspin | `set_volume`, live | already used by the session |
| AP2 | `ap2_volume::set_volume` (`ap2_volume.rs:168`) | exists, but the session deliberately leaves AP2 level device-authoritative — needs snapshot/restore alongside `saved_sendspin`, and the "no-impose" decision must be revisited for the session's duration only. **Restore cannot simply mirror `saved_sendspin`:** `ap2_volume` treats an absent level as genuinely *unknown*, so the restore entry is optional and "unknown" must mean *leave the receiver alone*, never write an invented level |
| pw-sink | **`pwsink_agent::Agents::set_volume`, when an agent is answering** (W20) | Corrected: this row said "none in this path", which was true only until the receiver agent existed. The host's `SetVolume` drives the receiving sink and its `HostState.volume` reports the level back, so such a member is a `SnapshotRestore` knob — the *same shape as AP2 and for the same reason*, that the level lives on the device and is only restorable because the far end reports it. `HostState` speaks cubic 0.0–1.0 (`calibrate::host_level` is where that meets our 0–100), and "unknown ⇒ leave the host alone" applies exactly as for AP2 — the agent reports a level only while it is *receiving*, so a host whose stream came up after the snapshot pass is genuinely unknown |
| anything else | none | no agent answering, a sink with neither a device route nor a node volume (`pwrouter-agent` prints `lever: <none>`), a future output kind |

**The level knob is a per-output capability, not a property of the kind** (W20, the same
correction §12.3.2 made for the mute). Two pw-sink members can differ, and one member's
answer changes when its agent drops mid-walk, so it is resolved per position
(`calibrate::level_plan`, beside the silencing) and carried into the solve as
`align_levels::LevelMemberSpec::with_knob` rather than derived from
`LevelMemberKind`. Unlike the mute there is **no universal fallback**: `relay_delay` has a
mute and no gain, so "no level knob" is a real outcome rather than a degraded one.

An unadjustable member — now meaning *genuinely* unadjustable, not "a pw-sink member" —
does not just risk being **too quiet** (which is what this section originally considered,
and which is merely a report-only nuisance). The dangerous direction is the reverse: **an
unadjustable member that clips**. No amount of turning the others down rescues it, because
the ceiling is set by the member you cannot touch. That is a distinct refusal, and it must
name that member. A member whose level *did* reach the far end must not be named there:
that would send the user to the wrong speaker, which is exactly the §12.3.2 mistake about
display names in another guise.

Outputs:

- a per-member calibration level, fed forward into the measurement stage;
- a hard **refusal** if the target SNR is unreachable without clipping — do not
  proceed to measure. Name **both** roles: the member that is too quiet (what the
  user can act on) and the member setting the ceiling (why they must). Naming only
  one of the two leaves the user with no action.

---

## 8. Orchestration and cost budget

```
  IDLE
   └─ start(group, mode) ────────────────────────────────────────────┐
  ARMING       mic socket open? constraints honoured? loop-phase lock?
   └─ LEARNING     ramp levels, build crosstalk matrix, validate assignment
       └─ MEASURING    N phases + uncertainties, ≥2 passes (drift fit needs 2)
           └─ SOLVING      pick reference = latest member; deltas; sanity-check
               └─ PROPOSED     ← parks here; the user sees deltas + confidence
                   └─ WRITING      batch all delay writes → one reconnect wave
                       └─ SETTLING     collapses into the per-member gate (see below)
                           └─ VERIFYING    residual; transitivity (§10.2); merged peak
                               └─ DONE / RETRY-ONCE / REFUSE
```

**`PROPOSED` was missing from this diagram** and is forced by §11: `apply` is an
explicit user step, so the machine has to park between solving and writing. Nothing
is written without passing through it.

**`SETTLING` is not observable as its own state.** `GroupSnapshot.sendspin_members`
lists *configured* devices, not live connections, so "wait for lock to return on
every member" cannot be asked directly — it collapses into the per-member gate with
the reconnect timeout (180 s, sized by §2.3).

Every transition into a measuring state goes through the same gate: **re-acquire
loop-phase lock with stable amplitude before accepting any window.** One mechanism
covers mute settling, reconnect recovery, the user moving, and the socket
reconnecting.

Budget, 5 speakers:

| Stage | Sequential (§6.1) | Parallel (§6.2) |
|---|---|---|
| Learning | `2N + 4` rounds ≈ 30 s (sequential — §7.1: Stage 1 *cannot* ramp in parallel) | `6 + N` rounds ≈ 25 s |
| Measure | ~110 s (gate + guard ≈ 11 s per member per pass, × 2 passes) | ~15 s |
| Write + settle | one reconnect wave, ~30–60 s | same |
| Verify | ~55 s (one pass) | ~15 s |
| **Total** | **~4 min** | **~1.5 min, ~flat in N** |

The sequential column was originally optimistic by about a minute: the gate has to
re-acquire lock per member per pass, and two passes are structural (§5.3). Measured
from the implementation's own timing constants, not guessed.

Both are a vast improvement on by-ear. Parallel's win is that it stops growing
with the number of speakers.

---

## 9. Writing the delays

### 9.1 Reference selection

The knobs only add delay (§2.4), so the reference **must** be the latest-arriving
member. The daemon picks it from the measurement; the UI shows which one won and
why. This is a behaviour change from the current wizard, where the user picks the
reference — keep manual override available for the by-ear path.

### 9.2 Minimise absolute delay

Normalise so the smallest applied delay is as small as possible. Raising a
member's delay far enough to lift the group's send-ahead high-water mark triggers
a **group-wide** stream reconfiguration, not a single-device reconnect — see the
comment at `api.rs:3136`. Warn before crossing that line, and prefer the
normalisation that avoids it.

### 9.3 Persistence

The existing handlers already persist before pushing live
(`api.rs:3123`), which is what a calibrated offset needs. Nothing new required —
but write **through the existing endpoints** rather than touching
`sync_settings` directly, so the reconnect and high-water logic is not duplicated.

### 9.4 Undo

Snapshot every member's delay at session start and offer a single "revert to
before alignment" action. The write phase is destructive to a previously-tuned
setup, and one bad measurement should be one click to undo.

---

## 10. Verification and cross-checks

Three independent checks, all cheap once the estimator exists:

1. **Residual.** Re-measure after settling; every member's phase should now match
   the reference within the estimator's standard error.
2. **Transitivity — mandatory, but not in the form written above.** The literal
   formulation ("align B and C against A, then measure B against C") is
   **arithmetically vacuous in this design**, and earlier revisions of this plan were
   simply wrong to lean on it. Every phase is read off *one shared grid* (§3), so
   `d(B,C)` is by construction `d(A,C) − d(A,B)`: the triangle closes exactly,
   whatever the per-speaker bias. No arrangement of A-referenced measurements can
   expose a per-speaker constant.

   The axis that *is* independent is **frequency**. Every soloed member emits both
   bursts (§2.2), so each pair's delta is measured twice, over 3 kHz and 1.5 kHz. An
   early reflection arrives at a fixed *delay*, so its interference with the direct
   sound is strongly frequency-dependent and biases the two bands differently.
   Closing the triangle with edges from different bands has a non-zero residual
   exactly when a *per-speaker* band-dependent bias exists, and cancels when the bias
   is *shared* — which is the discrimination §5.6 needs. This is what is implemented,
   and it blocks the write.

   **But its tolerance cannot be the estimator's precision.** A loudspeaker's
   crossover legitimately delays 1.5 kHz and 3 kHz differently, often by a
   millisecond or two and differently per model, and in a mixed-model group that is
   indistinguishable from a reflection. Hence a 3 ms tolerance — which means **a pass
   is not proof that §5.6 did not happen.** The check is real but much weaker than
   "mandatory cross-check" suggests, and that materially strengthens the case for W9.
3. **Merged peak — should NOT be built. Dropped 2026-08-11.** The idea was to put every
   member on one identical burst and confirm the mic sees a *single* correlation peak
   rather than N: the numerical form of "perfect overlap", and the check that most
   directly matches what the user was doing by ear.

   It is expressible — set-based audibility (W10) makes an N-member merged peak
   straightforward, so the original "audibility solos at most two members" objection is
   stale. The reason to drop it is that **its resolution is bounded by the estimator's
   guard distance** (burst plus analysis window, ~12 ms at 48 kHz). Two arrivals closer
   than that merge into one candidate *by construction*, so the check would report
   "single peak" for a 5 ms error that the residual check beside it already catches to
   ~0.1 ms. It would cost a reconnect-length gate per member and tell the user nothing
   the other checks have not already said, more precisely.

   Worse than useless, in fact: a reassuring verdict that resolves 100× coarser than the
   number printed next to it invites the user to trust the wrong one.

   **If the reassurance is still wanted, give it as sound, not as a verdict:** offer a
   "play the click on all of them together" button at the end so the user can *listen*.
   That is honest — the ear confirms what the numbers say — without dressing a much
   blunter instrument up as a measurement.

Report the two real checks in the UI. A green residual with a failed transitivity check
is the interesting failure and must not be hidden.

### 10.4 Near field verifies by **walking again**, and loses a check doing it

Two corrections that §8 and §10.1 got wrong by assuming one measuring position.

**A stationary residual cannot verify a near-field run.** A reading taken from one spot
measures `wire + path(P)`. After a *correct* wire alignment the wire terms are equal, so
what remains is each speaker's path difference to wherever the phone is standing — tens
of milliseconds against a 2 ms tolerance. It would fail **every** near-field run and
report a correct alignment as broken. So near-field verification re-walks: the same
arrival-driven pass, with its own closure.

**The repeatability check becomes vacuous, and reporting it green would be dishonest.**
Under near field the only member with two readings is the closure anchor — and the drift
slope was fitted from exactly those two points, so its residual is *identically zero*.
That is an identity, not evidence. It is therefore reported as absent for a walk rather
than as a pass.

Say what that costs: nothing in a near-field run detects "the user changed how they hold
the phone partway through", which pass-to-pass agreement does catch for multi-position. A
walk has **fewer** independent cross-checks than a stationary run — transitivity and
closure, not three.

**The same rule applies to a chain, for the same reason (W12).** A chain's write can only
be checked where the phone is, which is the **last** position: that position's own set —
its speakers *and* its overlaps, which its Δ put in step with them — is the one set that is
genuinely aligned there. The earlier positions were aligned at *their* spots, so
re-measuring them from here would read their path difference to this spot and fail however
correct the chain is. So the residual is scoped to the last position and
`Verification::scope_note` says so in a sentence; re-checking the rest means walking the
chain again. Each position's own §10 checks (transitivity, repeatability) run and **block
that step** as it is measured, which is where they are cheap and where a failure is still
retryable.

---

## 11. API surface

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/align/mic/ws` | binary mic ingest, one socket, bound to the session |
| `POST` | `/api/align/measure/start` | `{mode: "sweet_spot" \| "near_field"}` — begin learning + measuring |
| `GET` | `/api/align/measure` | current phase, per-member state, SNR, uncertainties, refusal reasons |
| `POST` | `/api/align/measure/apply` | write the solved delays (explicit user confirmation, never automatic) |
| `POST` | `/api/align/measure/revert` | restore the start-of-session delay snapshot |
| `DELETE` | `/api/align/measure` | abandon, leave delays untouched |

The measurement state rides the existing `/api/align` status shape where it
overlaps (members, reference, target) rather than duplicating it. Progress should
be pushed, not polled — extend the alignment panel's existing subscription rather
than adding a second WebSocket.

**This table cannot express a walk**, which §1's near-field mode needs. It gains:

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/align/measure/arrival` | "I am at this speaker now" — solo it, apply its level, gate, measure |
| `POST` | `/api/align/measure/close` | the closure reading, back at the walk's first speaker |

and it cannot express a **chain** either (§1.1), which gains:

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/align/measure/position` | `{members, overlaps}` — "these are the speakers I can hear from where I am now, and these already-aligned ones link it to the rest". Measures them, applies the step's delays **provisionally** (§1.1.1), parks for the next position |
| `POST` | `/api/align/measure/finish` | every held speaker is aligned somewhere: renormalise the whole chain globally and propose the single write |

`measure/start` gains `chain: bool` (default `false` — the single-position case is a chain
with one step and needs no calls), and `link_to`, which is **refused** while W8b is unbuilt
so a client cannot believe in cross-session coherence that does not exist. Chaining now
exists *within* a run; what does not exist is a store of a finished run's aligned set with
its delays, which is what linking two **runs** would propagate a shift into.

`apply` being a separate, explicit step is deliberate: the user sees the proposed
deltas and the confidence before anything is written.

---

## 12. Frontend

### 12.1 The wizard, and where it lives

**It belongs on the Outputs page, not on a source card.** The panel currently lives on
source cards because a group *is* its source set (`align/calibrate.rs:213` resolves the group
from `sources`, and requires it to already exist with ≥2 present members). The new
model inverts that: the user picks *speakers*, and alignment forms a group around
them. Moving the entry point and forming a temporary group are the same change, and it
is the largest piece of work in this revision — it reaches into routing, not just UI.

Pages:

1. **Mode** — multi-position (default), near field, manual (§1).
2. **Selection + levels** — pick the speakers, then set each one's level.
3. **Mode-specific body** — the measurement run, the walk, or the by-ear sliders.
4. **Review** — the proposal, its confidence and the checks. This is the `Proposed`
   state of §8, which is why that state exists.

**Selection means something different per mode**, and the UI must say so rather than
letting the user discover it through an error:

| Mode | Selection is | Validation |
|---|---|---|
| Multi-position | "the speakers I can hear from here" | Overlap needed for every step after the first; candidates pre-selected and labelled (two preferred, §1.1) |
| Near field | any subset the user wants coherent — one floor, one wing | No overlap needed *within* the walk; one needed to link to earlier **sessions** (§1.2). The UI owns the walk order |
| Manual | the set to tune by ear | ≥2, as today |

Near field must **not** force "all speakers": a multi-story apartment is exactly the
case where the user wants one floor now and another later, and excluding speakers is
the point.

Both measuring modes therefore ask the same question once the selection is made —
**link this set to already-aligned speakers, or keep it independent?** "Link" requires
an overlap; "independent" must state plainly that this set will not be coherent with
the others. Independence is a legitimate answer (two floors that are never heard
together), so it has to be a deliberate choice rather than a silent consequence.

### 12.2 Level setting

Solo **one** speaker at a time (not two — `apply_audibility` currently makes reference
*and* target audible, which is also what blocks §7's all-play round). The tone plays
continuously on it, every other member muted; deselecting or clicking away **stops the
tone**. An indicator turns green when the estimator would accept the level, from
`GET /api/align/mic/signal` (§5.4.1's verdict).

Two details that matter in practice:

- **Default start level 20 %**, not `DEFAULT_CAL_VOLUME`'s 50. Real in-room readings
  came out at 65 dB / 70 dB peak SNR (§0), roughly 40 dB above target, so 50 is
  needlessly loud for a procedure done standing in a living room.
- **The green indicator must be faster than the measurement gate.** The signal check
  uses `GATE_MIN_PERIODS` (4 periods ≈ 8 s), which is right for a gate and far too slow
  for a volume slider. The pre-flight wants one or two periods — it needs a rough SNR,
  not a phase.

**Near field breaks the two-phase shape.** Its level is only meaningful *at* each
speaker, and the risk inverts from too-quiet to clipping. Fold level-setting into each
arrival — walk up, it goes green, measure, move on — which also makes near field a
single pass instead of two.

**Stop must work at every point**, in every mode, and restore levels, mutes, routing
and the codec-independent state exactly as teardown does today.

### 12.3 Exclusivity: what "temporary group" has to block

The selected speakers form a temporary group that no other audio reaches. Membership
alone does **not** achieve that, and this was checked rather than assumed:

- `announce_arbiter` derives occupancy purely from **in-flight announcements**
  (`occupancy()` over `self.active`), so an aligning output is invisible to it and
  `begin()` takes the immediate-start path. `announce.rs` also *creates* a per-device
  sender when none exists — including an on-demand AP2 session — so being unrouted
  protects nothing.
- Even a reservation would not stop `barge_in`: the admission order is
  `if !overlaps { start } else if barge_in { preempt } else { on_busy }`.
- Voice ducking is a second, independent interferer: a `DuckHold` attenuates music with
  no clip of its own, so an assistant turn would duck the calibration tone.

The right semantics are **not** "block announcements" — nobody wants a fire alarm
suppressed by a calibration:

- Ordinary announcements: the reservation participates in `occupancy()`, so they queue
  (default) or reject. A small arbiter extension.
- Barge-in announcements and duck holds: they **win**, and the reservation holder is
  *notified*, so the affected member's measurement is discarded with a reason naming
  the cause.

The notification is the load-bearing part. Without it this is the same class of bug as
§2.3.2's intermittent stream: the gate catches it as unstable amplitude and blames the
user's hand for something a doorbell did.

### 12.3.1 Forming the group costs a reconnect wave — DECIDED, and built

§12.1 did not cost this, and it undercuts §1.1.1's whole point if left alone.

Grouping is *derived*: same source set ⇒ one group ⇒ one anchor. So forming a group
around an arbitrary output set means giving those outputs a source set nothing else has —
a **new group key**, hence a new anchor, hence **new per-device senders**. Every selected
sendspin member therefore reconnects when the hold forms *and* again when it releases:
tens of seconds each way (§2.3).

For a single-position run that is one-off and tolerable. For a **multi-position** run it
is not: if each position re-selects a different speaker set, every step pays two waves.
Five positions ≈ ten reconnect waves — which is exactly the cost §1.1.1 removed from the
write path, reintroduced in the formation path.

**DECIDED 2026-08-11: option 1.** Three ways were considered:

1. **Hold the union once; scope each step by audibility.** ← chosen. Select every speaker the run
   will touch up front (usually "all of them", or "this floor"), form **one** hold, and
   let each position choose which held members are *audible*. Mutes are live and free
   (`sendspin_volume::set_mute`), so the whole multi-position run costs **one** form and
   **one** release. This needs no new mechanism — the set-based audibility
   (`set_audible` / `solo`) already built is exactly it. The only UX consequence is that
   the up-front selection is the run's full scope rather than the first position's, which
   for whole-apartment alignment is what the user means anyway.
2. **The no-op special case:** when the selection is exactly one existing group's member
   set, keep that anchor and its senders and suppress only the group's *source* links.
   Zero reconnects. Not built — it is a second formation mechanism with its own restore
   obligations, and (1) makes it unnecessary for the multi-position case.
3. Accept the cost. Only reasonable for single-position runs.

It also composes with §1.1.1: with the union held once and provisional delays in the
relay, a whole multi-position run contains exactly two reconnect waves — the formation
and the final real write.

**How it is implemented** (`align/group.rs`, `align/calibrate.rs`, `api.rs`):

- `POST /api/align/start {outputs}` means "**hold all of these for the whole run**". Its
  doc comment says so, because it reads counter-intuitively next to a wizard that then
  works on a subset.
- `align_group::plan_hold(held, requested)` is the whole rule, pure and unit-tested:
  `requested ⊆ held` (and ≥2) ⇒ `HoldPlan::Scope`, anything else ⇒ `HoldPlan::Form(why)`.
  `AlignManager::begin` consults it **before** it bumps the start generation or tears
  anything down; `Scope` goes to `AlignManager::rescope`, which touches nothing that
  could reconnect a speaker — not the reconciler's override, not the reservation, not the
  anchor, not the click loop, not the level/mute snapshot. It updates the echoed
  selection, the mode, reference/target/audibility and re-arms the safety timeout (so a
  long walk is not cut off 15 minutes after the first position), and deliberately does
  **not** reset the playback level.
- A **superset** re-forms, on purpose. The reconciler would cope with growing the hold in
  place (the align group's key is the constant `align-hold-source`, so adding a member
  dials only that member) — but the session's snapshot/restore set, member list and level
  state would all have to grow mid-run, and the added speaker dials anyway. Not worth a
  second formation mechanism with its own restore obligations.
- The **cost is reported where the user is**: `AlignState` carries `hold_id` (unchanged ⇒
  nothing re-formed), `hold_reused`, and `hold_cost` — a sentence saying either "every
  member reconnected for this and will again on release; scope positions with
  `/api/align/audible` instead" or "no speaker reconnected: these speakers are already
  held".
- The test that pins the claim asserts on **hold identity**, not on a side effect:
  `(hold.id(), hold.anchor_node_id())` is unchanged across a subset `start` and across
  the same-union `start`, while a start naming an uncovered speaker leaves no session at
  all (it tore the old one down to re-form).

### 12.3.2 Corrections to §12.3 from building it

- **"The reservation participates in `occupancy()`" is not implementable literally.**
  `occupancy()` is also the before/after basis for the action diff, so a reservation
  placed in it emits `DuckMusic`/`StartAnnouncement` for a clip that does not exist. Two
  distinct notions are needed: **action occupancy** (announcements only) and **admission
  busy-ness** (announcements ∪ reservations). Admission and `settle()` consult the
  latter; the diff keeps using the former.
- **§12.3 missed the queued case.** A clip admitted *before* the hold sits in the queue
  and would start the instant its output frees — over the calibration. `settle()` must
  consult reservations too, not only `begin()`.
- **The on-demand-session hazard is sharper than recorded.** During a hold the *stored*
  intent still shows a held output as unrouted, so the announce path would open a
  **second** AP2 session for a barge-in, competing with the align group's own sender on a
  receiver that accepts one. The intent override has to be applied where announce
  transport is ensured, not only where the group is derived.
- **An interference sentence has to name the speaker the way the user does.** It quoted
  the node name (`'sendspin-dev-kitchen'`) while the chip beside it in the UI said
  "Küche" — the same report reading as two different speakers. Fixed by resolving display
  names **once, when the hold forms** (`align_group::resolve_labels`: the rename store
  first, `routing::output_display_name` as the fallback — exactly what the Outputs page
  and the routing matrix do) and carrying them *with* the hold. The reporters
  (`announce.rs`, `overlay_mixer.rs`) run where the outputs store is not reachable, so
  they could not have resolved a name themselves; `HoldRegistry` holds one
  `node_name → label` map and renders the sentence from it. `Interference` still carries
  the node name in `member` for matching, plus `member_label` so a consumer that writes
  its own sentence agrees with ours.
- **pw-sink members are admitted (W15, done 2026-08-11).** `calibrate::MemberKind::PwSink`
  exists, `align_group::member_kind` maps `pwsink-dev-*` to it, and `validate_selection`
  no longer refuses one — so §7's "an unadjustable member that clips" refusal is reachable.
  What such a member cannot do is **stated, not implied**: `AlignState.unlevellable` lists
  them and `AlignState.level_note` is one sentence saying that its volume can only be
  changed on the host, that it therefore sets the clip ceiling the others must fit under
  (if *it* clips, turning everything else down cannot rescue the run), and that it cannot
  be muted from here either — so it keeps playing the click while another member is
  soloed. **Both halves of that were later narrowed to the outputs they are true of:** W17
  made every member silenceable (so the mute clause is gone) and W20 made a pw-sink host
  with a live agent levellable (so `unlevellable` is now the *resolved* per-output list, not
  "the pw-sink members"). Two loose ends, both outside `align_group`/`calibrate`:
  - **No in-band mute for pw-sink, and this is a hazard rather than a loose end.**
    Audibility is implemented as mute, so a held pw-sink member is *always* audible: it
    keeps emitting the click through every other member's solo. The mic then hears two
    arrivals, and both outcomes are bad — separated by more than the estimator's ~12 ms
    guard it refuses as `AmbiguousPeak` (so **every** member fails, not just the
    pw-sink), and separated by less it merges into one pulled peak, which is §5.6's
    silent bias by construction.

    So admitting pw-sink members without a silencing mechanism would make a run *worse*
    than refusing them did. **Resolved 2026-08-11 (W17): audibility is now honoured for
    every member kind, so no such refusal is needed and none was added.** The mechanism
    is resolved **per output**, in one place, and re-resolved on every audibility change
    because an agent can drop mid-walk:

    | Channel | When |
    |---|---|
    | sendspin in-band mute | the transport has one |
    | AP2 in-band mute | the transport has one |
    | **out-of-band, via the receiver agent** | a pw-sink host whose agent answers *right now* |
    | **relay zero-fill** | everything else — no agent, a lever-less sink, a future kind |

    Ordering is safety-first: the relay batch silences everything non-in-band **before**
    the host round trip, and the relay mute is released only once the agent write
    *lands*. A write that fails leaves the member relay-muted, so an agent that vanishes
    between resolve and use can never leave a speaker audible.

    **The primary fix already exists: the pwrouter-agent.** This section was written as
    though pw-sink had no mute at all; that is wrong. `pwsink_agent.rs` carries
    `DaemonMsg::SetVolume { volume }` and `SetMute { muted }`, `Agents::set_volume` /
    `set_mute` return `false` when the host is not connected, the agent implements both
    (`pwrouter-agent/src/client.rs:461/465` → `pw::thread::apply_master`), and
    `api.rs:1954/1972` already drive them in production. `HostState` reports `volume` and
    `muted` back, so a value can be snapshotted and restored.

    Two consequences, both good:

    - **Solo becomes a real solo** for a pw-sink member with a connected agent. It is
      also the *better* mechanism where available: only the receiver's sink volume
      changes, the stream keeps flowing, so the jitter buffer never re-anchors and
      unmuting cannot introduce a discontinuity the estimator would read as an offset.
    - **§7's level gap closes too — done 2026-08-11 (W20).** pw-sink stops being *always*
      `LevelKnob::None` and becomes `SnapshotRestore` wherever an agent answers — the same
      shape as AP2, and for the same reason: the level is device-authoritative, and
      `HostState` makes it readable so it can be put back. The `OutOfBandMute` seam grew the
      level pair (`level` → `Option<f32>` on the host's own cubic scale, doubling as the
      capability query *and* the snapshot; `set_level` → did it land), `LevelChannel` is the
      level's twin of the silencing table below and is resolved in the same pass,
      `Session::saved_oob_levels` carries the snapshot with **absent = unknown = write
      nothing**, and `AlignState::level_channels`/`unlevellable` publish the resolved
      answer. Four things it turned up that were not obvious:
      - **The level has no fallback.** The mute always has one (the relay), so a failed
        write degrades; the relay has no *gain*, so a failed level write means the member
        has no level knob this position and must be **reported** (`unlevellable`), never
        silently skipped. That asymmetry is why `LevelKnob::None` stays reachable.
      - **The two capabilities are genuinely independent, not two views of one.** The
        agent's `master_props` falls back to the sink **node**'s `Props` when the sink has
        no device route (a virtual sink) and that path reports `channel_volumes` with
        `mute: None`. Such a host is levelled out of band while its mute still needs the
        relay — so they are two questions into one seam, not one question.
      - **A host reports either lever only while it is *receiving*** (the lever is found
        through the receive stream's target sink), so "unknown pre-session level" is
        routine, not exotic: a host whose stream came up after the snapshot pass has one.
      - **No `set_volume_transient`/`forget_volume` counterpart is needed** for the agent
        path. `ap2_volume` needed those because it *stores* a desired level and re-applies a
        user-set one on every reconnect; `Agents` stores nothing (`set_volume` queues one
        message, `HostState` is a report), so a write leaves no daemon-side mark and "leave
        an unknown level alone" is implemented by simply not writing.

      Two loose ends it left, both outside `calibrate`/`align_levels`/`pwsink_agent`:
      - `align_group::ExclusiveHold::unlevellable`/`level_constraint` still derive the list
        from member **kinds**, so they claim the clip-ceiling sentence of every pw-sink
        member — including one its agent is levelling perfectly well. `AlignState` now
        sources both from the resolved channels (`calibrate::level_note`), which leaves those
        two methods used only by their own tests: either delete them or give
        `level_constraint` the resolved list as an argument.
      - `align_measure::learn_levels` still builds `LevelMemberSpec`s from kinds alone, so a
        levellable pw-sink host is *fixed* as far as the solve is concerned. One
        `.with_knob(state.level_channels[node].knob())` closes it once level learning is
        actually driven (W4's wiring, still `learned: false`).

    Conditions that keep a fallback necessary:

    - `set_mute` / `set_volume` return `false` when the agent is not connected. That
      must be **checked**, not assumed — an unmuted member silently corrupts the run.
    - It is the host's **master** sink, so muting silences everything on that machine,
      not just our stream. Snapshot and restore, and say so: if the host is someone's
      desktop, alignment briefly mutes their computer.
    - A sink can have neither a device route nor node volume — `pwrouter-agent`'s own
      diagnostic prints `lever: <none>` for exactly that case — so a connected agent is
      not a guarantee.

    **Why the control was invisible in the UI (investigated 2026-08-11).** The *write*
    path was complete end to end; only the *read* path and the UI were missing, in four
    independent places that all encoded the same wrong assumption:

    1. `routing.rs` populated `RoutingNode.volume`/`muted` for `sendspin-dev-*` and
       `ap2-dev-*` only, `None` for everything else — so the matrix never carried a
       pw-sink host's level even though `PUT /api/pwsink/volume` could set it. **Fixed**:
       both are now sourced from the agent's `HostState` under the same lock as
       `connected_targets()`, and stay `None` when the host reports nothing, so a missing
       agent cannot read as "unmuted".
    2. `OutputsTab.svelte` gates the volume column on
       `o.kind === 'airplay2' || o.kind === 'sendspin'`, with a comment asserting pw-sink
       "has no volume to control". Needs `'pwsink'` added and the comment corrected.
    3. `FlowGraph.svelte` gates on `isVirtual(name)`, which is
       `SENDSPIN_DEV_PREFIX || AP2_DEV_PREFIX`. Needs the pw-sink prefix.
    4. `types.ts` documents `RoutingNode.volume`/`muted` as "sendspin devices" only.

    `VolumeControl` already treats `percent == null` as genuinely unknown rather than
    fabricating full scale, so items 2–4 are presentation-only once the daemon reports
    the state — and an agent-less host correctly shows no control instead of a slider
    that silently does nothing.

    So **W17 is a per-device calibration mute at the relay hook** — the same
    `mix_into(node, …)` site `align/relay_delay.rs` already uses, transport-agnostic — kept as
    the **universal fallback** for a disconnected agent, a lever-less sink, or a future
    output kind. Reusing the duck-hold path instead would make the session report
    interference against itself (§12.3), so it needs its own mechanism. The "how do I
    silence this member?" decision is therefore **per-output capability, not per-kind**.
  - **`sync_group::GroupSnapshot` has no `pwsink_members`**, so the by-ear entry point
    that resolves a group from a source set (`AlignManager::groups` / `start(sources)`)
    still cannot see or hold a group's pw-sink targets. Selecting them on the Outputs page
    works; closing that gap belongs with `sync_group`.

### 12.4 Component notes

`frontend/src/components/AlignPanel.svelte` and
`frontend/src/lib/align.svelte.ts` gain a measurement mode beside the manual
sliders. Needed pieces:

- an `AudioWorklet` processor as a separate module file. **Vite's `?url` import is
  a trap here:** below 4 kB it inlines the asset as a `data:text/javascript;base64,…`
  URL, and `audioWorklet.addModule()` on a data URL is not reliably supported
  (Firefox rejects it). A small worklet therefore needs `?url&no-inline`, so a real
  `assets/mic-worklet-<hash>.js` is emitted and resolves correctly under ingress.
  Verify the emitted reference after any build-config change — the failure is a
  runtime 404 or a silent `addModule` rejection, not a build error;
- a secure-context / permission pre-flight that explains the HTTPS requirement
  (§4.1) *before* asking for the mic, and a constraint read-back check (§4.2);
- a live level meter and per-channel SNR readout — the user must be able to see
  that the mic is working and that the room is quiet enough, or every failure looks
  like a bug;
- the proposed delta table with confidences, and the apply/revert buttons;
- for near-field mode, a "walk to speaker N" prompt driven by which channel is
  currently dominant.

Keep the by-ear controls visible throughout. When the estimator refuses (§5.5),
the user should already be looking at the fallback.

---

## 13. Risks and open questions

| Risk | Severity | Mitigation |
|---|---|---|
| ~~Phone denies mic in the ingress iframe~~ | **RETIRED** | W0 passed on Android 2026-08-11 (§14.5) |
| ~~Browser ignores `echoCancellation: false`~~ | **RETIRED** for the supported target | Android honours and reports the constraint. The Safari-omission hole (§4.2) and its behavioural detector remain in the code and remain correct, but iOS is out of scope so nothing depends on them |
| User's HA is HTTP-only | high | detect and explain; no workaround exists |
| **Early reflection inside the analysis window biases a speaker by multiple ms, undetectably** | **high**, but now *measurable*: W22 reads the per-member cross-band splits (§5.6.1), which is what decides W9 | no refusal rule can see it (§5.6); §10.2's transitivity is the only exposure and it needs a 3 ms tolerance, so 1–2 ms of bias passes every check; near-field mode mitigates, only W9 fixes it |
| Mute settling exceeds the 3 s guard, so two speakers are briefly audible in the same band | medium | arrivals closer than the estimator's ~12 ms guard distance **merge into one peak** instead of raising `AmbiguousPeak` — a silently wrong answer, not a refusal. Needs a real group to size the guard |
| Amplitude-stability gate reads a broadband peak, so room noise restart-loops it | medium | gate times out blaming the level; a band-limited stability measure would fix it if W0 shows it biting |
| A member wedges and renders intermittently while the daemon reports itself clean | medium | observed on hardware (§2.3.2); diagnosed as `GateReason::Intermittent` and the remedy (reconnect the speaker) is named. The gate reports it but cannot *fix* it — an automatic reconnect-and-retry is a candidate once §2.3.2 is seen more than once |
| AP2 receiver ignores a live render-delay change until its next reconnect | medium | verification fails as `ResidualTooLarge` and points at the measurement rather than the transport — misleading diagnosis, not a wrong write |
| Reverberant room smears peaks below usable accuracy | low | measured: a +7…+71 ms reverb train still yields 0.14 ms and the direct arrival (§5.4.1) |
| Noise floor too high for the estimator | low | measured: works to 0 dB broadband SNR, ~20 dB more headroom than a quiet room needs (§5.4.1) |
| Harmonic distortion crosstalk between channels | medium | one-octave assignment for N ≤ 4; measure the crosstalk matrix and validate at runtime (§6.2) |
| Biquad group-delay bias mis-compensated | medium | analytic value + unit test on a synthetic burst |
| Relay-thread CPU cost of per-device filtering on a 4-core Pi | medium | biquad cascade not FIR; measure with the existing xrun badges before shipping |
| Acoustic path baked into a whole-house group | medium | mode choice is explicit and explained (§1) |
| One bad measurement silently degrades a tuned system | medium | refusal rules (§5.5), transitivity (§10), one-click revert (§9.4) |

Open questions:

- Does the ESPHome `sendspin_delay_live` path work well enough to enable during a
  measurement session only? If yes, the settling stage collapses and iterative
  refinement becomes affordable — which would change §8 substantially.
- Should pw-sink members become first-class in the session's member model
  (`align/calibrate.rs:92`), or stay out of scope for v1?
- Near-field mode without soloing assumes ≥15 dB near/far dominance. Does that
  hold in a small room with a speaker on each wall?

---

## 14. Work packages

The authoritative list. §0 summarises status in prose and does **not** repeat this table.

### 14.1 Remaining

One feature carries most of what is left in the daemon — **parallel excitation** (W7) —
and the frontend's chaining UI (W6c) is now unblocked.

| WP | Content | Depends on | Size |
|---|---|---|---|
| **W7** | **Parallel excitation** (§6.2): the content generator, a per-device `cal_gate` in all three relays, analytic group-delay compensation, frequency assignment plus the runtime crosstalk validation §7.1 requires. Independent of W12 | W5 | large |
| **W6c** | Frontend: the **chaining UI** — position posting, overlap picking, per-step Δ and confidence, the error statement — and the **Outputs-page move** (§12.1), which is one `<AlignWizard …/>` tag now that the wizard is self-contained. The daemon side is done: `chain: true`, `POST measure/position`, `POST measure/finish`, and `MeasureStatus.chain` | W12 | medium |
| **W8b** | Near-field **across sessions**: optional linking to an already-aligned set through an overlap (§1.2). No longer blocked on Δ propagation (W12 built it) — what it needs is a **store** of a finished run's aligned set with its applied delays, which nothing has | W12 | small |
| **W9** | Chirp + matched filter — the only proper fix for §5.6's early-reflection bias, and **not a noise fix** (noise is a non-problem, §5.4.1). **Gated on W22's cross-band split data** (§5.6.1), and if built it is an *automatic escalation* or a per-member diagnostic, never a preference | W22 | medium |
| **W22** | **Live acceptance on real speakers.** Nothing here has ever run against hardware beyond the W0 mic spike: formation cost, mute-settling time, real reconnect duration, and whether the amplitude-stability gate survives a real room are all unmeasured. **Also read the per-member cross-band splits** — they decide W9 (§5.6.1). Needs a deploy | most of the above | — |

### 14.2 In flight

Nothing. **W7** is next in the daemon, **W6c** in the frontend (§14.1).

### 14.3 Done

| WP | What it delivered |
|---|---|
| **W0** | Device spike — passed on Android (§14.5) |
| **W1** | `align/mic.rs`: WS ingest, gap detection, AudioWorklet client, `MicCapture.svelte` |
| **W2** | `align/estimator.rs`: the DSP, with the measured accuracy in §5.4.1 |
| **W3** | `align/measure.rs`: run state machine, the loop-phase gate, §11 endpoints |
| **W4** | `align/levels.rs`: the two-sided level solver and crosstalk verdict |
| **W5** | Solve / write / settle / verify — landed inside W3. Merged-peak **dropped by decision** (§10.3), not deferred |
| **W6a** | Wizard shell, run page, review page, and the measure client |
| **W6b** | Selection page, per-speaker solo and level, WS push with polling fallback |
| **W10** | `align/group.rs`: the temporary exclusive group, as a routing-intent override |
| **W11** | Arbiter reservation; barge-in still wins and is *reported* as `InterferenceCause` |
| **W13** | `align/relay_delay.rs`: the per-device provisional delay line. Found §2.4.1 and §1.1.2 |
| **W14** | The §2.4.2 feasible-interval solver, replacing the inverted reference-member rule |
| **W15** | `MemberKind::PwSink` — pw-sink outputs are alignable |
| **W16** | `revert_scope`, the run-status push channel, the faster pre-flight window, display names in interference |
| **W17** | Per-**output** silencing: in-band / receiver agent / relay zero-fill |
| **W18** | AP2 volume driven for the session and restored, unknown levels left alone |
| **W19** | Per-member calibration levels are session-owned, so they survive a reload |
| **W20** | Per-**output** level knob: the `OutOfBandMute` seam grew a level pair, a pw-sink host with a live agent is levelled and restored like AP2, and only genuinely lever-less outputs are named as setting the clip ceiling. Corrected §7's table row and §12.3.2 |
| **W21** | The relay-vs-device equivalence experiment: six bracketed readings, three writes, reporting scale and sign with a resolution bound and no silent correction. Corrected §1.1.3 |
| **W12** | **Multi-position chaining** (§1.1): `POST measure/position` per listening spot, the two-overlap consistency **refusal**, Δ propagation to the whole already-aligned set, provisional delays in the relay throughout and **one** write wave at the end, the global renormalisation through the §2.4.2 solver, and a per-joint error statement that withholds a total when a joint had one overlap. Steps *inside* the union hold (§12.3.1) — no formation path was added. Corrected §1.2 (a chain does not depend on capture continuity across positions), §1.1 (five things it did not say — §1.1.4) and §10.4 (a chain's residual covers the last position only) |
| **W8a** | Near-field walk with the closure measurement. **Depends on W19** (the per-arrival level lives in `AlignState.levels`), which §14.1 did not list. Corrected §10.4 (a stationary residual cannot verify a walk), §5.3 (the closure *is* the drift fit) and §1.2 (a 15-minute deadline a walk would hit) |

### 14.4 W22 — live acceptance, step by step

Nothing below has been done. Every number elsewhere in this document comes from unit
tests, synthetic signals or the W0 microphone spike, so this is where the design either
holds or does not.

1. **Deploy** (`scripts/deploy-dev.sh`; note the add-on pulls rather than builds when
   `image:` is set — see the deploy notes) and open the UI over the **HTTPS** URL, since
   §4.1's secure-context requirement is not negotiable.
2. **Time the formation.** Selecting a union and starting gives every sendspin member a
   reconnect (§12.3.1). Measure it: the plan asserts "tens of seconds each way" from the
   group-churn results, and the whole union-hold decision rests on paying it once.
3. **Time a mute settle.** §12.2's `MUTE_GUARD` is 3 s and §13 records the hazard: if a
   group's send-ahead exceeds it, two speakers are briefly audible in the same band and
   arrivals closer than the ~12 ms guard distance **merge** instead of raising
   `AmbiguousPeak` — a silently wrong answer rather than a refusal.
4. **Watch the gate in a normal room.** The amplitude-stability criterion reads a
   *broadband* peak, so speech or clatter may restart-loop it into a timeout that blames
   the level (§13). If that happens, a band-limited stability measure is the fix.
5. **Run a full single-position measurement** and check the proposal reads sensibly — in
   particular that a sendspin group aligns to its **earliest** member with *advances*
   (§2.4.1/§2.4.2), which is the inversion no synthetic test can confirm against firmware.
6. **Read the per-member cross-band splits** — `split_i` inside §10.2's transitivity check.
   **This decides W9** (§5.6.1). Near zero ⇒ reflections are not biasing much and W9 stays
   unbuilt; clustered at 1–3 ms ⇒ they are, and W9 becomes an automatic escalation. Record
   the distribution, not a verdict.
7. **Run the equivalence experiment** (§1.1.3, `POST /api/align/equivalence`) before
   trusting any real write: it reports the relay-vs-device **scale and sign** with a
   resolution bound. A sign disagreement invalidates the deferred-write scheme.
8. **Then a chained multi-position run**, and check the two claims that only a real
   apartment can test: that Δ propagation actually keeps the earlier rooms in step, and
   that two overlaps agree within the 8 ms geometric tolerance (§1.1).
9. **Confirm teardown.** Stop mid-run, let the idle timeout fire, and start a superseding
   session — each must restore routing, levels and mutes exactly. A half-restored house is
   much worse than a failed alignment, and this is the only place that claim is tested
   against real devices.

### 14.5 W0 — the device spike, step by step

Needs a real phone; cannot be done from a shell. Run it against the **HTTPS**
URL (the duckdns hostname through `core_nginx_proxy`), not `homeassistant.local`
— §4.1 explains why the `.local` address cannot work.

1. Open HA over HTTPS on the phone, navigate to the add-on panel so the UI is
   inside the ingress iframe, and open the alignment panel.
2. Press start on the mic capture control. **Does the permission prompt appear at
   all?** This is the single question the spike exists to answer. If no prompt
   appears, capture the console error verbatim — "not allowed by Permissions
   Policy" and "only in secure contexts" mean completely different things and lead
   to different fixes.
3. Grant it. Confirm the level meter moves when you speak.
4. Check the reported sample rate. iOS is expected to give 44100, Android 48000;
   both must work, and a third value is a finding worth recording.
5. Confirm the constraint read-back passed (§4.2). If the browser silently ignored
   `echoCancellation: false`, the meter will still move and everything will look
   fine — this is the failure mode that would otherwise be discovered much later,
   as inexplicably drifting measurements.
6. Start an alignment session so the click track plays, hold the phone near one
   speaker, and capture a few seconds. Confirm the gap count stays at zero.
7. Read the **signal check** panel, not the meter. It grades the weaker of the two
   tones by peak SNR and states the action: `good` (≥25 dB, margin to spare),
   `marginal` (≥15 dB, works now and may not once the room gets noisier),
   `too_quiet` (below the estimator's refusal threshold), `unusable` (clipped or
   gapped, which no level change fixes). The meter cannot substitute: a reassuring
   15 % reading spans everything from ample to unmeasurable.

**Result, 2026-08-11: W0 PASSES — done.** On Android: the permission prompt appears
inside the ingress iframe over HTTPS, the capture reports **48000 Hz mono**, the
constraint read-back is honoured, zero lost frames, and the signal check reports
**Good**. The feature is viable.

**iOS is explicitly out of scope**, not merely untested — there is no device to test on
and it is not a target. That retires the iOS-shaped risks from §13 rather than leaving
them open: the §4.2 `getSettings()` omission (Safari-only) and the 44.1 kHz capture path
still exist in the code and are still correct, but nothing depends on them.

Repeat on a second browser if possible (one iOS Safari, one Android Chrome): the
constraint-honouring behaviour differs between them and that difference is the
thing most likely to bite later.

Record the outcome in §0 either way. A negative result at step 2 is not a small
setback — it means the feature needs a different delivery surface than the ingress
panel, and that decision should be made before W3 rather than after W6.
