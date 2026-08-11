<script lang="ts">
  // The microphone-assisted alignment wizard (docs/mic-alignment-plan.md §12.1).
  //
  // Self-contained on purpose: it takes a name resolver, an optional group to seed
  // the selection from, and a close callback, and owns nothing else.
  //
  // The model plan §12.1 asked for is now the real one: the user *picks speakers*
  // and the daemon forms a temporary exclusive group around them
  // (`POST /api/align/start {outputs, mode}`), so the wizard no longer needs a
  // pre-existing sync group at all. `group` survives only as a *seed* — mounted from
  // a source card, "the speakers this source is playing to" is a good first guess at
  // the scope — which is also what keeps moving this to the Outputs page down to
  // dropping the prop.
  //
  // Page flow: mode → speakers → run → review. The run's own phase drives the
  // page forward (a run that starts moves you to the run page; a proposal moves you
  // to review), but only when the phase *changes*, so pressing Back still works.
  //
  // The microphone lives here rather than on a page, because the capture must
  // survive page changes: the analysis grid is one continuous capture, and a mic
  // that restarted mid-run has thrown away the reference frame everything measured
  // so far shares.
  import AlignRefusal from './AlignRefusal.svelte';
  import AlignWizardMode from './AlignWizardMode.svelte';
  import AlignWizardReview from './AlignWizardReview.svelte';
  import AlignWizardRun from './AlignWizardRun.svelte';
  import AlignWizardSpeakers from './AlignWizardSpeakers.svelte';
  import MicCapture from './MicCapture.svelte';
  import { align } from '../lib/align.svelte';
  import { askConfirm } from '../lib/confirm.svelte';
  import { MODE_LABELS, elapsed, isLive, measure, phaseLabel } from '../lib/measure.svelte';
  import type { AlignGroup, MeasureMode, MeasurePhase } from '../lib/types';

  interface Props {
    /** Speakers to pre-select as the run's scope — the group this wizard was opened
     *  from, when it was opened from one. The user can add to or cut from it; the
     *  scope is theirs, not the source's. */
    group?: AlignGroup;
    label: (nodeName: string) => string;
    onClose: () => void;
  }
  let { group, label, onClose }: Props = $props();

  // Seeded once, here rather than in the selection page: that page is destroyed and
  // rebuilt every time the user steps back to the mode picker, and re-seeding a scope
  // the user had just edited would be maddening. The guard is what makes it once —
  // not a defensive check.
  let seeded = false;
  $effect(() => {
    if (seeded) return;
    seeded = true;
    const seed = group?.members.map((m) => m.node_name) ?? [];
    if (seed.length && !align.sessionActive && align.selection.length === 0) align.setSelection(seed);
  });

  /** Is a session holding speakers right now? Read from the store rather than passed
   *  in: a wizard run's session is identified by its *selection*, so a caller that
   *  compares source sets (the by-ear panel does) would say "not mine" about the
   *  very session this wizard started. */
  const holding = $derived(align.sessionActive);

  type Page = 'mode' | 'speakers' | 'run' | 'review';
  const PAGES: { id: Page; name: string }[] = [
    { id: 'mode', name: 'Mode' },
    { id: 'speakers', name: 'Speakers' },
    { id: 'run', name: 'Measure' },
    { id: 'review', name: 'Review' },
  ];

  let page = $state<Page>('mode');
  let mode = $state<MeasureMode>('sweet_spot');

  const status = $derived(measure.status);
  const live = $derived(measure.live);
  const hasResult = $derived(!!status && (!!status.proposal || !!status.verification));

  // One poll loop for the whole run, ref-counted in the store.
  $effect(() => measure.attach());

  // The *session* is polled too, and only from here. `AlignState.interference`
  // changes without the UI doing anything — a barge-in announcement or a voice-duck
  // hold legitimately outranks the alignment's exclusive hold (plan §12.3) — and
  // that report is worth nothing if it appears an hour later. Slower than the run
  // poll: it is an explanation, not a progress bar.
  $effect(() => {
    void align.refreshStatus();
    const id = setInterval(() => void align.refreshStatus(), 3000);
    return () => clearInterval(id);
  });

  const interference = $derived(align.interference);

  // Follow the run, without pinning the user: only a *change* of phase moves the
  // page, so Back and the step buttons keep working while a run is in progress.
  // Deliberately not `$state`: this is the effect's own bookkeeping, and nothing
  // renders it. As reactive state it would re-run the effect it is written by.
  let lastPhase: MeasurePhase | null = null;
  $effect(() => {
    const st = measure.status;
    if (!st || st.phase === lastPhase) return;
    lastPhase = st.phase;
    // Everything the daemon is actively doing belongs on the run page — including
    // `settling`, which is where the gate explains a group that has gone quiet.
    if (isLive(st.phase)) page = 'run';
    else if (st.phase === 'proposed' || st.phase === 'done') page = 'review';
    else if (st.phase === 'refused') page = st.proposal ? 'review' : 'run';
  });

  function reachable(id: Page): boolean {
    if (id === 'run') return !!status && status.phase !== 'idle';
    if (id === 'review') return hasResult;
    return true;
  }

  async function start() {
    measure.clearError();
    if (await measure.start(mode)) page = 'run';
  }

  async function apply() {
    const n = status?.proposal?.members.filter((m) => m.added_ms !== 0).length ?? 0;
    const ok = await askConfirm({
      title: n === 1 ? 'Write the new setting to 1 speaker?' : `Write the new settings to ${n} speakers?`,
      body: [
        'Each speaker whose setting changes reconnects, so the group goes quiet for tens of seconds while they come back.',
        'This overwrites whatever those speakers were set to before — including anything you tuned by ear. The old values are remembered, so Revert puts them back.',
      ],
      confirmLabel: 'Apply',
      danger: true,
    });
    if (ok) await measure.apply();
  }

  /** Stop everything: abandon the run and end the session, which restores levels,
   *  mutes and routing exactly as finishing by hand does — and releases the exclusive
   *  hold, so the displaced music comes back. Available on every page, in every state,
   *  including while the hold is still forming (plan §12.2); it never touches a speaker's knob,
   *  so an applied write stays revertable afterwards. */
  async function stopAll() {
    await measure.abandon();
    if (align.sessionActive) await align.stop();
    onClose();
  }
