<script lang="ts">
  // "This alignment gives the speakers back in about N minutes" — the session's idle
  // timeout, on screen (plan §1.2, §12.3).
  //
  // Why this exists at all: the hold is *exclusive*, and the daemon tears the session
  // down after 15 minutes of idleness. When it fires, speakers that were held go back to
  // normal and any wizard still open is describing a session that no longer exists. Until
  // now nothing said that was coming, and it bit a real multi-position run — because the
  // step where a session runs out is the **review page**, and reading a page of numbers is
  // *quiet*, so it refreshes nothing.
  //
  // Three rules this display follows, all of them corrections of the obvious version:
  //
  //   * **no precision it does not have.** The daemon's watchdog only looks every
  //     `timeout_slack_s`, so the real close can be that much later. A ticking `13:42`
  //     would be a lie with a decimal point, and it would also invite someone to sit and
  //     watch it. So: "in about 13 minutes", from `holdCloseLabel`.
  //   * **say what refreshes it.** A countdown with no visible remedy is worse than no
  //     countdown — the user is told their room is about to be handed back and given
  //     nothing to do. So the thing that resets it is named (and it is named accurately:
  //     what counts is touching the run, not looking at it), and "Keep it open" is
  //     offered, which is one deliberate click and never a timer (see `align.stillHere`).
  //   * **zero is not "gone".** The countdown reaching 0 only means the watchdog has not
  //     looked yet. The end is rendered when the daemon *says* the session is over, which
  //     arrives on the session socket — never by this component deciding.
  import { align, holdCloseLabel } from '../../lib/align.svelte';

  interface Props {
    /** `compact` is the one-line form for a notice that already has its own frame (the
     *  Outputs page's "an alignment is holding these speakers" box); the default form
     *  carries the explanation, for the wizard where there is room to read it. */
    compact?: boolean;
  }
  let { compact = false }: Props = $props();

  const left = $derived(align.closesIn);
  const soon = $derived(align.closingSoon);
  const label = $derived(left === null ? null : holdCloseLabel(left, align.closeSlack));
  const minutes = $derived(Math.round(align.idleTimeout / 60));
</script>

<!-- Nothing at all when the daemon did not send a countdown: an older add-on has no
     `closes_in_s`, and inventing one would be worse than the silence this replaces. -->
{#if align.sessionActive && label}
  <div class="hold-timer" class:soon class:compact role={soon ? 'alert' : undefined}>
    <div class="line">
      <span class="when">
        {#if compact}Gives them back {label}{:else}These speakers go back to normal {label}{/if}
      </span>
      <button
        class="ghost"
        title="Keep this alignment session — and its hold on these speakers — for another {minutes} minutes"
        onclick={() => void align.stillHere()}
      >
        Keep it open
      </button>
    </div>
    {#if !compact}
      <p class="why">
        The alignment releases them by itself after {minutes} minutes with nothing changed — so a page left open cannot leave
        part of the house silent. <strong>Reading this page is not a change.</strong> Playing a speaker, moving a level or
        measuring one resets the clock; so does the button here.
      </p>
    {/if}
  </div>
{/if}

<style>
  .hold-timer {
    margin-top: 8px;
    padding: 6px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: var(--input-fill-color);
    font-size: 0.82rem;
    color: var(--secondary-text-color);
  }
  /* The remaining time and its remedy on one line — the two belong together, and the
     explanation underneath is for the first read only. */
  .line {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  /* Amber only near the end. Nothing is wrong for the first thirteen minutes, and a
     permanently warning-coloured box is a box people learn to stop reading. */
  .hold-timer.soon {
    border-color: color-mix(in srgb, var(--warning-color, #ffa600) 55%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 12%, transparent);
  }
  /* Inside a notice that already has its own frame and its own flex row: no box of its
     own, and no margin that would break that row's baseline. */
  .compact {
    margin: 0;
    padding: 0;
    border: 0;
    background: transparent;
  }
  .compact.soon {
    color: var(--warning-color, #ffa600);
  }
  .when {
    color: var(--primary-text-color);
  }
  .soon .when {
    font-weight: 600;
  }
  .why {
    margin: 4px 0 0;
    font-size: 0.78rem;
  }
  .why strong {
    color: var(--primary-text-color);
    font-weight: 600;
  }
  .hold-timer button {
    padding: 2px 10px;
    font-size: 0.78rem;
    white-space: nowrap;
  }
</style>
