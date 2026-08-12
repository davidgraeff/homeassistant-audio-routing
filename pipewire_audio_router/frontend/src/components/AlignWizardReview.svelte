<script lang="ts">
  // Wizard page 4: the proposal, its confidence, the checks — plan §8's `Proposed`
  // state, which exists precisely so this page can exist (§11: `apply` is never
  // automatic).
  //
  // Three rules shape what is on this page:
  //
  //   * a blocked proposal keeps its numbers. A green residual with a failed
  //     cross-band check is the interesting failure and hiding it would hide the
  //     one thing worth knowing (§10);
  //   * a delta is shown with its standard error, always. 9 ms ± 0.05 and
  //     9 ms ± 0.9 are not the same claim, and a column of bare milliseconds says
  //     they are;
  //   * a pass is not a proof. §5.6: a reflection landing inside the analysis
  //     window biases the answer while looking excellent to every check here. So
  //     the success state says what was checked, not that the speakers are perfect.
  //
  // And one correction that W14 forced through this page (plan §2.4.1/§2.4.2): **the
  // sendspin knob is an advance, not a delay.** Raising it makes that speaker play
  // *earlier*; an AirPlay-2 render delay and a pw-sink playout delay make theirs play
  // *later*. So there is no "align to the latest speaker" rule any more — the solver
  // intersects each member's reachable arrivals and the member that ends up at the
  // smallest knob falls out of the arithmetic. Two consequences here:
  //
  //   * the direction of every change is taken from the daemon's own `effect`
  //     sentence, never composed from `added_ms`, whose sign describes the *knob* and
  //     not the sound;
  //   * `reference` is labelled as an outcome, because presenting it as a chosen
  //     reference would re-tell the story the code no longer implements.
  import AlignChainSteps from './AlignChainSteps.svelte';
  import AlignCheck from './AlignCheck.svelte';
  import AlignRefusal from './AlignRefusal.svelte';
  import { memberKindLabel } from '../lib/align.svelte';
  import { WALK_FEWER_CHECKS, confidenceBand, uncertaintyDominates, warningKindLabel } from '../lib/measure.svelte';
  import type { MeasureStatus, Warning } from '../lib/types';

  interface Props {
    status: MeasureStatus;
    label: (nodeName: string) => string;
    busy: boolean;
    onApply: () => void;
    onRevert: () => void;
    onDiscard: () => void;
    onRetry: () => void;
  }
  let { status, label, busy, onApply, onRevert, onDiscard, onRetry }: Props = $props();

  const proposal = $derived(status.proposal);
  const verification = $derived(status.verification);
  /** Was this a **walk**? It changes which checks exist rather than which ones passed
   *  (plan §10.4), and the difference has to survive onto this page: a walk gains the
   *  closure and loses pass-to-pass repeatability, and reporting the second as a green
   *  check — its residual is zero by construction, because the drift slope was fitted
   *  from exactly those two points — would be dishonest. */
  const walked = $derived(status.mode === 'near_field');
  const closure = $derived(proposal?.checks.closure ?? null);
  /** A chained run's positions. They belong on this page as much as on the run page: the
   *  proposal below is one write over the whole house, and how that house was joined up —
   *  which joins were checked, which were not, and what the total error can be said to
   *  be — is exactly what the Apply decision rests on. */
  const chain = $derived(status.chain);
  /** Run-level and proposal-level warnings are the same population; one kind is
   *  raised once, so keying by kind merges them without repeating any.
   *
   *  `chain_scope` is dropped when the chain section below is rendering: that warning's
   *  message *is* `chain.scope_note`, printed there under a heading that says what it is
   *  about. The same paragraph twice on one page trains the reader to skip it. */
  const warnings = $derived.by(() => {
    const seen = new Map<string, Warning>();
    for (const w of [...status.warnings, ...(proposal?.warnings ?? [])]) if (!seen.has(w.kind)) seen.set(w.kind, w);
    if (status.chain?.steps.length) seen.delete('chain_scope');
    return [...seen.values()];
  });

  /** Per-member peak SNR from the run, for the confidence tooltip: the standard
   *  error says how firm the reading was, the SNR says how much signal it had. */
  function snrOf(nodeName: string): string {
    const m = status.members.find((x) => x.node_name === nodeName)?.last;
    return m ? `${m.peak_snr_db.toFixed(0)} dB over the noise, peak/runner-up ${m.second_peak_ratio.toFixed(1)}×` : 'no reading kept';
  }
