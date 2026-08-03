<script lang="ts">
  // "Explain speaker alignment" — the document behind the help button in the
  // Input-sources header. Alignment lives on this page because a sync group is
  // identified by the source feeding it (bridge-daemon/src/calibrate.rs): the
  // speakers listed on a source card are exactly the ones that play off one
  // clock, and those are the ones worth aligning against each other.
  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

  // A long document: focus it so Page Down / arrows scroll the dialog, not the
  // page behind it (same as RtpSenderDocs).
  let dialogEl = $state<HTMLDivElement>();
  $effect(() => {
    dialogEl?.focus();
  });
</script>

<svelte:window onkeydown={(e) => e.key === 'Escape' && onClose()} />

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="modal-backdrop" onclick={onClose}>
  <div
    class="modal-card card wide"
    role="dialog"
    aria-modal="true"
    aria-labelledby="align-docs-title"
    tabindex="-1"
    bind:this={dialogEl}
    onclick={(e) => e.stopPropagation()}
  >
    <div class="card-head">
      <h2 id="align-docs-title">Aligning speakers by ear</h2>
      <button class="ghost" type="button" onclick={onClose}>Close</button>
    </div>
    <p class="card-sub">
      Speakers that play the same stream should be sample-locked, but each one adds its own delay on the way to the
      cone — a different decoder, a different amplifier, a DSP stage, a slower Wi-Fi link. A few milliseconds is
      inaudible; ten or twenty smears the stereo image, and a hundred sounds like an echo between rooms. Alignment adds
      a compensating delay to the speakers that are <em>early</em>, until they all land together.
    </p>

    <section>
      <h3>Why it's per source</h3>
      <p>
        Speakers fed by the same source play off one clock — that's what makes them a sync group, and only speakers in
        one group can be aligned against each other. So each source card lists the speakers currently playing it and
        offers <strong>Align speakers</strong> for exactly that set. Two speakers on different sources are not
        comparable: they're two independent streams.
      </p>
      <p class="hint">
        Nothing listed on a card? Route the source to speakers first — <strong>Music groups</strong> → a group's
        <em>Source</em> dropdown, or the routing graph below it.
      </p>
    </section>

    <section>
      <h3>The click, and why it alternates</h3>
      <p>
        Starting a session mutes the group and loops an <strong>alternating two-tone click</strong> through it: a high
        click, then a low one, one second apart. A single uniform click would be ambiguous — once a speaker is off by
        about a full click interval you can't tell whether you're lining up on the same click or the next one. With the
        two tones, a speaker that has slipped a whole click lands its high click on the reference's low one, which is
        immediately, audibly wrong.
      </p>
    </section>

    <section>
      <h3>How to run it</h3>
      <ul>
        <li>
          <strong>Pick the reference.</strong> Offsets only ever <em>add</em> delay, so the reference must be the
          physically <em>latest</em> speaker — the one that already lags the most. Everything else is delayed up to
          meet it. If you pick a fast speaker, the slow ones have nothing left to give.
        </li>
        <li>
          <strong>Only two speakers are audible</strong> at a time: the reference and the one being tuned. The rest are
          muted, because three clicks at once can't be judged. Use <strong>Tune</strong> to move to the next speaker.
        </li>
        <li>
          <strong>Drag the offset</strong> (or nudge ±1 / ±10 ms) until the two clicks fuse into one. Coming from the
          wrong side sounds like a fast flutter that slows as you approach; a perfect match is a single click with no
          doubling.
        </li>
        <li>
          <strong>Stand where you listen.</strong> Sound covers about 34 cm per millisecond, so a 3 m difference in
          your distance to the two speakers is already ~9 ms of the very thing you're tuning. Align from the seat you
          actually use, or accept that it's a compromise between positions.
        </li>
        <li>
          <strong>Finish</strong> when done. That stops the click and restores every speaker's volume. The offsets
          themselves are persisted per device, so they survive restarts and apply to normal playback.
        </li>
      </ul>
    </section>

    <section>
      <h3>What the offset actually changes</h3>
      <p>
        For a <strong>Sendspin</strong> speaker it's that device's static delay: it renders each chunk that much later
        than the timeline says. Current ESPHome firmware doesn't apply a delay change to a running stream, so each
        change reconnects that one speaker — expect tens of seconds of silence from it before the click returns, while
        its group-mates keep playing. (If your firmware does apply it live, turn on
        <em>Sendspin delay applies live</em> under <strong>Settings</strong> and the slider becomes continuous.)
      </p>
      <p>
        For an <strong>AirPlay 2</strong> receiver it's the render delay we ask it to use. Those are committed when you
        release the slider rather than while dragging.
      </p>
      <p class="hint">
        Because the delay is added on our side, aligning also raises how far ahead the group must be sent. A large
        offset therefore increases that group's overall latency — see <em>Group sync</em> under
        <strong>Settings</strong>.
      </p>
    </section>

    <section>
      <h3>When alignment is the wrong tool</h3>
      <ul>
        <li>
          <strong>Drift</strong> — in sync at first, apart after a few minutes — is a clock problem, not an offset. A
          fixed delay can't fix it. For AirPlay 2 that's usually a missing PTP lock in a multi-room group
          (<strong>Diagnostics</strong>).
        </li>
        <li>
          <strong>Dropouts and stutter</strong> need buffer, not delay: raise the source's jitter buffer on this page,
          or the group lead under <strong>Settings</strong>. Watch the ⚠ xrun badges in the routing graph to see which
          node is dropping.
        </li>
        <li>
          <strong>One speaker silent</strong> is a routing or connection problem — the click plays on every member of
          the group, so if one stays quiet, look at its card under <strong>Outputs</strong>.
        </li>
      </ul>
    </section>
  </div>
</div>

<style>
  /* Same dialog chrome as the RTP sender document — this is a document too. */
  .modal-card.wide {
    width: min(880px, 100%);
  }
  .card-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 4px;
  }
  .card-head h2 {
    margin: 0;
  }
  section {
    margin-top: 20px;
    padding-top: 12px;
    border-top: 1px solid var(--divider-color);
  }
  section h3 {
    margin: 0 0 8px;
    font-size: 0.95rem;
    font-weight: 500;
  }
  section p,
  section li {
    font-size: 0.85rem;
    color: var(--secondary-text-color);
  }
  section p {
    margin: 8px 0;
  }
  section ul {
    margin: 8px 0;
    padding-left: 1.2rem;
  }
  section li {
    margin-bottom: 6px;
  }
  section strong,
  section em {
    color: var(--primary-text-color);
  }
  .hint {
    font-size: 0.78rem;
  }
</style>
