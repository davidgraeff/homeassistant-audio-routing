# Microphone-assisted speaker alignment

**What it does.** It replaces the by-ear alignment wizard's *judgement* step with a
measurement. The user opens the add-on UI on a phone, grants microphone access, and
the daemon computes each group member's render offset in milliseconds from the mic
signal, writes the delays, and verifies the result — instead of the user dragging a
slider until two clicks sound like one.

**What it reuses.** The session model in `align/calibrate.rs` — server-owned
playback, level/mute snapshot and restore, a safety timeout, the two-tone click
track — was the right shape and is used wholesale. The measurement path sits
*beside* the by-ear path, which remains the fallback when the phone has no usable
microphone (§4.1) or the estimator refuses (§5.5).

Section numbers are load-bearing: several hundred comments in
`bridge-daemon/src/align/`, `api/measure.rs` and the frontend cite them, so they are
kept stable even where a section's content has been rewritten.

---

## 0. Status

**Feature-complete for the target scenario, and never run against real speakers.**
The microphone path is proven on hardware (§14.4), the DSP is measured against
synthetic signals (§5.4.1), the orchestration and the multi-position chain run end
to end in tests, and the UI renders all of it — including the parts a user cannot
infer from the numbers, like a speaker that moved without being audible. **W7
(parallel excitation) is the only unbuilt feature**, and it is a speed optimisation:
it makes measurement time roughly independent of speaker count rather than enabling
anything new.

The gate is **W22, live acceptance** (§14.3). Every figure in this document comes
from unit tests, synthetic signals or the W0 microphone spike — formation cost,
mute settling, real reconnect duration, and whether §5.6's bias bites are all
unmeasured.

As of 2026-08-12: **450 daemon tests passing / 0 failing**, `cargo clippy
--all-targets` at exactly its 6 pre-existing warnings (none from this work),
frontend `npm run check` clean.

### Decisions taken

| Decision | Where | Why |
|---|---|---|
| Alignment does **not** change the wire codec | §2.3.1 | The codec is part of the latency chain, so calibrating under PCM and running under Opus measures a lead that does not exist |
| Real writes are **deferred to the end**; provisional delays live in the relay | §1.1.1 | A write costs a device reconnect; a per-step chain would spend its wall clock waiting for speakers to come back |
| The sendspin knob is an **advance**, so the solver models knob *ranges* | §2.4.1, §2.4.2 | Settled in `Sendspin/sendspin-cpp`, the client the devices run — "reference = latest arrival" was inverted for sendspin |
| **Hold the union once**, scope positions by audibility | §12.3.1 | Forming a group costs a reconnect wave; audibility is live and free |
| Drive **AP2 volume** for the session, and restore it | §7 | Otherwise the level phase is a silent no-op for AP2 members |
| Silencing **and** level are per-**output** capabilities, resolved in one place and re-resolved every position | §12.3.2 | An agent can drop mid-walk, so two members of one kind differ; the level then has no fallback and must be *reported*, not skipped |
| Calibration levels are **session-owned, not persisted** | §12.2 | Survives a reload; a stored level is a good seed and a bad promise, since it depends on where the phone is |
| The **merged-peak check is dropped**, not deferred | §10.3 | Expressible, but strictly less sensitive than the residual check beside it |
| **W9 is not built, and will never be a user-facing toggle** | §5.6.1 | W22 can *quantify* whether §5.6 bites from data the current code already produces; and choosing between two DSP front-ends is a harder judgement than the one this feature exists to remove |

### What is not yet trustworthy

1. **§5.6 is the top technical risk and no amount of code closes it.** An early
   reflection inside the analysis window biases a speaker 1–2 ms and passes *every*
   check, including §10.2's — whose tolerance has to absorb loudspeaker crossover
   split and codec band-split too. Only W9 fixes it properly.
2. **The write-back, not the estimator, is the precision floor** (§1.1.2):
   integer-ms knobs round by up to 0.5 ms per member, and pw-sink's 15 ms playout
   floor means a sub-15 ms correction cannot be expressed at all.
3. **The deferred-write scheme rests on an equivalence that has been *built* but
   not *run*** (§1.1.3): relay delay ≡ device delay, in scale and sign.
4. **Never run against real speakers** (§14.3).

**Document precedence.** The design was revised on 2026-08-11 for the real target
scenario — a large apartment where no single position hears every speaker. That
added the three-mode wizard on the **Outputs** page (§1, §12.1), multi-position
chaining (§1.1) and the exclusivity requirements (§12.3). Anything below that reads
as single-group, source-card framing is superseded wherever the two disagree.

---

## 1. What "aligned" means, and the three modes

A single microphone in a single place measures **electrical delay + acoustic
propagation delay** and cannot separate them. At ~3 ms per metre of path
difference, a mic 2 m from speaker A and 5 m from speaker B reads a 9 ms offset
that is entirely real and entirely geometric. This is not a DSP problem to solve;
it is a product decision about what the feature promises — hence three modes, one
estimator, different orchestration. The user picks on the wizard's first page.

| Mode | User does | Aligns | Good for |
|---|---|---|---|
| **Multi-position** (default) | Aligns a locally-audible set, then repositions and aligns the next set through an overlap (§1.1) | Each region at *its* listening spot, regions made mutually consistent through the overlaps | A large apartment — the case where no single position hears everything |
| **Near field** | Walks to each speaker in turn, holding the phone at it | The *wire* — path difference collapses below ~1 ms, so it is right everywhere | Whole-house coherence: correct while walking around, not at N specific spots |
| **Manual** | Today's by-ear wizard | Whatever the ear decides | Fallback when the mic is unusable (§4.1) or the estimator refuses (§5.5) |

A single-position group is multi-position with one step, so it needs no mode of its
own. Near field has a bonus property: with the mic stream **open while the user
walks**, the analysis grid is just `frame_index mod period` on a continuous capture
(§3), so it survives the walk, and a **closure measurement** (revisit the first
speaker at the end) separates accumulated clock drift from real offsets. That
removes chaining entirely — one reference frame, no overlaps, no error accumulation
— at the cost of walking to every speaker.

### 1.1 Multi-position chaining

Maintain a set of already-aligned speakers with their applied delays. Each
subsequent step selects a locally-audible set **plus overlap members** from the
aligned set. At the new position, measure everything including the overlaps — an
overlap's arrival already includes its applied delay. Target = the **latest**
arrival among the step's members, and each new member's delay follows from that.
If a *new* member arrives later than the overlap, the overlap must gain Δ — and
because **a common delay added to an already-aligned set preserves that set's
internal alignment**, Δ is applied to *every* member of the aligned set, not just
the overlap. That is the whole trick, and it is what makes the chain work. So the
overlap's delay is the step's floor: new members are placed relative to it, and it
only ever moves *up*, dragging its whole history with it.

**Renormalise globally at the end.** Every step can only add delay, so the floor
ratchets upward across an apartment; after the last step the whole set is shifted
back down (§1.1.4 item 4). Without it you accumulate latency for nothing and risk
crossing the send-ahead high-water mark (§9.2), which costs a group-wide
reconfiguration rather than a per-device reconnect.

**Use two overlaps, not one.** With a single overlap the chain has no redundancy at
its most dangerous point: that one measurement is applied as a common shift to the
entire aligned set *and* anchors everything downstream, so a §5.6 reflection bias
on it propagates across the apartment undetected. Two overlaps have a *known*
relationship from the previous step, so measuring both gives an independent
estimate of the step's error — spatial redundancy, which is far stronger than
§10.2's cross-band check. Disagreement beyond tolerance **refuses the step** rather
than poisoning the chain.

Expectation-setting on that tolerance: two overlaps will **not** read identically
at the new position, because the previous step aligned them at the *previous*
position and their path difference here is different. What is checkable is that the
disagreement stays inside plausible geometry — hence `OVERLAP_AGREEMENT_TOL_MS`
= 8 ms (§1.1.4 item 2).

**What chaining does and does not guarantee.** Each step inherits the reference
frame of the position it was measured from. A, B, X coincide at P1; X, Y, Z
coincide at P2. A and Y are only *indirectly* related, so in the doorway between
the two rooms they are approximate. That is inherent to one microphone, and given
the premise that no single position hears everything it is the right trade. Users
who want corridor coherence want near field.

### 1.1.1 Provisional delays live in the relay; real writes happen once, at the end

A real delay write costs a device reconnect — tens of seconds (§2.3) — so a chain
that wrote per step would spend most of its wall clock waiting for speakers to come
back. Instead the chain applies its delays **provisionally, in the daemon**, and
writes the real knobs **once** after the last step.

The mechanism is a **per-device delay line** (`align/relay_delay.rs`), not
per-device tone synthesis. Every per-device relay already calls
`mix_into(node_name, block, &mut buf)`, so a per-device ring buffer read at an
offset of *d* samples emits older content against the unchanged timestamp
schedule, and the device renders it later — exactly a delay. Why this shape rather
than the §6.3 synthesis:

- **Transport-agnostic.** A delay line needs no presentation timestamp and no
  timeline anchor — it only buffers — so it works for sendspin, AP2 and pw-sink
  alike. This is the thing §6.3 could not do.
- **Sample-accurate despite block-granular relays.** Reading a ring at an offset is
  indifferent to block boundaries, so it is not quantised to Opus's 20 ms frames.
- **Nothing is persisted.** A daemon restart mid-session drops the provisional
  delays and leaves the user's stored config untouched — safer than per-step writes.
- **Renormalisation becomes free.** Between positions it is just an offset change.

### 1.1.2 What the delay line can and cannot stand in for

**Confirmed unaffected, checked against the code rather than assumed:** shifting
content against a fixed timestamp schedule does **not** touch the send-ahead lead
or any jitter buffer. The line emits exactly as many bytes as it consumes, one
block per block, at the same instant. `SharedTimeline::stamp` derives its stamp
from block *length* plus the clock and is called once per block *before* the
per-device fan-out; the AppleMIDI sender advances its RTP timestamp by
`frames_per_pkt`; the AP2 sender stamps whatever it is fed. None of them can
observe that the content is older.

Four ways relay-side and device-side delay still differ:

1. **The sign is inverted for sendspin** (§2.4.1). Not a property of the delay line
   — the line is a true delay, and the knob it stands in for is an advance.
