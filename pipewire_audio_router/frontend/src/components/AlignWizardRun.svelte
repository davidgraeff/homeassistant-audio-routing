<script lang="ts">
  // Wizard page 3: the run itself — plan §8's state machine, made watchable.
  //
  // A measurement is minutes long and spends most of it apparently doing nothing:
  // waiting for a mute to settle, for a reconnected speaker to render again, for
  // the phone to be held still. So the gate is the centrepiece here, not a detail —
  // `waiting_for` plus its message is the difference between "this is working" and
  // "this is hung", and the plan is explicit that a run which cannot explain itself
  // gets blamed on the user's hand for something a doorbell did.
  import AlignRefusal from './AlignRefusal.svelte';
  import AlignWizardChain from './AlignWizardChain.svelte';
  import AlignWizardWalk from './AlignWizardWalk.svelte';
  import { knobNoun, memberKindLabel } from '../lib/align.svelte';
  import {
    elapsed,
    gateReasonLabel,
    isLive,
    phaseChain,
    phaseLabel,
    phaseNote,
    warningKindLabel,
  } from '../lib/measure.svelte';
  import type { MeasureStatus } from '../lib/types';

  interface Props {
    status: MeasureStatus;
    label: (nodeName: string) => string;
    /** Start another run with the same settings (after a refusal). */
    onRetry: () => void;
    /** Post one listening position of a chain (plan §1.1). */
    onPosition: (members: string[], overlaps: string[]) => void;
    /** Renormalise the chain and propose the single write. */
    onFinish: () => void;
    /** Near field: "I am standing at this speaker" — takes its reading (plan §1, W8a). */
    onArrival: (nodeName: string) => void;
    /** Near field: the closure reading, back at the walk's first speaker. */
    onClose: () => void;
    /** Make exactly these members audible — how a position is previewed, and how a walk
     *  hears the speaker it is standing at to set its level. */
    onHear: (nodeNames: string[]) => void;
    busy: boolean;
  }
  let { status, label, onRetry, onPosition, onFinish, onArrival, onClose, onHear, busy }: Props = $props();

  const live = $derived(isLive(status.phase));
  // A chained run's acquisition loop is `positioning` → `measuring` per listening spot,
  // and a walk's is `walking` → `measuring` per speaker, so the strip has to contain the
  // phase the run is actually in — otherwise every step reads as unreached while the
  // house is being walked.
  const chainPhases = $derived(phaseChain({ chained: !!status.chain, mode: status.mode }));
  const reached = $derived(chainPhases.indexOf(status.phase));
  const gate = $derived(status.gate);
  /** Gate fill: how much of the required lock has been accumulated. */
  const gatePct = $derived(gate ? Math.min(100, Math.round((gate.periods / Math.max(1, gate.needed)) * 100)) : 0);
  /** Newest observations first — the run appends, and the recent ones are the
   *  interesting ones while it is still going. */
  const recent = $derived([...status.observations].reverse().slice(0, 12));
  const walk = $derived(status.walk ?? null);
  /** Near field's premise, as the daemon words it. Handed to the walk panel so it can be
   *  quoted where the user is about to take a reading, instead of only at the bottom of
   *  the page with everything else. */
  const premise = $derived(status.warnings.find((w) => w.kind === 'near_field_path_assumed')?.message);
  /** Two caveats are carried twice by the API — the chain's doorway note is
   *  `chain.scope_note` *and* a `chain_scope` warning with the identical sentence, and
   *  near field's path premise is a `near_field_path_assumed` warning the walk panel
   *  quotes where it matters. Each is shown once, under a heading that says what it is
   *  about: the same paragraph twice on one page teaches the reader to skip it, which is
   *  the opposite of what it is for. */
  const warnings = $derived(
    status.warnings.filter(
      (w) => !(status.chain && w.kind === 'chain_scope') && !(walk && w.kind === 'near_field_path_assumed'),
    ),
  );
</script>

