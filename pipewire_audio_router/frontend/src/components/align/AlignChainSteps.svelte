<script lang="ts">
  // The positions a chain has already aligned, and what each one did to the speakers
  // that were aligned before it (plan §1.1, §1.1.4).
  //
  // This component exists because of one thing that is invisible in a per-position
  // reading: **a step's Δ moves every speaker aligned so far**, not just the overlap it
  // was measured on. A user standing in the kitchen sees the kitchen's numbers and
  // concludes the living room was left alone — while the living room's speakers have in
  // fact just been shifted by the same 4 ms, deliberately, because a common delay added
  // to an aligned set preserves that set's internal alignment. That is the trick the
  // whole feature rests on and it has to be *shown*, so every step names the speakers it
  // moved and says which of them the user could not hear from there.
  //
  // Two honesty rules are also enforced here rather than left to the caller:
  //
  //   * the accumulated error is printed **only** when the daemon bounded it. One
  //     single-overlap step anywhere makes `bounded` false, and the daemon then reports
  //     no number at all — because "a total that quietly left out the one joint it could
  //     not measure would be worse than none". Nothing here sums the joints it does have;
  //   * `scope_note` — the doorway caveat — is shown on every chained run, not only when
  //     something looks wrong, because what it describes looks like a perfect result.
  import AlignCheck from './AlignCheck.svelte';
  import { overlapConfidenceLabel, ONE_OVERLAP_PENALTY } from '../../lib/measure.svelte';
  import type { ChainError, ChainStep } from '../../lib/types';

  interface Props {
    steps: ChainStep[];
    error: ChainError;
    /** What a chain's result is coherent with, in the daemon's words. Verbatim. */
    scopeNote: string;
    label: (nodeName: string) => string;
    /** Open the per-position detail by default (the review page wants it folded away;
     *  mid-chain the newest position is what the user is reading). */
    expandLast?: boolean;
  }
  let { steps, error, scopeNote, label, expandLast = false }: Props = $props();

  /** The speakers that were already aligned when a given step ran — i.e. exactly the
   *  ones its Δ moved. Derived from the earlier steps' own members rather than from
   *  `aligned`, so it stays right for a step in the middle of the list. */
  function alignedBefore(index: number): string[] {
    return steps.slice(0, index).flatMap((s) => s.members);
  }

  /** Of those, the ones the user could *not* hear from this position: not new here, not
   *  used as an overlap. These are the surprising ones, so they are named separately. */
  function movedUnheard(step: ChainStep, i: number): string[] {
    const heard = new Set(step.overlaps.map((o) => o.node_name));
    return alignedBefore(i).filter((n) => !heard.has(n));
  }
</script>

