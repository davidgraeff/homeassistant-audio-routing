<script lang="ts">
  // Wizard page 1: what "aligned" is going to mean (plan §1 and §12.1).
  //
  // All three modes are listed even though only one can be run today, because they
  // are not variations on a setting — they make *different promises* about where
  // the speakers will be aligned, and a user who cannot see the other two cannot
  // tell which promise they are getting. Each disabled one says why in one line,
  // rather than being hidden and rediscovered as a missing feature.
  import type { MeasureMode } from '../lib/types';

  interface Props {
    mode: MeasureMode;
    /** Multi-position only: measure from several listening spots and join them through
     *  overlap speakers (plan §1.1). A sub-choice of the mode rather than a fourth mode,
     *  because the promise is the same one — "aligned at the spot it was measured from" —
     *  made once per position instead of once. */
    chained: boolean;
    onPick: (mode: MeasureMode) => void;
    onChain: (chained: boolean) => void;
  }
  let { mode, chained, onPick, onChain }: Props = $props();
</script>

<p class="lead">
  A single microphone in a single place measures the electrical delay <em>and</em> the sound's travel time together, and
  cannot separate them — a metre of extra distance is about 3 ms. So the first thing to choose is what should end up
  aligned.
</p>

<div class="modes">
  <label class="mode" class:on={mode === 'sweet_spot'}>
    <input type="radio" name="align-mode" value="sweet_spot" checked={mode === 'sweet_spot'} onchange={() => onPick('sweet_spot')} />
    <div class="body">
      <div class="title">Multi-position <span class="badge on">available</span></div>
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

  <label class="mode disabled">
    <input type="radio" name="align-mode" value="near_field" disabled />
    <div class="body">
      <div class="title">Near field <span class="badge off">not built yet</span></div>
      <p>
        Walking to each speaker in turn aligns the <em>wiring</em> rather than one spot, so it is right everywhere in the
        house.
      </p>
      <p class="hint">The daemon refuses this mode explicitly rather than quietly measuring a position instead.</p>
    </div>
  </label>

  <label class="mode disabled">
    <input type="radio" name="align-mode" value="manual" disabled />
    <div class="body">
      <div class="title">Manual <span class="badge off">already below</span></div>
      <p>By ear: nudge one speaker until its clicks sit on the reference's. The fallback when the microphone cannot be
        used or the estimator refuses.</p>
      <p class="hint">
        It is not a wizard page yet — the sliders are on this panel, under the wizard, and stay usable throughout.
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
  .modes {
    display: grid;
    gap: 10px;
  }
  /* Three peers, one enabled: the disabled two are dimmed but not hidden, and keep
     their explanation — that is the whole point of listing them. */
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
  .mode.disabled {
    cursor: default;
    opacity: 0.65;
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
