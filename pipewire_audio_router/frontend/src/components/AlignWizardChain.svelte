<script lang="ts">
  // The chaining body of the run page: one listening position at a time
  // (plan §1.1, §1.1.1, §1.1.4, §12.3.1).
  //
  // What the user is doing here, and why the page is shaped this way:
  //
  //   * **Picking a position is free; picking the scope was not.** The speakers were
  //     chosen once, on the Speakers page, and forming that hold is what cost a reconnect
  //     wave (§12.3.1). A position is a *subset of that hold*, selected by muting, so
  //     choosing one costs nothing and no speaker reconnects. This page therefore offers
  //     the held speakers and never a picker that could change the hold — which is also
  //     what keeps a five-position run at two reconnect waves instead of ten.
  //   * **Two overlaps, not one.** Every position after the first has to name speakers it
  //     shares with the already-aligned set. Two of them have a *known* relationship from
  //     the previous position, so measuring both is an independent check on this joint;
  //     one is accepted (a user may genuinely share only one speaker) but is the chain's
  //     weakest point, because that single reading is applied as a common shift to
  //     everything aligned so far and anchors everything after it. So two are
  //     pre-selected, one is allowed, and the penalty is stated *before* posting rather
  //     than discovered in the result.
  //   * **Nothing has been written.** Every delay in this run lives in the daemon's
  //     per-device delay line: the speakers already sound aligned while no speaker's own
  //     setting has been touched and nothing is persisted (§1.1.1). Stopping discards it;
  //     `finish` is the single write. That has to be legible, or a user reads a
  //     mid-chain state as a finished one.
  //   * **A refused position is not a refused run.** The chain parks, the aligned set
  //     keeps its delays, and the same position can be posted again (§1.1.4 item 3).
  //
  // Audibility is offered rather than driven: W12 deliberately does not move the mutes
  // between positions, so "hear this position" is the user's own check that they picked
  // the set they can actually hear — and it is a live mute, so it is free.
  import AlignChainSteps from './AlignChainSteps.svelte';
  import AlignRefusal from './AlignRefusal.svelte';
  import { ONE_OVERLAP_PENALTY, chainActionLabel } from '../lib/measure.svelte';
  import type { ChainProgress } from '../lib/types';

  interface Props {
    chain: ChainProgress;
    label: (nodeName: string) => string;
    busy: boolean;
    /** Post this position: the speakers to align here, and the overlaps linking it. */
    onPost: (members: string[], overlaps: string[]) => void;
    /** Renormalise the chain and propose the one write. */
    onFinish: () => void;
    /** Make exactly these audible (empty = silence everything). A live mute. */
    onHear: (nodeNames: string[]) => void;
  }
  let { chain, label, busy, onPost, onFinish, onHear }: Props = $props();

  let picked = $state<string[]>([]);
  let overlaps = $state<string[]>([]);

  /** Effect bookkeeping, deliberately not `$state`: nothing renders it, and as reactive
   *  state it would re-run the effect that writes it. Same pattern as the wizard's
   *  phase follower. */
  let seededForStep = -1;

  // A new position starts from a clean selection, with the overlaps pre-selected — the
  // two most recently aligned speakers, because those were being measured at the
  // position the user has just walked away from and are the likeliest to still be
  // audible. A guess the user can change, not a decision: which speakers carry into the
  // next room is something only they can know.
  $effect(() => {
    const n = chain.steps.length;
    if (seededForStep === n) return;
    seededForStep = n;
    picked = [];
    overlaps = chain.aligned.slice(-2);
  });

  const accepting = $derived(chain.next === 'position' || chain.next === 'finish');
  const isFirst = $derived(chain.aligned.length === 0);
  const provisionalOrder = $derived(
    [...chain.provisional].sort((a, b) => chain.aligned.indexOf(a.node_name) - chain.aligned.indexOf(b.node_name)),
  );

  /** Why this position cannot be posted yet, in the user's terms. The daemon validates
   *  the same rules and is the authority (it refuses with a sentence naming the speaker);
   *  this only saves the round trip and explains the *shape* of a position. */
  const why = $derived(
    picked.length === 0
      ? 'Tick the speakers you can hear from where you are standing now.'
      : picked.length + overlaps.length < 2
        ? 'A position needs at least two speakers: one to align, and something to align it to.'
        : !isFirst && overlaps.length === 0
          ? 'Pick at least one already-aligned speaker you can also hear from here — without one, this position and the rest of the house would each be aligned internally and mean nothing to each other.'
          : null,
  );

  function toggle(list: string[], node: string): string[] {
    return list.includes(node) ? list.filter((n) => n !== node) : [...list, node];
  }
