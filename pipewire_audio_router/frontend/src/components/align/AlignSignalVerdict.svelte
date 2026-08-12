<script lang="ts">
  // "Is the level good enough to measure?" — the estimator's own verdict on the
  // current capture (`GET /api/align/mic/signal`, plan §5.4.1 / §12.2).
  //
  // One component, two callers: the microphone pre-flight (MicCapture) and the
  // per-speaker level setting (AlignWizardSpeakers). It is shared rather than
  // duplicated on purpose — a second presentation of the same four verdicts would
  // drift, and then "green" would mean two different things on two pages of one
  // wizard.
  //
  // Two rules it enforces for both callers:
  //
  //   * `marginal` is **accepted** by the estimator (it is above `MIN_PEAK_SNR_DB`),
  //     so it must not read as a failure — but it is not comfortable either, so it is
  //     amber and says what it costs.
  //   * while `settling` is set there is deliberately **no verdict**, because the
  //     daemon's window is trailing: for ~8 s after the tone moves or a level is
  //     dragged, a reading describes the previous state. Showing the old green there
  //     is the one wrong answer that would start a run that cannot succeed.
  import type { SignalCheck } from '../../lib/types';

  interface Props {
    /** The daemon's verdict, or null when there is none to show yet. */
    signal: SignalCheck | null;
    /** A verdict is being withheld because what the mic hears just changed. */
    settling?: boolean;
    /** What the reading is *about*, when it is not simply "the microphone" — the
     *  speaker being soloed. Shown so a verdict is never ambiguous about its
     *  subject. */
    subject?: string | null;
  }
  let { signal, settling = false, subject = null }: Props = $props();

  const good = $derived(signal?.verdict === 'good');
  const tight = $derived(signal?.verdict === 'marginal');
  const bad = $derived(signal?.verdict === 'too_quiet' || signal?.verdict === 'unusable');
</script>

{#if settling || !signal}
  <div class="signal waiting">
    <div class="verdict">
      <span class="dot"></span>
      <strong>Listening{subject ? ` to ${subject}` : ''}…</strong>
      <span class="snr">about 8 seconds of the click track</span>
    </div>
    <p class="why">
      {#if settling}
        Something just changed, so the last reading was about the level before it. The next verdict covers only what is
        playing now.
      {:else}
        Collecting audio to judge the level — the verdict needs four repeats of the pattern.
      {/if}
    </p>
  </div>
{:else}
  <div class="signal" class:good class:tight class:bad>
    <div class="verdict">
      <span class="dot"></span>
      <strong>
        {#if good}Level good{:else if tight}Level tight{:else if signal.verdict === 'too_quiet'}Too quiet to measure{:else}Cannot measure{/if}
      </strong>
      {#if subject}<span class="subject">{subject}</span>{/if}
      {#if signal.worst_peak_snr_db !== null}
        <span class="snr">{signal.worst_peak_snr_db.toFixed(0)} dB on the weaker tone</span>
      {/if}
    </div>
    <!-- Verbatim: the daemon writes this sentence, and it names both the problem and
         the action. -->
    <p class="why">{signal.message}</p>
    {#if tight}
      <p class="why">
        The estimator would accept this, so a run can start — but there is little margin, and any extra room noise will
        spoil it. Raising the level is worth the few seconds.
      </p>
    {/if}
    {#if signal.channels.length}
      <div class="chans">
        {#each signal.channels as c (c.label)}
          <span title={`${c.periods_used} periods used, peak/runner-up ${c.second_peak_ratio.toFixed(1)}×`}>
            {(c.center_hz / 1000).toFixed(1)} kHz: {c.peak_snr_db.toFixed(0)} dB
          </span>
        {/each}
      </div>
    {/if}
  </div>
{/if}

<style>
  /* Colour is a reinforcement, never the only carrier — the wording states the
     verdict too, and the dot has a distinct fill per state, so it survives both
     themes and colour-vision differences. */
  .signal {
    margin-top: 10px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: color-mix(in srgb, var(--secondary-text-color) 6%, transparent);
  }
  .signal.good {
    border-color: color-mix(in srgb, var(--success-color, #43a047) 55%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 10%, transparent);
  }
  .signal.tight {
    border-color: color-mix(in srgb, var(--warning-color, #ffa600) 55%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 12%, transparent);
  }
  .signal.bad {
    border-color: color-mix(in srgb, var(--error-color, #db4437) 55%, transparent);
    background: color-mix(in srgb, var(--error-color, #db4437) 10%, transparent);
  }
  .verdict {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 0.82rem;
  }
  .dot {
    flex: 0 0 auto;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--secondary-text-color);
  }
  .signal.good .dot {
    background: var(--success-color, #43a047);
  }
  .signal.tight .dot {
    background: var(--warning-color, #ffa600);
  }
  .signal.bad .dot {
    background: var(--error-color, #db4437);
  }
  /* The waiting state pulses so it reads as "working", not as a fourth verdict. */
  .signal.waiting .dot {
    animation: pulse 1.6s ease-in-out infinite;
  }
  @keyframes pulse {
    0%,
    100% {
      opacity: 0.35;
    }
    50% {
      opacity: 1;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .signal.waiting .dot {
      animation: none;
    }
  }
  .subject {
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid var(--divider-color);
    background: var(--input-fill-color);
  }
  .snr {
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
  .why {
    margin: 5px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .chans {
    display: flex;
    gap: 12px;
    flex-wrap: wrap;
    margin-top: 6px;
    font-size: 0.75rem;
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
</style>