<div class="chain-steps">
  <div class="err" class:unbounded={!error.bounded}>
    <div class="err-head">
      <strong>Accumulated error across the joins</strong>
      {#if error.bounded && error.joint_ms !== null}
        <span class="num">±{error.joint_ms.toFixed(2)} ms worst case</span>
      {:else}
        <!-- Not "unknown" and not a partial sum: the one joint that cannot be measured
             is exactly the one a total would have to leave out. -->
        <span class="num none">no total</span>
      {/if}
    </div>
    <!-- Verbatim: the daemon is the only thing that knows which joints were checkable. -->
    <p>{error.message}</p>
  </div>

  {#each steps as step, i (step.index)}
    {@const unheard = movedUnheard(step, i)}
    <article class="step">
      <header>
        <span class="idx">Position {step.index}</span>
        <span class="chips">
          {#each step.members as m (m)}
            <span class="spk new">{label(m)}</span>
          {/each}
        </span>
        <span class="spacer"></span>
        <span
          class="badge conf {step.confidence}"
          title={step.confidence === 'single' ? ONE_OVERLAP_PENALTY : 'How well this position’s link to the already-aligned speakers could be checked'}
        >
          {overlapConfidenceLabel(step.confidence)}
        </span>
      </header>

      {#if step.overlaps.length}
        <div class="row">
          <span class="k">Linked through</span>
          <span class="chips">
            {#each step.overlaps as o (o.node_name)}
              <span
                class="spk ov"
                title="Its arrival here already includes the {o.applied_ms.toFixed(0)} ms it was carrying — that is what makes the chain work"
              >
                {label(o.node_name)}
                <span class="mini">{o.arrival_ms.toFixed(2)} ms · carrying {o.applied_ms.toFixed(0)} ms</span>
              </span>
            {/each}
          </span>
        </div>
      {/if}

      {#if step.confidence === 'single'}
        <p class="caution">{ONE_OVERLAP_PENALTY}</p>
      {/if}

      <!-- Δ propagation: the whole point of the chain, and the one number a user cannot
           infer from what they can hear. -->
      {#if step.delta_ms !== 0 && alignedBefore(i).length}
        <div class="delta">
          <div class="delta-head">
            <strong>Everything aligned before this moved {step.delta_ms.toFixed(2)} ms later</strong>
            <span class="badge warn">{alignedBefore(i).length} speaker{alignedBefore(i).length === 1 ? '' : 's'}</span>
          </div>
          <p>
            A speaker new at this position arrived <em>later</em> than the already-aligned set does here, and the only way
            to meet it is to hold the whole set back by the same amount. Adding one common delay to a set that is already
            aligned leaves it aligned with itself — which is why this is applied to
            <strong>every</strong> speaker aligned so far, not only to the overlap it was measured on.
          </p>
          <span class="chips">
            {#each alignedBefore(i) as n (n)}
              <span class="spk moved" class:unheard={unheard.includes(n)}>{label(n)} +{step.delta_ms.toFixed(2)} ms</span>
            {/each}
          </span>
          {#if unheard.length}
            <p class="caution">
              {unheard.map(label).join(', ')}
              {unheard.length === 1 ? 'is' : 'are'} not audible from this position, so nothing here sounded different when
              {unheard.length === 1 ? 'it' : 'they'} moved. {unheard.length === 1 ? 'It' : 'They'} moved anyway, and
              {unheard.length === 1 ? 'is' : 'are'} still aligned with everything
              {unheard.length === 1 ? 'it was' : 'they were'} aligned with before.
            </p>
          {/if}
        </div>
      {:else if steps.length > 1 && i > 0}
        <p class="hint">
          Nothing already aligned had to move for this position: everything new here arrives earlier than the
          already-aligned set does, so it was brought back to meet them instead.
        </p>
      {/if}

      <div class="nums">
        {#if step.anchor_ms !== null}
          <span
            ><span class="k">Aligned set arrives here</span> {step.anchor_ms.toFixed(2)} ms
            <span class="hint">the mean of the overlap readings — where the rest of the house sits, from here</span></span
          >
        {/if}
        <span><span class="k">Aligned at</span> {step.target_ms.toFixed(2)} ms</span>
        <span><span class="k">Spread here</span> {step.spread_ms.toFixed(2)} ms</span>
        {#if step.disagreement_ms !== null}
          <span
            ><span class="k">Overlaps disagree by</span> {step.disagreement_ms.toFixed(2)} ms
            <span class="hint">
              of a {step.tolerance_ms.toFixed(1)} ms tolerance{step.worst_pair
                ? ` · ${label(step.worst_pair[0])} vs ${label(step.worst_pair[1])}`
                : ''} — two overlaps never read identically here, because their geometry
              changed with the position; what is checked is that the difference stays inside plausible geometry
            </span></span
          >
        {/if}
        {#if step.joint_error_ms !== null}
          <span
            ><span class="k">This join is good to</span> ±{step.joint_error_ms.toFixed(2)} ms
            <span class="hint">half the disagreement: the anchor is the mean of two readings that far apart</span></span
          >
        {:else if step.confidence === 'origin'}
          <!-- Not a missing check: there was nothing to join to. Saying "cannot be
               checked" here would read as a fault in the first position. -->
          <span
            ><span class="k">No join</span> this position is the reference
            <span class="hint">nothing was aligned before it, so there is no join here to be right or wrong about</span></span
          >
        {:else}
          <span
            ><span class="k">This join</span> cannot be checked
            <span class="hint">one overlap gives nothing to compare against, so this join has no error estimate</span></span
          >
        {/if}
        <span
          ><span class="k">Drift here</span> {step.drift_ppm.toFixed(0)} ppm
          <span class="hint">fitted at this position; a chain has no single figure</span></span
        >
        <span
          ><span class="k">Capture</span> #{step.grid_epoch}
          <span class="hint">positions may differ — no two positions' readings are ever compared</span></span
        >
      </div>

      <!-- Per-position §10 checks. They blocked *this step* as it was measured, which is
           where a failure is cheap and still retryable — so a step that is listed here
           passed them. Folded away because they are the same two checks every time. -->
      <details open={expandLast && i === steps.length - 1}>
        <summary>Checks at this position</summary>
        <div class="checks">
          <AlignCheck
            name="Cross-band agreement"
            state={step.checks.transitivity.passed ? 'pass' : 'fail'}
            blocking
            detail={`worst ${
              step.checks.transitivity.worst_pair
                ? `${label(step.checks.transitivity.worst_pair[0])} / ${label(step.checks.transitivity.worst_pair[1])}`
                : '—'
            }: ${step.checks.transitivity.worst_ms.toFixed(2)} ms of a ${step.checks.transitivity.tolerance_ms.toFixed(1)} ms tolerance`}
            note={step.checks.transitivity.caveat}
          />
          {#if step.checks.repeatability}
            <AlignCheck
              name="Repeatability between passes"
              state={step.checks.repeatability.passed ? 'pass' : 'fail'}
              detail={`worst ${
                step.checks.repeatability.worst_member ? label(step.checks.repeatability.worst_member) : '—'
              }: ${step.checks.repeatability.worst_ms.toFixed(2)} ms of a ${step.checks.repeatability.tolerance_ms.toFixed(1)} ms tolerance`}
            />
          {:else}
            <AlignCheck
              name="Repeatability between passes"
              state="unavailable"
              detail="only one pass was usable at this position, so there is nothing to compare it against"
            />
          {/if}
        </div>
        <p class="hint">{step.note}</p>
      </details>
    </article>
  {/each}

  <!-- The doorway caveat (plan §1.1). On every chained run: the failure it describes —
       two rooms that are each internally perfect and approximate with respect to each
       other — sounds like a good result from inside either room. -->
  <div class="scope">
    <strong>What this is, and is not, coherent with</strong>
    <p>{scopeNote}</p>
  </div>
</div>

<style>
  .chain-steps {
    display: grid;
    gap: 10px;
  }
  .err {
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: color-mix(in srgb, var(--secondary-text-color) 6%, transparent);
    font-size: 0.84rem;
  }
  /* Amber, not red: an unbounded total is not a failure — it is the daemon declining to
     print a number it cannot stand behind. */
  .err.unbounded {
    border-color: color-mix(in srgb, var(--warning-color, #ffa600) 55%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 10%, transparent);
  }
  .err-head {
    display: flex;
    align-items: baseline;
    gap: 8px;
    flex-wrap: wrap;
  }
  .err p {
    margin: 5px 0 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .num {
    font-variant-numeric: tabular-nums;
  }
  .num.none {
    font-variant-numeric: normal;
    color: var(--warning-color, #b26a00);
  }
  .step {
    padding: 8px 10px;
    border: 1px solid var(--divider-color);
    border-radius: 8px;
  }
  .step header {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 0.86rem;
  }
  .idx {
    font-weight: 600;
  }
  .spacer {
    flex: 1 1 auto;
  }
  .chips {
    display: inline-flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .spk {
    display: inline-flex;
    align-items: baseline;
    gap: 6px;
    font-size: 0.8rem;
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid var(--divider-color);
    background: var(--input-fill-color);
  }
  .spk.new {
    border-color: color-mix(in srgb, var(--primary-color) 45%, transparent);
    background: color-mix(in srgb, var(--primary-color) 10%, transparent);
  }
  .spk.ov {
    border-style: dashed;
  }
  /* A speaker that moved without being audible here reads differently from one that
     moved in front of the user — that difference is the point. */
  .spk.moved {
    font-variant-numeric: tabular-nums;
  }
  .spk.moved.unheard {
    border-color: color-mix(in srgb, var(--warning-color, #ffa600) 60%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 12%, transparent);
  }
  .mini {
    font-size: 0.72rem;
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 6px;
  }
  .k {
    font-size: 0.7rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--secondary-text-color);
  }
  .nums {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 20px;
    margin-top: 8px;
    font-size: 0.82rem;
    font-variant-numeric: tabular-nums;
  }
  .nums .k {
    display: block;
  }
  .nums span > .hint {
    display: block;
    max-width: 30rem;
  }
  .delta {
    margin-top: 8px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid color-mix(in srgb, var(--primary-color) 45%, transparent);
    background: color-mix(in srgb, var(--primary-color) 8%, transparent);
    font-size: 0.84rem;
  }
  .delta-head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .delta p {
    margin: 5px 0 6px;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .delta p strong,
  .delta p em {
    color: var(--primary-text-color);
  }
  .caution {
    margin: 6px 0 0;
    font-size: 0.78rem;
    color: var(--warning-color, #b26a00);
  }
  .hint {
    margin: 6px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
    font-variant-numeric: normal;
  }
  details {
    margin-top: 8px;
    font-size: 0.82rem;
  }
  summary {
    cursor: pointer;
    color: var(--secondary-text-color);
  }
  .checks {
    display: grid;
    gap: 8px;
    margin-top: 6px;
  }
  .scope {
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: color-mix(in srgb, var(--secondary-text-color) 5%, transparent);
    font-size: 0.84rem;
  }
  .scope p {
    margin: 5px 0 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
</style>
