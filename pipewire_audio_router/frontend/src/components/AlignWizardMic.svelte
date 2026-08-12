<script lang="ts">
  // Wizard page 1: **does the microphone work?** — asked before the mode is picked
  // (plan §4.1, §4.2, §12.1).
  //
  // It comes first because the answer *removes options*. Two of plan §1's three modes
  // are measurements and cannot happen without a capture; the third exists precisely
  // for the case where there is none. Asking "how should this be aligned?" before
  // knowing whether the phone can hear anything is how a user picks Near field, walks
  // through a speaker selection, and only then finds out that the page was served over
  // plain HTTP and never had a microphone to begin with.
  //
  // The *control* is not here: <MicCapture> is mounted by the wizard shell, above this,
  // and stays mounted for the wizard's whole life because the analysis grid is one
  // continuous capture (§1.2) — a capture that restarts throws away the timing
  // reference every earlier reading shares, and for a walk that costs the user the walk.
  // So this page reads the outcome and says what it *means*, and the panel above owns
  // starting, stopping, the level meter and the daemon's own verdict.
  //
  // Nothing here is a gate. The step can always be left: with no usable microphone the
  // next page offers Manual, which is the documented fallback rather than a consolation.
  import type { MicOutcome } from '../lib/mic.svelte';

  interface Props {
    /** What the check established (see `MicOutcome`). */
    outcome: MicOutcome;
    /** Why the measured modes are unavailable, or null when they are not — the same
     *  sentence the mode page shows inside the disabled options, so the two pages cannot
     *  give different reasons for the same reduction. */
    block: string | null;
    /** A §4.2 constraint read-back caveat that travels with a measured run, or null. */
    caveat: string | null;
  }
  let { outcome, block, caveat }: Props = $props();

  const working = $derived(outcome.state === 'working');
  /** Working, but with something to carry forward — amber rather than green, because
   *  "we could not confirm the processors are off" is not the same as "they are off". */
  const qualified = $derived(working && !!caveat);
  const failed = $derived(outcome.state === 'failed');
  /** §4.1's one precondition with no workaround. Worth its own wording: every other
   *  failure here is worth retrying, and this one is not. */
  const insecure = $derived(outcome.failure === 'insecure');

  const HEADLINES: Record<MicOutcome['state'], string> = {
    unchecked: 'The microphone has not been checked',
    checking: 'Asking the browser for the microphone…',
    working: 'The microphone works',
    failed: 'This browser cannot measure',
  };

  /** The three modes and what this outcome leaves of them. Written out rather than
   *  filtered: a user has to be able to see that Near field exists at all, instead of
   *  finding a shorter list than the documentation describes and no way to tell whether
   *  the mode is missing or merely unavailable here. */
  const MODES: { name: string; note: string; measured: boolean }[] = [
    {
      name: 'Multi-position',
      note: 'measured from where you sit, one or several listening spots',
      measured: true,
    },
    { name: 'Near field', note: 'measured by walking to each speaker in turn — aligns the wiring', measured: true },
    { name: 'Manual', note: 'tuned by ear, no microphone involved', measured: false },
  ];
</script>

<p class="lead">
  The next page asks <em>what</em> should end up aligned, and two of the three answers are measurements taken with this
  phone's microphone. So this step establishes whether there is a microphone to measure with — the same capture a run
  depends on, started here where it costs nothing rather than four minutes into a run.
</p>