<div class="chain" aria-label="Measurement stage">
  {#each chainPhases as p, i (p)}
    <span class="step" class:done={reached > i} class:now={status.phase === p}>{phaseLabel(p)}</span>
  {/each}
</div>

<div class="now-row">
  <span class="badge" class:on={live} class:warn={status.phase === 'refused'}>{phaseLabel(status.phase)}</span>
  <span class="msg">{status.message}</span>
  <span class="elapsed" title="Time since this run started">{elapsed(status.elapsed_s)}</span>
</div>
{#if phaseNote(status.phase)}
  <p class="hint">{phaseNote(status.phase)}</p>
{/if}

<!-- A chained run's controls come *before* the gate and the per-speaker table: while the
     chain is parked the only thing that matters is where the user is standing and which
     speakers they can hear, and the table below is the detail of whatever position was
     measured last. Kept mounted for the whole run, not only while parked, so the
     per-position numbers stay readable while the next one is being measured. -->
{#if status.chain}
  <AlignWizardChain chain={status.chain} {label} {busy} onPost={onPosition} {onFinish} {onHear} />
{/if}

<!-- Near field's body, in the same place and for the same reason: while the walk is parked
     the only thing that matters is which speaker the user is standing at. Kept mounted
     after the walk ends, because the closure numbers are part of the verdict. -->
{#if walk}
  <AlignWizardWalk
    {walk}
    {label}
    {busy}
    {premise}
    message={status.message}
    {onArrival}
    {onClose}
    onHear={(node) => onHear([node])}
    onSilence={() => onHear([])}
  />
{/if}

{#if gate}
  <!-- Every entry into a measuring state goes through one gate: re-acquire the
       loop-phase lock with a stable amplitude before any window is accepted. This
       is the readout that keeps a slow run legible. -->
  <div class="gate" class:locked={gate.locked}>
    <div class="gate-head">
      <span class="lbl">{gate.locked ? 'Signal locked' : 'Acquiring signal'}</span>
      {#if gate.member}<span class="spk">{label(gate.member)}</span>{/if}
      <span class="count">{gate.periods}/{gate.needed} pattern repeats</span>
      {#if gate.restarts > 0}
        <span class="badge caution" title="Each restart threw away the window collected so far">
          restarted {gate.restarts}×
        </span>
      {/if}
    </div>
    <div class="bar"><div class="fill" style={`width:${gatePct}%`}></div></div>
    {#if gate.waiting_for}
      <p class="waiting">Waiting: {gateReasonLabel(gate.waiting_for)}.</p>
    {/if}
    <p class="gate-msg">{gate.message}</p>
  </div>
{/if}

<table>
  <thead>
    <tr><th>Speaker</th><th>Passes</th><th>Last reading</th><th>Note</th></tr>
  </thead>
  <tbody>
    {#each status.members as m (m.node_name)}
      <tr class:current={gate?.member === m.node_name}>
        <td>
          {label(m.node_name)}
          <span class="badge">{memberKindLabel(m.kind)}</span>
          <!-- "delay now" would be wrong for half the kinds: a sendspin knob is an
               advance (plan §2.4.1), so the noun comes from the kind. -->
          <div class="sub">level {m.level}% · {knobNoun(m.kind)} now {m.current_delay_ms} ms</div>
        </td>
        <td class="num">{m.passes_done}</td>
        <td>
          {#if m.last}
            <div class="reading">
              <span title="Arrival of the 3 kHz burst on the shared grid — only differences between speakers mean anything">
                {m.last.phase_a_ms.toFixed(2)} ms
              </span>
              <span class="pm" title="Spread across pattern repeats: how firm this reading is">
                ±{m.last.std_error_ms.toFixed(2)}
              </span>
            </div>
            <div class="sub">
              {m.last.peak_snr_db.toFixed(0)} dB over the noise · peak/runner-up {m.last.second_peak_ratio.toFixed(1)}× ·
              drift {m.last.drift_ppm.toFixed(0)} ppm
            </div>
          {:else}
            <span class="muted">—</span>
          {/if}
        </td>
        <td class="note">{m.note ?? ''}</td>
      </tr>
    {/each}
  </tbody>
</table>

{#if warnings.length}
  <ul class="warnings">
    {#each warnings as w (w.kind)}
      <li><strong>{warningKindLabel(w.kind)}.</strong> {w.message}</li>
    {/each}
  </ul>
{/if}

{#if status.refusal}
  <AlignRefusal refusal={status.refusal} {label} />
  <div class="retry">
    <button class="ghost" onclick={onRetry}>Measure again</button>
    <!-- Where the fallback actually is, now that it is a mode of this wizard rather than a
         panel underneath it: the speakers are still held, so switching to it costs nothing
         and no speaker reconnects. -->
    <span class="hint">
      Nothing was written, so a retry costs only the time. If the estimator keeps refusing, go back to
      <strong>Mode</strong> and pick <strong>Manual</strong> — by ear, no microphone, and the speakers stay held.
    </span>
  </div>
{/if}

{#if recent.length}
  <details>
    <summary>Accepted readings ({status.observations.length})</summary>
    <table class="obs">
      <thead>
        <tr><th>Speaker</th><th>Pass</th><th>3 kHz</th><th>1.5 kHz</th><th>±</th><th>SNR</th><th>Grid</th></tr>
      </thead>
      <tbody>
        {#each recent as o (`${o.node_name}-${o.pass}-${o.period_centre}`)}
          <tr>
            <td>{label(o.node_name)}</td>
            <td class="num">{o.pass + 1}</td>
            <td class="num">{o.phase_a_ms.toFixed(2)}</td>
            <td class="num">{o.phase_b_ms.toFixed(2)}</td>
            <td class="num">{o.std_error_ms.toFixed(2)}</td>
            <td class="num">{o.peak_snr_db.toFixed(0)} dB</td>
            <td class="num" title="Readings from different captures are never compared — a change here means the microphone reconnected">
              {o.grid_epoch}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
    <p class="hint">
      Newest first. The two tones are measured separately on purpose: an early reflection interferes with them
      differently, which is the one independent cross-check this design has.
    </p>
  </details>
{/if}

<style>
  /* The §8 chain, so "settling" reads as a step of a known sequence rather than a
     word that appeared. */
  .chain {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-bottom: 10px;
  }
  .step {
    font-size: 0.7rem;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--divider-color);
    color: var(--secondary-text-color);
  }
  .step.done {
    color: var(--primary-text-color);
    border-color: color-mix(in srgb, var(--success-color, #43a047) 45%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 10%, transparent);
  }
  .step.now {
    color: var(--text-on-primary);
    background: var(--primary-color);
    border-color: var(--primary-color);
  }
  .now-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .msg {
    flex: 1 1 auto;
    font-size: 0.84rem;
  }
  .elapsed {
    font-variant-numeric: tabular-nums;
    color: var(--secondary-text-color);
    font-size: 0.82rem;
  }
  .gate {
    margin: 10px 0;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: color-mix(in srgb, var(--secondary-text-color) 6%, transparent);
  }
  .gate.locked {
    border-color: color-mix(in srgb, var(--success-color, #43a047) 45%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 8%, transparent);
  }
  .gate-head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 0.8rem;
  }
  .lbl {
    font-weight: 600;
  }
  .spk {
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid var(--divider-color);
    background: var(--input-fill-color);
  }
  .count {
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
  .bar {
    height: 6px;
    margin-top: 6px;
    border-radius: 3px;
    overflow: hidden;
    background: var(--input-fill-color);
    border: 1px solid var(--divider-color);
  }
  .fill {
    height: 100%;
    background: var(--primary-color);
    transition: width 200ms linear;
  }
  .waiting,
  .gate-msg {
    margin: 5px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .waiting {
    color: var(--warning-color, #b26a00);
  }
  table {
    margin-top: 10px;
  }
  tr.current td {
    background: color-mix(in srgb, var(--primary-color) 8%, transparent);
  }
  .reading {
    display: flex;
    gap: 6px;
    align-items: baseline;
    font-variant-numeric: tabular-nums;
  }
  .pm {
    color: var(--secondary-text-color);
    font-size: 0.8rem;
  }
  .sub {
    font-size: 0.74rem;
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
  .num {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .note {
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .warnings {
    margin: 10px 0 0;
    padding-left: 18px;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .retry {
    margin-top: 8px;
    display: flex;
    gap: 10px;
    align-items: center;
    flex-wrap: wrap;
  }
  details {
    margin-top: 12px;
    font-size: 0.82rem;
  }
  summary {
    cursor: pointer;
    color: var(--secondary-text-color);
  }
  table.obs {
    font-size: 0.76rem;
  }
  .hint {
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .muted {
    color: var(--secondary-text-color);
  }
</style>
