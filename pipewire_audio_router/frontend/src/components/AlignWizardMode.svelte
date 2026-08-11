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
    onPick: (mode: MeasureMode) => void;
  }
  let { mode, onPick }: Props = $props();
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
      <p class="hint">
        Measuring a second position and joining the two through a shared speaker is not built yet, so today this aligns
        one position — which is the same thing with one step.
      </p>
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
</style>