</script>

<div class="wizard">
  <div class="head">
    <strong>Measure with a microphone</strong>
    <!-- A run's own mode wins over the picker: once something is running, the
         header must describe what is running, not what is selected. -->
    <span class="badge">{MODE_LABELS[status && status.phase !== 'idle' ? status.mode : mode]}</span>
    {#if status && status.phase !== 'idle'}
      <span class="badge" class:on={live} class:warn={status.phase === 'refused'}>{phaseLabel(status.phase)}</span>
    {/if}
    <span class="spacer"></span>
    {#if live}
      <button class="ghost" disabled={measure.busy} title="Stop measuring, leave the test tone playing" onclick={() => void measure.abandon()}>
        Stop measuring
      </button>
    {/if}
    <!-- Never disabled by a forming hold: stopping has to work at every point, and
         the daemon's stop is safe against a start that is still in flight. -->
    <button class="danger" disabled={measure.busy} title="Stop measuring, give the speakers back and put levels, mutes and routing back to normal" onclick={() => void stopAll()}>
      Stop and restore
    </button>
    {#if !live && !holding}
      <button class="ghost" title="Leave the wizard; nothing is being held and the by-ear sliders stay as they are" onclick={onClose}>
        Close
      </button>
    {/if}
  </div>

  {#if holding && !live}
    <!-- Closing is deliberately not offered while speakers are held: the hold is
         exclusive, so leaving it running would leave part of the house silent with no
         visible reason. "Stop and restore" is the way out, and it says what it does. -->
    <p class="hint">
      These speakers are held for the alignment, so something else may have stopped playing on them. Use
      <strong>Stop and restore</strong> to give them back.
    </p>
  {/if}

  <div class="steps">
    {#each PAGES as p, i (p.id)}
      <button class="step" class:on={page === p.id} disabled={!reachable(p.id)} onclick={() => (page = p.id)}>
        <span class="n">{i + 1}</span>{p.name}
      </button>
    {/each}
  </div>

  <!-- Mounted for the wizard's whole life, not per page: a capture that restarts
       loses the timing reference every earlier reading shares. -->
  <MicCapture />

  <!-- Shown on every page, because it can happen on any of them. Exclusivity over
       the aligning speakers is real but not absolute: an urgent announcement or a
       voice-assistant turn wins on purpose, and the affected speaker's reading is
       discarded. Without this the gate reports the same event as an unstable
       amplitude and sends the user to hold the phone stiller — which is the exact
       misdiagnosis this report exists to prevent. -->
  {#if interference.length}
    <div class="interference">
      <strong>Something else played on these speakers</strong>
      <ul>
        {#each interference as i (`${i.member}-${i.at_ms}-${i.cause.kind}`)}
          <li>
            <span class="spk">{label(i.member)}</span>
            <span class="when">{elapsed(Math.round(i.at_ms / 1000))} into the session</span>
            <!-- Verbatim, and it names the cause rather than the user. -->
            <div>{i.reason}</div>
          </li>
        {/each}
      </ul>
      <p class="hint">
        This is by design — an alarm or a doorbell is never held back for a calibration. The readings taken while it
        played were thrown away, so measuring the affected speaker again is all it costs.
      </p>
    </div>
  {/if}

  <div class="page">
    {#if page === 'mode'}
      <AlignWizardMode {mode} onPick={(m) => (mode = m)} />
      <div class="nav">
        <button class="primary" onclick={() => (page = 'speakers')}>Next: speakers</button>
      </div>
    {:else if page === 'speakers'}
      <AlignWizardSpeakers {mode} {label} onStart={() => void start()} />
      <div class="nav">
        <button class="ghost" onclick={() => (page = 'mode')}>Back</button>
      </div>
    {:else if page === 'run'}
      {#if status && status.phase !== 'idle'}
        <AlignWizardRun {status} {label} onRetry={() => void start()} />
      {:else}
        <p class="empty">Nothing is being measured. Go back to Speakers and start a run.</p>
      {/if}
      <div class="nav">
        <button class="ghost" onclick={() => (page = 'speakers')}>Back</button>
        {#if hasResult}<button class="primary" onclick={() => (page = 'review')}>Next: review</button>{/if}
      </div>
    {:else if status}
      <AlignWizardReview
        {status}
        {label}
        busy={measure.busy}
        onApply={() => void apply()}
        onRevert={() => void measure.revert()}
        onDiscard={() => void measure.abandon().then(() => (page = 'speakers'))}
        onRetry={() => void start()}
      />
      <div class="nav">
        <button class="ghost" onclick={() => (page = 'run')}>Back to the run</button>
      </div>
    {/if}
  </div>

  <!-- A refused action (start, apply, revert) in full: kind, sentence, member,
       and the estimator's own verdict when it had one. -->
  {#if measure.actionRefusal}
    <AlignRefusal refusal={measure.actionRefusal} {label} />
  {:else if measure.actionError}
    <p class="problem">{measure.actionError}</p>
  {/if}
</div>

<style>
  .wizard {
    margin: 12px 0;
    padding: 12px;
    border: 1px solid var(--divider-color);
    border-radius: 8px;
    background: var(--card-background-color);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 0.9rem;
  }
  .spacer {
    flex: 1 1 auto;
  }
  .head button {
    padding: 4px 10px;
    font-size: 0.8rem;
  }
  .steps {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    margin: 12px 0;
  }
  /* Steps are buttons, not decoration: going back to a finished page is how the
     user re-reads the mode they chose without abandoning a run. */
  .step {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 3px 10px;
    font-size: 0.78rem;
    border: 1px solid var(--divider-color);
    border-radius: 999px;
    background: transparent;
  }
  .step.on {
    border-color: var(--primary-color);
    background: color-mix(in srgb, var(--primary-color) 12%, transparent);
  }
  .step .n {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    font-size: 0.68rem;
    background: var(--input-fill-color);
    color: var(--secondary-text-color);
  }
  .step.on .n {
    background: var(--primary-color);
    color: var(--text-on-primary);
  }
  .page {
    margin-top: 12px;
  }
  .nav {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 14px;
  }
  .empty {
    font-size: 0.84rem;
    color: var(--secondary-text-color);
    font-style: italic;
  }
  .hint {
    margin: 8px 0 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .hint strong {
    color: var(--primary-text-color);
  }
  .problem {
    margin: 10px 0 0;
    font-size: 0.8rem;
    color: var(--error-color, #db4437);
  }
  /* Amber, not red: nothing is broken — something more important happened. */
  .interference {
    margin-top: 10px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid color-mix(in srgb, var(--warning-color, #ffa600) 55%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 10%, transparent);
    font-size: 0.82rem;
  }
  .interference ul {
    margin: 6px 0 0;
    padding-left: 18px;
  }
  .interference li {
    margin-bottom: 4px;
    color: var(--secondary-text-color);
  }
  .interference .spk {
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid var(--divider-color);
    background: var(--input-fill-color);
    color: var(--primary-text-color);
  }
  .interference .when {
    margin-left: 6px;
    font-size: 0.76rem;
    font-variant-numeric: tabular-nums;
  }
  .interference .hint {
    margin: 6px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
</style>
