<script lang="ts">
  // Wizard page 2: what "aligned" is going to mean (plan §1 and §12.1).
  //
  // All three of plan §1's modes are here. They are not variations on a setting — they
  // make *different promises* about where the speakers end up aligned — so each option
  // says what it aligns and what it costs rather than only what it does.
  //
  // It is page 2 rather than page 1 because the microphone step in front of it decides
  // which of the three can be *kept*: two are measurements, and a browser with no usable
  // capture cannot make them however clearly they are described.
  //
  // Two things this page used to get wrong, both fixed by the modes actually existing:
  //
  //   * **near field is built** (W8a). The daemon parks the run in a walk and takes one
  //     arrival per speaker, then a closure reading. A disabled option saying "not built
  //     yet" was describing the plan, not the code;
  //   * **manual is a mode, not a leftover.** It used to point at by-ear sliders that sat
  //     under this wizard on a source card; those moved with the wizard (§12.1), so
  //     "already below" pointed at nothing. It is the documented fallback for a mic that
  //     cannot be used (§4.1) or an estimator that refuses (§5.5), and it needs no
  //     microphone — but it does need a group, so it goes through the same selection and
  //     the same temporary hold (§12.3.1) as the other two.
  //
  // **The choice is reduced by the microphone step before it** (§4.1, §4.2). Where there
  // is no usable capture the two measured modes are disabled *with the reason attached*
  // and Manual is left — never hidden, because a user who cannot see that Near field
  // exists cannot find out what would make it work. That is the same disabled-with-reason
  // shape the old "not built yet" option used; only its content was wrong.
  import type { WizardMode } from '../../lib/align.svelte';

  interface Props {
    mode: WizardMode;
    /** Multi-position only: measure from several listening spots and join them through
     *  overlap speakers (plan §1.1). A sub-choice of the mode rather than a fourth mode,
     *  because the promise is the same one — "aligned at the spot it was measured from" —
     *  made once per position instead of once. */
    chained: boolean;
    /** Why the two measured modes cannot be chosen right now, or null when they can
     *  (`measuredBlock`, from the microphone step). Rendered inside each disabled option:
     *  a dimmed radio with no reason is indistinguishable from a broken one. */
    measuredBlock?: string | null;
    /** §4.2's read-back caveat, when the capture works but the browser would not say
     *  whether it honoured the constraints. Shown *before* the measured modes are picked,
     *  because it qualifies what they can promise. */
    micCaveat?: string | null;
    onPick: (mode: WizardMode) => void;
    onChain: (chained: boolean) => void;
  }
  let { mode, chained, measuredBlock = null, micCaveat = null, onPick, onChain }: Props = $props();

  const blocked = $derived(!!measuredBlock);
</script>

<p class="lead">
  A single microphone in a single place measures the electrical delay <em>and</em> the sound's travel time together, and
  cannot separate them — a metre of extra distance is about 3 ms. So the first thing to choose is what should end up
  aligned.
</p>

