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
  //
  // **It says as little as it can get away with.** It used to open with a paragraph on why
  // the step comes first, then list all three modes with a note each, then explain that
  // Manual is not a consolation prize — three screens of prose in front of one button, most
  // of it repeated on the mode page immediately afterwards. What is left is the verdict, and
  // for a failure the part the panel above cannot know: whether it is worth retrying. The
  // background belongs in "Explain speaker alignment", which is one click away.
  import type { MicOutcome } from '../../lib/mic.svelte';

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
</script>

<p class="lead">Measuring needs this phone's microphone. Without one you can still align by ear.</p>

<div class="verdict" class:good={working && !qualified} class:tight={qualified} class:bad={failed}>
  <div class="head">
    <span class="dot"></span>
    <strong>{HEADLINES[outcome.state]}</strong>
    {#if qualified}<span class="badge caution">with a caveat</span>{/if}
  </div>

  {#if outcome.state === 'unchecked'}
    <p>Press <strong>Use microphone</strong> below and allow access. The checks above then fill in.</p>
  {:else if outcome.state === 'checking'}
    <p>
      Waiting for the permission prompt. If none appeared, the browser may have remembered a refusal — check the
      permission control in the address bar.
    </p>
  {:else if working}
    <p><strong>Leave it running:</strong> restarting it mid-run loses the timing reference, and in Near field that costs you the walk again.</p>
    {#if caveat}
      <!-- Carried forward rather than dropped: §4.2's read-back cannot refuse on
           absence, so the uncertainty is what travels with the run. -->
      <p class="carry">{caveat}</p>
    {/if}
  {:else}
    <!-- The failure's own sentence is deliberately *not* repeated here: <MicCapture> prints
         it verbatim directly above. What this adds is what the panel cannot know — whether
         it is worth retrying. -->
    {#if insecure}
      <p>
        No workaround for this one: on a plain-HTTP address the browser hides the microphone entirely, so there is
        nothing to grant. Open Home Assistant on its <code>https://</code> address and this step passes.
      </p>
    {:else if outcome.failure === 'processing'}
      <p>
        The capture works; what it would deliver does not. The browser is still processing the signal being measured, so
        a run would start out plausible and drift. Another browser may honour the request.
      </p>
    {:else}
      <p>Worth one retry — press <strong>Use microphone</strong> below.</p>
    {/if}
  {/if}
</div>

{#if block}
  <p class="fallback">
    <strong>By ear is a real alternative</strong>, not a consolation prize: same speakers, same test click, and you
    judge the timing instead of the estimator. What it cannot give you is a number or a check.
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
  .carry {
    padding-top: 6px;
    border-top: 1px solid var(--divider-color);
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
