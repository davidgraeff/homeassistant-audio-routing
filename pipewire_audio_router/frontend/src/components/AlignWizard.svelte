<script lang="ts">
  // The speaker-alignment wizard (docs/mic-alignment-plan.md §12.1) — **all three of
  // plan §1's modes**, not just the measured ones.
  //
  // Self-contained on purpose: it takes a name resolver and a close callback, and owns
  // nothing else. In particular it takes no group: the model §12.1 asked for is the real
  // one now, so the user *picks speakers* and the daemon forms a temporary exclusive
  // group around them (`POST /api/align/start {outputs, mode}`). The old `group` seed
  // came from being mounted on a source card, and with the wizard on a page of its own
  // there is nothing to seed from — which is the point rather than a loss.
  //
  // Page flow: microphone → mode → speakers → the mode's own body → review. The run's own
  // phase drives the page forward (a run that starts moves you to the body; a proposal
  // moves you to review), but only when the phase *changes*, so pressing Back still works.
  //
  // **The microphone step comes before the mode**, and that ordering is the point rather
  // than a nicety: two of the three modes are measurements, so whether there is a usable
  // capture decides which modes exist at all (§4.1, §4.2). Asked afterwards, a user with
  // no microphone picks Near field, picks speakers, pays the reconnect wave that forms the
  // hold, and *then* learns the page was served over plain HTTP. So the check happens
  // first and the mode page is reduced from its outcome — by disabling with the reason,
  // never by hiding, because a shorter list than the documentation describes is its own
  // kind of lie.
  //
  // The three modes differ in exactly two places, and everything else is shared:
  //
  //   * **the body**: a stationary measurement (with the chain inside it), a walk, or the
  //     by-ear sliders;
  //   * **the microphone**: by-ear does not use one, so the capture is not mounted once
  //     that mode is chosen — the mode exists precisely for the case where there is no
  //     usable mic (§4.1), and offering "Use microphone" on it would offer the thing that
  //     just failed. The microphone *step* is the exception, because that is where the
  //     failure is established.
  //
  // The microphone lives here rather than on a page, because the capture must survive
  // page changes: the analysis grid is one continuous capture, and a mic that restarted
  // mid-run has thrown away the reference frame everything measured so far shares.
  import AlignRefusal from './AlignRefusal.svelte';
  import AlignWizardManual from './AlignWizardManual.svelte';
  import AlignWizardMic from './AlignWizardMic.svelte';
  import AlignWizardMode from './AlignWizardMode.svelte';
  import AlignWizardReview from './AlignWizardReview.svelte';
  import AlignWizardRun from './AlignWizardRun.svelte';
  import AlignWizardSpeakers from './AlignWizardSpeakers.svelte';
  import MicCapture from './MicCapture.svelte';
  import { align, isMeasured, type WizardMode } from '../lib/align.svelte';
  import { askConfirm } from '../lib/confirm.svelte';
  import { MODE_LABELS, elapsed, isRunning, measure, phaseLabel } from '../lib/measure.svelte';
  import { measuredBlock, measuredCaveat, mic } from '../lib/mic.svelte';
  import type { MeasurePhase } from '../lib/types';

  interface Props {
    label: (nodeName: string) => string;
    onClose: () => void;
  }
  let { label, onClose }: Props = $props();

  /** Is a session holding speakers right now? Read from the store rather than passed
   *  in: a session's identity is the *selection* it was formed over, and no page outside
   *  this wizard has an opinion about whose it is. */
  const holding = $derived(align.sessionActive);

  type Page = 'mic' | 'mode' | 'speakers' | 'body' | 'review';

  let page = $state<Page>('mic');
  /** The mode the user has picked. Not what the wizard acts on — see `mode`, which is this
   *  reduced by what the microphone step found. */
  let pick = $state<WizardMode>('sweet_spot');
  /** Measure from more than one listening position, joining them through overlap
   *  speakers (plan §1.1). Chosen on the mode page and only meaningful for
   *  multi-position; a chain with one step *is* the single-position run, which is why the
   *  daemon defaults it off rather than treating one position as a degenerate chain. */
  let chained = $state(false);

  // ---- What the microphone step established, and what it costs the mode choice ----
  //
  // Read from the store rather than re-derived here: the same three fields drive the mic
  // panel, and a second interpretation of them is a second policy that can disagree with
  // what the user was just shown.
  const outcome = $derived(mic.outcome);
  /** Why the measured modes cannot be offered, or null. **A run in progress always wins**:
   *  a capture that drops during `settling` must not retroactively disable the mode of the
   *  run that is holding the group. The daemon has its own `mic_lost` refusal for that,
   *  which says what happened — where a silently changed radio button would not. */
  const micBlock = $derived(measure.running ? null : measuredBlock(outcome));
  /** §4.2's read-back caveat, carried onto the mode page rather than quietly dropped. */
  const micCaveat = $derived(measuredCaveat(outcome));

  /** The mode the wizard **acts on**: the user's pick, reduced to by-ear while the
   *  microphone step says a measurement is impossible.
   *
   *  Derived rather than written back into `pick`, and that is not a style choice. Forcing
   *  the state would make the reduction permanent: the capture is mounted for measured
   *  modes, so a `pick` overwritten with `manual` before the check has even run would
   *  unmount — and therefore *stop* — the microphone the moment the user left this step,
   *  seconds after proving it works. Reducing a copy leaves the default intact for the
   *  instant the block clears, and still guarantees that nothing downstream (the hold, the
   *  body, `measure.start`) can act on a mode this browser cannot perform. */
  const mode = $derived<WizardMode>(micBlock && isMeasured(pick) ? 'manual' : pick);

  const status = $derived(measure.status);
  /** A run parked in `positioning` or `walking` is alive: it holds the group, and a chain
   *  is carrying provisional delays nobody has written. So every question this shell asks
   *  — may it be abandoned, may the user leave, is a run in progress — is `running`, never
   *  `live` (which only says whether the *daemon* is busy this second). */
  const running = $derived(measure.running);
  const hasResult = $derived(!!status && (!!status.proposal || !!status.verification));

  // One poll loop for the whole run, ref-counted in the store.
  $effect(() => measure.attach());

  // The *session* is loaded and then polled from here, and only from here — no other page
  // touches it (plan §12.1). Two reasons it is polled at all: `AlignState.interference`
  // changes without the UI doing anything, because a barge-in announcement or a voice-duck
  // hold legitimately outranks the alignment's exclusive hold (plan §12.3) and that report
  // is worth nothing if it appears an hour later; and a by-ear session's reference/target
  // can be moved by something else holding the same session.
  //
  // The full load matters for by-ear specifically: it is what fetches each speaker's
  // current knob value and the `sendspin_delay_live` setting, without which the tuning
  // sliders would start from zero and stream changes to firmware that cannot take them.
  //
  // `attachSession` stops nothing on teardown — see the store. Leaving this page is not
  // consent to drop a hold; "Stop and restore" is.
  $effect(() => align.attachSession());

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
    // Everything the daemon is actively doing belongs on the body page — including
    // `settling`, which is where the gate explains a group that has gone quiet;
    // `positioning`, where a chain asks for the next listening position; and `walking`,
    // where near field asks which speaker the phone is at. `walking` is also how the
    // *verification* of a near-field write asks for its second walk (plan §10.4), which is
    // why coming back here from the review page is right rather than a regression.
    if (isRunning(st.phase)) page = 'body';
    else if (st.phase === 'proposed' || st.phase === 'done') page = 'review';
    else if (st.phase === 'refused') page = st.proposal ? 'review' : 'body';
  });

  /** The mode the wizard is *showing*: a run's own mode wins over the picker, for the same
   *  reason the header badge does — once something is running, every label has to describe
   *  what is running rather than what is selected. */
  const shownMode = $derived<WizardMode>(status && status.phase !== 'idle' ? status.mode : mode);
  /** The body page's name, which is the mode's own verb. Not cosmetic: "Measure" over a
   *  page of by-ear sliders would promise a measurement that is not happening, and
   *  "Measure" over a walk hides the fact that the user is the one doing the work. */
  const bodyName = $derived(shownMode === 'manual' ? 'Tune' : shownMode === 'near_field' ? 'Walk' : 'Measure');
  /** Does this mode drive the measurement state machine? Where it does not, `measure.*`
   *  is never called and the microphone is not mounted.
   *
   *  Read from `shownMode`, so **anything the daemon has to show wins over the picker**:
   *  a run in progress obviously (switching the radio mid-run must not unmount the capture
   *  it depends on), but equally a finished or refused one. That last case is not
   *  hypothetical — with the mode reduced to by-ear by an unchecked microphone, deriving
   *  this from the picker alone put "Align speakers by ear" above a measured proposal and
   *  took the microphone panel away from a run that had just produced it. */
  const measured = $derived(isMeasured(shownMode));
  const PAGES: { id: Page; name: string }[] = $derived([
    { id: 'mic', name: 'Microphone' },
    { id: 'mode', name: 'Mode' },
    { id: 'speakers', name: 'Speakers' },
    { id: 'body', name: bodyName },
    { id: 'review', name: 'Review' },
  ]);

  function reachable(id: Page): boolean {
    // By-ear's body is the hold itself: there is no run to have started, so what makes it
    // reachable is that speakers are held.
    if (id === 'body') return measured ? !!status && status.phase !== 'idle' : holding;
    // ...and it never produces a proposal to review, because every nudge is already
    // written. A step that could never light up would be a promise of a page that is not
    // coming.
    if (id === 'review') return measured && hasResult;
    return true;
  }

  async function start() {
    measure.clearError();
    // By-ear starts nothing: the hold formed on the Speakers page *is* the session, the
    // daemon has already made a reference/target pair audible, and there is no run to
    // begin. Calling `measure.start` here would be refused — `manual` is not one of the
    // measurement state machine's modes.
    if (!isMeasured(mode)) {
      page = 'body';
      return;
    }
    // `chained` only reaches the daemon for multi-position: near field has its own
    // acquisition (a walk) and needs no overlaps at all, and sending both would invite
    // the reader to think the two compose.
    if (await measure.start(mode, mode === 'sweet_spot' && chained)) page = 'body';
  }

  /** Near field: "I am standing at this speaker." A refusal here is the *call's* — the
   *  wrong speaker, or one already measured — and the walk stays parked exactly where it
   *  was, so there is nothing to unwind and no page to leave. */
  async function arrival(nodeName: string) {
    measure.clearError();
    await measure.arrival(nodeName);
  }

  /** Near field: the closure reading. Refused until every speaker has been visited; a
   *  closure too large for any plausible clock refuses the whole walk, and that arrives as
   *  the run's own refusal rather than as this call's. */
  async function closeWalk() {
    measure.clearError();
    await measure.close();
  }

  /** One listening position of a chain. A refusal here is the *step's*: the chain stays
   *  parked with the reason in `status.chain.refusal` and the user can post again, so
   *  there is nothing to unwind and no page to leave. */
  async function position(members: string[], overlaps: string[]) {
    measure.clearError();
    await measure.position(members, overlaps);
  }

  async function finish() {
    measure.clearError();
    await measure.finish();
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
    <!-- The title is the mode's promise, not the feature's name: "Measure with a
         microphone" over the by-ear sliders would be simply false. On the microphone step
         no promise has been made yet — and stating one there would state the wrong one,
         since an unchecked microphone reduces the mode to by-ear until it is proven. -->
    <strong>
      {#if page === 'mic' && !running}
        Align the timing between speakers
      {:else if measured}
        Measure the timing between speakers
      {:else}
        Align speakers by ear
      {/if}
    </strong>
    <!-- A run's own mode wins over the picker: once something is running, the
         header must describe what is running, not what is selected. `manual` has no
         `MeasureMode`, and that is exactly why it is named separately here. -->
    {#if (status && status.phase !== 'idle') || page !== 'mic'}
      <span class="badge">
        {#if status && status.phase !== 'idle'}
          {MODE_LABELS[status.mode]}
        {:else if mode === 'manual'}
          By ear
        {:else}
          {MODE_LABELS[mode]}
        {/if}
      </span>
    {/if}
    {#if status && status.phase !== 'idle'}
      <span class="badge" class:on={running} class:warn={status.phase === 'refused'}>{phaseLabel(status.phase)}</span>
    {/if}
    <span class="spacer"></span>
    {#if running}
      <!-- `running`, not `live`: a chain parked between positions is not doing anything
           at this instant but is very much a run — it is holding the group and carrying
           provisional delays — so abandoning it has to be offered there too. -->
      <button class="ghost" disabled={measure.busy} title="Stop measuring, leave the test tone playing" onclick={() => void measure.abandon()}>
        Stop measuring
      </button>
    {/if}
    <!-- Never disabled by a forming hold: stopping has to work at every point, and
         the daemon's stop is safe against a start that is still in flight. -->
    <button class="danger" disabled={measure.busy} title="Stop measuring, give the speakers back and put levels, mutes and routing back to normal" onclick={() => void stopAll()}>
      Stop and restore
    </button>
    {#if !running && !holding}
      <button class="ghost" title="Leave the wizard; nothing is being held and no run is in progress" onclick={onClose}>
        Close
      </button>
    {/if}
  </div>

  {#if holding && !running}
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

  <!-- Mounted for the wizard's whole life, not per page: a capture that restarts loses the
       timing reference every earlier reading shares — and for near field that costs the
       *user* another walk. Rendered here, outside the page block and in one fixed
       position, so stepping between pages cannot remount it.

       Two conditions, for two different reasons:

         * `measured` — by-ear does not use a microphone, and that is the mode's whole
           reason for existing (plan §1, §4.1). Once it is chosen the panel goes away
           rather than offering the thing that just failed. `measured` is sticky while a
           run is in progress, so flipping the radio mid-run cannot pull the capture out
           from under it;
         * the **microphone step** — which is where a capture is established or found
           impossible, and therefore the one page that must show the control even when the
           selected mode is by-ear (which it will be, the moment the check fails). -->
  {#if measured || page === 'mic'}
    <MicCapture />
  {/if}

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
    {#if page === 'mic'}
      <AlignWizardMic {outcome} block={micBlock} caveat={micCaveat} />
      <div class="nav">
        <!-- Always enabled: this step reduces the choice, it does not gate it. A browser
             with no microphone at all still has Manual waiting on the next page, and a
             blocked Next would strand the very user the fallback exists for. -->
        <button class="primary" onclick={() => (page = 'mode')}>
          {micBlock ? 'Continue without a microphone' : 'Next: mode'}
        </button>
      </div>
    {:else if page === 'mode'}
      <AlignWizardMode {mode} {chained} measuredBlock={micBlock} {micCaveat} onPick={(m) => (pick = m)} onChain={(c) => (chained = c)} />
      <div class="nav">
        <button class="ghost" onclick={() => (page = 'mic')}>Back</button>
        <button class="primary" onclick={() => (page = 'speakers')}>Next: speakers</button>
      </div>
    {:else if page === 'speakers'}
      <AlignWizardSpeakers {mode} chained={mode === 'sweet_spot' && chained} {label} onStart={() => void start()} />
      <div class="nav">
        <button class="ghost" onclick={() => (page = 'mode')}>Back</button>
      </div>
    {:else if page === 'body'}
      <!-- Page 3 is the mode's own body (plan §12.1): the measurement run — which carries
           the chain or the walk inside it — or the by-ear sliders. -->
      {#if !measured}
        <AlignWizardManual {label} />
      {:else if status && status.phase !== 'idle'}
        <AlignWizardRun
          {status}
          {label}
          busy={measure.busy}
          onRetry={() => void start()}
          onPosition={(members, overlaps) => void position(members, overlaps)}
          onFinish={() => void finish()}
          onArrival={(node) => void arrival(node)}
          onClose={() => void closeWalk()}
          onHear={(nodes) => void align.hear(nodes)}
        />
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
        <button class="ghost" onclick={() => (page = 'body')}>Back to the run</button>
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
