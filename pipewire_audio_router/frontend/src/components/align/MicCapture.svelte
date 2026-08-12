<script lang="ts">
  // Microphone status strip for measurement-assisted alignment
  // (docs/mic-alignment-plan.md §4, §12).
  //
  // This is the piece that makes every *later* failure legible. If the user cannot
  // see that the mic is live, that the phone hears the room and that nothing is
  // clipping, then a refused measurement (plan §5.5) looks like a broken feature
  // instead of a room that needs the phone moved. So the readouts come from the
  // daemon's ingest (`/api/align/mic`), not from the browser: a moving meter proves
  // the whole path — worklet, socket, ring buffer — not just that the phone has a
  // microphone.
  //
  // Two rules it now follows, both learned from using it:
  //
  //   * **Fixed height, whatever the state.** It sits above the wizard's page, so
  //     every line it grows by pushes the step the user is reading further down. The
  //     same five checks are therefore always rendered — as pending, met or failed —
  //     and each one is a single clipped line, so nothing here can reflow the page.
  //     That is also why the meter is drawn while idle rather than appearing on start.
  //   * **No level verdict here.** The estimator's verdict
  //     (`/api/align/mic/signal`) needs the click track, and on this step nothing is
  //     playing it: the wizard has not formed a group yet, so the honest answer is
  //     always "still collecting audio" and the user reads a complaint about a sound
  //     that was never made. The verdict belongs where a speaker *is* clicking — the
  //     Speakers step shows it per speaker (AlignSignalVerdict), which is the reading
  //     that decides anything.
  //
  // Starting and stopping live in the wizard's button row, not here: this is a
  // status panel, and the one thing the microphone step asks the user to do should
  // be where they look for what to press next.
  //
  // The capture is stopped when this component goes away: nothing good comes of a
  // microphone that stays open behind a page that is not showing it.
  import { mic } from '../../lib/mic.svelte';

  const st = $derived(mic.status);
  const blocked = $derived(mic.preflightError);
  const capturing = $derived(mic.phase === 'capturing');
  const peak = $derived(st?.peak ?? 0);
  /** Peak as a meter width. Full scale is 1.0; the top ~20% is the clip zone. */
  const peakPct = $derived(capturing ? Math.min(100, Math.round(peak * 100)) : 0);
  const seconds = $derived((st?.frames_received ?? 0) / Math.max(1, mic.rate));
  const lost = $derived((st?.gap_count ?? 0) + mic.dropped);

  /** Has anything at all reached the microphone since it was started?
   *
   *  Latched rather than read live, because `peak` decays between polls: a check that
   *  followed it would tick and untick twice a second and read as a fault. What the
   *  user needs to know is whether the phone has *ever* heard the room on this
   *  capture — after that, the meter is the live reading. */
  let heard = $state(false);
  $effect(() => {
    if (!capturing) heard = false;
    else if ((mic.status?.peak ?? 0) > 0.02) heard = true;
  });

  /** One acceptance criterion: met, failed, or not answered yet.
   *
   *  `pending` exists so the row can be shown before the capture starts without
   *  claiming anything — the alternative is a panel that grows four lines the moment
   *  the microphone comes up, which is the layout shift this is written to avoid. */
  type State = 'pending' | 'ok' | 'bad' | 'warn';
  interface Check {
    what: string;
    state: State;
    /** One short line, clipped rather than wrapped. Numbers when there are numbers,
     *  the remedy when something is wrong — never both. */
    note: string;
  }

  const checks = $derived<Check[]>([
    {
      what: 'Audio reaches the add-on',
      state: !capturing ? 'pending' : seconds > 0.5 ? 'ok' : 'pending',
      note: capturing ? `${seconds.toFixed(0)} s captured at ${mic.rate / 1000} kHz` : 'not started',
    },
    {
      what: 'The phone hears the room',
      state: !capturing ? 'pending' : heard ? 'ok' : 'pending',
      note: !capturing
        ? '—'
        : heard
          ? `peak ${Math.round(peak * 100)}% of full scale`
          : 'almost silent — hold the phone where it can hear the speakers',
    },
    {
      what: 'Nothing lost on the way',
      state: !capturing ? 'pending' : lost > 0 ? 'bad' : 'ok',
      note: !capturing
        ? '—'
        : lost > 0
          ? `${lost} block${lost === 1 ? '' : 's'} lost — the network is not keeping up`
          : 'no gaps, no dropped blocks',
    },
    {
      what: 'Nothing clipping',
      state: !capturing ? 'pending' : st?.clipped ? 'bad' : 'ok',
      note: !capturing
        ? '—'
        : st?.clipped
          ? `${st.clip_count} samples at full scale — turn the playback down`
          : 'headroom left at the top',
    },
    {
      // §4.2: an explicit `true` already refused the capture, so what is left here is
      // "the browser would not say" — a caveat that travels with the run, not a fault.
      what: 'Browser processing off',
      state: !capturing ? 'pending' : mic.caveats.length ? 'warn' : 'ok',
      note: !capturing
        ? '—'
        : mic.caveats.length
          ? `this browser did not confirm ${mic.caveats.join(', ')}`
          : 'echo cancellation, gain control and noise suppression off',
    },
  ]);

  const MARK: Record<State, string> = { pending: '•', ok: '✓', bad: '✕', warn: '!' };

  $effect(() => () => mic.stop());
