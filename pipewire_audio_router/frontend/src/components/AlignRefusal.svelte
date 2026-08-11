<script lang="ts">
  // One refusal from the measurement daemon, rendered in full
  // (docs/mic-alignment-plan.md §5.5).
  //
  // The whole point of the estimator refusing rather than guessing is that the
  // user is told what went wrong and what to do about it. So all four parts of a
  // `Refusal` are shown: the kind as a headline, the daemon's sentence verbatim,
  // the speaker to blame when there is one, and the estimator's own verdict when
  // the refusal came from it. There is no code path here that can produce
  // "something went wrong".
  import { estimatorReasonLabel, refusalKindLabel } from '../lib/measure.svelte';
  import type { Refusal } from '../lib/types';

  interface Props {
    refusal: Refusal;
    /** Friendly name for a node name, so `member` reads as the user's speaker. */
    label?: (nodeName: string) => string;
    /** True when this refusal is what *blocks a write*, rather than how a run
     *  ended. Rendered as a stop, not as a note (plan §10.2: a failed
     *  transitivity check is blocking, never a warning). */
    blocking?: boolean;
  }
  let { refusal, label = (n: string) => n, blocking = false }: Props = $props();
</script>

<div class="refusal" class:blocking role="alert">
  <div class="head">
    <span class="dot"></span>
    <strong>{refusalKindLabel(refusal.kind)}</strong>
    {#if refusal.member}
      <span class="badge warn" title="This speaker is the one the daemon named">{label(refusal.member)}</span>
    {/if}
    {#if blocking}<span class="badge warn">blocks the write</span>{/if}
  </div>
  <!-- Verbatim: written for the user, by the code that knows what happened. -->
  <p class="msg">{refusal.message}</p>
  {#if refusal.estimator_reason}
    <p class="sub">The estimator's own reason: {estimatorReasonLabel(refusal.estimator_reason)}.</p>
  {/if}
  <p class="sub code">Reason code: <code>{refusal.kind}</code></p>
</div>

<style>
  .refusal {
    margin: 10px 0 0;
    padding: 10px 12px;
    border-radius: 8px;
    border: 1px solid color-mix(in srgb, var(--error-color, #db4437) 55%, transparent);
    background: color-mix(in srgb, var(--error-color, #db4437) 10%, transparent);
  }
  /* A blocked write is a stop, not a note — so it carries a heavier edge than an
     ordinary refusal, which is already over and done with. */
  .refusal.blocking {
    border-width: 2px;
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
    background: var(--error-color, #db4437);
  }
  .msg {
    margin: 6px 0 0;
    font-size: 0.84rem;
  }
  .sub {
    margin: 4px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .code {
    font-variant-numeric: tabular-nums;
  }
</style>