2. **Codec frame phase — inherent, not calibratable.** The line sits *upstream* of
   `outputs::sendspin::codec::Reblocker`, so a delay that is not a whole multiple of
   the codec frame (20 ms for Opus) moves a transient to a different position inside
   the MDCT window, whereas the device knob shifts rendering with the
   content-to-frame phase untouched. The decoded audio is still delayed by exactly
   *d*, but Opus's smearing is window-position dependent, so the measured *peak
   position* can move by a fraction of a frame the device knob would never produce.
   This is the concrete mechanism behind §2.3.1's "sub-millisecond peak position
   through Opus is unverified". sendspin-with-Opus only; PCM is exact and FLAC is
   lossless.
3. **The device arm of an equivalence measurement needs two reconnects, not one.**
   Writing the knob forces a reconnect, which restarts the stream, clock sync and
   buffer fill, any of which can shift that device's offset by ε. "Relay *d*, no
   reconnect" versus "device *d*, after reconnect" measures *d ± Δ ± ε* and cannot
   separate the two, so the device arm must be a **difference of two
   post-reconnect measurements**.
4. **What gets written is not what was applied, and this is where sub-ms alignment
   dies.** The line is sample-accurate (20.8 µs at 48 kHz); the knobs are integer
   milliseconds, so the write-back rounds by up to 0.5 ms per member. Worse,
   pw-sink's playout delay has a hard floor of three packet times
   (`PWSINK_JITTER_MIN_MS` = 15 ms), so a sub-15 ms pw-sink delay **cannot be
   written at all.** Independent of any equivalence question, the write-back — not
   the estimator — is the precision bottleneck.

One operational asymmetry: only the **real** write feeds `required_send_ahead_us`,
so only it can cross the group's high-water mark (§9.2). The provisional delays
never do, which means the walk cannot feel the mark approaching — check it before
writing, not after.

### 1.1.3 What the equivalence experiment can actually measure (W21)

Built, with its own manager, two endpoints and a WebSocket
(`POST /api/align/equivalence`), and not yet run against hardware. Three
corrections it forced:

1. **"A per-transport constant to correct for" is not measurable — and mostly is
   not the problem.** §1.1.2 item 3's difference of two post-reconnect measurements
   cancels any constant, including exactly the one this scheme hoped to find. What
   *is* measurable is the **scale** and the **sign**, and those are the ones that
   matter: a constant is a common shift within a kind, and a common shift is free
   (§2.4.2), whereas a gain error *g* leaves every member wrong by `(g − 1)·d_i`,
   which nothing absorbs. A constant *between* kinds does matter, and nothing
   measurable on one speaker can see it.
2. **Two reconnects are not enough either.** They are tens of seconds apart, and a
   100 ppm phone clock creeps ~6 ms across one — against a step §9.2 caps at 20 ms.
   So each arm is **bracketed** (baseline → changed → baseline) and the device arm
   costs **three** writes. Without the bracket the experiment reports a ~30 % scale
   error that does not exist.
3. **ε is a number, not an argument.** The part of reconnect variation the bracket
   cannot separate from drift becomes a floor under the reported resolution, so the
   claim degrades honestly when two reconnects land differently.

The step is **20 ms = exactly one Opus frame**, which leaves the content-to-frame
phase untouched and so nulls item 2's MDCT confound *for this measurement* — at the
cost of saying nothing about item 2 for an arbitrary delay.

Two incidental findings worth keeping: **AP2's render delay is applied live** by
`set_output_latency`, contradicting §2.4 and `routing/sync_settings.rs`'s "on next
reconnect" comment; and `set_sendspin_delay_handler` reconnects
**unconditionally**, which is what makes a `from → from` write a usable symmetric
baseline. Consequence for §2.3: if the equivalence holds, "tens of seconds per
delay change" governs only the final write, not the iteration loop — the open-loop
design does not change, but a failed verification becomes cheap to retry.

### 1.1.4 Five things chaining needed that §1.1 did not say (W12)

1. **Where the aligned set "arrives" at a new position is ambiguous, and the mean
   is the answer.** §1.1 says to refuse on disagreement but not what to do when two
   overlaps disagree *within* tolerance — the normal case. Their arrivals bracket the
   aligned set, so the anchor is their **mean**, and **half the disagreement is that
   joint's error**, reported per step and summed across the chain (`ChainError`).
   One overlap gives no such estimate at all, so a chain containing a single-overlap
   step reports **no total** rather than a total with a hole in it.
2. **The 8 ms tolerance is not a precision figure.** What two overlaps read apart
   at a new position is a *difference of differences* — how their relative geometry
   changed between the two spots — which is real and unbounded by anything in the
   capture. 8 ms ≈ 2.7 m of that at 3 ms/m, and the failures it exists to catch (an
   overlap that was never aligned, a wrapped phase, a §5.6 reflection lock at +5 ms,
   a speaker that was moved) are 5 ms to hundreds. A 1–2 ms per-speaker bias sails
   through it, exactly as it sails through §10.2.
3. **Refusing the *step* is not refusing the run.** Everything already aligned is
   still good and still carries its provisional delays, so a disagreement (or a
   failed transitivity/repeatability at that position) parks the chain with the
   reason and lets the user stand there and try again. Only the run's *bindings* —
   session, capture, delay line, cancellation — are fatal. Losing an apartment's
   chain to one bad joint would be the wrong trade.
4. **"Subtract the global minimum" is not expressible.** With mixed polarities the
   minimum is not a delay anyone can subtract (§2.4.2), so the renormalisation *is*
   the interval solver: feed it `max(p) − pᵢ` as each member's arrival and its own
   target choice becomes the free common shift, which it then picks to keep the
   largest knob smallest (§9.2). A sendspin-only chain still lands on its earliest
   member at advance 0.
5. **The chain's state has to be what the line is *applying*, not what the step
   solved for.** The line and the knobs are both whole milliseconds; a model
   carrying the exact ideal would disagree with reality by up to 0.5 ms per step,
   and that error would land between the aligned set and every later position.
   Rounding when the delay is applied keeps the two identical, and the error stops
   accumulating because every later position measures its overlaps *through* the
   line.

### 1.2 The reference frame is one continuous microphone capture

**Everything measured within a single continuous capture is comparable**, because
the analysis grid is just `frame_index mod period` on that stream (§3) — no clock
sync with anything. **Nothing is comparable across captures:** a mic reconnect
restarts `align/mic.rs`'s frame counter *and* a new session restarts the click
player, so both the grid origin and the content phase move by unknown amounts.
Inside a run that is handled by `grid_epoch`, which discards a set and restarts
once.

Consequences:

- **A chain does not depend on capture continuity across positions.** What crosses
  a position boundary is a **provisional delay in milliseconds**, not a phase, and
  every position re-measures its overlaps in its own frame — so a capture that
  reconnects *between* two positions costs nothing. What is not survivable is a
  reconnect *inside* one position, and that voids **that position only**: its
  readings are discarded, the user stands there again, and everything aligned
  earlier is untouched. Each step records its own `grid_epoch`, no two steps'
  observations are ever compared, and the honest bound on a joint is the overlap
  disagreement (§1.1.4) rather than the capture's continuity. Voiding the whole
  chain instead would send someone round the apartment again for nothing.
- **Near field needs no overlap within a walk**, however long: keep the mic stream
  open while walking between floors and the whole house is one frame, with the
  closure measurement absorbing drift. It needs an overlap only to link **separate
  sessions** (W8b, unbuilt).
- **A near-field overlap is *better* than a multi-position one.** Measured at arm's
  length it is a clean wire-delay comparison, where a multi-position overlap is
  measured across a room and its Δ carries a path-difference ambiguity that can only
  be bounded. Cost: one extra stop, ideally a speaker reachable from both walks.

So the advice to users is: **prefer one continuous session for everything that
should be coherent**, and split only when you must. Two practical costs of honouring
that:

- **The session's 15-minute deadline was a one-shot timer** armed at `start` and
  never re-armed, so a whole-house walk died mid-walk as a lost session. It is now an
  **idle** timeout: each arrival re-solos its speaker, so a walk refreshes it without
  the measurement code knowing the watchdog exists. A genuinely abandoned session
  still tears down.
- **A mid-walk mic reconnect costs the *user* a re-walk**, not the daemon a loop.
  Budget it as a user-visible failure and allow more restarts than the internal path
  does, because each one is expensive to the person, not the process.

---

## 2. The session, and the constraints it operates under

### 2.1 The session

`align/calibrate.rs` owns one alignment session at a time:

- `click_wav()` builds a 2 s S16LE/44.1k stereo loop: an 8 ms Hann-enveloped
  3000 Hz burst ("A") at t=0, an 8 ms 1500 Hz burst ("B") at t=1.0 s, amplitude 0.5.
- `start()` resolves the members, snapshots their levels and mutes, and loops the
  WAV into the group's **sync anchor** via `pw::player::play_loop_to_target`.
- `apply_audibility()` solos the members that should be heard and silences the rest
  — per-output, through whichever channel that output has (§12.3.2).
- The safety timeout tears the session down so a closed tab cannot leave the room
  muted with a click looping (now idle-based, §1.2).

Members are `Sendspin | Airplay2 | PwSink` (`MemberKind`), and pw-sink members are
alignable (W15) rather than refused.

### 2.2 Both speakers play both tones

The A/B alternation is **not** a per-speaker label. One WAV goes into one anchor
and audibility only gates *audibility*, so every audible member emits both the
3 kHz A burst and the 1.5 kHz B burst. A/B exists so that a target slipped by a
whole click lands its A on the reference's B, making the error audible — it
disambiguates the ±2 s window for the *ear*. Two consequences run through the rest
of this document: there is no "overlap of two different frequencies" to detect
(per-speaker frequency labelling is W7, §6.2), and every soloed member's delta is
measured *twice*, over two bands, which is what §10.2's check exploits.

### 2.3 A delay write costs tens of seconds, not 5

`set_sendspin_delay_handler` persists the delay, pushes it live, and then — unless
`sendspin_delay_live` is on, which it is not by default — forces a reconnect for
that one device, because current ESPHome firmware reads the static delay only at
stream start. Hardware-confirmed: a reconnecting sendspin device takes **tens of
seconds** to render again regardless of how the previous session ended.

This is the single most important constraint on the orchestration, and it has a
clean answer: **the estimator returns milliseconds directly, so there is no search
loop.** Measure everything, write once, verify once (§8). Do not build a
hill-climber; at ~30 s per iteration it would be unusable, and it is unnecessary.