</script>

<div class="mic">
  <div class="row">
    <span class="label" class:on={capturing}>Microphone</span>
    <!-- The meter answers "is the mic alive", not "is the level good enough" — it is a
         decaying broadband peak, so it under-reads by up to ~20 dB depending on when it
         samples. Drawn empty while idle so starting the capture does not move the page. -->
    <div class="meter" class:live={capturing} title={capturing ? `Peak ${(peak * 100).toFixed(0)}% of full scale` : 'Not capturing'}>
      <div class="fill" class:hot={peak > 0.8} style={`width:${peakPct}%`}></div>
      <!-- The clip zone: measurement is refused on a capture that reached it. -->
      <div class="ceiling"></div>
    </div>
  </div>

  <ul class="checks">
    {#each checks as c (c.what)}
      <li class={c.state}>
        <span class="mark" aria-hidden="true">{MARK[c.state]}</span>
        <span class="what">{c.what}</span>
        <span class="note" title={c.note}>{c.note}</span>
      </li>
    {/each}
  </ul>

  <!-- The one thing that may add a line: a failure with a sentence of its own. It is worth
       the reflow because it does not come and go with a poll — it stays until the user acts
       on it. -->
  {#if blocked}
    <p class="problem">{blocked}</p>
  {:else if mic.error}
    <p class="problem">{mic.error}</p>
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
    gap: 10px;
  }
  .label {
    flex: 0 0 auto;
    font-size: 0.72rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--secondary-text-color);
  }
  .label.on {
    color: var(--primary-color);
  }
  .meter {
    position: relative;
    flex: 1 1 auto;
    height: 10px;
    border-radius: 5px;
    overflow: hidden;
    background: var(--input-fill-color);
    border: 1px solid var(--divider-color);
    opacity: 0.5;
  }
  .meter.live {
    opacity: 1;
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
  /* Five fixed rows, each exactly one line: this is what keeps the panel's height
     constant, so the wizard page under it never moves. */
  .checks {
    margin: 8px 0 0;
    padding: 0;
    list-style: none;
    font-size: 0.78rem;
  }
  .checks li {
    display: grid;
    grid-template-columns: 14px auto 1fr;
    align-items: baseline;
    gap: 8px;
    line-height: 1.6;
    color: var(--secondary-text-color);
  }
  .mark {
    text-align: center;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  li.ok .mark {
    color: var(--success-color, #43a047);
  }
  li.bad .mark {
    color: var(--error-color, #db4437);
  }
  li.warn .mark {
    color: var(--warning-color, #ffa600);
  }
  li.ok .what,
  li.bad .what,
  li.warn .what {
    color: var(--primary-text-color);
  }
  li.pending .what {
    color: var(--secondary-text-color);
  }
  /* Clipped, never wrapped — a second line here is a moved page. The full text stays
     reachable as the row's tooltip. */
  .note {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }
  li.bad .note {
    color: var(--error-color, #db4437);
  }
  .problem {
    margin: 8px 0 0;
    font-size: 0.8rem;
    color: var(--error-color, #db4437);
  }
</style>