<div class="verdict" class:good={working && !qualified} class:tight={qualified} class:bad={failed}>
  <div class="head">
    <span class="dot"></span>
    <strong>{HEADLINES[outcome.state]}</strong>
    {#if qualified}<span class="badge caution">with a caveat</span>{/if}
  </div>

  {#if outcome.state === 'unchecked'}
    <p>
      Nothing has been asked of the browser yet. Press <strong>Use microphone</strong> above and give the page access —
      the meter should move with the clicks, and the daemon's own verdict appears under it. Until then the two measured
      modes stay unavailable, because they need a live capture and there is none.
    </p>
  {:else if outcome.state === 'checking'}
    <p>
      Waiting for the permission prompt and the audio worklet. If no prompt appeared, the browser may have remembered a
      refusal for this site — check the permission control in the address bar.
    </p>
  {:else if working}
    <p>
      Audio is arriving at the add-on, which proves the whole path rather than just that this device has a microphone:
      the browser's capture, the socket, and the daemon's ring buffer.
    </p>
    {#if caveat}
      <!-- Carried forward rather than dropped: §4.2's read-back cannot refuse on
           absence, so the uncertainty is what travels with the run. -->
      <p class="carry">{caveat}</p>
    {/if}
    <p class="keep">
      <strong>Leave it running.</strong> The whole run is measured against one continuous capture, so a microphone that
      is stopped and started again throws away the timing reference everything measured so far shares — and in Near
      field that means walking the house a second time.
    </p>
  {:else}
    <!-- The failure's own sentence is deliberately *not* repeated here: <MicCapture> prints
         it verbatim directly above, and a page that says the same thing three times teaches
         the reader to skip all three. What this block adds is what the panel cannot know —
         what the failure costs, and whether it is worth retrying. -->
    {#if insecure}
      <p>
        This is the one precondition with no workaround: on a plain-HTTP address the browser does not merely refuse the
        microphone, it hides the API completely, so there is nothing to grant. Reach Home Assistant over
        <strong>HTTPS</strong> — the same instance on its <code>https://</code> address — and this step will pass.
      </p>
    {:else if outcome.failure === 'processing'}
      <p>
        The capture itself is fine; what it would deliver is not. Echo cancellation exists to remove loudspeaker sound
        from a microphone signal, which is exactly the sound being measured, and it adapts over seconds — so a run would
        start out plausible and decay. Refusing it is the honest answer. Another browser may honour the request.
      </p>
    {:else}
      <p>
        Measuring needs a capture, so the measured modes are unavailable until this succeeds. It is worth one retry —
        press <strong>Use microphone</strong> above.
      </p>
    {/if}
  {/if}
</div>

<h4>What this leaves you</h4>
<!-- The reason is stated once, in the verdict above, and again inside each disabled option
     on the mode page itself — where it is attached to a control the user is trying to click,
     which is the place it is actually needed. Repeating it per row here would print the same
     sentence twice more on one screen. -->
<ul class="modes">
  {#each MODES as m (m.name)}
    {@const off = m.measured && !!block}
    <li class:off>
      <span class="state">{off ? 'unavailable' : 'available'}</span>
      <div><strong>{m.name}</strong> <span class="note">— {m.note}</span></div>
    </li>
  {/each}
</ul>

{#if block}
  <p class="fallback">
    <strong>Manual is not a consolation prize.</strong> It is plan §1's third mode and the documented fallback for
    exactly this case: the speakers are still grouped and taken over for the alignment, the click still plays, and you
    judge the timing by ear instead of the estimator judging it. What it cannot give you is a number, a confidence, or a
    check — so if the microphone can be made to work, it is worth doing that first.
  </p>
{/if}

<style>
  .lead {
    margin: 0 0 12px;
    font-size: 0.84rem;
    color: var(--secondary-text-color);
  }
  /* Same visual language as the level verdict (AlignSignalVerdict): colour reinforces,
     the wording carries the state on its own. */
  .verdict {
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: color-mix(in srgb, var(--secondary-text-color) 6%, transparent);
  }
  .verdict.good {
    border-color: color-mix(in srgb, var(--success-color, #43a047) 55%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 10%, transparent);
  }
  .verdict.tight {
    border-color: color-mix(in srgb, var(--warning-color, #ffa600) 55%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 12%, transparent);
  }
  .verdict.bad {
    border-color: color-mix(in srgb, var(--error-color, #db4437) 55%, transparent);
    background: color-mix(in srgb, var(--error-color, #db4437) 10%, transparent);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 0.86rem;
  }
  .dot {
    flex: 0 0 auto;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--secondary-text-color);
  }
  .verdict.good .dot {
    background: var(--success-color, #43a047);
  }
  .verdict.tight .dot {
    background: var(--warning-color, #ffa600);
  }
  .verdict.bad .dot {
    background: var(--error-color, #db4437);
  }
  .verdict p {
    margin: 6px 0 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .verdict p strong {
    color: var(--primary-text-color);
  }
  .carry,
  .keep {
    padding-top: 6px;
    border-top: 1px solid var(--divider-color);
  }
  h4 {
    margin: 16px 0 6px;
    font-size: 0.85rem;
    font-weight: 600;
  }
  .modes {
    display: grid;
    gap: 6px;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .modes li {
    display: flex;
    gap: 10px;
    align-items: baseline;
    padding: 6px 9px;
    border: 1px solid var(--divider-color);
    border-radius: 8px;
    font-size: 0.84rem;
  }
  /* Dimmed, never removed — and the reason travels with it. */
  .modes li.off {
    background: color-mix(in srgb, var(--secondary-text-color) 6%, transparent);
  }
  .modes li.off strong,
  .modes li.off .note {
    color: var(--secondary-text-color);
  }
  .state {
    flex: 0 0 auto;
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid color-mix(in srgb, var(--success-color, #43a047) 55%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 10%, transparent);
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .modes li.off .state {
    border-color: var(--divider-color);
    background: transparent;
    color: var(--secondary-text-color);
  }
  .note {
    color: var(--secondary-text-color);
  }
  .fallback {
    margin: 12px 0 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .fallback strong {
    color: var(--primary-text-color);
  }
</style>