### 2.3.1 Alignment does not change the wire codec

A session measures through whatever wire codec the group actually runs. The reason
is correctness, not preference: **the codec is part of the latency chain, not a
neutral transport.** Both `codec_delay_us` and the send-ahead floor are
codec-dependent, so calibrating under PCM and running under Opus would measure a
lead that does not exist in normal operation and the offsets would not transfer —
the substitution would quietly invalidate its own results. The codec's constant
contribution to delay is something you *want* folded into the measured offset. It
follows that verifying one's chosen codec actually works is a precondition the user
owns, not something alignment papers over.

**What that costs, recorded honestly.** A lossy transform codec is entitled to
smear an 8 ms transient: Opus's MDCT window spreads it, and per-band bit allocation
differs between the 1.5 kHz and 3 kHz channels. A *constant* per-codec delay is
harmless (it is calibrated out), but a *frequency-dependent* one lands directly on
§10.2's cross-band check, whose tolerance must therefore absorb codec band-split on
top of loudspeaker crossover split. Detectability through Opus is not the problem —
65 dB and 70 dB peak SNR on the two channels in a real room — but sub-millisecond
peak *position* through Opus is still unverified.

### 2.3.2 A wedged member is a real state, and a reconnect clears it

Hardware-observed 2026-08-11: a sendspin device can wedge such that a stream
renders **intermittently** — the click track audibly breaking up — while the daemon
side is provably clean (relay logs showed 50.0 blocks/s, a constant 20000 µs
timestamp gap, zero drops). Switching codec cleared it; so did nudging the static
delay, because that forces a reconnect. So the cure is a reconnect and the fault is
past the last point the daemon can observe.

Consequence for the gate: intermittency must be its own diagnosis. Caught by the
amplitude-spread check instead, it is reported as "hold the phone still" and sends
the user after entirely the wrong thing. `GateReason::Intermittent` fires only
after the tone has been *heard and then lost twice*, so a mute settling is not
mislabelled as a broken stream.

### 2.4 The knobs

| Member kind | Delay knob | API | Applies |
|---|---|---|---|
| sendspin | static delay, 0–5000 ms, integer ms — an **advance** (§2.4.1) | `PUT /api/sendspin/delay` | on device reconnect (or live if `sendspin_delay_live`) |
| AP2 | render delay, ms | `PUT /api/outputs/{node}/latency` | live in practice (§1.1.3), despite the comment saying next reconnect |
| pw-sink | playout delay, ms, floored at 3 packet times (15 ms) | `PUT /api/outputs/{node}/latency` | agent reloads its receiver |

### 2.4.1 The sendspin knob is an advance

The natural assumption — "all three are additive only; you can delay a speaker,
never advance it", and therefore that the reference must be the *latest*-arriving
member (§9.1) — is right for AP2 render delay and pw-sink playout delay and
**exactly backwards for sendspin**. Five independent confirmations:

- the ESPHome speakers run `Sendspin/sendspin-cpp`, whose `sync_task.cpp` computes
  `client_timestamp = get_client_time(entry->timestamp) −
  get_effective_static_delay_ms() * US_PER_MS − config.fixed_delay_us` — the static
  delay is **subtracted**, so a larger value plays **earlier**;
- the Rust reference client does the same (`instant.checked_sub(output_latency_micros)`);
- its comment says so outright — *"Emit each sample `delay` earlier so downstream
  (amp/speaker) latency lands it on time"*, i.e. it is an output-latency
  compensation, not a delay;
- the protocol field doc agrees;
- and our own `required_send_ahead_us` adds each member's `static_delay_ms` to the
  group lead *"because the device plays that much earlier"*.

So for sendspin you can only ever **advance** a speaker (0–5000 ms). A sendspin-only
group's reference is the **earliest**-arriving member; a mixed group holds knobs of
**both signs** at once; and the write-back stays expressible (`advance_i = max_j(d_j)
− d_i` reproduces the same relative geometry, since a common shift is free) but the
numbers are **inverted, not offset by a constant** — a sign bug, not a calibration.
Asking by ear was not viable, which is the point of this feature.

### 2.4.2 The solver models knob *ranges*, not a reference member

Each member's knob has a **polarity and a range**, so the right model is a
feasible-interval intersection. For member *i*, let τ_i be its measured arrival with
its **current** knob value:

| Kind | Knob | Arrival as a function of the knob | Achievable arrivals |
|---|---|---|---|
| sendspin | advance `a ∈ [0, 5000]` ms | `τ_i + a_i − a` | `[τ_i + a_i − 5000, τ_i + a_i]` |
| AP2 / pw-sink | delay `d ∈ [D_min, D_max]` | `τ_i − d_i + d` | `[τ_i − d_i + D_min, τ_i − d_i + D_max]` |

`D_min` is 0 for AP2 but **15 ms** for pw-sink, so a pw-sink member cannot be placed
arbitrarily early. The common target **T** must lie in the intersection of every
member's interval:

- **Empty intersection ⇒ refuse**, naming the two members whose ranges do not
  overlap. This is a real case in a mixed group: sendspin can only move *earlier*
  from where it sits and AP2/pw-sink only *later*, so a group whose sendspin member
  is already the earliest and whose AP2 member is the latest is alignable only within
  whatever headroom their current values provide.
- **Otherwise pick the T that minimises the largest knob value.** Both polarities
  cost latency — an AP2 delay directly, a sendspin advance because
  `required_send_ahead_us` adds it to the group lead — so this generalises §9.2 while
  keeping the high-water mark low.
- For a **sendspin-only** group it reduces to `T = min_i(τ_i + a_i)`: the member
  with the earliest *intrinsic* (knob-zero) arrival gets advance 0 and every other
  member is advanced to meet it.

Consequence for §9.2: the send-ahead high-water mark is driven by *advances* as well
as delays, so the check must consider both.

---

## 3. The measurement principle

**Everything is measured as a relative time within one continuous microphone
stream.** The daemon never needs to know when a burst was emitted in the phone's
time base, and the phone's clock never needs to be synchronised with anything. That
deletes an entire class of hard problems: WebSocket jitter, browser timestamp
resolution, NTP, PTP, network delay.

Why it works: every member renders the *same content* from the *same anchor
stream*, so member *i* renders content-frame *N* at `T_content(N) + delay_i +
path_i`. Differencing two members eliminates `T_content(N)` entirely, leaving
`(delay_i − delay_j) + (path_i − path_j)` — the thing we want, plus the geometry
term §1 is about. This holds across transports without any of them sharing a clock,
because the phase reference lives in the *audio content*, not in a clock.

Requirements on the mic stream: **gapless and monotonic** (count frames; never
derive time from wall-clock timestamps or `AudioContext.currentTime`), **drop
detection** (a sequence number per chunk, and a gap invalidates the window in
progress rather than silently shifting every later measurement), and **continuity
across a measurement set** (§1.2).

---

## 4. Microphone ingest

### 4.1 Permission and secure context

The HA ingress view renders the add-on UI in an `<iframe>` with **no `allow=` and
no `sandbox=` attribute** (verified in the frontend bundle on the live instance).
That is fine rather than fatal: `ingress_url` is a same-origin path
(`/api/hassio_ingress/<token>/`) and the Permissions-Policy default allowlist for
`microphone` is `self`, which covers same-origin iframes. So `getUserMedia` needs no
`allow="microphone"` and nothing sandbox-blocks it.

**The real gate is secure context.** On `http://homeassistant.local:8123`,
`navigator.mediaDevices` does not exist — plain HTTP on a LAN hostname is not a
secure context and only `localhost` is exempt. This instance has no `ssl_*` in
`configuration.yaml`'s `http:` block but runs `core_duckdns` + `core_nginx_proxy`,
so the HTTPS path exists and **that** is the URL this feature must be used and
tested on. Product consequence: "open Home Assistant over HTTPS" is a documented
precondition, the UI detects the insecure case and says so plainly, and there is no
workaround.

### 4.2 Capture constraints

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

`track.getSettings()` is read back, but the check **cannot be a hard refusal on
absence**: Safari omits `echoCancellation` / `autoGainControl` / `noiseSuppression`
from `getSettings()` entirely, so refusing when they are unreported would rule out
every iPhone. So an explicit `true` refuses and explains why; unreported proceeds
with a visible caveat. That leaves a real hole — an iPhone silently applying AEC
looks identical to one not reporting — for which the practical detector is
behavioural rather than declarative: AEC converges over seconds, so a burst
amplitude that decays monotonically across a measurement is the signature. iOS is
out of scope (§14.4), so nothing currently depends on it.

### 4.3 Transport

`AudioWorkletNode` → 128-frame blocks → batched to ~20 ms → `Int16Array` → binary
WebSocket frame with a 4-byte sequence number. At 48 kHz mono that is ~96 kB/s,
which is nothing on a LAN. A 44.1 kHz `AudioContext` (iOS) is handled as well as
48 kHz, with the rate carried in a JSON hello frame.

`GET /api/align/mic/ws` takes one socket at a time; a second connection is
rejected. The server sends **one text frame back** before any audio flows —
`{"type":"ready",…}` or `{"type":"error","reason":…}` followed by a close — because
without it the "another capture is already connected" rejection reaches the browser
only as an opaque close and the client cannot tell refusal from a dead socket.

**The socket is deliberately not bound to an alignment session.** The mic check has
independent value *before* a session exists — it is how the user finds out their
phone works at all — and the binding belongs at the orchestration layer, where the
loop-phase gate lives anyway. Closing the socket does not tear down the session
(the user may be switching modes).

Back-pressure policy: if the send queue stalls, the client drops blocks and bumps
the sequence number so the server sees a gap and discards the window. Never buffer
to catch up — a stale window is worse than a missing one.

---

## 5. The estimator

### 5.1 Signal

The existing click track. An 8 ms Hann-enveloped burst has ~250 Hz null-to-null
main-lobe width and gives ~0.3–1 ms peak-position accuracy at reasonable SNR —
comfortably matching the 1 ms granularity of the delay knobs. The upgrade path, if
a real room smears the peaks, is a 200–300 ms log chirp (200 Hz → 8 kHz) inside the
same 2 s loop with a matched filter: ~25 dB more processing gain, and an FFT
dependency. That is W9, and §5.6.1 explains why it waits.

### 5.2 Detection needs no new dependencies