{#if blocked}
  <!-- Said once, above the options, and then again inside each disabled one: this is why
       two of the three are not selectable, and it is not the user having done anything
       wrong. -->
  <p class="reduced">
    <strong>Only Manual is available.</strong> {measuredBlock} Go back to <strong>Microphone</strong> to see what would
    change that.
  </p>
{:else if micCaveat}
  <p class="caveat"><strong>Measuring is available, with a caveat.</strong> {micCaveat}</p>
{/if}

<div class="modes">
  <label class="mode" class:on={mode === 'sweet_spot'} class:off={blocked}>
    <input
      type="radio"
      name="align-mode"
      value="sweet_spot"
      checked={mode === 'sweet_spot'}
      disabled={blocked}
      onchange={() => onPick('sweet_spot')}
    />
    <div class="body">
      <div class="title">
        Multi-position <span class="badge">default · you stay put</span>
        {#if blocked}<span class="badge caution">unavailable — needs the microphone</span>{/if}
      </div>
      {#if blocked}<p class="why">{measuredBlock}</p>{/if}
      <p>
        Aligns the speakers you can hear from where you are standing, at <em>that</em> spot. Stand where you normally
        listen and hold the phone still.
      </p>
      <!-- The chain choice lives inside the mode it belongs to, so it cannot be read as
           a fourth promise. Only offered when the mode is selected: a disabled control
           under an unselected radio reads as "not available". -->
      {#if mode === 'sweet_spot'}
        <div class="sub-choice">
          <label class="opt" class:on={!chained}>
            <input type="radio" name="align-chain" checked={!chained} onchange={() => onChain(false)} />
            <span>
              <strong>One position</strong>
              <span class="hint">
                Every speaker is measured from where the phone is sitting. Right for one room, and for speakers that are
                all audible from one seat.
              </span>
            </span>
          </label>
          <label class="opt" class:on={chained}>
            <input type="radio" name="align-chain" checked={chained} onchange={() => onChain(true)} />
            <span>
              <strong>Several positions, joined up</strong>
              <span class="hint">
                For a house where no single spot hears everything: align what you can hear, walk to the next room, and
                name one or two speakers you can hear from <em>both</em> — those links are what tie the rooms together.
                Each room ends up aligned at its own spot, so the transition between two rooms is approximate; nothing
                is written until the last position is done.
              </span>
            </span>
          </label>
        </div>
      {/if}
    </div>
  </label>

  <label class="mode" class:on={mode === 'near_field'} class:off={blocked}>
    <input
      type="radio"
      name="align-mode"
      value="near_field"
      checked={mode === 'near_field'}
      disabled={blocked}
      onchange={() => onPick('near_field')}
    />
    <div class="body">
      <div class="title">
        Near field <span class="badge">you walk to every speaker</span>
        {#if blocked}<span class="badge caution">unavailable — needs the microphone</span>{/if}
      </div>
      {#if blocked}<p class="why">{measuredBlock}</p>{/if}
      <p>
        You walk to each speaker in turn and hold the phone <em>at</em> it. That takes the room out of the measurement, so
        what gets aligned is the <em>wiring</em> — and a wire alignment is right everywhere in the house rather than at one
        seat. Choose this for whole-house coherence: correct while walking around, not at N specific spots.
      </p>
      <p class="hint">
        Costs a walk, and depends on you holding the phone <strong>within a hand's width of each speaker</strong>: held a
        metre away it reads as that speaker being 3 ms late, and nothing in the measurement can tell the difference. The
        last stop is a <strong>revisit</strong> of the speaker you started at, which is what separates the phone's clock
        drift over a long walk from real offsets. Keep the microphone running the whole way — reopening it starts the walk
        again.
      </p>
    </div>
  </label>

  <label class="mode" class:on={mode === 'manual'}>
    <input type="radio" name="align-mode" value="manual" checked={mode === 'manual'} onchange={() => onPick('manual')} />
    <div class="body">
      <div class="title">
        Manual <span class="badge">by ear · no microphone</span>
        {#if blocked}<span class="badge on">the one that works here</span>{/if}
      </div>
      <p>
        You judge it: the reference speaker and the one you are tuning play together, and you nudge the tuned one until
        its clicks sit exactly on the reference's. Aligned wherever you happen to be standing, to whatever your ears
        accept.
      </p>
      <p class="hint">
        The fallback when the microphone cannot be used at all — no HTTPS, permission denied, no working input — or when
        the estimator refuses to answer. It still needs a <strong>group</strong>, so you pick the speakers on the next
        page exactly as the measured modes do; only the microphone step is skipped.
      </p>
    </div>
  </label>
</div>

<style>
  .lead {
    margin: 0 0 12px;
    font-size: 0.84rem;
    color: var(--secondary-text-color);
  }
  /* Why the choice is shorter than the documentation. Amber, not red: nothing is broken
     — the feature is reduced to what this browser can actually deliver. */
  .reduced,
  .caveat {
    margin: 0 0 12px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid color-mix(in srgb, var(--warning-color, #ffa600) 55%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 10%, transparent);
    font-size: 0.82rem;
    color: var(--secondary-text-color);
  }
  .reduced strong,
  .caveat strong {
    color: var(--primary-text-color);
  }
  .modes {
    display: grid;
    gap: 10px;
  }
  /* Three peers, and normally all three are selectable: they are alternatives with
     different promises, not a feature and two placeholders. The exception is the
     microphone step having found no usable capture, and then the two measured ones are
     dimmed *with their reason inside them* rather than removed. */
  .mode {
    display: flex;
    gap: 10px;
    align-items: flex-start;
    margin: 0;
    padding: 10px 12px;
    border: 1px solid var(--divider-color);
    border-radius: 8px;
    cursor: pointer;
    color: var(--primary-text-color);
    font-size: 0.9rem;
  }
  .mode.on {
    border-color: var(--primary-color);
    background: color-mix(in srgb, var(--primary-color) 8%, transparent);
  }
  .mode.off {
    cursor: default;
    background: color-mix(in srgb, var(--secondary-text-color) 6%, transparent);
  }
  .mode.off .title {
    color: var(--secondary-text-color);
  }
  /* The reason, first thing inside the disabled option — above the description of what
     the mode would do, because it decides whether reading the rest is worth anything. */
  .mode .why {
    margin-top: 4px;
    color: var(--warning-color, #b26a00);
  }
  .mode input {
    margin-top: 3px;
    flex: 0 0 auto;
  }
  .title {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-weight: 500;
  }
  .body p {
    margin: 5px 0 0;
    font-size: 0.82rem;
    color: var(--secondary-text-color);
  }
  .body p.hint {
    font-size: 0.78rem;
  }
  /* Indented under the mode it qualifies, and visibly nested rather than a sibling. */
  .sub-choice {
    display: grid;
    gap: 6px;
    margin: 8px 0 0;
    padding-left: 10px;
    border-left: 2px solid color-mix(in srgb, var(--primary-color) 35%, transparent);
  }
  .opt {
    display: flex;
    gap: 8px;
    align-items: flex-start;
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
  .opt input {
    margin-top: 3px;
    flex: 0 0 auto;
  }
  .opt .hint {
    display: block;
    margin-top: 3px;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
</style>
