<script lang="ts">
  // "Explain announcement groups" — the document behind the help button in the
  // Announcements header. It holds the detail the page used to state in two
  // paragraphs above the editor: overlapping membership, what Duck and Priority
  // actually do, and how to play to a group from Home Assistant.
  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

  // Focus the dialog so Page Down / arrows scroll it, not the page behind it
  // (same as AlignDocs / RtpSenderDocs).
  let dialogEl = $state<HTMLDivElement>();
  $effect(() => {
    dialogEl?.focus();
  });
</script>

<svelte:window onkeydown={(e) => e.key === 'Escape' && onClose()} />

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="modal-backdrop" onclick={onClose}>
  <div
    class="modal-card card"
    role="dialog"
    aria-modal="true"
    aria-labelledby="announcement-docs-title"
    tabindex="-1"
    bind:this={dialogEl}
    onclick={(e) => e.stopPropagation()}
  >
    <div class="card-head">
      <h2 id="announcement-docs-title">Announcement groups</h2>
      <button class="ghost" type="button" onclick={onClose}>Close</button>
    </div>
    <p class="card-sub">
      A named set of speakers a short clip goes to — a doorbell, a TTS sentence, an alarm. Each group is one Home
      Assistant <code>media_player</code> entity, so one service call reaches every speaker in it.
    </p>

    <section>
      <h3>Building one</h3>
      <p>
        Drop a speaker on <strong>New group</strong> and it becomes a group straight away, named
        <em>Announcement 1</em>, <em>2</em>, … — click the name to rename it. Drag more speakers in from
        <strong>Available</strong>, or drag a chip out of a group to remove it from that group.
      </p>
      <p>
        Membership <strong>overlaps freely</strong>: a speaker may sit in any number of announcement groups (and in a
        music group as well), so <em>Everywhere</em>, <em>Upstairs</em> and <em>Kitchen only</em> can all contain the
        kitchen speaker. That's why <strong>Available</strong> always lists every speaker — dragging from it copies. A
        speaker needs no music group and needn't be playing anything to be announced to.
      </p>
    </section>

    <section>
      <h3>Duck</h3>
      <p>
        The clip is <strong>overlaid</strong> on whatever the speakers are already playing rather than interrupting it.
        Duck is the level that music drops to for the duration: <code>0.25</code> means a quarter of its volume,
        <code>0</code> silences it completely, <code>1</code> leaves it untouched (announcement mixed on top at full
        music level). The original level returns when the clip ends.
      </p>
    </section>

    <section>
      <h3>Priority</h3>
      <p>
        Decides what happens when a second announcement arrives while one is playing. A <strong>higher</strong> priority
        preempts a lower one — the running clip is cut off. <strong>Equal</strong> priorities queue, so a doorbell and a
        TTS notification both get played, in order. Put alarms above notifications above chimes.
      </p>
      <p class="hint">
        A group can also be refused: if none of its speakers has a live session (they're offline, or reachable with
        nothing attached), there is nothing to overlay and the request is rejected rather than silently dropped.
      </p>
    </section>

    <section>
      <h3>Playing to a group</h3>
      <p>
        Use the group's entity like any other media player — <code>tts.speak</code> with it as the target, or
        <code>media_player.play_media</code> with a URL or a media-source URI. <strong>Test</strong> on a group's card
        plays the built-in test clip to it right now, which is the quickest way to confirm the members and the duck
        level before wiring an automation.
      </p>
    </section>
  </div>
</div>

<style>
  /* Same document chrome as AlignDocs / RtpSenderDocs. */
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
  section p {
    margin: 8px 0;
    font-size: 0.85rem;
    color: var(--secondary-text-color);
  }
  section strong,
  section em {
    color: var(--primary-text-color);
  }
  .hint {
    font-size: 0.78rem;
  }
</style>