Per channel: a complex bandpass at the channel's centre frequency → magnitude
envelope → peak pick → parabolic interpolation on the envelope maximum for
sub-sample resolution. O(n) with a handful of multiplies per sample, on a tokio task
off the RT threads.

**The filter shape matters.** A single boxcar/Goertzel integrator matched to the
burst length is the obvious choice and it does not work well: the correlation peak
it produces is quartically flat, and the negative-frequency image leaves a ripple at
2·f_c whose crests become local maxima, so the picked peak hops between crests.
Measured: ±0.5 ms per-period spread and a 0.15 ms bias at 30 dB SNR. The fix costs
one extra running sum and no dependency — **two cascaded boxcars of L/2, i.e. a
triangular analysis window**, whose stopband is the boxcar's squared. Measured:
±0.04 ms spread, 0.013 ms bias, about 20× better. Use a linear-phase integrator
rather than a resonator or one-pole, for the same reason §6.2 cares about group
delay: a frequency-dependent delay in the *analysis* filter becomes a per-channel
bias, which is exactly what must not happen.

### 5.3 Phase, epoch and ambiguity

Every arrival is reduced to a **phase within the 2 s pattern**, and the A/B
frequency labelling identifies *which* burst was detected, so arrival
identification is unambiguous over the full 2 s. **The measured *spread* still wraps
at ±1 s**, though, and nothing distinguishes a wrap from a small offset — so a group
misaligned by more than a large fraction of the period is refused rather than
silently mismeasured. The implementation refuses beyond 0.4 × pattern (800 ms) and
says to rough the group in by ear first. AP2 members carrying 800 ms render delays
sit right at that boundary, so this is not theoretical.

**Drift needs a fit, not just alternation.** §6.1's solo-alternation reduces
mic-clock drift error but leaves a residual, and removing that needs a pooled
common-drift slope fit, which needs **≥2 measurement passes** — so two passes are
structural, not padding, and a single-pass run carries an explicit "no drift
correction applied" warning.

**Near field is the exception, and it is not a loophole.** Its closure measurement
gives *one* member — the anchor — a second reading, and that is all a slope needs.
Fed to the same pooled fit, the anchor is the only member with two points, so the
slope reduces exactly to `closure_error / walk_span` and every single-reading
member's offset becomes `phase − slope · time`. The closure measurement **is** the
drift fit rather than a second mechanism beside it, so a near-field walk is one pass
**plus** closure and must *not* carry the no-drift-correction warning.

Averaging over several loop periods brings noise down as √N; reverb does not average
down (it is deterministic for a fixed mic position), which is why the chirp upgrade
exists at all. **A per-period validity gate is required:** the period grid origin is
arbitrary, so a burst can straddle a period boundary and that period then
contributes a pure-noise phase to the fit. Periods below an SNR gate are excluded
from the fit but **still counted in the reported SNR** — so a hopeless capture is
refused as low-SNR rather than quietly disappearing as "too few periods", which
would tell the user nothing about what went wrong.

### 5.4 Output

Per member: phase in ms, a peak SNR, the ratio of largest to second-largest peak in
its channel, drift in ppm, and a **standard error** from the spread across loops.
Everything downstream consumes the uncertainty, not just the point estimate. The
straight-line fit's intercept needs an **origin shared across channels** — the
abscissa is centred on the aggregation window — because per-channel origins would
make the intercepts incomparable and inflate the standard error with pointless
extrapolation.

### 5.4.1 Measured accuracy (synthetic signals)

Delta error on a 17.3 ms injected offset:

| Condition | Result |
|---|---|
| Noiseless, 48 kHz / 44.1 kHz | +0.005 ms / +0.012 ms |
| 30 → 5 dB broadband SNR | 0.014 → 0.012 ms |
| 0 dB broadband SNR | 0.065 ms (still accepted) |
| −3 dB and below | refused |
| +100 ppm clock drift | drift reported as +100.0 ppm; delta still 0.02 ms |
| Reverb train, +7…+71 ms at −4…−19 dB | direct arrival found, 0.14 ms, accepted |

Broadband SNR here is burst peak / noise RMS; the *reported* peak SNR runs ~17 dB
higher. The cliff from "0.15 ms" to "meaningless" spans about **3 dB**, and
`MIN_PEAK_SNR_DB` = 15 sits on the safe side of it. `std_error_ms` is the sharpest
discriminator across that cliff (0.06 ms → 51 ms).

**Noise is not the limiting factor.** A phone in a quiet room has 20+ dB more SNR
than this needs. §5.6 is the limit.

### 5.5 Refusal rules

The estimator must be willing to say no. It refuses — and falls back to by-ear —
when peak SNR is below ~15 dB in any channel after the learning phase has done its
best; when the second-peak ratio is too close to 1 (ambiguous which arrival is the
direct sound); when the standard error across loops is above ~1 ms (user moving,
drifting, or a bad room); when any block in the window clipped; or when there is a
sequence gap in the window.

Writing a delay from a bad measurement is much worse than not writing one: it
silently degrades a system the user previously had aligned by ear.

### 5.6 The blind spot the refusal rules cannot cover

**None of §5.5's checks can detect an early reflection that lands inside the
analysis window.** It merges with the direct arrival into one peak and pulls it. To
every verdict the result looks excellent: the peak is single and strong (second-peak
ratio > 10), and because the reflection is deterministic the *same wrong answer*
arrives every period, so the standard error is under 0.01 ms. The estimator is
confidently wrong.

Measured with a 0.9× reflection on one channel: −1.72 ms at +1 ms, +0.89 at +2 ms,
−0.93 at +3 ms, −0.40 at +5 ms, ~0 at +8 ms. With the reflection *louder* than the
direct sound (1.4× at +5 ms) the estimator locks onto the reflection and reports
+5.2 ms as accepted. Beyond the window a reflection becomes a separate peak and is
correctly refused as ambiguous — so the exposure is bounded by the guard distance,
not unbounded. This is codified as a **passing test**
(`an_early_reflection_biases_silently`) with asserted bounds, so a future change
that fixes or worsens it will be noticed.

Three consequences, all load-bearing:

- It is the real argument for **W9** (chirp + matched filter), which resolves the
  direct arrival from an early reflection instead of merging them. W9 was originally
  framed as a noise-driven upgrade; it is not (§5.4.1 shows noise is a non-problem).
  It is a *reflection* fix, gated on evidence (§5.6.1).
- It makes §10.2's transitivity check non-negotiable — **and that check is far
  weaker than it looks.** Transitivity as literally specified is arithmetically
  vacuous here (§10.2), and the frequency-band form that replaced it must carry a
  3 ms tolerance to avoid firing on loudspeaker crossover differences. So a pass does
  **not** prove this blind spot was avoided; 1–2 ms of per-speaker bias sails through
  every check in the design.
- **Near-field mode is a partial mitigation for free**: at arm's length the direct
  sound dominates any reflection by far more than the 0.9× that produced these
  numbers.

### 5.6.1 W9 waits for evidence, and will never be a toggle

W9 is the only proper fix for the blind spot above and is nonetheless **not built**.
Three reasons, in order of weight.

**A toggle would ask the user the one question this whole feature exists to
remove.** Choosing between two DSP front-ends is a strictly harder judgement than
"did that speaker move earlier or later", which is already beyond the ear — that is
why we measure. A "use chirp" checkbox hands someone a decision they have no
instrument to make, and no way to know afterwards whether they chose right.

**The need can be measured with what already ships.** §10.2's cross-band check
computes, per member, `split_i = phase_B(i) − phase_A(i) − 1000 ms` — the split
between the 1.5 kHz and 3 kHz arrivals. That *is* the reflection signature: a merged
early reflection biases the two bands differently, a clean direct arrival does not.
So **W22 should read the distribution of `split_i` across a real run.** Splits
clustering near zero say reflections are not biasing much and W9 buys little; splits
clustering at 1–3 ms say they are. Far cheaper than implementing a chirp, and
available the moment this is deployed.

**The cost is not the FFT, it is the second calibration.** Every threshold here was
derived against the 8 ms Hann burst: `MIN_PEAK_SNR_DB`, the second-peak ratio, the
~12 ms guard distance, §10.2's 3 ms tolerance, §1.2's closure rate bound, §1.1's
8 ms overlap tolerance. A chirp changes peak shape and guard distance, so all of them
need re-deriving — and several are justified by *measured* numbers rather than
argument. Add `rustfft` and FFT correlation on a 4-core Pi with a documented history
of CPU starvation, and "optionally enabled" stops being a flag and becomes a second
configuration of the estimator, needing its own validation against synthetic signals
just like the first — which is itself still unvalidated against a real room.

**If W22's splits say it bites**, the shape is an **automatic escalation**, not a
preference: measure with the click track, and when the evidence points at a
reflection (a persistently non-zero cross-band split, or a marginal second-peak
ratio) switch *that member* to the chirp, re-measure, and report that it did so and
why. The daemon decides, because the daemon is the only party holding the numbers —
the same principle as the per-output silence and level channels (§12.3.2). A
narrower first step is legitimate too: a chirp offered as an on-demand **diagnostic**
on one member ("why is this speaker uncertain?"), outside the default path, needing
no recalibration of the run.

**The argument on the other side, recorded rather than buried:** if the bias does
bite, not having W9 ready means another development cycle before anyone can align an
apartment properly. That is a real cost. It loses to the risk of calibrating a second
signal path on the same unvalidated foundation as the first.

---

## 6. Excitation: sequential now, parallel later

### 6.1 Stage 1 — sequential solo-alternation (built)

Keep the shared anchor stream exactly as it is, alternate the solo between members
every loop period, and measure each member's phase in its own window.

- **Zero new audio plumbing** — it reuses `play_loop_to_target` and the audibility
  machinery as-is.
- **Works for every member kind**, because the phase reference is carried in the
  content (§3) and is therefore transport-agnostic.
- Alternating (rather than all of A then all of B) makes mic-clock drift average out
  instead of accumulating.
- Mute is not sample-precise — it lands within the stream's send-ahead window — so
  one loop period of guard sits between the switch and the first accepted window.
  That guard is the same "wait for a confirmed tone sequence" gate that covers
  reconnects and settling, implemented once as *re-acquire loop-phase lock with
  stable amplitude before accepting a window* (§8).
- Cost: ~8–10 s per member per pass, linear in N.

This is the stage that proves the estimator against real rooms and real phone
microphones, which is where the actual risk lives.

