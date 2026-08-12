<script lang="ts">
  // Speaker alignment, as a **page of its own** (plan §12.1).
  //
  // It used to be a card on the Outputs page, opened by a button. That was right about
  // *whose* choice alignment is — a set of speakers the user picks, not a source group —
  // and wrong about how much room it needs: the wizard is five pages deep and its Review
  // page is a dense report (a proposal table with a confidence per speaker, the chain's
  // joints, the checks and what each one does *not* prove) that someone reads carefully
  // and comes back to. Nesting that inside a card inside a tab made a report people are
  // meant to study look like a widget, and pushed the outputs it is about off screen.
  //
  // What stayed on Outputs is the *entry point and the state*: the button that comes here,
  // the notice that an alignment is holding speakers right now, and the offer to revert a
  // measurement that was written — all three are things a user needs while looking at their
  // speakers. What is **not** there any more is the wizard: one alignment session exists
  // process-wide, and two pages rendering the wizard is two pages that can each believe
  // they own it.
  //
  // The wizard is mounted unconditionally here, so the page *is* the wizard rather than a
  // button that reveals one. That also means its poll loops (`measure.attach`,
  // `align.attachSession`) run exactly while this page is open, which is the same
  // ref-counted lifecycle the card gave it — the difference is that opening this page is
  // now the deliberate act that used to be pressing "Align speakers".
  import { onMount } from 'svelte';
  import { routing } from '../../lib/routing';
  import { align } from '../../lib/align.svelte';
  import AlignDocs from './AlignDocs.svelte';
  import AlignWizard from './AlignWizard.svelte';

  interface Props {
    /** Leave the page — the wizard's own "Close", which it offers only when nothing is
     *  held and no run is in progress. Owned by App.svelte, which owns the tab. */
    onDone: () => void;
  }
  let { onDone }: Props = $props();

  // Long-form detail lives in the dialog, the same split every other page uses. It moved
  // here with the wizard: it explains the three modes and when alignment is the wrong
  // tool, which is this page's subject and no longer the Outputs page's.
  let docsOpen = $state(false);

  /** The adopted outputs, so a node name can be resolved to the user's own name for it
   *  even before the wizard's picker has been visited. Cheap and idempotent (the store's
   *  own note), and this page names speakers from its very first step — the "these are
   *  held right now" line and every refusal do. */
  onMount(() => void align.loadOutputs());

  /** Friendly name for a node name: the routing matrix's display name (which is where a
   *  rename lands) first, then the output's own name, then the raw node name rather than
   *  an empty space. Sources are included because a refusal or an interference report can
   *  name one. */
  function alignLabel(nodeName: string): string {
    const matrix = [...$routing.matrix.outputs, ...$routing.matrix.sources];
    return (
      matrix.find((n) => n.node_name === nodeName)?.display_name ??
      align.outputs.find((o) => o.node_name === nodeName)?.name ??
      nodeName
    );
  }
</script>

<div class="card info">
  <div class="card-head">
    <h2>Timing between speakers</h2>
    <div class="actions">
      <button
        class="ghost"
        type="button"
        title="Why speakers on one stream drift apart, what each of the three ways of aligning them promises, and when alignment is the wrong tool"
        onclick={() => (docsOpen = true)}
      >
        Explain speaker alignment
      </button>
    </div>
  </div>
  <p class="card-sub" style="margin-bottom:0">
    Speakers playing one stream should land together, but each adds its own delay on the way to the cone. Pick the
    speakers to align and the add-on takes them over for the run — whatever they are playing stops and comes back
    afterwards — then choose how to align them: <strong>measured with a phone</strong> from where you listen,
    <strong>measured by walking</strong> to each speaker in turn (which aligns the wiring, so it is right everywhere), or
    <strong>by ear</strong> when the microphone cannot be used. The two measured ways
    <strong>write nothing until you approve the proposal</strong>; by ear, each nudge goes straight to the speaker.
  </p>
</div>

<!-- No card wrapper: the wizard brings its own frame, and a frame inside a frame is what
     made this feel like a widget on the Outputs page. -->
<AlignWizard label={alignLabel} onClose={onDone} />

{#if docsOpen}
  <AlignDocs onClose={() => (docsOpen = false)} />
{/if}
