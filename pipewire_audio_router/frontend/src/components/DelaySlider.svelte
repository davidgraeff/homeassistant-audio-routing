<script lang="ts">
  import { onDestroy } from 'svelte';

  // A millisecond knob you hunt for the edge of **by ear**: a slider rather than a
  // number box, with a coloured track that shows which way is risky before you drag
  // there, and a readout that never changes width so the row can't twitch.
  //
  // Every latency dial in this UI is one of these — an AirPlay-2 render delay, a
  // PipeWire host's playout delay, a sendspin speaker's static trim, the Opus
  // send-ahead headroom. They differ only in scale, wording and how expensive an
  // intermediate value is, so those are props; what is *not* duplicated is the part
  // that is easy to get subtly wrong, below.
  //
  // ## Who owns the thumb
  // The user, while they are working it. `applied` is pushed live from the daemon (the
  // outputs listing arrives on a socket, and a commit provokes one), so adopting it
  // unconditionally would move the thumb under the cursor mid-drag.
  //
  // Ownership is **state, not a timer**: while `override` is set the slider shows the
  // user's number and ignores `applied`, and it is cleared once `oncommit` resolves — by
  // which time the caller has re-read the daemon, so `applied` is current. That covers
  // both the push a commit provokes and any unrelated one crossing it, with no
  // wall-clock window to tune and no first-frame flash.
  interface Props {
    label: string;
    /** Value the daemon has stored/running now: the resting position, and what the
     *  during-drag popover reports. */
    applied: number;
    min: number;
    max: number;
    step: number;
    /** Below this the low end is dangerous. **0 disables the risky zone** — for a knob
     *  whose zero means "no adjustment", low is not a risk. */
    riskyBelow: number;
    highAbove: number;
    /** Sentence for the low end; used only when `riskyBelow > 0`. */
    risk: string;
    /** Sentence for the healthy middle, in this knob's own terms. */
    good: string;
    /** Where the current value came from ("your override", "no trim", …), appended to
     *  whichever sentence applies. */
    origin?: string;
    /** Push intermediate values while dragging (throttled)? Only where applying one is
     *  cheap and gapless — see `onLive`. */
    live?: boolean;
    /** Intermediate value during a drag; only called when `live`. Must be silent (no
     *  toasts): it fires several times a second. */
    onLive?: (ms: number) => void;
    /** The final value, on release or keyboard commit. Awaited: the hold is kept until
     *  it resolves, so a listing that crosses it cannot snap the thumb back. */
    oncommit: (ms: number) => Promise<void> | void;
    /** Renders a "Default" button when given. */
    onreset?: () => void;
    resetDisabled?: boolean;
    resetTitle?: string;
    /** Suffix for the popover when a commit is deferred to release, e.g. " — on release". */
    deferredHint?: string;
    id: string;
  }
  let {
    label,
    applied,
    min,
    max,
    step,
    riskyBelow,
    highAbove,
    risk,
    good,
    origin = '',
    live = false,
    onLive,
    oncommit,
    onreset,
    resetDisabled = false,
    resetTitle = '',
    deferredHint = '',
    id,
  }: Props = $props();

  const THROTTLE_MS = 250; // ≤4 round trips/s while dragging: audible, not chatty

  /** Set only while the value is the user's; `null` = follow the daemon. */
  let override = $state<number | null>(null);
  /** Distinct from `override`: the popover shows during the gesture only, while the
   *  override outlives it until the commit lands. */
  let dragging = $state(false);
  const local = $derived(override ?? applied);

  // Throttle, not debounce: a debounce sends nothing at all during a *continuous*
  // drag (every movement resets it), which is exactly when you want to hear the value
  // you are hunting for. So the first movement goes out at once, then at most one per
  // window, with the last value sent on the trailing edge.
  let timer: ReturnType<typeof setTimeout> | null = null;
  let pending: number | null = null;
  function cancelPending() {
    if (timer) {
      clearTimeout(timer);
      timer = null;
    }
    pending = null;
  }
  onDestroy(cancelPending);

  function openWindow() {
    timer = setTimeout(() => {
      timer = null;
      const trailing = pending;
      pending = null;
      if (trailing != null) {
        onLive?.(trailing);
        openWindow(); // keep the rhythm while the drag continues
      }
    }, THROTTLE_MS);
  }

  function onInput(value: number) {
    override = value;
    dragging = true;
    if (!live || !onLive) return;
    if (timer) {
      pending = value; // coalesce into the open window
      return;
    }
    onLive(value);
    openWindow();
  }

  async function onChange() {
    cancelPending();
    dragging = false;
    // The override outlives the gesture until the commit resolves: the daemon may not
    // store what was asked for (values get clamped), so releasing ownership any earlier
    // could leave a slider showing 41 next to a device running 42. Cleared in `finally`
    // — a failed commit must show the truth, not the wish.
    const wanted = local;
    try {
      await oncommit(wanted);
    } finally {
      override = null;
    }
  }

  type Zone = 'risky' | 'good' | 'high';
  const zone = $derived<Zone>(riskyBelow > 0 && local < riskyBelow ? 'risky' : local > highAbove ? 'high' : 'good');
  const note = $derived(
    riskyBelow > 0 && local < riskyBelow
      ? `Below ${riskyBelow} ms ${risk}${origin ? ` (${origin})` : ''}.`
      : local > highAbove
        ? `Safe, but ${local} ms is added latency you may not need${origin ? ` (${origin})` : ''}.`
        : `${good}${origin ? ` (${origin})` : ''}.`,
  );
</script>