### 6.2 Stage 2 — parallel, frequency-division with per-device gating (W7, unbuilt)

Give every member its own burst frequency and measure all N in a single loop pass;
measurement time becomes roughly **independent of N** (10–20 s for the whole group
versus 10 s × N). The mechanism that makes this work across transports is **not**
per-device tone *synthesis* (§6.3) but **per-device filtering of shared content**:

1. the anchor stream carries a 2 s loop containing all N bursts, each at its own
   frequency, **time-staggered** into its own slot;
2. each per-device relay applies a narrow **bandpass at that device's assigned
   frequency**, so each speaker emits only its own burst;
3. the mic separates the channels by frequency, and the assigned slot is a
   cross-check on identity rather than the identification mechanism.

Why filtering rather than synthesis: a filter needs **no timing information at
all** — no content position, no frame index, no presentation timestamp — which is
precisely what the AP2 and pw-sink relays cannot provide. The injection point
already exists in all three relays (`overlay_mixer::mix_into(node_name, …)` is
called per device); a sibling `cal_gate(node_name, block, &mut buf)` slots in beside
it.

Design notes:

- **Filter delay is a differential bias.** A biquad's group delay depends on centre
  frequency and Q, so different channels are delayed differently. A linear-phase FIR
  is exact by construction but ~129 taps × 48 kHz × 2 ch per device is real work on a
  relay thread that is already tight on a 4-core Pi; take a 2–3 biquad cascade
  (~15 MAC/sample) with **analytic group-delay compensation** per channel — known at
  design time to well under 0.1 ms, and a constant rather than a measurement error.
  Verify it in a unit test by filtering a synthetic burst and checking the peak shift
  against the analytic value.
- **Time-staggering is free and worth doing** even though the bandpass makes it
  unnecessary for identification: only one speaker is energised at a time, so the
  room's reverb floor stays low. With simultaneous bursts every speaker's reverb tail
  lands in every channel and the floor rises with N — that, not band crowding, is the
  real scaling limit. With a 2 s loop and N slots, a 6-speaker group gets 333 ms
  slots, tolerating ±166 ms of slippage before slot confusion, and the frequency label
  resolves it even beyond that.
- **Frequency assignment must avoid harmonic collisions.** A burst driven hard
  produces 2nd/3rd harmonic distortion in the speaker, and if that lands on another
  channel's centre frequency it creates a stable spurious peak at the wrong time —
  much more dangerous than noise. For N ≤ 4 keep the whole set inside one octave,
  e.g. **2000 / 2500 / 3050 / 3700 Hz** (spacings ≥ 500 Hz, and 2×2000 = 4000 sits
  300 Hz above the highest channel). For N > 4 the set needs a small constrained
  search (no member within ±300 Hz of 2× or 3× another, ≥500 Hz spacing, inside the
  phone-mic + small-speaker usable band of roughly 800 Hz–6 kHz). Do not hand-wave a
  table: the learning phase measures actual per-channel crosstalk (§7), so validate
  the assignment empirically at runtime and reassign or fall back to Stage 1 if a
  channel pair fails.
- **Something must still pump the graph.** The relays only run when capture delivers
  blocks, so `play_loop_to_target` keeps running — it is what feeds the sink. The
  per-device gate shapes that content; it does not replace the source.

### 6.3 Why per-device synthesis does not generalise

Recorded so it is not retried. Synthesising each device's tone as a pure function of
a shared timeline position is sample-aligned by construction, needs no filter, and
gives the best possible SNR — but it works **only inside the sendspin per-device
relay**, which has exactly what is needed: `let ts = timeline.stamp(len)` is computed
once per block and fanned identically to every member, so `tone(ts, device_i)` is
coincident across sendspin members by construction.

The other two transports have no such anchor. The AP2 relay just fans PCM chunks to
each `LiveFrameSender`, with no presentation timestamp in the loop, and it runs from
its **own** capture with an independent frame origin; pw-sink is the same shape.
Using each relay's local monotonic "now" instead would inject that relay's
scheduling jitter (~one quantum, 10–20 ms) straight into the measurement — far worse
than the sub-millisecond target. So synthesis is an optional optimisation for
all-sendspin groups; §6.2's filtering is the one that covers mixed groups, and if
only one is ever built it should be §6.2.

---

## 7. The level-learning phase

Needed, and for a sharper reason than "until the mic can hear it". The constraint is
**two-sided**: clipping is broadband, so one clipped mic block corrupts **every**
channel rather than just the loud speaker's, and with AGC off (§4.2) nothing manages
headroom, so the *sum* of all speakers must stay under the mic's ceiling — while
each *individual* channel needs margin over the noise + reverb floor in its own
band. **Target 25 dB, not the estimator's 15 dB refusal threshold**: §5.4.1's cliff
is only ~3 dB wide and 15 dB sits *on* its safe edge, leaving zero margin for the
floor moving between the learning pass and the measurement. (§7.1 has the second
reason.) A near speaker can easily be 20 dB hotter at the mic than a far one, so
this is a per-speaker gain solve, not a global volume.

The solver lives in `align/levels.rs` and is complete, including the two-sided solve
and the crosstalk verdict. **Its automated ramp is not yet driven** —
`align/measure.rs`'s `learn_levels` still reports `learned: false` — so in practice
levels are set per speaker in the wizard (§12.2) rather than solved for.

Level knob availability differs by member kind:

| Kind | Level knob | Note |
|---|---|---|
| sendspin | `set_volume`, live | already used by the session |
| AP2 | `outputs::ap2::volume::set_volume` | exists, but the session otherwise leaves AP2 level device-authoritative, so this needs snapshot/restore and a for-the-session-only exception to the "no-impose" decision. **Restore cannot simply mirror the sendspin snapshot:** an absent level is genuinely *unknown*, so the restore entry is optional and "unknown" must mean *leave the receiver alone*, never write an invented level |
| pw-sink | `outputs::pwsink::agent::Agents::set_volume`, **when an agent is answering** | The host's `SetVolume` drives the receiving sink and its `HostState.volume` reports back, so such a member is a `SnapshotRestore` knob — the same shape as AP2 and for the same reason. `HostState` speaks cubic 0.0–1.0, and "unknown ⇒ leave the host alone" applies exactly as for AP2; a host reports a level only while it is *receiving*, so a stream that came up after the snapshot pass is genuinely unknown |
| anything else | none | no agent answering, a sink with neither a device route nor a node volume (`pwrouter-agent` prints `lever: <none>`), a future output kind |

**The level knob is a per-output capability, not a property of the kind** — the same
correction §12.3.2 made for the mute. Two pw-sink members can differ, and one
member's answer changes when its agent drops mid-walk, so it is resolved per
position (`calibrate::level_plan`, beside the silencing) and carried into the solve
as `align_levels::LevelMemberSpec::with_knob` rather than derived from the kind.
Unlike the mute there is **no universal fallback**: the relay has a mute and no
gain, so "no level knob" is a real outcome rather than a degraded one.

An unadjustable member — *genuinely* unadjustable — does not just risk being **too
quiet**, which is a report-only nuisance. The dangerous direction is the reverse:
**an unadjustable member that clips.** No amount of turning the others down rescues
it, because the ceiling is set by the member you cannot touch. That is a distinct
refusal and it must name that member — and must *not* name a member whose level did
reach the far end, which would send the user to the wrong speaker.

Outputs of the phase: a per-member calibration level fed forward into the
measurement stage, and a hard **refusal** if the target SNR is unreachable without
clipping. The refusal names **both** roles — the member that is too quiet (what the
user can act on) and the member setting the ceiling (why they must) — because naming
one leaves the user with no action.

### 7.1 What parallelises, and what the crosstalk matrix costs

**The learning phase does not parallelise in Stage 1.** Under the shared click track
every speaker emits *both* bursts (§2.2), so the measurement channels are shared and
a per-member SNR cannot be attributed from an all-play round at all. Stage 1 must
ramp **sequentially**, one solo per member; only Stage 2's per-speaker frequency
assignment makes a parallel ramp possible. The solver therefore carries two ramp
modes, and the parallel one refuses duplicate channel labels at construction rather
than returning an unattributable answer.

**The crosstalk matrix is not a free by-product of a parallel ramp.** The estimator
returns one peak per channel per window, so leakage landing inside another member's
slot cannot be separated out of an all-play round; in Stage 2 the matrix costs **N
extra solo rounds after convergence.** It *is* genuinely free in Stage 1, where the
ramp is already solo-per-member. And **the matrix's dynamic range is bounded by the
driven channel's SNR** — leakage below the noise floor reads as ~0 dB — so the SNR
target must exceed the crosstalk threshold in magnitude or the verdict is
unprovable. A 25 dB target against a −20 dB "usable" bound is what makes it mean
anything, and that is the second reason the target is 25 and not 15.

Round bounds, both enforced: parallel `6 + N` (6 ramp rounds plus one solo per
member for the matrix), sequential `2N + 4`.

---

## 8. Orchestration and cost budget

```
  IDLE
   └─ start(group, mode) ────────────────────────────────────────────┐
  ARMING       mic socket open? constraints honoured? loop-phase lock?
   └─ LEARNING     ramp levels, build crosstalk matrix, validate assignment
       └─ MEASURING    N phases + uncertainties, ≥2 passes (drift fit needs 2)
           └─ SOLVING      knob intervals (§2.4.2); deltas; sanity-check
               └─ PROPOSED     ← parks here; the user sees deltas + confidence
                   └─ WRITING      batch all delay writes → one reconnect wave
                       └─ SETTLING     collapses into the per-member gate (below)
                           └─ VERIFYING    residual; transitivity (§10.2)
                               └─ DONE / RETRY-ONCE / REFUSE
```

`PROPOSED` exists because `apply` is an explicit user step (§11): the machine has to
park between solving and writing, and nothing is written without passing through it.

`SETTLING` is **not observable as its own state**: the group snapshot lists
*configured* devices, not live connections, so "wait for lock to return on every
member" cannot be asked directly. It collapses into the per-member gate with the
reconnect timeout (180 s, sized by §2.3).

Every transition into a measuring state goes through the same gate: **re-acquire
loop-phase lock with stable amplitude before accepting any window.** One mechanism
covers mute settling, reconnect recovery, the user moving, and the socket
reconnecting. `GATE_MIN_PERIODS` is the estimator's floor plus margin (4 periods
≈ 8 s), which is why the pre-flight signal check uses a shorter window (§12.2).