</script>

{#if proposal}
  {#if proposal.blocked}
    <!-- Blocked, and visibly so: the write will not happen, and which check said
         no is right here. The table below stays exactly as it is. -->
    <div class="blocked-head">
      <strong>This proposal will not be written.</strong>
      <span>A cross-check failed, so applying it is refused. The measured numbers are below unchanged.</span>
    </div>
    <AlignRefusal refusal={proposal.blocked} {label} blocking />
  {/if}

  <div class="summary">
    <span
      ><span class="k">Ends up at the smallest knob</span> {label(proposal.reference)}
      <span class="hint">an outcome of the arithmetic, not a speaker anyone picked — everything else moves towards it</span></span
    >
    <span><span class="k">Spread</span> {proposal.spread_ms.toFixed(2)} ms</span>
    <span
      ><span class="k">Aligned at</span> {proposal.target_ms.toFixed(2)} ms
      <span class="hint">
        relative to the earliest arrival measured; the whole group can only meet between
        {proposal.feasible_lo_ms.toFixed(1)} and {proposal.feasible_hi_ms.toFixed(1)} ms
      </span></span
    >
    <span
      ><span class="k">Biggest knob written</span> {proposal.largest_knob_ms} ms
      <span class="hint">kept as small as the ranges allow — every millisecond of it costs buffer</span></span
    >
    <span
      ><span class="k">Clock drift</span> {proposal.drift_ppm.toFixed(0)} ppm
      <span class="hint">phone against the audio clock</span></span
    >
  </div>

  <!-- Stated once, above the table, because the table's numbers are meaningless
       without it — and because it is the opposite of what this page used to say. -->
  <p class="polarity-note">
    The two speaker kinds move in <strong>opposite directions</strong>: a Sendspin speaker's knob is an
    <em>advance</em> — raising it makes that speaker play <em>earlier</em> — while an AirPlay 2 or PipeWire-host knob is a
    delay and makes it play <em>later</em>. So a group is not aligned "to the latest speaker": each speaker can only reach
    a limited range of arrival times, and the group meets somewhere all those ranges overlap.
  </p>

  <table>
    <thead>
      <tr>
        <th>Speaker</th>
        <th class="num">Arrives</th>
        <th class="num">Knob now</th>
        <th class="num">Knob after</th>
        <th>What changes</th>
        <th>Confidence</th>
      </tr>
    </thead>
    <tbody>
      {#each proposal.members as m (m.node_name)}
        {@const band = confidenceBand(m.std_error_ms)}
        <tr class:ref={m.is_reference}>
          <td>
            {label(m.node_name)}
            <span class="badge">{memberKindLabel(m.kind)}</span>
            <!-- The polarity is part of the speaker's identity here, not a footnote:
                 every number in this row means the opposite for the other value. -->
            <span class="badge pol" title={m.polarity === 'advance' ? 'Raising this knob makes this speaker play earlier' : 'Raising this knob makes this speaker play later'}>
              {m.polarity === 'advance' ? 'advance knob' : 'delay knob'}
            </span>
            {#if m.is_reference}<span class="badge on" title="Ends up at the smallest knob — an outcome, not a chosen reference">smallest knob</span>{/if}
          </td>
          <td class="num" title="Measured arrival relative to the earliest speaker, before any knob arithmetic">
            {m.arrival_ms.toFixed(2)} ms
          </td>
          <td class="num">{m.current_delay_ms} ms</td>
          <td class="num strong">{m.new_delay_ms} ms</td>
          <td>
            <!-- Verbatim from the daemon: the one place the *direction* is guaranteed
                 correct. Composing this from `added_ms` is how a knob that advances
                 gets described as a delay. -->
            <span class="effect">{m.effect}</span>
            <span
              class="hint"
              title="This speaker can only be placed inside this range; the group's common target had to fall inside every member's range"
            >
              reachable {m.achievable_lo_ms.toFixed(1)}–{m.achievable_hi_ms.toFixed(1)} ms · knob {m.knob_min_ms}–{m.knob_max_ms} ms
            </span>
          </td>
          <td>
            <span class="pm {band}" title={snrOf(m.node_name)}>±{m.std_error_ms.toFixed(2)} ms</span>
            <span class="hint">{band}</span>
            {#if uncertaintyDominates(m.added_ms, m.std_error_ms)}
              <div class="hint caution">the uncertainty is a large share of this change</div>
            {/if}
          </td>
        </tr>
      {/each}
    </tbody>
  </table>

  {#if chain && chain.steps.length}
    <h4>How this chain was joined up</h4>
    <p class="chain-lead">
      Each position below was aligned where it was measured from, and the positions are tied to one another only through
      their overlap speakers. The delays in the table above are what all of that comes to after the accumulated delay was
      taken back out — a common shift, so nothing moved relative to anything else.
    </p>
    <AlignChainSteps steps={chain.steps} error={chain.error} scopeNote={chain.scope_note} {label} />
  {/if}

  <h4>Checks</h4>
  <div class="checks">
    <AlignCheck
      name="Cross-band agreement"
      state={proposal.checks.transitivity.passed ? 'pass' : 'fail'}
      blocking
      detail={`worst pair ${
        proposal.checks.transitivity.worst_pair
          ? `${label(proposal.checks.transitivity.worst_pair[0])} / ${label(proposal.checks.transitivity.worst_pair[1])}`
          : '—'
      }: ${proposal.checks.transitivity.worst_ms.toFixed(2)} ms of a ${proposal.checks.transitivity.tolerance_ms.toFixed(1)} ms tolerance`}
      note={proposal.checks.transitivity.caveat}
    />
    {#if closure}
      <!-- Near field's own check, and the one a stationary run does not have: the walk's
           first speaker read again at the end. It is *not* a bonus — the drift correction
           applied to every member came out of it, which is why an implausible closure
           refuses the whole walk rather than one reading. -->
      <AlignCheck
        name="Closure (the first speaker, measured again at the end)"
        state={closure.passed ? 'pass' : 'fail'}
        blocking
        detail={`${label(closure.anchor)}: ${closure.error_ms.toFixed(2)} ms apart over ${closure.span_s.toFixed(0)} s — ${closure.drift_ppm.toFixed(0)} ppm of clock drift, against a ${closure.tolerance_ms.toFixed(1)} ms allowance for a walk this long`}
        note={closure.caveat}
      />
    {/if}
    {#if proposal.checks.repeatability}
      <AlignCheck
        name="Repeatability between passes"
        state={proposal.checks.repeatability.passed ? 'pass' : 'fail'}
        detail={`worst ${
          proposal.checks.repeatability.worst_member ? label(proposal.checks.repeatability.worst_member) : '—'
        }: ${proposal.checks.repeatability.worst_ms.toFixed(2)} ms of a ${proposal.checks.repeatability.tolerance_ms.toFixed(1)} ms tolerance`}
      />
    {:else if walked}
      <!-- Absent by construction, not failed and not skipped (plan §10.4). A walk reads
           each speaker once, so the only member with two readings is the closure anchor —
           and the drift slope was fitted from exactly those two points, making its
           residual identically zero. Printing that as a pass would be reporting an
           identity as evidence. -->
      <AlignCheck
        name="Repeatability between passes"
        state="unavailable"
        detail="a walk measures each speaker once, so there is no second pass to compare"
        note="Not a failure and not a shortcut: the one speaker read twice is the closure anchor above, and the drift was fitted from exactly those two readings, so its agreement with itself is arithmetic rather than evidence. What this costs is that nothing here would notice you changing how you hold the phone partway through the walk — which is the thing pass-to-pass agreement catches for a stationary run."
      />
    {:else}
      <AlignCheck
        name="Repeatability between passes"
        state="unavailable"
        detail="only one pass was usable, so there is nothing to compare it against"
        note="Two passes are what separate a real offset from clock drift; with one, the drift figure above is not fitted."
      />
    {/if}
    <AlignCheck
      name="Merged peak"
      state="unavailable"
      detail="not implemented"
      note={proposal.checks.merged_peak.reason}
    />
    <AlignCheck
      name="Residual after writing"
      state="unavailable"
      detail={walked ? 'runs after the knobs are written, and it is another walk' : 'runs after the knobs are written'}
      note={walked
        ? 'A reading taken from one spot cannot check a near-field alignment: once the wiring is right, what is left is each speaker’s distance to wherever the phone is standing — tens of milliseconds against a two-millisecond tolerance, which would fail every correct run. So applying this walks the same route again, with its own closure. Expect to be asked to walk, and do not read it as the run repeating itself.'
        : 'It re-measures the group once the speakers are back, and is the check that most directly says the write landed.'}
    />
  </div>
  {#if walked}
    <!-- Said with the checks rather than in the warning list: it is a property of the
         method, and a user comparing this page against a stationary run's will otherwise
         read the shorter list as something having gone wrong. -->
    <p class="honest">{WALK_FEWER_CHECKS}</p>
  {/if}
{:else if status.phase === 'writing' || status.phase === 'settling' || status.phase === 'verifying'}
  <p class="lead">{status.message}</p>
{/if}

{#if warnings.length}
  <h4>Worth knowing</h4>
  <ul class="warnings">
    {#each warnings as w (w.kind)}
      <li><strong>{warningKindLabel(w.kind)}.</strong> {w.message}</li>
    {/each}
  </ul>
{/if}

{#if verification}
  <h4>After writing</h4>
  {#if verification.scope_note}
    <!-- Before the numbers, because it changes what they are *about*: a chain can only be
         re-measured where the phone is, so this residual covers the last position's own
         speakers and its overlaps — not the whole house. Re-checking the rest means
         walking the chain again. Reading a last-room residual as a whole-house verdict is
         the easy mistake, and the daemon writes the sentence rather than the UI guessing
         which positions were covered. -->
    <p class="scope-note">
      <strong>What this checked:</strong> {verification.scope_note}
    </p>
  {/if}
  <div class="checks">
    <AlignCheck
      name="Residual"
      state={verification.residual.passed ? 'pass' : 'fail'}
      blocking={false}
      detail={`worst ${verification.residual.worst_member ? label(verification.residual.worst_member) : '—'}: ${verification.residual.worst_ms.toFixed(2)} ms of a ${verification.residual.tolerance_ms.toFixed(1)} ms tolerance`}
    />
    <AlignCheck
      name="Cross-band agreement"
      state={verification.transitivity.passed ? 'pass' : 'fail'}
      detail={`worst ${verification.transitivity.worst_ms.toFixed(2)} ms of a ${verification.transitivity.tolerance_ms.toFixed(1)} ms tolerance`}
      note={verification.transitivity.caveat}
    />
    <AlignCheck name="Merged peak" state="unavailable" detail="not implemented" note={verification.merged_peak.reason} />
  </div>
  {#if walked}
    <!-- The re-measurement was a second walk, and saying so is the difference between
         "that was the check" and "why did it make me do it again" (plan §10.4). -->
    <p class="scope-note">
      <strong>How this was checked:</strong> by walking the same route again, speaker by speaker, with its own closure
      reading. A near-field write cannot be checked from one spot — the residual would measure the phone's distance to
      each speaker and fail every correct run — so the second walk is the check rather than a repeat of the first.
    </p>
  {/if}
  <p class="honest">
    {#if verification.passed}
      The knobs were written and re-measured, and {verification.scope_note
        ? 'every speaker this re-measurement could cover'
        : 'every speaker'} now arrives within the estimator's own precision. That
      is what was checked — not that the result is right: a reflection arriving one or two milliseconds after the direct
      sound biases the measurement while looking excellent to all of these checks, and the cross-band tolerance is a few
      milliseconds wide so that different tweeter crossovers don't trip it. If it still sounds smeared from where you
      listen, trust your ears: go back to <strong>Mode</strong>, pick <strong>Manual</strong>, and tune it by hand — the
      speakers are still held, so nothing has to be set up again.
    {:else}
      The knobs were written, but the re-measurement did not confirm them. <strong>Revert</strong> puts every speaker back
      to the value it had before this run; <strong>Manual</strong> on the mode page is the by-ear fallback, and the
      speakers stay held while you switch to it.
    {/if}
  </p>
{/if}

{#if status.refusal}
  <AlignRefusal refusal={status.refusal} {label} />
{/if}

<div class="actions">
  <!-- Only offered while the run is parked on a proposal. Once it has been written
       the button would be a no-op, and a disabled primary button reads as
       "something is wrong" rather than "already done". -->
  {#if status.phase === 'proposed'}
    <button
      class="primary"
      disabled={!status.can_apply || busy}
      title={status.can_apply
        ? 'Write these values, then re-measure to check they landed'
        : 'A cross-check failed, so this proposal cannot be written'}
      onclick={onApply}
    >
      Apply this alignment
    </button>
  {/if}
  {#if status.can_revert}
    <button class="ghost" disabled={busy} title="Put every speaker back to the value it had before this run" onclick={onRevert}>
      Revert
    </button>
  {/if}
  <button class="ghost" disabled={busy} onclick={onRetry}>Measure again</button>
  <button class="ghost" disabled={busy} title="Throw the proposal away; nothing is written" onclick={onDiscard}>
    Discard
  </button>
</div>
<p class="hint">
  Writing a speaker's knob reconnects it, so expect the group to go quiet for tens of seconds afterwards while they come
  back. Nothing is written until you press Apply.
</p>

<style>
  .blocked-head {
    display: flex;
    flex-direction: column;
    gap: 2px;
    font-size: 0.84rem;
  }
  .blocked-head span {
    color: var(--secondary-text-color);
    font-size: 0.8rem;
  }
  .summary {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 20px;
    margin: 12px 0 4px;
    font-size: 0.82rem;
  }
  .summary .k {
    display: block;
    font-size: 0.7rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--secondary-text-color);
  }
  h4 {
    margin: 16px 0 6px;
    font-size: 0.85rem;
    font-weight: 600;
  }
  .num {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .strong {
    font-weight: 600;
  }
  tr.ref td {
    background: color-mix(in srgb, var(--primary-color) 8%, transparent);
  }
  .polarity-note {
    margin: 10px 0 4px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: color-mix(in srgb, var(--secondary-text-color) 6%, transparent);
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .polarity-note strong,
  .polarity-note em {
    color: var(--primary-text-color);
  }
  .badge.pol {
    font-variant: small-caps;
  }
  /* The sentence, not the delta, is the load-bearing cell — so it reads as text. */
  .effect {
    display: block;
    font-size: 0.82rem;
  }
  /* The standard error is the claim's width, so it is styled as a value in its own
     right rather than as a footnote to the delta. */
  .pm {
    font-variant-numeric: tabular-nums;
    font-size: 0.82rem;
    padding: 1px 6px;
    border-radius: 999px;
    border: 1px solid var(--divider-color);
  }
  .pm.tight {
    border-color: color-mix(in srgb, var(--success-color, #43a047) 55%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 10%, transparent);
  }
  .pm.good {
    border-color: var(--divider-color);
  }
  .pm.soft {
    border-color: color-mix(in srgb, var(--warning-color, #ffa600) 60%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 12%, transparent);
  }
  .checks {
    display: grid;
    gap: 8px;
  }
  .warnings {
    margin: 0;
    padding-left: 18px;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .honest {
    margin: 10px 0 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .chain-lead {
    margin: 0 0 8px;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  /* Not amber: nothing is wrong. It is a scope statement, and it has to be read before
     the residual beside it. */
  .scope-note {
    margin: 0 0 8px;
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
  .actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 16px;
  }
  .lead,
  .hint {
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .hint {
    display: block;
    margin-top: 6px;
  }
  .hint.caution {
    color: var(--warning-color, #b26a00);
  }
</style>