</script>

<div class="chain">
  <!-- Where the chain is, in one line, followed by the daemon's own prompt. -->
  <div class="head">
    <strong>{chainActionLabel(chain.next)}</strong>
    {#if chain.measuring}
      <span class="badge on">measuring position {chain.measuring}</span>
    {:else if accepting}
      <span class="badge">next: position {chain.steps.length + 1}</span>
    {/if}
    <span class="badge" title="Speakers aligned at some position so far, of the whole held scope">
      {chain.aligned.length} of {chain.aligned.length + chain.remaining.length} speakers aligned
    </span>
  </div>
  <p class="prompt">{chain.prompt}</p>

  {#if chain.restarts > 0}
    <p class="caution">
      The microphone capture restarted {chain.restarts}× while this position was being measured, so this position's
      readings were thrown away and it is being measured again. Everything aligned at earlier positions is untouched:
      what carries a chain forward is the delay each speaker holds, not the capture.
    </p>
  {/if}

  <!-- Provisional state, stated before any numbers so it frames all of them. This is the
       single most misreadable thing about a chain: the speakers *sound* aligned.
       Dropped once the chain is over, because from `finish` onwards the run is proposing
       (or has written) real settings and this banner would then be a lie. -->
  {#if chain.next !== 'done'}
    <div class="prov">
      <div class="prov-head">
        <strong>Nothing has been written yet</strong>
        <span class="badge warn">provisional</span>
      </div>
      <p>
        These delays are being applied inside the add-on, in the stream on its way to each speaker — so the speakers
        already sound aligned, while <strong>no speaker's own setting has been touched</strong> and nothing has been
        stored. <strong>Stop and restore</strong> throws all of it away and every speaker goes back to what it was.
        <strong>Finish</strong> is the one write, and the only point at which speakers reconnect.
      </p>
      {#if provisionalOrder.length}
        <table>
          <thead>
            <tr><th>Speaker</th><th class="num">Held back by</th><th class="num">Applied</th></tr>
          </thead>
          <tbody>
            {#each provisionalOrder as p (p.node_name)}
              <tr>
                <td>{label(p.node_name)}</td>
                <td class="num" title="What the chain's arithmetic holds for this speaker">{p.delay_ms.toFixed(2)} ms</td>
                <!-- The applied value is the truth: the line and the final knobs are both
                     whole milliseconds, and every later position measures its overlaps
                     *through* this, so what was applied is observed rather than assumed. -->
                <td class="num strong" title="What was actually pushed to the delay line — whole milliseconds, like the final write">
                  {p.applied_ms} ms
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
        <p class="hint">
          The lowest of these is {chain.floor_ms.toFixed(2)} ms. Every position can only ever <em>add</em> delay, so this
          floor creeps upward across a house; finishing subtracts it from everyone at once, which moves nothing relative
          to anything else and gives the latency back.
        </p>
      {/if}
    </div>
  {/if}

  {#if chain.refusal}
    <!-- A step, not the run. Said before the refusal itself, because the refusal's own
         sentence is about this position and the reassurance is about everything else. -->
    <div class="parked">
      <strong>This position was not accepted — the chain is still going.</strong>
      <span>
        The {chain.aligned.length} speaker{chain.aligned.length === 1 ? '' : 's'} aligned so far
        {chain.aligned.length === 1 ? 'keeps its' : 'keep their'} delays and nothing has been undone. Stand where you
        are, change the overlaps or move the phone away from a wall, and post this position again.
      </span>
    </div>
    <AlignRefusal refusal={chain.refusal} {label} />
  {/if}

  {#if accepting}
    <!-- ── Picking this position ───────────────────────────────────────────────── -->
    <div class="pick">
      <!-- The §12.3.1 framing the Speakers page set up, carried through: this is a subset
           of a set that was chosen once, and picking from it is a mute. -->
      <p class="hint lead">
        Everything below is one of the speakers this run is already holding — the set does not change again. Choosing which
        of them this position covers only moves mutes, so it is instant and no speaker reconnects.
      </p>
      <div class="pick-head">
        <span class="lbl">Speakers I can hear from here</span>
        <span class="count">{picked.length} picked</span>
      </div>
      {#if chain.remaining.length}
        <div class="grid">
          {#each chain.remaining as n (n)}
            {@const on = picked.includes(n)}
            <label class="opt" class:on>
              <input type="checkbox" checked={on} disabled={busy} onchange={() => (picked = toggle(picked, n))} />
              <span>{label(n)}</span>
            </label>
          {/each}
        </div>
      {:else}
        <p class="hint">
          Every held speaker has been aligned at some position. You can still measure another position — a region linked
          through more overlaps is better tied down — but nothing has to be.
        </p>
      {/if}

      {#if !isFirst}
        <div class="pick-head">
          <span class="lbl">Already aligned, and still audible from here</span>
          <span class="count" class:enough={overlaps.length >= 2} class:thin={overlaps.length === 1}>
            {overlaps.length} overlap{overlaps.length === 1 ? '' : 's'}
          </span>
        </div>
        <p class="hint">
          These are what tie this position to the rest of the house. <strong>Two is what you want:</strong> their
          relationship is already known from the position they were aligned at, so measuring both here checks this join
          against itself. Pre-selected for you are the last speakers that were aligned — change them if those are not
          the ones you can hear.
        </p>
        <div class="grid">
          {#each chain.aligned as n (n)}
            {@const on = overlaps.includes(n)}
            <label class="opt ov" class:on>
              <input type="checkbox" checked={on} disabled={busy} onchange={() => (overlaps = toggle(overlaps, n))} />
              <span>{label(n)}</span>
            </label>
          {/each}
        </div>
        {#if overlaps.length === 1}
          <!-- Before the fact, not after: the confidence penalty is a reason to look for
               a second speaker while the user is still standing there. -->
          <p class="caution">{ONE_OVERLAP_PENALTY}</p>
        {:else if overlaps.length > 2}
          <p class="hint">
            More than two is fine — every extra one is another check on this join — but each one is another speaker to
            measure, so it costs about eleven seconds a pass.
          </p>
        {/if}
      {:else}
        <p class="hint">
          This is the first position, so there is nothing to overlap with yet: the speakers you pick here become the
          reference the whole chain is built on. Pick the ones you can hear <em>clearly</em> — everything measured later
          is tied back to this.
        </p>
      {/if}

      <div class="listen">
        <button
          class="ghost"
          disabled={busy || picked.length + overlaps.length === 0}
          title="Play the click on exactly this set, muting everything else — a live mute, so it costs nothing and no speaker reconnects"
          onclick={() => onHear([...picked, ...overlaps])}
        >
          Play these {picked.length + overlaps.length} speakers
        </button>
        <button class="ghost" disabled={busy} title="Mute every speaker in the run" onclick={() => onHear([])}>
          Silence
        </button>
        <span class="hint">
          Worth doing before posting: a speaker you cannot really hear from here will spend the position's whole budget
          waiting for a signal, and an overlap you cannot hear cannot link anything.
        </span>
      </div>

      {#if why}<p class="caution">{why}</p>{/if}

      <div class="go">
        <button class="primary" disabled={!!why || busy} onclick={() => onPost(picked, overlaps)}>
          Measure this position
        </button>
        {#if chain.next === 'finish'}
          <button
            class="primary"
            disabled={busy}
            title="Take the accumulated delay back out, then propose the one write for every speaker"
            onclick={onFinish}
          >
            Finish and propose the write
          </button>
        {:else}
          <span class="hint">
            {chain.remaining.length} speaker{chain.remaining.length === 1 ? '' : 's'} still
            {chain.remaining.length === 1 ? 'has' : 'have'} no reading anywhere ({chain.remaining.map(label).join(', ')}),
            and finishing needs all of them: a speaker with nothing measured has nothing to write.
          </span>
        {/if}
      </div>
      {#if chain.next === 'finish'}
        <p class="hint">
          Finishing writes each speaker's own timing setting once, which is when they reconnect — one wave for the whole
          run instead of one per position. Nothing is written before you approve the proposal it produces.
        </p>
      {/if}
    </div>
  {:else if chain.next === 'busy'}
    <p class="hint">
      Stay where you are and keep the phone still until this position is done. Each speaker at this position is measured
      twice, about eleven seconds a pass.
    </p>
  {/if}

  {#if chain.steps.length}
    <h4>Positions measured so far</h4>
    <AlignChainSteps steps={chain.steps} error={chain.error} scopeNote={chain.scope_note} {label} expandLast />
  {/if}
</div>

<style>
  .chain {
    margin: 12px 0;
    padding: 10px 12px;
    border: 1px solid color-mix(in srgb, var(--primary-color) 40%, transparent);
    border-radius: 8px;
    background: color-mix(in srgb, var(--primary-color) 5%, transparent);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 0.88rem;
  }
  .prompt {
    margin: 6px 0 0;
    font-size: 0.84rem;
  }
  /* The provisional banner is not a warning — nothing is wrong — but it must not read as
     a result either, so it gets its own dashed frame. */
  .prov {
    margin-top: 10px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px dashed color-mix(in srgb, var(--warning-color, #ffa600) 65%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 8%, transparent);
    font-size: 0.84rem;
  }
  .prov-head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .prov p {
    margin: 5px 0 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .prov p strong {
    color: var(--primary-text-color);
  }
  .prov table {
    margin-top: 8px;
  }
  .parked {
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin-top: 10px;
    font-size: 0.84rem;
  }
  .parked span {
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .pick {
    margin-top: 12px;
  }
  .pick-head {
    display: flex;
    align-items: baseline;
    gap: 8px;
    flex-wrap: wrap;
    margin: 12px 0 6px;
  }
  .lbl {
    font-size: 0.72rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--secondary-text-color);
  }
  .count {
    font-size: 0.8rem;
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
  .count.enough {
    color: var(--primary-text-color);
  }
  .count.thin {
    color: var(--warning-color, #b26a00);
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
    gap: 6px;
  }
  .opt {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0;
    padding: 6px 9px;
    border: 1px solid var(--divider-color);
    border-radius: 8px;
    cursor: pointer;
    font-size: 0.84rem;
  }
  .opt.on {
    border-color: var(--primary-color);
    background: color-mix(in srgb, var(--primary-color) 8%, transparent);
  }
  /* Overlaps are a different kind of choice from "what I can hear": dashed, like the
     overlap chips in the step list. */
  .opt.ov {
    border-style: dashed;
  }
  .listen {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 10px;
  }
  .listen button {
    padding: 3px 10px;
    font-size: 0.8rem;
  }
  .go {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 12px;
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
  .hint {
    display: block;
    margin: 6px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .hint strong {
    color: var(--primary-text-color);
  }
  .hint.lead {
    margin-top: 0;
  }
  .caution {
    margin: 8px 0 0;
    font-size: 0.78rem;
    color: var(--warning-color, #b26a00);
  }
</style>