Budget, 5 speakers, from the implementation's own timing constants:

| Stage | Sequential (§6.1) | Parallel (§6.2) |
|---|---|---|
| Learning | `2N + 4` rounds ≈ 30 s (sequential — §7.1) | `6 + N` rounds ≈ 25 s |
| Measure | ~110 s (gate + guard ≈ 11 s per member per pass, × 2 passes) | ~15 s |
| Write + settle | one reconnect wave, ~30–60 s | same |
| Verify | ~55 s (one pass) | ~15 s |
| **Total** | **~4 min** | **~1.5 min, ~flat in N** |

Both are a vast improvement on by-ear. Parallel's win is that it stops growing with
the number of speakers.

---

## 9. Writing the delays

### 9.1 Reference selection

Superseded by §2.4.2: there is no reference *member*, because the knobs are not all
additive. The solver intersects the members' feasible intervals and picks the common
target inside that intersection; for a sendspin-only group that lands on the
**earliest**-arriving member at advance 0, which is inverted from the original
"reference = latest arrival" rule. The UI shows which member ended up at knob zero
and why. Manual override stays available on the by-ear path.

### 9.2 Minimise absolute delay

Normalise so the largest applied knob is as small as possible (§2.4.2). Raising a
member far enough to lift the group's send-ahead high-water mark triggers a
**group-wide** stream reconfiguration rather than a single-device reconnect, so warn
before crossing that line and prefer the normalisation that avoids it. Note that
advances count toward the mark as well as delays, and that only the real write can
cross it (§1.1.2).

### 9.3 Persistence

The existing handlers already persist before pushing live, which is what a
calibrated offset needs. Write **through the existing endpoints** rather than
touching `sync_settings` directly, so the reconnect and high-water logic is not
duplicated.

### 9.4 Undo

Every member's delay is snapshotted at session start and `revert` restores it in one
action (`revert_scope`). The write phase is destructive to a previously-tuned setup,
and one bad measurement should be one click to undo.

---

## 10. Verification and cross-checks

Two independent checks, both cheap once the estimator exists; a third was
considered and dropped.

1. **Residual.** Re-measure after settling; every member's phase should match the
   target within the estimator's standard error.

2. **Transitivity, over frequency rather than over members (§10.2).** The literal
   formulation — align B and C against A, then measure B against C — is
   **arithmetically vacuous in this design.** Every phase is read off *one shared
   grid* (§3), so `d(B,C)` is by construction `d(A,C) − d(A,B)`: the triangle closes
   exactly, whatever the per-speaker bias, and no arrangement of A-referenced
   measurements can expose a per-speaker constant.

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
   indistinguishable from a reflection. Hence `TRANSITIVITY_TOL_MS` = 3.0 — which
   means **a pass is not proof that §5.6 did not happen.** The check is real but much
   weaker than "mandatory cross-check" suggests, and that materially strengthens the
   case for W9. Its per-member `split_i` values are the data W22 must read (§5.6.1).

3. **Merged peak — dropped, not deferred (§10.3).** The idea was to put every member
   on one identical burst and confirm the mic sees a *single* correlation peak rather
   than N: the numerical form of "perfect overlap", and the check that most directly
   matches what the user was doing by ear. It is expressible — set-based audibility
   makes an N-member merged peak straightforward — but **its resolution is bounded by
   the estimator's guard distance** (burst plus analysis window, ~12 ms at 48 kHz).
   Two arrivals closer than that merge into one candidate *by construction*, so the
   check would report "single peak" for a 5 ms error that the residual check beside it
   already catches to ~0.1 ms, at the cost of a reconnect-length gate per member.
   Worse than useless, in fact: a reassuring verdict that resolves 100× coarser than
   the number printed next to it invites the user to trust the wrong one. **If the
   reassurance is wanted, give it as sound, not as a verdict** — a "play the click on
   all of them together" button at the end, so the ear confirms what the numbers say
   without dressing a blunter instrument up as a measurement.

Both real checks are reported in the UI. A green residual with a failed transitivity
check is the interesting failure and must not be hidden.

### 10.4 Walks and chains verify differently, and lose a check doing it

**A stationary residual cannot verify a near-field run.** A reading taken from one
spot measures `wire + path(P)`. After a *correct* wire alignment the wire terms are
equal, so what remains is each speaker's path difference to wherever the phone is
standing — tens of milliseconds against a 2 ms tolerance. It would fail **every**
near-field run and report a correct alignment as broken. So near-field verification
re-walks: the same arrival-driven pass, with its own closure.

**The repeatability check then becomes vacuous, and reporting it green would be
dishonest.** Under near field the only member with two readings is the closure
anchor — and the drift slope was fitted from exactly those two points, so its
residual is *identically zero*. That is an identity, not evidence, so it is reported
as **absent** for a walk rather than as a pass. Say what that costs: nothing in a
near-field run detects "the user changed how they hold the phone partway through",
which pass-to-pass agreement does catch for multi-position. A walk has **fewer**
independent cross-checks than a stationary run.

**The same rule applies to a chain.** A chain's write can only be checked where the
phone is, which is the **last** position: that position's own set — its speakers
*and* the overlaps its Δ put in step with them — is the one set genuinely aligned
there. The earlier positions were aligned at *their* spots, so re-measuring them
from here would read their path difference to this spot and fail however correct the
chain is. The residual is therefore scoped to the last position and
`Verification::scope_note` says so in a sentence; re-checking the rest means walking
the chain again. Each position's own checks (transitivity, repeatability) run and
**block that step** as it is measured, which is where they are cheap and where a
failure is still retryable.

---