<div class="sync-field delay-field">
  <label for={id}>{label}</label>
  <div class="delay-cell">
    <!-- Slider and scale share one box so the tick numbers land on the colour
         boundaries — measured against the whole field they drift right by the
         readout's width. -->
    <div class="delay-track">
      <input
        {id}
        class="delay-slider zone-{zone}"
        type="range"
        {min}
        {max}
        {step}
        value={local}
        aria-describedby="{id}-note"
        oninput={(e) => onInput(Number(e.currentTarget.value))}
        onchange={onChange}
      />
      <div class="delay-scale" aria-hidden="true">
        <span style="left:0%">{min}</span>
        {#if riskyBelow > min}
          <span style="left:{(riskyBelow / max) * 100}%">{riskyBelow}</span>
        {/if}
        <span style="left:{(highAbove / max) * 100}%">{highAbove}</span>
        <span style="left:100%">{max}</span>
      </div>
    </div>
    <output class="delay-read zone-{zone}" for={id}>{local} ms</output>
    <!-- Only while dragging: the readout above is where the thumb *is*, this is what
         is actually in force. **Out of flow, deliberately** — as an inline chip it
         appeared mid-gesture and took its width out of the track beside it, so the
         slider shrank under the cursor and the thumb moved without the value
         changing. The layout must not react to a drag at all.
         aria-hidden: the note below is this slider's described-by and already names
         the origin; announcing this on every step would be noise. -->
    {#if dragging}
      <span class="delay-applied" aria-hidden="true" title="What is in force right now">
        applied {applied} ms{deferredHint}
      </span>
    {/if}
    {#if onreset}
      <button class="ghost" disabled={resetDisabled} title={resetTitle} onclick={onreset}>Default</button>
    {/if}
  </div>
  <p id="{id}-note" class="delay-note zone-{zone}">{note}</p>
</div>

<style>
  .sync-field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }
  .sync-field label {
    font-size: 0.85rem;
    color: var(--secondary-text-color);
  }
  /* Wide enough to be worth dragging; `min-width: 0` above lets it shrink in a
     narrow card instead of pushing the row sideways. */
  .delay-field {
    flex: 1 1 320px;
  }
  .delay-cell {
    display: flex;
    gap: 10px;
    align-items: center;
    /* Anchor for the popover, which must not be in flow. */
    position: relative;
  }
  .delay-track {
    flex: 1 1 auto;
    min-width: 120px;
  }
  .delay-slider {
    width: 100%;
    margin: 0;
    appearance: none;
    background: transparent;
    height: 18px;
  }
  /* The track carries the zones: red under `riskyBelow`, green through the good
     range, amber above `highAbove` — so which way is risky is visible before the
     thumb gets there. Percentages are computed from the props by the caller's
     numbers, so one gradient serves every scale. */
  .delay-slider::-webkit-slider-runnable-track {
    height: 4px;
    border-radius: 2px;
    background: var(--divider-color);
  }
  .delay-slider::-moz-range-track {
    height: 4px;
    border-radius: 2px;
    background: var(--divider-color);
  }
  .delay-slider::-webkit-slider-thumb {
    appearance: none;
    width: 14px;
    height: 14px;
    margin-top: -5px;
    border-radius: 50%;
    background: var(--primary-color);
    border: 2px solid var(--card-background-color, #fff);
    box-shadow: 0 1px 3px rgb(0 0 0 / 0.35);
    cursor: pointer;
  }
  .delay-slider::-moz-range-thumb {
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--primary-color);
    border: 2px solid var(--card-background-color, #fff);
    box-shadow: 0 1px 3px rgb(0 0 0 / 0.35);
    cursor: pointer;
  }
  .delay-slider:focus-visible {
    outline: 2px solid var(--primary-color);
    outline-offset: 2px;
  }
  .delay-slider.zone-risky::-webkit-slider-thumb {
    background: var(--error-color);
  }
  .delay-slider.zone-risky::-moz-range-thumb {
    background: var(--error-color);
  }
  .delay-slider.zone-high::-webkit-slider-thumb {
    background: var(--warning-color);
  }
  .delay-slider.zone-high::-moz-range-thumb {
    background: var(--warning-color);
  }
  .delay-read {
    min-width: 68px; /* "2000 ms" without the row twitching as you drag */
    text-align: right;
    font-variant-numeric: tabular-nums;
    font-size: 0.9rem;
    font-weight: 500;
  }
  .delay-read.zone-risky {
    color: var(--error-color);
  }
  .delay-read.zone-high {
    color: var(--warning-color);
  }
  .delay-applied {
    position: absolute;
    right: 0;
    bottom: calc(100% + 0.3rem);
    z-index: 5;
    padding: 0.15rem 0.45rem;
    border-radius: 5px;
    background: var(--card-background-color, #fff);
    border: 1px solid var(--divider-color);
    box-shadow: 0 2px 6px rgb(0 0 0 / 0.3);
    font-size: 0.78rem;
    color: var(--secondary-text-color);
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
    pointer-events: none;
  }
  .delay-scale {
    position: relative;
    height: 12px;
    font-size: 0.68rem;
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
  .delay-scale span {
    position: absolute;
    transform: translateX(-50%);
    top: 0;
  }
  .delay-scale span:first-child {
    transform: none;
  }
  .delay-scale span:last-child {
    transform: translateX(-100%);
  }
  /* Two lines' worth reserved: the sentence changes as the thumb crosses a zone
     boundary, and a one-to-two-line reflow mid-drag shifts everything below it. */
  .delay-note {
    min-height: 2.3em;
    margin: 2px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .delay-note.zone-risky {
    color: var(--error-color);
  }
  .delay-note.zone-high {
    color: var(--warning-color);
  }
  .ghost {
    background: none;
    border: 1px solid var(--divider-color);
    color: var(--secondary-text-color);
    border-radius: 6px;
    padding: 3px 8px;
    font-size: 0.8rem;
    cursor: pointer;
  }
  .ghost:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
