<script lang="ts">
  // Wizard page 2: the speakers this run will touch, and the level each one plays
  // the test tone at (plan §12.1, §12.2, §12.3.1).
  //
  // The counter-intuitive part, and the reason this page is worded the way it is:
  // **the selection is the whole run's scope, not the first position's subset.**
  // Forming the temporary exclusive group gives those speakers a source set nothing
  // else has, so every sendspin member reconnects when the hold forms *and* again
  // when it releases — tens of seconds each way (§12.3.1). A multi-position run that
  // re-picked speakers per position would pay that twice per position. So the run
  // forms **one** hold over everything it will ever touch, and scopes each position
  // by *audibility* instead, which is a live mute and therefore free.
  //
  // That makes the page's job as much explanatory as functional: a user who reads
  // this as "the speakers I can hear from here" has picked the wrong set, and will
  // only find out several minutes into a run. Hence the lead, the "whole run"
  // framing on the scope summary, and — once the hold exists — a selection that is
  // visibly *locked* rather than quietly ignored.
  //
  // The second half is §12.2's level phase: exactly one speaker audible at a time,
  // its own level, and a verdict from the daemon saying whether the estimator could
  // work with it. One speaker, not two: a shared click track means every member
  // emits both bursts, so an SNR measured with two of them playing cannot be
  // attributed to either.
  import AlignSignalVerdict from './AlignSignalVerdict.svelte';
  import { DEFAULT_MEASURE_LEVEL, align, levelControl, memberKindLabel } from '../lib/align.svelte';
  import { askConfirm } from '../lib/confirm.svelte';
  import { measure } from '../lib/measure.svelte';
  import { mic } from '../lib/mic.svelte';
  import type { MeasureMode, OutputInfo, SignalCheck } from '../lib/types';

  interface Props {
    /** The promise picked on page 1 — it travels with the session, so the hold the
     *  daemon forms is labelled with the same mode the run will make. */
    mode: MeasureMode;
    /** Multi-position, measured from several spots (plan §1.1). Changes nothing about
     *  what this page *does* — the scope is the whole run's either way, which is the
     *  point of §12.3.1 — only what starting it will do next. */
    chained: boolean;
    label: (nodeName: string) => string;
    /** Start measuring (the wizard owns the run). */
    onStart: () => void;
  }
  let { mode, chained, label, onStart }: Props = $props();

  $effect(() => {
    void align.loadOutputs();
  });

  const held = $derived(align.sessionActive);
  const selection = $derived(align.selection);
  const members = $derived(align.session?.members ?? []);
  const capturing = $derived(mic.phase === 'capturing');
  const soloed = $derived(align.soloed);

  /** Every adopted output is offerable — all three kinds are alignable now — but an
   *  offline one would simply never produce a tone, so it is shown and refused
   *  rather than silently included. */
  const offerable = $derived(align.outputs.filter((o) => o.present));
  const offline = $derived(align.outputs.filter((o) => !o.present));
  const chosen = $derived(new Set(selection));
  const pwsinkChosen = $derived(align.outputs.filter((o) => chosen.has(o.node_name) && o.kind === 'pwsink'));

  /** Which speakers in the scope have not yet been heard at an acceptable level.
   *  Advisory, never blocking: the daemon has its own level-learning phase (§7) and
   *  refuses with a reason if it cannot get there, so a UI veto here would only
   *  duplicate that judgement less well. Every kind is listed — a speaker whose level
   *  is set elsewhere can still be *judged* here, which is the point. */
  const unchecked = $derived(
    members.filter((m) => !acceptable(align.verdicts[m.node_name])).map((m) => m.node_name),
  );

  /** The estimator accepts anything at or above its minimum SNR, which includes
   *  `marginal` — so "would the estimator accept this?" is broader than "is it
   *  green". Both are shown; only this one gates anything. */
  function acceptable(v: SignalCheck['verdict'] | undefined): boolean {
    return v === 'good' || v === 'marginal';
  }

  function verdictClass(v: SignalCheck['verdict'] | undefined): string {
    if (v === 'good') return 'ok';
    if (v === 'marginal') return 'tight';
    return v ? 'bad' : '';
  }

  function verdictText(v: SignalCheck['verdict'] | undefined): string {
    if (v === 'good') return 'level good';
    if (v === 'marginal') return 'level tight';
    if (v === 'too_quiet') return 'too quiet';
    if (v === 'unusable') return 'unusable';
    return 'not checked yet';
  }

  // Record the verdict for whichever speaker is currently soloed. Only once the
  // window is the current speaker's own (`signalSettling` off), so a reading is never
  // filed against the speaker that was playing before it.
  $effect(() => {
    const node = align.soloed;
    const s = mic.signal;
    if (!node || !s || mic.signalSettling) return;
    align.recordVerdict(node, s.verdict);
  });

  // Leaving this page stops the tone — unless a run is under way, which solos members
  // itself and must not have the audibility pulled out from under it. `running`, not
  // `live`: a chain parked between positions has the set the user is about to measure
  // made audible, and silencing that because they stepped back a page would undo the
  // check they came here to make.
  $effect(() => () => {
    if (!measure.running) void align.stopTone();
  });

  async function formHold() {
    const n = selection.length;
    const ok = await askConfirm({
      title: n === 2 ? 'Take these 2 speakers for the alignment?' : `Take these ${n} speakers for the alignment?`,
      body: [
        'These speakers are grouped on their own for the whole run, so whatever they are playing now stops and comes back when you finish.',
        'Grouping them makes each one reconnect, which takes tens of seconds — and the same again when the run ends. That is why it happens once, for the whole run, instead of once per listening position.',
      ],
      confirmLabel: 'Take them',
    });
    if (ok) await align.startSelection(mode);
  }

  /** Change the scope after the hold exists. Deliberately expensive-looking: it
   *  releases the hold and re-forms it, i.e. two more reconnect waves. */
  async function changeScope() {
    const ok = await askConfirm({
      title: 'Change which speakers this run covers?',
      body: [
        'The group has to be released and formed again, so every speaker reconnects twice more — tens of seconds each time.',
        'Levels you have already set are remembered; the readings that confirmed them are not, so they need checking again.',
      ],
      confirmLabel: 'Change the selection',
      danger: true,
    });
    if (ok) await align.stop();
  }

  function toggle(o: OutputInfo) {
    align.toggleSelected(o.node_name);
  }

  async function tap(nodeName: string) {
    if (soloed === nodeName) await align.stopTone();
    else await align.solo(nodeName);
  }

  /** How many speakers the run covers: the held members once a hold exists, the
   *  pending selection before that. Not the same thing as `selection.length` — a
   *  session can also have been started from the by-ear panel, and then the hold is
   *  real while this browser never picked anything. */
  const scopeCount = $derived(held ? members.length : selection.length);

  const why = $derived(
    scopeCount < 2
      ? 'Measuring compares speakers against each other, so it needs at least two.'
      : !held
        ? 'The speakers have to be taken for the alignment first — that is what puts them on one clock, which is what makes their arrivals comparable.'
        : !capturing
          ? 'Start the microphone above — the daemon refuses to measure without a capture, because it has nothing to listen to.'
          : null,
  );
