<script lang="ts">
  // Microphone pre-flight + level meter for measurement-assisted alignment
  // (docs/mic-alignment-plan.md §4, §12).
  //
  // This is the piece that makes every *later* failure legible. If the user cannot
  // see that the mic is live, that the room is quiet enough and that nothing is
  // clipping, then a refused measurement (plan §5.5) looks like a broken feature
  // instead of a room that needs the phone moved. So the readouts here come from
  // the daemon's ingest (`/api/align/mic`), not from the browser: a moving meter
  // proves the whole path — worklet, socket, ring buffer — not just that the phone
  // has a microphone.
  //
  // The capture is stopped when this component goes away: nothing good comes of a
  // microphone that stays open behind a collapsed panel.
  import AlignSignalVerdict from './AlignSignalVerdict.svelte';
  import { mic } from '../../lib/mic.svelte';

  const st = $derived(mic.status);
  const blocked = $derived(mic.preflightError);
  const capturing = $derived(mic.phase === 'capturing');
  const peak = $derived(st?.peak ?? 0);
  /** Peak as a meter width. Full scale is 1.0; the top ~20% is the clip zone. */
  const peakPct = $derived(Math.min(100, Math.round(peak * 100)));
  const quiet = $derived(capturing && (st?.frames_received ?? 0) > 24000 && peak < 0.02);

  // The meter answers "is the mic alive", not "is the level good enough" — it is a
  // decaying broadband peak read against an 8 ms burst once per second, so it
  // under-reads by up to ~20 dB depending on when it samples. Only per-channel peak
  // SNR decides whether a measurement can succeed, and that verdict is polled by the
  // mic store (one request for the whole wizard, because the level page needs the
  // same answer) and rendered by the shared AlignSignalVerdict.
  $effect(() => () => mic.stop());
</script>

<div class="mic">
  <div class="row">
    <span class="label" class:on={capturing}>Microphone</span>
    {#if blocked}
      <span class="muted">unavailable</span>
    {:else if capturing}
      <span class="rate">{mic.rate / 1000} kHz mono</span>
      <button class="danger" onclick={() => mic.stop()}>Stop microphone</button>
    {:else}
      <button
        class="ghost"
        disabled={mic.phase === 'starting'}
        title="Grant microphone access so the daemon can measure the speakers instead of you judging by ear"
        onclick={() => mic.start()}
      >
        {mic.phase === 'starting' ? 'Starting…' : 'Use microphone'}
      </button>
    {/if}
  </div>

  {#if blocked}
    <p class="problem">{blocked}</p>
  {:else if mic.error}
    <p class="problem">{mic.error}</p>
  {/if}

  {#if capturing}
    <div class="meter" title={`Peak ${(peak * 100).toFixed(0)}% of full scale`}>
      <div class="fill" class:hot={peak > 0.8} style={`width:${peakPct}%`}></div>
      <!-- The clip zone: measurement is refused on a capture that reached it. -->
      <div class="ceiling"></div>
    </div>

    <div class="stats">
      <span>{((st?.frames_received ?? 0) / Math.max(1, mic.rate)).toFixed(1)} s captured</span>
      <span class:bad={(st?.gap_count ?? 0) > 0}>{st?.gap_count ?? 0} gaps</span>
      {#if mic.dropped > 0}
        <span class="bad">{mic.dropped} blocks dropped (network too slow)</span>
      {/if}
    </div>

    <!-- The verdict that actually decides whether a measurement can succeed. Shown
         next to the meter precisely because the meter looks reassuring at levels
         that are nowhere near good enough. -->
    <AlignSignalVerdict signal={mic.signal} settling={mic.signalSettling} />

    {#if st?.clipped}
      <p class="problem">
        The microphone clipped ({st.clip_count} samples at full scale). A clipped block is broadband, so it corrupts
        every speaker's measurement at once — turn the playback volume down, or move the phone away from the nearest
        speaker, then start the microphone again.
      </p>
    {:else if quiet}
      <p class="hint">
        Almost nothing is reaching the microphone. Raise the playback volume above, or hold the phone where it can
        actually hear the speakers.
      </p>
    {:else}
      <p class="hint">
        Keep the phone still and the room quiet. The meter should move with the clicks and stay out of the red.
      </p>
    {/if}

    {#if mic.caveats.length}
      <p class="hint">
        This browser did not confirm whether it switched off {mic.caveats.join(', ')}. If the measurement comes out
        inconsistent, that is the first thing to suspect — try another browser.
      </p>
    {/if}
  {/if}
</div>

<style>
  .mic {
    margin: 10px 0 4px;
    padding-top: 10px;
    border-top: 1px solid var(--divider-color);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .label {
    font-size: 0.72rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--secondary-text-color);
  }
  .label.on {
    color: var(--primary-color);
  }
  .rate {
    font-size: 0.8rem;
    color: var(--secondary-text-color);
    flex: 1 1 auto;
  }
  .row button {
    flex: 0 0 auto;
    padding: 4px 10px;
    font-size: 0.8rem;
  }
  .muted {
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .meter {
    position: relative;
    height: 10px;
    margin-top: 10px;
    border-radius: 5px;
    overflow: hidden;
    background: var(--input-fill-color);
    border: 1px solid var(--divider-color);
  }
  .fill {
    height: 100%;
    background: var(--primary-color);
    transition: width 120ms linear;
  }
  .fill.hot {
    background: var(--error-color, #db4437);
  }
  /* Marks 80% of full scale — where clipping becomes likely. */
  .ceiling {
    position: absolute;
    top: 0;
    bottom: 0;
    left: 80%;
    right: 0;
    background: color-mix(in srgb, var(--error-color, #db4437) 18%, transparent);
    pointer-events: none;
  }
  .stats {
    display: flex;
    gap: 12px;
    flex-wrap: wrap;
    margin-top: 6px;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
  .stats .bad {
    color: var(--error-color, #db4437);
  }
  .hint,
  .problem {
    margin: 6px 0 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .problem {
    color: var(--error-color, #db4437);
  }
</style>
