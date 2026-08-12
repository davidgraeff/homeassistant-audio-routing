<script lang="ts">
  // The near-field body of the run page: one speaker at a time, on foot
  // (plan §1, §1.2, §5.3, §10.4, §12.2 — W8a).
  //
  // What the user is doing here, and why the page is shaped this way:
  //
  //   * **The measurement's premise is the user's posture, and nothing can check it.**
  //     Near field works because the sound has no distance to travel; a phone held a
  //     metre from the speaker instead of at it adds ~3 ms, and that is indistinguishable
  //     from the speaker genuinely playing 3 ms late. The daemon raises
  //     `near_field_path_assumed` on *every* walk for that reason, so this panel states
  //     it at the top and repeats it on the button that takes each reading — a caveat in
  //     the report at the end would arrive after the mistake was made.
  //   * **The level belongs to each arrival, not to a setup page** (§12.2). At arm's
  //     length the risk inverts from too-quiet to clipping, so the level is set *at* the
  //     speaker: hear it, adjust, watch the verdict, then take the reading. That also
  //     makes near field one pass instead of two. `walk.level_note` says the same thing
  //     in the daemon's words and is deliberately *not* quoted here: unlike the premise
  //     warning, it is written for an API caller and names the endpoints to post to, which
  //     is not an instruction anyone holding a phone can act on. Its content is the
  //     sentence above the level sliders instead.
  //   * **The last stop is a revisit, not a speaker.** One reading per member leaves the
  //     drift fit with no time baseline, and a house-sized walk takes minutes; reading
  //     the first speaker again at the end *is* the drift fit (§5.3). An implausible
  //     closure refuses the **whole** walk, because the correction it carries was applied
  //     to every member — so the closure step is explained before it is asked for.
  //   * **Verification walks again**, and that is not a repeat or a failure (§10.4). A
  //     stationary residual after a correct wire alignment measures the phone's distance
  //     to each speaker — tens of ms against a 2 ms tolerance — so it would fail every
  //     near-field run. `purpose` says which walk this is, in those words.
  //   * **A capture restart costs the *user* a walk**, not the daemon a retry: a walk is
  //     one continuous capture (§1.2), so the readings are void and the walk starts from
  //     the first speaker again. Said plainly, because it is expensive to a person.
  //
  // Audibility here is the user's own: `onHear` solos the speaker being stood at so its
  // level can be set. The daemon solos it again itself while taking the reading.
  import AlignSignalVerdict from './AlignSignalVerdict.svelte';
  import { DEFAULT_MEASURE_LEVEL, align, memberKindLabel } from '../../lib/align.svelte';
  import {
    WALK_CLOSURE_WHY,
    WALK_FEWER_CHECKS,
    WALK_PATH_PREMISE,
    walkActionLabel,
    walkPurposeLabel,
  } from '../../lib/measure.svelte';
  import { mic } from '../../lib/mic.svelte';
  import type { AlignMemberKind, WalkProgress } from '../../lib/types';

  interface Props {
    walk: WalkProgress;
    label: (nodeName: string) => string;
    busy: boolean;
    /** The daemon's own sentence about the path assumption, when it raised it. Shown
     *  verbatim beside this panel's wording rather than instead of it: the warning is the
     *  authority, the framing is what makes it act-on-able before the reading. */
    premise?: string;
    /** What the run is saying right now (`MeasureStatus.message`), which the run page has
     *  already printed above this panel. While the walk is parked the daemon uses the
     *  walk's own prompt *as* the phase message, so the two are frequently the same
     *  sentence — and printing a paragraph twice on one page teaches the reader to skip
     *  it. Passed in so this panel can tell, rather than guessing. */
    message?: string;
    /** "I am at this speaker" — takes the reading (`POST measure/arrival`). */
    onArrival: (nodeName: string) => void;
    /** "I am back at the first speaker" — takes the closure reading. */
    onClose: () => void;
    /** Play the click on exactly this speaker so its level can be set (a live mute). */
    onHear: (nodeName: string) => void;
    /** Silence every member. */
    onSilence: () => void;
  }
  let { walk, label, busy, premise, message, onArrival, onClose, onHear, onSilence }: Props = $props();

  /** Only when it adds something. See `message`. */
  const prompt = $derived(walk.prompt.trim() === (message ?? '').trim() ? null : walk.prompt);

  const verifying = $derived(walk.purpose === 'verify');
  const soloed = $derived(align.soloed);
  /** The walk's own progress. `measured` is in walk order, which is also the abscissa of
   *  the drift correction — so it is listed in order rather than sorted. */
  const total = $derived(walk.measured.length + walk.remaining.length);

  function kindOf(nodeName: string): AlignMemberKind | null {
    return align.kindOf(nodeName);
  }

  /** Hearing a speaker and taking its reading are two different acts, and only the
   *  second one is irreversible-ish (it is a reading in the walk's order). So tapping a
   *  speaker plays it; the button under it takes the reading. */
  function tap(nodeName: string) {
    if (soloed === nodeName) onSilence();
    else onHear(nodeName);
  }