</script>

<!-- The framing, before anything is clickable. Everything below reinforces it. -->
<div class="scope-lead">
  {#if !held}
    <strong>Pick every speaker this alignment will touch</strong>
    <p>
      Not just the ones you can hear from where you are standing — the whole set, usually all of them or one floor. They
      are grouped together once, for the entire run, because grouping them makes each speaker reconnect and that costs
      tens of seconds every time.
    </p>
    {#if mode === 'sweet_spot'}
      <p class="hint">
        When measuring from more than one position, you will not choose speakers again: you say which of <em>these</em>
        you can hear from each spot, and that is a mute, which is instant.
      </p>
    {/if}
  {:else}
    <strong>These speakers are held for the whole run</strong>
    <p>
      Nothing else plays on them until you stop, and the set does not change again — each listening position works from
      what it can hear of <em>this</em> group, which costs nothing. Now set each one's level.
    </p>
  {/if}
</div>

{#if !held}
  <!-- ── Choosing the scope ─────────────────────────────────────────────────── -->
  <div class="pick-head">
    <span class="lbl">Speakers in this run</span>
    <span class="count" class:enough={selection.length >= 2}>
      {selection.length} of {offerable.length} selected
    </span>
    <span class="spacer"></span>
    <button class="ghost" onclick={() => align.setSelection(offerable.map((o) => o.node_name))}>Select all</button>
    <button class="ghost" disabled={!selection.length} onclick={() => align.clearSelection()}>Clear</button>
  </div>

  {#if !align.outputsLoaded}
    <p class="hint">Loading the speakers you have added…</p>
  {:else if offerable.length === 0}
    <p class="hint blocked">
      None of your added speakers is on the network right now. Alignment streams a click to them, so there is nothing to
      measure until at least two are back.
    </p>
  {/if}

  <div class="grid">
    {#each offerable as o (o.node_name)}
      {@const on = chosen.has(o.node_name)}
      <label class="pick" class:on>
        <input type="checkbox" checked={on} onchange={() => toggle(o)} />
        <span class="body">
          <span class="name">{label(o.node_name)}</span>
          <span class="kind">{memberKindLabel(o.kind)}</span>
          {#if levelControl(o.kind) === 'receiver'}
            <!-- Honest at the point of choosing, not discovered at the point of
                 turning: this one's level is set on the device. -->
            <span class="note">level is set on the receiver</span>
          {:else if levelControl(o.kind) === 'none'}
            <span class="note">no level or mute control from here</span>
          {/if}
        </span>
      </label>
    {/each}
  </div>

  {#if offline.length}
    <div class="offline">
      <span class="lbl">Not on the network</span>
      <span class="chips">
        {#each offline as o (o.node_name)}
          <span class="spk">{label(o.node_name)}<span class="kind">{memberKindLabel(o.kind)}</span></span>
        {/each}
      </span>
      <p class="hint">
        These cannot be included: the click would be sent to them and never arrive, and the run would spend its whole
        budget waiting for a signal that is not coming.
      </p>
    </div>
  {/if}

  {#if pwsinkChosen.length}
    <p class="hint caution">
      {pwsinkChosen.map((o) => label(o.node_name)).join(', ')}
      {pwsinkChosen.length === 1 ? 'is a PipeWire host' : 'are PipeWire hosts'}: the add-on has no level or mute control
      over {pwsinkChosen.length === 1 ? 'it' : 'them'}, so {pwsinkChosen.length === 1 ? 'it keeps' : 'they keep'} playing
      the click while other speakers are being measured, and the others have to be audible over
      {pwsinkChosen.length === 1 ? 'it' : 'them'}. Set the volume on that machine before starting.
    </p>
  {/if}

  <!-- Only the blocker that belongs to *this* step. "Take the speakers first" is what
       the button below does, so stating it as an obstacle would be noise. -->
  {#if scopeCount < 2 && why}<p class="hint blocked">{why}</p>{/if}

  <div class="go">
    <button class="primary" disabled={selection.length < 2 || align.busy} onclick={() => void formHold()}>
      {#if align.busy}
        Grouping the speakers…
      {:else if selection.length >= 2}
        Take these {selection.length} speakers
      {:else}
        Take the selected speakers
      {/if}
    </button>
    <span class="hint">
      This mutes everything else on them and loops a two-tone click through them off one clock — that shared clock is what
      makes the arrivals comparable. Expect the first tone tens of seconds after you press it.
    </span>
  </div>
{:else}
  <!-- ── The scope is fixed; now the levels ─────────────────────────────────── -->
  <div class="scope">
    <div class="scope-head">
      <span class="lbl on">This run covers</span>
      <span class="chips">
        {#each members as m (m.node_name)}
          <span class="spk">{label(m.node_name)}<span class="kind">{memberKindLabel(m.kind)}</span></span>
        {/each}
      </span>
      <button class="ghost" disabled={align.busy} onclick={() => void changeScope()}>Change the selection</button>
    </div>
    <p class="hint">
      Held for the whole run — every position, every pass. Changing it means releasing the group and forming it again, so
      it is a restart rather than a click.
    </p>
    {#if align.displaced.length}
      <!-- The hold is exclusive, so something else stopped playing. Said plainly
           rather than discovered as silence somewhere else in the house. -->
      <p class="hint">
        {align.displaced.length} route{align.displaced.length === 1 ? '' : 's'} to these speakers
        {align.displaced.length === 1 ? 'is' : 'are'} paused while this runs: {align.displaced
          .map((d) => `${label(d.source)} → ${label(d.output)}`)
          .join(', ')}. Finishing puts them back.
      </p>
    {/if}
  </div>

  <div class="level-head">
    <span class="lbl">Levels</span>
    <span class="hint">
      One speaker at a time. Tap one to hear it; tap it again to silence it. Set it loud enough for the microphone and no
      louder — {DEFAULT_MEASURE_LEVEL}% is usually plenty.
    </span>
  </div>

  <ul class="levels">
    {#each members as m (m.node_name)}
      {@const on = soloed === m.node_name}
      {@const v = align.verdicts[m.node_name]}
      <li class:on>
        <div class="row">
          <button class="tap" class:on onclick={() => void tap(m.node_name)} disabled={align.busy}>
            {on ? 'Playing — tap to stop' : 'Play tone'}
          </button>
          <span class="name">{label(m.node_name)}</span>
          <span class="kind">{memberKindLabel(m.kind)}</span>
          <!-- The short form on every row; the full explanation only on the row being
               worked on, so five speakers do not produce five paragraphs of amber. -->
          {#if levelControl(m.kind) === 'receiver'}
            <span class="note">level on the receiver</span>
          {:else if levelControl(m.kind) === 'none'}
            <span class="note">no level or mute here</span>
          {/if}
          <span class="spacer"></span>
          <span class="verdict-pill {verdictClass(v)}" title="What the daemon last said about this speaker's level">
            {verdictText(v)}
          </span>
        </div>

        {#if levelControl(m.kind) === 'session'}
          <div class="knob" class:dim={!on}>
            <input
              type="range"
              min="0"
              max="100"
              step="5"
              value={align.levelOf(m.node_name)}
              disabled={!on || align.busy}
              oninput={(e) => align.previewSoloLevel(Number((e.currentTarget as HTMLInputElement).value))}
              onchange={(e) => void align.setSoloLevel(Number((e.currentTarget as HTMLInputElement).value))}
            />
            <span class="pct">{align.levelOf(m.node_name)}%</span>
          </div>
        {:else if on}
          <!-- No slider where there is no knob: a disabled one reads as "broken", and
               a working-looking one that changes nothing is worse than either. -->
          <p class="hint caution">
            {#if levelControl(m.kind) === 'receiver'}
              The level is the receiver's own. Alignment mutes and unmutes AirPlay 2 speakers but never sets their volume
              — it only snapshots the mute, so a level written from here could not be put back afterwards. Set it on the
              receiver (or with its volume control on the Outputs page); the verdict below still tells you whether it is
              loud enough.
            {:else}
              No level control from here — this is a PipeWire host and its volume belongs to that machine. It also cannot
              be muted from here, so it keeps playing the click while the others are measured: the others have to be
              loud enough to be heard over it.
            {/if}
          </p>
        {/if}

        {#if on}
          <AlignSignalVerdict signal={mic.signal} settling={mic.signalSettling} subject={label(m.node_name)} />
        {/if}
      </li>
    {/each}
  </ul>

  {#if soloed}
    <button class="ghost" disabled={align.busy} onclick={() => void align.stopTone()}>Silence all speakers</button>
  {/if}

  {#if why}<p class="hint blocked">{why}</p>{/if}

  {#if !why && unchecked.length}
    <!-- Advisory. The daemon learns levels itself and refuses with a reason if it
         cannot, so this is "you have not looked at these", not "you may not start". -->
    <p class="hint caution">
      Not heard at a usable level yet: {unchecked.map(label).join(', ')}. You can start anyway — the run sets levels for
      itself and says so if one cannot be made loud enough — but checking here is quicker than finding out four minutes
      in.
    </p>
  {/if}

  <div class="go">
    <button
      class="primary"
      disabled={!!why || align.busy}
      title={why ?? (chained ? 'Start the chain and ask for the first listening position' : 'Measure every speaker and propose delays')}
      onclick={onStart}
    >
      {chained ? 'Start the first position' : 'Start alignment'}
    </button>
    <span class="hint">
      {#if chained}
        Nothing is measured until you say which of these speakers you can hear from where you are standing — that is the
        first position. Each position takes about eleven seconds per speaker per pass, and the delays it works out are
        held inside the add-on until the whole chain is finished, so no speaker reconnects in between.
      {:else}
        Expect about four minutes for five speakers: each speaker is measured twice, and every switch waits for the tone
        to settle before anything is accepted. Nothing is written until you approve the proposal.
      {/if}
    </span>
  </div>
{/if}

<style>
  .scope-lead {
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid color-mix(in srgb, var(--primary-color) 40%, transparent);
    background: color-mix(in srgb, var(--primary-color) 7%, transparent);
    font-size: 0.84rem;
  }
  .scope-lead p {
    margin: 4px 0 0;
    color: var(--secondary-text-color);
    font-size: 0.82rem;
  }
  .lbl {
    font-size: 0.72rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--secondary-text-color);
  }
  .lbl.on {
    color: var(--primary-color);
  }
  .pick-head,
  .level-head {
    display: flex;
    align-items: baseline;
    gap: 8px;
    flex-wrap: wrap;
    margin: 14px 0 8px;
  }
  .pick-head button {
    padding: 3px 9px;
    font-size: 0.78rem;
  }
  .spacer {
    flex: 1 1 auto;
  }
  .count {
    font-size: 0.8rem;
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
  .count.enough {
    color: var(--primary-text-color);
  }
  /* A grid of peers: the point is that this is one set, chosen once. */
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(210px, 1fr));
    gap: 8px;
  }
  .pick {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    margin: 0;
    padding: 8px 10px;
    border: 1px solid var(--divider-color);
    border-radius: 8px;
    cursor: pointer;
    font-size: 0.86rem;
  }
  .pick.on {
    border-color: var(--primary-color);
    background: color-mix(in srgb, var(--primary-color) 8%, transparent);
  }
  .pick input {
    margin-top: 2px;
    flex: 0 0 auto;
  }
  .pick .body {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-width: 0;
  }
  .name {
    overflow-wrap: anywhere;
  }
  .kind,
  .note {
    font-size: 0.74rem;
    color: var(--secondary-text-color);
  }
  .note {
    color: var(--warning-color, #b26a00);
  }
  .offline {
    margin-top: 12px;
    padding-top: 10px;
    border-top: 1px dashed var(--divider-color);
  }
  .chips {
    display: inline-flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .spk {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 0.8rem;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--divider-color);
    background: var(--input-fill-color);
  }
  .scope {
    margin: 14px 0 4px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: color-mix(in srgb, var(--secondary-text-color) 5%, transparent);
  }
  .scope-head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .scope-head button {
    padding: 3px 9px;
    font-size: 0.78rem;
  }
  .levels {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 8px;
  }
  .levels li {
    padding: 8px 10px;
    border: 1px solid var(--divider-color);
    border-radius: 8px;
  }
  /* The soloed speaker is the only one making a sound, so it is the only one that
     looks active — the highlight is the answer to "which one am I hearing?". */
  .levels li.on {
    border-color: var(--primary-color);
    background: color-mix(in srgb, var(--primary-color) 7%, transparent);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 0.86rem;
  }
  .tap {
    flex: 0 0 auto;
    padding: 3px 10px;
    font-size: 0.78rem;
  }
  .tap.on {
    border-color: var(--primary-color);
    color: var(--primary-color);
  }
  .verdict-pill {
    font-size: 0.74rem;
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid var(--divider-color);
    color: var(--secondary-text-color);
  }
  .verdict-pill.ok {
    border-color: color-mix(in srgb, var(--success-color, #43a047) 55%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 12%, transparent);
    color: var(--primary-text-color);
  }
  .verdict-pill.tight {
    border-color: color-mix(in srgb, var(--warning-color, #ffa600) 55%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 14%, transparent);
    color: var(--primary-text-color);
  }
  .verdict-pill.bad {
    border-color: color-mix(in srgb, var(--error-color, #db4437) 55%, transparent);
    background: color-mix(in srgb, var(--error-color, #db4437) 12%, transparent);
    color: var(--primary-text-color);
  }
  .knob {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 6px;
    max-width: 380px;
  }
  .knob.dim {
    opacity: 0.5;
  }
  .knob input[type='range'] {
    flex: 1;
    min-width: 120px;
  }
  .pct {
    min-width: 46px;
    text-align: right;
    font-variant-numeric: tabular-nums;
    font-size: 0.82rem;
  }
  .hint {
    display: block;
    margin: 6px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .blocked {
    color: var(--warning-color, #b26a00);
  }
  .caution {
    color: var(--warning-color, #b26a00);
  }
  .go {
    margin-top: 14px;
  }
</style>
