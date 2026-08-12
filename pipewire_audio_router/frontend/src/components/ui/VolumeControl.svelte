<script lang="ts">
  import { untrack } from 'svelte';

  // Shared per-device volume + mute control (sendspin / AirPlay 2). Presentational
  // and callback-driven, but it owns one bit of local state: a drag guard so an
  // incoming (possibly a beat stale) live value can't yank the thumb while the
  // user is dragging.
  //
  // SAFETY: AirPlay volume is a dB scale, so the top of a linear slider is
  // near-max power. Two consequences baked in here:
  //   * UNKNOWN volume (percent == null/undefined — receiver didn't report and the
  //     user hasn't set it) renders the thumb at 0 with an explanatory title —
  //     NEVER a fake 100%.
  //   * The last 20% of the track (80–100%) is tinted red as a danger warning.

  interface Props {
    /** Current volume as an integer 0–100 percent, or null/undefined when UNKNOWN. */
    percent: number | null | undefined;
    /** Mute state; the slider is disabled while muted. */
    muted: boolean;
    /** User dragged the slider — reports the new integer 0–100 percent. */
    onVolume: (percent: number) => void;
    /** User toggled the mute button (caller flips the state). */
    onMute: () => void;
    /** Disable the whole control (e.g. output offline). */
    disabled?: boolean;
    /** Is there a volume knob to drag / a mute to toggle? Both default to true, which is
     *  every case but one: a PipeWire host whose sink has no device route reports a volume
     *  and no mute, so it gets the slider and no mute button. A button that cannot work is
     *  worse than an absent one — it looks like the write failed silently, which is exactly
     *  how the pw-sink volume bug read. Both false renders nothing at all; the caller is
     *  expected not to mount this then. */
    canVolume?: boolean;
    canMute?: boolean;
  }
  let { percent, muted, onVolume, onMute, disabled = false, canVolume = true, canMute = true }: Props = $props();

  const known = $derived(percent != null);

  // Local slider position. Unknown stays at 0. While the user is actively
  // dragging we hold our own value and ignore incoming prop updates for 1500ms.
  let local = $state(0);
  let touched = 0;

  $effect(() => {
    const p = percent; // sole dependency
    untrack(() => {
      if (p == null) return; // unknown → leave the thumb at 0
      if (Date.now() - touched < 1500) return; // don't yank a live drag
      const pct = Math.round(p);
      if (local !== pct) local = pct;
    });
  });

  function onInput(e: Event & { currentTarget: HTMLInputElement }) {
    touched = Date.now();
    const pct = parseInt(e.currentTarget.value);
    local = pct;
    onVolume(pct);
  }
</script>

<div class="vol-control">
  {#if canMute}
    <button
      class="mute"
      class:on={muted}
      aria-pressed={muted}
      title={muted ? 'Unmute' : 'Mute'}
      {disabled}
      onclick={onMute}
    >{muted ? '🔇' : '🔊'}</button>
  {/if}
  {#if canVolume}
    <input
      class="vol-slider"
      type="range"
      min="0"
      max="100"
      step="1"
      disabled={disabled || muted}
      value={local}
      oninput={onInput}
      title={known
        ? `Volume ${local}%`
        : "Volume unknown — the receiver didn't report its level; move to set"}
    />
  {:else}
    <!-- Mute-only: say why the slider is missing rather than leaving a gap. -->
    <span class="no-slider" title="This output's own volume cannot be set from here — only its mute">no volume control</span>
  {/if}
</div>

<style>
  .vol-control {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
    width: 100%;
  }
  /* Danger zone: AirPlay volume is a dB scale, so the top of a linear slider is
     near-max power — tint the last 20% (80–100%) of the TRACK red as a warning.
     We must opt out of the native control (`appearance: none`) and clear
     `accent-color` (set globally in app.css): otherwise the browser paints an
     opaque native track OVER this gradient and the red never shows. The track is
     the element background (gradient); the pseudo-element tracks are transparent
     so it shows through; the thumb is drawn explicitly. */
  .vol-slider {
    flex: 1;
    min-width: 0;
    -webkit-appearance: none;
    appearance: none;
    accent-color: auto;
    height: 6px;
    border-radius: 999px;
    background: linear-gradient(
      to right,
      var(--divider-color) 0 80%,
      var(--error-color) 80% 100%
    );
    cursor: pointer;
  }
  .vol-slider::-webkit-slider-runnable-track {
    background: transparent;
    height: 6px;
    border-radius: 999px;
  }
  .vol-slider::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 14px;
    height: 14px;
    margin-top: -4px;
    border-radius: 50%;
    background: var(--primary-color);
    border: 2px solid var(--card-background-color, #fff);
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.3);
  }
  .vol-slider::-moz-range-track {
    background: transparent;
    height: 6px;
    border-radius: 999px;
  }
  .vol-slider::-moz-range-thumb {
    width: 14px;
    height: 14px;
    border: 2px solid var(--card-background-color, #fff);
    border-radius: 50%;
    background: var(--primary-color);
  }
  .vol-slider:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .no-slider {
    flex: 1;
    min-width: 0;
    font-size: 0.72rem;
    color: var(--secondary-text-color);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .mute {
    flex: none;
    border: none;
    background: none;
    cursor: pointer;
    font-size: 0.85rem;
    line-height: 1;
    padding: 2px;
    border-radius: 6px;
    opacity: 0.75;
  }
  .mute:hover:not(:disabled) {
    opacity: 1;
  }
  .mute.on {
    opacity: 1;
  }
  .mute:disabled {
    cursor: default;
    opacity: 0.4;
  }
</style>