</script>

<div class="walk">
  <div class="head">
    <strong>{walkActionLabel(walk.next)}</strong>
    <span class="badge" class:on={!verifying} class:check={verifying}>{walkPurposeLabel(walk.purpose)}</span>
    {#if walk.reading}
      <span class="badge on">reading {label(walk.reading)}</span>
    {/if}
    <span class="badge" title="Speakers read so far in this walk, of the whole held set">
      {walk.measured.length} of {total} measured
    </span>
  </div>
  {#if prompt}<p class="prompt">{prompt}</p>{/if}

  {#if verifying}
    <!-- Before anything else on a check walk: the user has just pressed Apply and is being
         asked to walk the house *again*. Without this it reads as the run having failed and
         started over. -->
    <div class="why-again">
      <strong>This is the check, not a repeat.</strong>
      <p>
        The settings have been written. A near-field alignment can only be checked where it was measured — at the
        speakers — because a reading taken from one spot would measure each speaker's <em>distance to that spot</em>,
        tens of milliseconds against a two-millisecond tolerance, and would report a perfectly good alignment as broken.
        So the check is the same walk once more, with its own closure. Nothing has gone wrong.
      </p>
    </div>
  {/if}

  <!-- The premise, at the top and framed: it is the one thing that decides whether the
       whole walk means anything, and it is invisible in every number the run produces. -->
  <div class="premise">
    <div class="premise-head">
      <strong>Hold the phone at the speaker</strong>
      <span class="badge warn">nothing can check this</span>
    </div>
    <p>{WALK_PATH_PREMISE}</p>
    {#if premise}
      <!-- The daemon's own sentence, verbatim. It says the same thing; quoting it keeps the
           report and the instruction from drifting apart. -->
      <p class="quoted">{premise}</p>
    {/if}
  </div>

  {#if walk.restarts > 0}
    <p class="caution">
      The microphone capture restarted {walk.restarts}× during this walk. Everything measured within one capture is
      comparable and nothing is comparable across a restart, so the readings taken so far were discarded and the walk
      starts again from the first speaker — that costs you the walk, not the add-on a retry. If it keeps happening, the
      phone is not holding the connection long enough to measure a whole house: try a shorter walk, or a set on one floor.
    </p>
  {/if}

  {#if walk.next !== 'done'}
    <!-- Said once, up front, because the two failures have opposite costs and the
         difference is not guessable: a reading the walk was not expecting is refused on
         its own, while the capture and the closure void the whole walk. -->
    <p class="hint">
      A reading the walk was not expecting — the wrong speaker, or one already done — is refused <em>on its own</em>, and
      what it was waiting for is still there to take. The two things that end a walk instead of one reading are the
      microphone capture restarting, and a closure the clocks cannot account for.
    </p>
  {/if}

  {#if walk.next === 'arrival'}
    <!-- ── Standing at a speaker ─────────────────────────────────────────────── -->
    <p class="hint lead">
      The order is yours. Play a speaker, set its level until the check under it is happy, then take its reading — about
      eleven seconds each. <strong>Level belongs here, at the speaker</strong>: this close the danger is
      <em>clipping</em> rather than being too quiet, so a level chosen anywhere else is the wrong one.
    </p>

    <ul class="stops">
      {#each walk.remaining as n (n)}
        {@const on = soloed === n}
        {@const kind = kindOf(n)}
        {@const lvl = align.levelControlOf(n)}
        <li class:on>
          <div class="row">
            <button class="tap" class:on disabled={busy} onclick={() => tap(n)}>
              {on ? 'Playing — tap to stop' : 'Play this one'}
            </button>
            <span class="name">{label(n)}</span>
            {#if kind}<span class="kind">{memberKindLabel(kind)}</span>{/if}
            <!-- Only where there is something to say, and only from the daemon's resolved
                 answer — never from the kind, which is not what decides it (see
                 `levelControl`). A working slider needs no label. -->
            {#if lvl === 'none'}
              <span class="note">no level control here</span>
            {/if}
            <span class="spacer"></span>
            <button
              class="primary"
              disabled={busy}
              title="Take this speaker's reading. Hold the phone within a hand's width of it — the measurement assumes the sound has no distance to travel."
              onclick={() => onArrival(n)}
            >
              I'm at {label(n)} — measure it
            </button>
          </div>

          <!-- Every member whose level the daemon can reach gets the slider, including the
               ones whose level is borrowed from the device for the run (AP2 receivers, and
               PipeWire hosts through their agent). Keying this off the *kind* denied the
               slider to both — and near field is where the level matters most, because at
               arm's length the danger is clipping. -->
          {#if lvl === 'live' || lvl === 'borrowed'}
            <div class="knob" class:dim={!on}>
              <input
                type="range"
                min="0"
                max="100"
                step="5"
                value={align.levelOf(n)}
                disabled={!on || busy}
                oninput={(e) => align.previewSoloLevel(Number((e.currentTarget as HTMLInputElement).value))}
                onchange={(e) => void align.setSoloLevel(Number((e.currentTarget as HTMLInputElement).value))}
              />
              <span class="pct">{align.levelOf(n)}%</span>
            </div>
          {:else if on && lvl === 'none'}
            <p class="hint caution">
              This one's level can only be set on the machine that plays it — no receiver is answering for it, or its
              sink has no volume control. Hold the phone a little further away if the check says it is clipping.
            </p>
          {/if}

          {#if on}
            <AlignSignalVerdict signal={mic.signal} settling={mic.signalSettling} subject={label(n)} />
          {/if}
        </li>
      {/each}
    </ul>
    <p class="hint">
      {DEFAULT_MEASURE_LEVEL}% is usually plenty at this distance. If the check says the capture is clipping, turn it
      <em>down</em> — a clipped block is broadband, so it spoils the reading rather than making it louder.
    </p>
  {:else if walk.next === 'close'}
    <!-- ── The closure ───────────────────────────────────────────────────────── -->
    <div class="closing">
      <div class="premise-head">
        <strong>One more stop: back to {walk.anchor ? label(walk.anchor) : 'the first speaker'}</strong>
        <span class="badge">a revisit, not a new speaker</span>
      </div>
      <p>{WALK_CLOSURE_WHY}</p>
      <p class="hint">
        Hold the phone the way you held it the first time. That second reading is compared against the first, and the
        arithmetic cannot separate clock drift from anything else that changed in between — a speaker that was moved, or
        a phone held differently. If the difference is too large for any plausible clock, the <strong>whole walk</strong>
        is refused rather than one reading, because its correction was applied to every speaker.
      </p>
      <div class="go">
        <button class="primary" disabled={busy} onclick={onClose}>
          I'm back at {walk.anchor ? label(walk.anchor) : 'the first speaker'} — take the closure reading
        </button>
      </div>
    </div>
  {:else if walk.next === 'busy'}
    <p class="hint">
      Stay where you are and keep the phone at the speaker until this reading is done — about eleven seconds. Moving it
      now is exactly the error nothing downstream can detect.
    </p>
  {/if}

  {#if walk.measured.length}
    <h4>Measured so far, in the order you walked</h4>
    <ol class="order">
      {#each walk.measured as n, i (n)}
        <li>
          <span class="n">{i + 1}</span>
          <span class="name">{label(n)}</span>
          {#if walk.anchor === n}
            <span class="badge" title="The one to come back to for the closure reading">the one you started at</span>
          {/if}
        </li>
      {/each}
    </ol>
    <p class="hint">
      The order matters: the drift taken out of each speaker is proportional to when in the walk it was measured, so this
      list is the correction's own timeline rather than a checklist.
    </p>
  {/if}

  {#if walk.closure}
    {@const c = walk.closure}
    <h4>The closure reading</h4>
    <div class="closure" class:pass={c.passed} class:fail={!c.passed}>
      <div class="row">
        <span class="dot"></span>
        <strong>{label(c.anchor)}</strong>
        <span class="verdict">{c.passed ? 'closed' : 'did not close'}</span>
      </div>
      <p class="detail">
        {c.error_ms.toFixed(2)} ms apart after {c.span_s.toFixed(0)} s ({c.span_periods.toFixed(0)} pattern repeats) —
        {c.drift_ppm.toFixed(0)} ppm of clock drift, against a {c.tolerance_ms.toFixed(1)} ms allowance for a walk this
        long.
      </p>
      <!-- Verbatim: the tolerance is a *rate* bound, so what a pass does and does not
           establish is not something to paraphrase. -->
      <p class="note">{c.caveat}</p>
    </div>
  {/if}

  <p class="scope-note"><strong>What this walk is coherent with:</strong> {walk.scope_note}</p>
  <p class="hint">{WALK_FEWER_CHECKS}</p>
</div>

<style>
  .walk {
    margin: 12px 0;
    padding: 10px 12px;
    border: 1px solid color-mix(in srgb, var(--primary-color) 40%, transparent);
    border-radius: 8px;
    background: color-mix(in srgb, var(--primary-color) 5%, transparent);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 0.88rem;
  }
  .badge.check {
    border-color: color-mix(in srgb, var(--success-color, #43a047) 55%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 12%, transparent);
  }
  .prompt {
    margin: 6px 0 0;
    font-size: 0.84rem;
  }
  /* The premise is not a warning — nothing is wrong — but it is the one thing that
     decides whether the walk means anything, so it gets a frame of its own. */
  .premise,
  .closing {
    margin-top: 10px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px dashed color-mix(in srgb, var(--warning-color, #ffa600) 65%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 8%, transparent);
    font-size: 0.84rem;
  }
  .premise-head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .premise p,
  .closing p {
    margin: 5px 0 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .closing p strong {
    color: var(--primary-text-color);
  }
  /* The daemon's own sentence, marked as a quotation so it does not read as a second,
     slightly different instruction. */
  .quoted {
    padding-left: 8px;
    border-left: 2px solid color-mix(in srgb, var(--secondary-text-color) 35%, transparent);
    font-style: italic;
  }
  /* Green, not amber: a check walk is the run working as designed. */
  .why-again {
    margin-top: 10px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid color-mix(in srgb, var(--success-color, #43a047) 45%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 8%, transparent);
    font-size: 0.84rem;
  }
  .why-again p {
    margin: 5px 0 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .stops {
    list-style: none;
    margin: 10px 0 0;
    padding: 0;
    display: grid;
    gap: 8px;
  }
  .stops li {
    padding: 8px 10px;
    border: 1px solid var(--divider-color);
    border-radius: 8px;
    background: var(--card-background-color);
  }
  /* The speaker being listened to is the only one making a sound, so it is the only one
     that looks active. */
  .stops li.on {
    border-color: var(--primary-color);
    background: color-mix(in srgb, var(--primary-color) 7%, transparent);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 0.86rem;
  }
  .spacer {
    flex: 1 1 auto;
  }
  .tap {
    flex: 0 0 auto;
    padding: 3px 10px;
    font-size: 0.78rem;
  }
  .tap.on {
    border-color: var(--primary-color);
    color: var(--primary-color);
  }
  .row button.primary {
    padding: 4px 10px;
    font-size: 0.8rem;
  }
  .name {
    overflow-wrap: anywhere;
  }
  .kind,
  .note {
    font-size: 0.74rem;
    color: var(--secondary-text-color);
  }
  .note {
    color: var(--warning-color, #b26a00);
  }
  .knob {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 6px;
    max-width: 380px;
  }
  .knob.dim {
    opacity: 0.5;
  }
  .knob input[type='range'] {
    flex: 1;
    min-width: 120px;
  }
  .pct {
    min-width: 46px;
    text-align: right;
    font-variant-numeric: tabular-nums;
    font-size: 0.82rem;
  }
  .go {
    margin-top: 10px;
  }
  h4 {
    margin: 16px 0 6px;
    font-size: 0.85rem;
    font-weight: 600;
  }
  /* Walk order, numbered: it is the drift correction's timeline, not a tick list. */
  .order {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 4px;
  }
  .order li {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 0.84rem;
  }
  .order .n {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    font-size: 0.7rem;
    background: var(--input-fill-color);
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
  .closure {
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: color-mix(in srgb, var(--secondary-text-color) 6%, transparent);
  }
  .closure.pass {
    border-color: color-mix(in srgb, var(--success-color, #43a047) 45%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 8%, transparent);
  }
  .closure.fail {
    border-color: color-mix(in srgb, var(--error-color, #db4437) 55%, transparent);
    background: color-mix(in srgb, var(--error-color, #db4437) 10%, transparent);
  }
  /* Colour reinforces; the word carries the state. */
  .closure .dot {
    flex: 0 0 auto;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--secondary-text-color);
  }
  .closure.pass .dot {
    background: var(--success-color, #43a047);
  }
  .closure.fail .dot {
    background: var(--error-color, #db4437);
  }
  .closure .verdict {
    color: var(--secondary-text-color);
  }
  .closure .detail,
  .closure .note {
    margin: 5px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .closure .detail {
    font-variant-numeric: tabular-nums;
  }
  /* Not amber: a scope statement, and it has to be read before the numbers are believed. */
  .scope-note {
    margin: 12px 0 0;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: color-mix(in srgb, var(--secondary-text-color) 6%, transparent);
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .scope-note strong {
    color: var(--primary-text-color);
  }
  .hint {
    display: block;
    margin: 6px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .hint strong {
    color: var(--primary-text-color);
  }
  .hint.lead {
    margin-top: 10px;
  }
  .caution {
    margin: 8px 0 0;
    font-size: 0.78rem;
    color: var(--warning-color, #b26a00);
  }
</style>
