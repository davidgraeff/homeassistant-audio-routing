<script lang="ts">
  // "Explain speaker alignment" — the document behind the help button on the
  // **Alignment** page, beside the wizard it describes (plan §12.1).
  //
  // It used to live in the Sources header and explain why alignment was *per source*: a
  // sync group was resolved from the source set feeding it, so a source card listed the
  // only set that could be aligned. That model is gone — alignment now forms a temporary
  // group around whichever speakers the user picks — so the document moved with the
  // feature and the "why it's per source" section is replaced by the thing that actually
  // decides what a user gets: which of plan §1's three modes they choose.
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
      <h2 id="align-docs-title">Aligning speakers</h2>
      <button class="ghost" type="button" onclick={onClose}>Close</button>
    </div>
    <p class="card-sub">
      Speakers that play the same stream should be sample-locked, but each one adds its own delay on the way to the
      cone — a different decoder, a different amplifier, a DSP stage, a slower Wi-Fi link. A few milliseconds is
      inaudible; ten or twenty smears the stereo image, and a hundred sounds like an echo between rooms. Alignment
      turns each speaker's own timing knob until they all land together — and the two kinds of knob work in
      <em>opposite</em> directions: a <strong>Sendspin</strong> speaker's knob makes it play <em>earlier</em>, an
      <strong>AirPlay 2</strong> receiver's or <strong>PipeWire host</strong>'s makes it play <em>later</em>.
    </p>

    <section>
      <h3>You pick the speakers, and they are held for the run</h3>
      <p>
        Speakers can only be compared if they are playing off <em>one clock</em>, so alignment starts by taking the ones
        you selected and grouping them on their own for the duration. Whatever they were playing stops and comes back when
        you finish, and nothing else — no other source, no ordinary announcement — reaches them in between. An alarm or a
        voice-assistant answer still gets through on purpose, and when it does you are told, because it spoils the reading
        it landed on.
      </p>
      <p>
        Grouping them makes each speaker reconnect, which costs tens of seconds — and the same again when the run ends. So
        it happens <strong>once, for the whole run</strong>: pick every speaker the run will touch, usually all of them or
        one floor. When a measurement then asks which ones you can hear from where you are standing, that is only muting,
        which is instant.
      </p>
    </section>

    <section>
      <h3>Three ways to align, and they promise different things</h3>
      <p>
        A microphone in one place hears the electrical delay <em>and</em> the sound's travel time together and cannot
        separate them — about 3 ms per extra metre. That is not a problem to be solved but a choice about what "aligned"
        should mean, so the wizard asks it as soon as it knows which answers this phone can deliver: it checks the
        microphone first, and the two measured modes are only offered when there is a capture to measure with.
      </p>
      <ul>
        <li>
          <strong>Multi-position</strong> — you sit still and the add-on measures every speaker it can hear. Aligned at
          <em>that spot</em>. If no single spot hears everything, do it once per room and name one or two speakers you can
          hear from both: those overlaps are what tie the rooms together, and each room still ends up aligned at its own
          spot, so a doorway between two of them is approximate.
        </li>
        <li>
          <strong>Near field</strong> — you walk to each speaker in turn and hold the phone <em>at</em> it. That takes the
          room out of the measurement, so what gets aligned is the wiring, and a wire alignment is right everywhere rather
          than at one seat. It depends entirely on you holding the phone within a hand's width of the driver: a metre away
          adds ~3 ms and reads as that speaker being late, which nothing can detect. The last stop is a
          <em>revisit</em> of the speaker you started at — that second reading is what separates the phone's clock drift
          over a long walk from real offsets, and it is also why checking the result means walking it again.
        </li>
        <li>
          <strong>Manual</strong> — by ear, no microphone. The fallback when there is no usable mic (it needs HTTPS and
          permission) or when the estimator refuses to answer. Same speaker selection, same hold; you just judge it
          yourself.
        </li>
      </ul>
      <p class="hint">
        The two measured modes propose a setting per speaker and <strong>write nothing until you approve it</strong>. By
        ear there is nothing to approve: each nudge goes straight to that speaker's own setting.
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
      <h3>How to run it by ear</h3>
      <ul>
        <li>
          <strong>Pick the reference — which one depends on the kinds.</strong> Each speaker's knob only moves it one
          way, so the reference has to be the speaker the others can actually reach. Among <em>Sendspin</em> speakers
          the knob is an <em>advance</em>, so leave the <em>earliest</em> one at zero and bring the late ones forward to
          meet it. Among <em>AirPlay 2</em> receivers and <em>PipeWire hosts</em> the knob is a delay, so it is the
          other way round: reference the <em>latest</em> one. A mixed group meets somewhere in between, and if the early
          speaker is a delay-only one while the late speaker is advance-only, no setting brings them together at all —
          then move a speaker, or drop one from the group.
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
          <strong>Stop and restore</strong> when done. That stops the click, gives the speakers back and puts their
          volumes, mutes and routing right. It does <em>not</em> undo your tuning: each offset was written to its device as
          you made it, so it is already persisted and applies to normal playback.
        </li>
      </ul>
    </section>

    <section>
      <h3>What the offset actually changes</h3>
      <p>
        For a <strong>Sendspin</strong> speaker it is that device's <em>static delay</em> setting, and despite the name
        the device <em>subtracts</em> it from the moment it was told to play: raising it makes that speaker play
        <em>earlier</em>, not later. Current ESPHome firmware doesn't apply the change to a running stream, so each
        change reconnects that one speaker — expect tens of seconds of silence from it before the click returns, while
        its group-mates keep playing. (If your firmware does apply it live, turn on
        <em>Sendspin delay applies live</em> under <strong>Settings</strong> and the slider becomes continuous.)
      </p>
      <p>
        For an <strong>AirPlay 2</strong> receiver it is the render delay we ask it to use, and for a
        <strong>PipeWire host</strong> the receiver's playout buffer: both make that speaker play <em>later</em>. Those
        are committed when you release the slider rather than while dragging.
      </p>
      <p class="hint">
        Either direction costs latency for the whole group: a delay is added on our side, and an advance means every
        speaker has to be sent that much further ahead. So a large offset increases that group's overall latency — see
        <em>Group sync</em> under <strong>Settings</strong>.
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
          <strong>Dropouts and stutter</strong> need buffer, not delay: raise the source's jitter buffer on the
          <strong>Sources</strong> page, or the group lead under <strong>Settings</strong>. Watch the ⚠ xrun badges in the
          routing graph to see which node is dropping.
        </li>
        <li>
          <strong>One speaker silent</strong> is a routing or connection problem — the click plays on every speaker in the
          run, so if one stays quiet, look at its card on this page.
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
  /* Tighter than the shared section head (styles/sections.css): a docs panel's
     title sits closer to its body. Only the differences are listed. */
  .card-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 4px;
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