## 11. API surface

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/align/mic/ws` | binary mic ingest, one socket (§4.3) |
| `GET` | `/api/align/mic`, `/api/align/mic/signal` | capture status; the pre-flight signal verdict |
| `POST` | `/api/align/measure/start` | `{mode, chain}` — begin learning + measuring |
| `GET` | `/api/align/measure` | phase, per-member state, SNR, uncertainties, refusal reasons |
| `GET` | `/api/align/measure/ws` | pushed run status (the panel does not poll) |
| `POST` | `/api/align/measure/arrival`, `/close` | near field: "I am at this speaker now"; the closure reading |
| `POST` | `/api/align/measure/position`, `/finish` | chain: `{members, overlaps}` per listening spot, measured and applied **provisionally** (§1.1.1); then renormalise globally and propose one write |
| `POST` | `/api/align/measure/apply` | write the solved delays — explicit, never automatic |
| `POST` | `/api/align/measure/revert` | restore the start-of-session delay snapshot (§9.4) |
| `DELETE` | `/api/align/measure` | abandon, leaving delays untouched |
| `GET/POST/DELETE` | `/api/align/equivalence` (+ `/ws`) | the relay-vs-device experiment (§1.1.3) |

The session endpoints it builds on: `POST /api/align/start` (hold these outputs for
the whole run, §12.3.1), `/api/align/select`, `/api/align/audible`,
`/api/align/volume`, `GET|DELETE /api/align`, `GET /api/align/groups`.

`measure/start`'s `link_to` is **refused** while W8b is unbuilt, so a client cannot
believe in cross-session coherence that does not exist. Chaining exists *within* a
run; what does not exist is a store of a finished run's aligned set with its delays.

The measurement state rides the existing `/api/align` status shape where it overlaps
(members, reference, target) rather than duplicating it.

---

## 12. Frontend

### 12.1 The wizard, and where it lives

**It lives on the Outputs page, not on a source card.** The panel used to live on
source cards because a group *is* its source set, and the session resolved the group
from `sources` and required it to already exist with ≥2 present members. The model
is inverted: the user picks *speakers*, and alignment forms a group around them.
Moving the entry point and forming a temporary group were the same change, and it
reached into routing rather than just UI. The source card lost the microphone entry
point entirely, since one session exists process-wide.

Pages: **Mode** (multi-position default, near field, manual) → **Selection +
levels** → **the mode-specific body** (the measurement run, the walk, or the by-ear
sliders) → **Review** (the proposal, its confidence and the checks — the `Proposed`
state of §8, which is why that state exists).

**Selection means something different per mode**, and the UI says so rather than
letting the user discover it through an error:

| Mode | Selection is | Validation |
|---|---|---|
| Multi-position | "the speakers I can hear from here" | Overlap needed for every step after the first; candidates pre-selected and labelled (two preferred, §1.1) |
| Near field | any subset the user wants coherent — one floor, one wing | No overlap needed *within* the walk; one needed to link to earlier **sessions** (§1.2). The UI owns the walk order |
| Manual | the set to tune by ear | ≥2, as today |

Near field must **not** force "all speakers": a multi-storey apartment is exactly
the case where the user wants one floor now and another later. Both measuring modes
therefore ask the same question once the selection is made — **link this set to
already-aligned speakers, or keep it independent?** "Link" requires an overlap;
"independent" must state plainly that this set will not be coherent with the others.
Independence is a legitimate answer (two floors never heard together), so it has to
be a deliberate choice rather than a silent consequence.

### 12.2 Level setting

Solo **one** speaker at a time (not reference *and* target, which is also what would
block §7's all-play round). The tone plays continuously on it, every other member
silenced; deselecting or clicking away **stops the tone**. An indicator turns green
when the estimator would accept the level, from `GET /api/align/mic/signal`. Two
details that matter in practice:

- **Default start level 20 %**, not the by-ear path's 50. Real in-room readings came
  out at 65 dB / 70 dB peak SNR, roughly 40 dB above target, so 50 is needlessly loud
  for a procedure done standing in a living room.
- **The green indicator must be faster than the measurement gate.** The gate uses
  `GATE_MIN_PERIODS` (≈8 s), which is right for a gate and far too slow for a volume
  slider, so the pre-flight uses one or two periods — it needs a rough SNR, not a
  phase. A compile-time assertion pins that relationship.

**Near field breaks the two-phase shape.** Its level is only meaningful *at* each
speaker, and the risk inverts from too-quiet to clipping — so level-setting folds
into each arrival (walk up, it goes green, measure, move on), which also makes near
field a single pass instead of two.

**Stop must work at every point**, in every mode, and restore levels, mutes and
routing exactly as teardown does.

### 12.3 Exclusivity: what a "temporary group" has to block

The selected speakers form a temporary group that no other audio reaches, and
membership alone does **not** achieve that. Checked rather than assumed:

- the announce arbiter derives occupancy purely from **in-flight announcements**, so
  an aligning output is invisible to it and admission takes the immediate-start path.
  `announce/mod.rs` also *creates* a per-device sender when none exists — including
  an on-demand AP2 session — so being unrouted protects nothing;
- even a reservation would not stop `barge_in`, whose admission order is
  `if !overlaps { start } else if barge_in { preempt } else { on_busy }`;
- voice ducking is a second, independent interferer: a `DuckHold` attenuates music
  with no clip of its own, so an assistant turn would duck the calibration tone.

The right semantics are **not** "block announcements" — nobody wants a fire alarm
suppressed by a calibration. Ordinary announcements **queue** (or reject) because
the reservation makes the output busy for admission purposes; barge-in
announcements and duck holds **win**, and the reservation holder is *notified*, so
the affected member's measurement is discarded with a reason naming the cause. The
notification is the load-bearing part: without it this is the same class of bug as
§2.3.2's intermittent stream, where the gate blames the user's hand for something a
doorbell did.

### 12.3.1 Forming the group costs a reconnect wave — so hold the union once

Grouping is *derived*: same source set ⇒ one group ⇒ one anchor. So forming a group
around an arbitrary output set means giving those outputs a source set nothing else
has — a **new group key**, hence a new anchor, hence **new per-device senders**.
Every selected sendspin member therefore reconnects when the hold forms *and* again
when it releases: tens of seconds each way (§2.3). Tolerable once for a
single-position run; for a **multi-position** run where each position re-selects a
different set, five positions would cost ten reconnect waves — exactly the cost
§1.1.1 removed from the write path, reintroduced in the formation path.

**Decided: hold the union once, scope each step by audibility.** Select every
speaker the run will touch up front (usually "all of them", or "this floor"), form
**one** hold, and let each position choose which held members are *audible*. Mutes
are live and free, and the set-based audibility already built is exactly the
mechanism, so the whole multi-position run costs **one** form and **one** release.
The only UX consequence is that the up-front selection is the run's full scope
rather than the first position's — which for whole-apartment alignment is what the
user means anyway. Two alternatives were rejected: a no-op special case when the
selection is exactly one existing group's member set (zero reconnects, but a second
formation mechanism with its own restore obligations, made unnecessary by the
above), and simply accepting the cost (reasonable only for single-position runs).
Composed with §1.1.1, a whole multi-position run then contains exactly two reconnect
waves — the formation and the final real write.

How it works (`align/group.rs`, `align/calibrate.rs`, `api/align.rs`):

- `POST /api/align/start {outputs}` means "**hold all of these for the whole run**".
  Its doc comment says so, because it reads counter-intuitively next to a wizard that
  then works on a subset.
- `align_group::plan_hold(held, requested)` is the whole rule, pure and unit-tested:
  `requested ⊆ held` (and ≥2) ⇒ `HoldPlan::Scope`, anything else ⇒
  `HoldPlan::Form(why)`. `AlignManager::begin` consults it **before** bumping the
  start generation or tearing anything down; `Scope` goes to `AlignManager::rescope`,
  which touches nothing that could reconnect a speaker — not the reconciler's
  override, not the reservation, not the anchor, not the click loop, not the
  level/mute snapshot. It updates the echoed selection, the mode, reference/target/
  audibility, and re-arms the safety timeout (so a long walk is not cut off), and
  deliberately does **not** reset the playback level.
- A **superset** re-forms, on purpose. The reconciler would cope with growing the
  hold in place (the align group's key is the constant `align-hold-source`, so adding
  a member dials only that member) — but the session's snapshot/restore set, member
  list and level state would all have to grow mid-run, and the added speaker dials
  anyway. Not worth a second formation mechanism with its own restore obligations.
- The **cost is reported where the user is**: `AlignState` carries `hold_id`
  (unchanged ⇒ nothing re-formed), `hold_reused` and `hold_cost` — a sentence saying
  either "every member reconnected for this and will again on release; scope positions
  with `/api/align/audible` instead" or "no speaker reconnected: these speakers are
  already held".
- The test that pins the claim asserts on **hold identity**, not on a side effect:
  `(hold.id(), hold.anchor_node_id())` is unchanged across a subset `start` and
  across the same-union `start`, while a start naming an uncovered speaker leaves no
  session at all (it tore the old one down to re-form).

### 12.3.2 Silencing and levelling are per-output capabilities

Four corrections from building §12.3, each of which started as a per-*kind*
assumption:

**Reservations cannot literally participate in `occupancy()`.** That function is
also the before/after basis for the action diff, so a reservation placed in it emits
`DuckMusic`/`StartAnnouncement` for a clip that does not exist. Two distinct notions
are needed: **action occupancy** (announcements only) and **admission busy-ness**
(announcements ∪ reservations). Admission and `settle()` consult the latter; the diff
keeps using the former. And §12.3 missed the **queued** case: a clip admitted
*before* the hold sits in the queue and would start the instant its output frees —
over the calibration — so `settle()` must consult reservations too, not only
`begin()`.

**The on-demand-session hazard is sharper than recorded.** During a hold the
*stored* intent still shows a held output as unrouted, so the announce path would
open a **second** AP2 session for a barge-in, competing with the align group's own
sender on a receiver that accepts one. The intent override therefore has to be
applied where announce transport is ensured, not only where the group is derived.

**An interference sentence has to name the speaker the way the user does.** It
quoted the node name (`'sendspin-dev-kitchen'`) while the chip beside it in the UI
said "Küche" — the same report reading as two different speakers. Display names are
now resolved **once, when the hold forms** (`align_group::resolve_labels`: the rename
store first, `routing::output_display_name` as the fallback — exactly what the
Outputs page and the routing matrix do) and carried *with* the hold, because the
reporters (`announce/mod.rs`, `outputs/overlay_mixer.rs`) run where the outputs store
is not reachable and could not have resolved a name themselves. `Interference` still
carries the node name for matching, plus `member_label` so a consumer writing its own
sentence agrees with ours.

**Silencing a member is a per-output capability, not a property of its kind (W17).**
This is the correction that mattered most, because getting it wrong would have made
runs *worse* than refusing pw-sink members did. Audibility is implemented as mute,
and a held member with no mute keeps emitting the click through every other member's
solo — so the mic hears two arrivals, and both outcomes are bad: separated by more
than the estimator's ~12 ms guard it refuses as `AmbiguousPeak` (so **every** member
fails, not just that one), and separated by less it merges into one pulled peak,
which is §5.6's silent bias by construction. The resolution is per output, in one
place, re-resolved on every audibility change because an agent can drop mid-walk:

| Channel | When |
|---|---|
| sendspin in-band mute | the transport has one |
| AP2 in-band mute | the transport has one |
| **out-of-band, via the receiver agent** | a pw-sink host whose agent answers *right now* |
| **relay zero-fill** | everything else — no agent, a lever-less sink, a future kind |

Ordering is safety-first: the relay batch silences everything non-in-band **before**
the host round trip, and the relay mute is released only once the agent write
*lands*. A write that fails leaves the member relay-muted, so an agent that vanishes
between resolve and use can never leave a speaker audible. The relay hook is the same
`mix_into(node, …)` site `align/relay_delay.rs` uses, which is what makes it
transport-agnostic; reusing the duck-hold path instead would make the session report
interference against itself (§12.3), so it needs its own mechanism.

**The level rides the same seam, and §7 states its asymmetry.** The
`OutOfBandMute` seam grew a level pair (`level` → `Option<f32>` on the host's cubic
scale, doubling as the capability query *and* the snapshot; `set_level` → did it
land), `Session::saved_oob_levels` carries the snapshot with **absent = unknown =
write nothing**, and `AlignState::level_channels`/`unlevellable` publish the
resolved answer. Three non-obvious findings from wiring it:

- **The two capabilities are genuinely independent, not two views of one.** The
  agent's `master_props` falls back to the sink **node**'s `Props` when the sink has
  no device route (a virtual sink), and that path reports `channel_volumes` with
  `mute: None`. Such a host is levelled out of band while its mute still needs the
  relay.
- **A host reports either lever only while it is *receiving*** (the lever is found
  through the receive stream's target sink), so "unknown pre-session level" is
  routine rather than exotic.
- **No `set_volume_transient`/`forget_volume` counterpart is needed** for the agent
  path. `ap2_volume` needed those because it *stores* a desired level and re-applies
  a user-set one on every reconnect; `Agents` stores nothing, so a write leaves no
  daemon-side mark and "leave an unknown level alone" is implemented by simply not
  writing.

Three conditions keep the relay fallback necessary: `set_mute`/`set_volume` return
`false` when the agent is not connected and that must be **checked**, not assumed
(an unmuted member silently corrupts the run); it is the host's **master** sink, so
muting silences everything on that machine and the UI has to say so (if the host is
someone's desktop, alignment briefly mutes their computer); and a sink can have
neither a device route nor a node volume, so a connected agent is not a guarantee.

**Why the pw-sink level control was invisible in the UI**, investigated 2026-08-11:
the *write* path was complete end to end and only the *read* path and the UI were
missing, in four independent places that all encoded the same wrong assumption —
`routing/mod.rs` populated `RoutingNode.volume`/`muted` for `sendspin-dev-*` and
`ap2-dev-*` only; `OutputsTab.svelte` gated the volume column on
`airplay2 || sendspin` with a comment asserting pw-sink "has no volume to control";
`FlowGraph.svelte` gated on a two-prefix `isVirtual`; and `types.ts` documented the
fields as sendspin-only. All four are fixed, and because `VolumeControl` already
treats `percent == null` as genuinely unknown rather than fabricating full scale, an
agent-less host correctly shows no control instead of a slider that silently does
nothing.

Two loose ends, both outside the alignment cluster:

- `align_group::ExclusiveHold::unlevellable`/`level_constraint` still derive their
  list from member **kinds**, so they would claim the clip-ceiling sentence for every
  pw-sink member — including one its agent is levelling perfectly well. `AlignState`
  now sources both from the resolved channels (`calibrate::level_note`), which leaves
  those two methods used only by their own tests: either delete them or give
  `level_constraint` the resolved list as an argument.
- `align_measure::learn_levels` still builds `LevelMemberSpec`s from kinds alone, so
  a levellable pw-sink host is *fixed* as far as the solve is concerned. One
  `.with_knob(state.level_channels[node].knob())` closes it once level learning is
  actually driven (§7, still `learned: false`).
- `routing::sync_group::GroupSnapshot` has no `pwsink_members`, so the by-ear entry
  point that resolves a group from a source set still cannot see or hold a group's
  pw-sink targets. Selecting them on the Outputs page works; closing that gap belongs
  with `sync_group`.

### 12.4 Component notes

`AlignWizard.svelte` and friends carry the measurement mode beside the manual
sliders. Pieces worth knowing:

- the `AudioWorklet` processor is a separate module file, and **Vite's `?url` import
  is a trap**: below 4 kB it inlines the asset as a `data:text/javascript;base64,…`
  URL, and `audioWorklet.addModule()` on a data URL is not reliably supported
  (Firefox rejects it). A small worklet therefore needs `?url&no-inline` so a real
  `assets/mic-worklet-<hash>.js` is emitted and resolves under ingress. Verify the
  emitted reference after any build-config change — the failure is a runtime 404 or a
  silent `addModule` rejection, not a build error;
- a secure-context / permission pre-flight that explains the HTTPS requirement
  (§4.1) *before* asking for the mic, plus the constraint read-back (§4.2);
- a live level meter and per-channel SNR readout — the user must be able to see that
  the mic is working and the room is quiet enough, or every failure looks like a bug;
- the proposed delta table with confidences, "provisional" stated above every number
  during a chain, Δ propagation naming the speakers that moved *without being
  audible*, and the apply/revert buttons;
- for near field, a "walk to speaker N" prompt driven by which channel is currently
  dominant.

Keep the by-ear controls visible throughout: when the estimator refuses (§5.5), the
user should already be looking at the fallback.

---

## 13. Risks

| Risk | Severity | Where it stands |
|---|---|---|
| ~~Phone denies mic in the ingress iframe~~ | **retired** | W0 passed on Android (§14.4) |
| ~~Browser ignores `echoCancellation: false`~~ | **retired** for the target | Android honours and reports it. The Safari-omission hole (§4.2) and its behavioural detector remain in the code and remain correct, but iOS is out of scope so nothing depends on them |
| User's HA is HTTP-only | high | detected and explained; no workaround exists (§4.1) |
| **An early reflection inside the analysis window biases a speaker by multiple ms, undetectably** | **high**, but now *measurable* | no refusal rule can see it (§5.6); §10.2 is the only exposure and its 3 ms tolerance lets 1–2 ms pass; near field mitigates, only W9 fixes it, and W22's cross-band splits decide whether it bites (§5.6.1) |
| Mute settling exceeds the 3 s `MUTE_GUARD`, so two speakers are briefly audible in the same band | medium | arrivals closer than the ~12 ms guard distance **merge into one peak** instead of raising `AmbiguousPeak` — a silently wrong answer, not a refusal. Needs a real group to size the guard |
| Amplitude-stability gate reads a broadband peak, so room noise restart-loops it | medium | the gate times out blaming the level; a band-limited stability measure is the fix if W22 shows it biting |
| A member wedges and renders intermittently while the daemon reports itself clean | medium | observed on hardware (§2.3.2); diagnosed as `GateReason::Intermittent` with the remedy named. The gate reports it but cannot *fix* it — automatic reconnect-and-retry is a candidate once it is seen more than once |
| AP2 receiver ignores a live render-delay change until its next reconnect | medium | would fail verification as `ResidualTooLarge`, pointing at the measurement rather than the transport — a misleading diagnosis, not a wrong write. §1.1.3 found the delay *is* applied live, which makes this less likely than assumed |
| Reverberant room smears peaks below usable accuracy | low | measured: a +7…+71 ms reverb train still yields 0.14 ms and the direct arrival (§5.4.1) |
| Noise floor too high | low | measured: works to 0 dB broadband SNR, ~20 dB more headroom than a quiet room needs |
| Harmonic distortion crosstalk between channels | medium | W7 only: one-octave assignment for N ≤ 4, plus the runtime crosstalk matrix (§6.2) |
| Biquad group-delay bias mis-compensated | medium | W7 only: analytic value + a unit test on a synthetic burst |
| Relay-thread CPU cost of per-device filtering on a 4-core Pi | medium | W7 only: biquad cascade not FIR, and measure with the existing xrun badges before shipping |
| Acoustic path baked into a whole-house group | medium | the mode choice is explicit and explained (§1) |
| One bad measurement silently degrades a tuned system | medium | refusal rules (§5.5), transitivity (§10), one-click revert (§9.4) |

Open questions:

- Does the ESPHome `sendspin_delay_live` path work well enough to enable during a
  measurement session only? If so the settling stage collapses and iterative
  refinement becomes affordable — which would change §8 substantially.
- Does near field's assumed ≥15 dB near/far dominance hold in a small room with a
  speaker on each wall?

---

## 14. Work packages

### 14.1 Remaining

| WP | Content | Depends on | Size |
|---|---|---|---|
| **W22** | **Live acceptance on real speakers** (§14.3). Nothing has run against hardware beyond the W0 mic spike, so formation cost, mute settling, real reconnect duration and whether the gate survives a real room are all unmeasured. **Also reads the per-member cross-band splits**, which decide W9 | a deploy | — |
| **W7** | **Parallel excitation** (§6.2): the content generator, a per-device `cal_gate` in all three relays, analytic group-delay compensation, frequency assignment plus the runtime crosstalk validation §7.1 requires | — | large |
| **W9** | Chirp + matched filter — the only proper fix for §5.6's bias, and **not** a noise fix. **Gated on W22's split data** (§5.6.1); if built it is an *automatic escalation* or a per-member diagnostic, never a preference | W22 | medium |
| **W8b** | Near field **across sessions**: optional linking to an already-aligned set through an overlap (§1.2). Δ propagation exists; what it needs is a **store** of a finished run's aligned set with its applied delays, which nothing has | — | small |
| — | Wire level learning through (`learned: false`, §7) and close §12.3.2's two loose ends | — | small |

### 14.2 Done

The microphone ingest and its worklet client (`align/mic.rs`, `MicCapture.svelte`);
the estimator and its measured accuracy (`align/estimator.rs`, §5.4.1); the run
state machine, the loop-phase gate and every §11 endpoint (`align/measure.rs`);
the two-sided level solver and crosstalk verdict (`align/levels.rs`); solve / write
/ settle / verify, with the merged-peak check **dropped by decision** (§10.3); the
temporary exclusive group as a routing-intent override (`align/group.rs`) and the
arbiter reservation with barge-in reported as `InterferenceCause`; the per-device
provisional delay line (`align/relay_delay.rs`), which is what found §2.4.1 and
§1.1.2; the §2.4.2 feasible-interval solver that replaced the inverted
reference-member rule; pw-sink membership; per-**output** silencing and per-**output**
level knobs (§12.3.2); AP2 volume driven for the session and restored, with unknown
levels left alone; session-owned calibration levels; the equivalence experiment
(§1.1.3); **multi-position chaining** with its two-overlap refusal, Δ propagation,
one write wave and global renormalisation (§1.1, §1.1.4); the **near-field walk**
with its closure measurement (which corrected §10.4, §5.3 and §1.2); and the whole
wizard — mode, selection, per-speaker solo and level, the run page with chaining, the
review page — on the **Outputs** page.

### 14.3 W22 — live acceptance, step by step

Nothing below has been done, and this is where the design either holds or does not.

1. **Deploy** (`scripts/deploy-dev.sh`; note the add-on *pulls* rather than builds
   when `image:` is set) and open the UI over the **HTTPS** URL, since §4.1's
   secure-context requirement is not negotiable.
2. **Time the formation.** Selecting a union and starting gives every sendspin member
   a reconnect (§12.3.1). The claim is "tens of seconds each way", and the whole
   union-hold decision rests on paying it once.
3. **Time a mute settle.** `MUTE_GUARD` is 3 s: if a group's send-ahead exceeds it,
   two speakers are briefly audible in the same band and arrivals closer than the
   ~12 ms guard distance **merge** instead of refusing.
4. **Watch the gate in a normal room.** The amplitude-stability criterion reads a
   *broadband* peak, so speech or clatter may restart-loop it into a timeout that
   blames the level. If that happens, a band-limited stability measure is the fix.
5. **Run a full single-position measurement** and check the proposal reads sensibly
   — in particular that a sendspin group aligns to its **earliest** member with
   *advances* (§2.4.1/§2.4.2), which is the inversion no synthetic test can confirm
   against firmware.
6. **Read the per-member cross-band splits** (`split_i` inside §10.2's check). **This
   decides W9** (§5.6.1). Near zero ⇒ reflections are not biasing much and W9 stays
   unbuilt; clustered at 1–3 ms ⇒ they are. Record the distribution, not a verdict.
7. **Run the equivalence experiment** (§1.1.3) before trusting any real write: it
   reports the relay-vs-device **scale and sign** with a resolution bound. A sign
   disagreement invalidates the deferred-write scheme.
8. **Then a chained multi-position run**, checking the two claims only a real
   apartment can test: that Δ propagation keeps the earlier rooms in step, and that
   two overlaps agree within the 8 ms geometric tolerance.
9. **Confirm teardown.** Stop mid-run, let the idle timeout fire, and start a
   superseding session — each must restore routing, levels and mutes exactly. A
   half-restored house is much worse than a failed alignment, and this is the only
   place that claim is tested against real devices.

### 14.4 W0 — the device spike: PASSED (2026-08-11)

On Android, over HTTPS, inside the ingress iframe: **the permission prompt appears**,
the capture reports **48000 Hz mono**, the constraint read-back is honoured, zero
lost frames, and the signal check reports **Good**. The feature is viable, which is
what this spike existed to establish before anything else was built.

The signal-check panel is what to read, not the level meter: it grades the weaker of
the two tones by peak SNR and states the action — `good` (≥25 dB, margin to spare),
`marginal` (≥15 dB, works now and may not once the room gets noisier), `too_quiet`
(below the refusal threshold), `unusable` (clipped or gapped, which no level change
fixes). A reassuring 15 % meter reading spans everything from ample to unmeasurable.

**iOS is explicitly out of scope**, not merely untested — there is no device to test
on and it is not a target. That retires the iOS-shaped risks rather than leaving them
open: §4.2's `getSettings()` omission (Safari-only) and the 44.1 kHz capture path
still exist in the code and are still correct, but nothing depends on them.
