<script lang="ts">
  // "Explain music groups" — the document behind the help button in the
  // Music-groups header: exclusive membership, what Source does, and the mixed
  // state the routing graph below can produce. Kept off the page itself so the
  // editor is the first thing you see.
  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

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
    aria-labelledby="music-docs-title"
    tabindex="-1"
    bind:this={dialogEl}
    onclick={(e) => e.stopPropagation()}
  >
    <div class="card-head">
      <h2 id="music-docs-title">Music groups</h2>
      <button class="ghost" type="button" onclick={onClose}>Close</button>
    </div>
    <p class="card-sub">
      A set of speakers that plays the same audio in sync — a room, a floor, the whole house. Each group is one Home
      Assistant <code>media_player</code> entity, so it can be selected, volume-controlled and automated as a unit.
    </p>

    <section>
      <h3>Building one</h3>
      <p>
        Drop a speaker on <strong>New group</strong> and it becomes a group straight away, named <em>Group 1</em>,
        <em>2</em>, … — click the name to rename it. Drag more speakers in from <strong>Available</strong>, drag a chip
        between groups to move it, or back to <strong>Available</strong> to release it.
      </p>
      <p>
        Membership is <strong>exclusive</strong>: a speaker is in at most one music group, because it can only play one
        stream at a time. Announcement groups are the opposite — they overlap freely, and a speaker needs no music group
        to be announced to.
      </p>
    </section>

    <section>
      <h3>Source</h3>
      <p>
        Picks what the whole group plays. Choosing one links that source to <em>every</em> member and removes any other
        source feeding them, which is what keeps the group in sync — the speakers share one clock precisely because they
        share one source. <code>(none)</code> un-routes the group without dissolving it.
      </p>
    </section>

    <section>
      <h3>⚠ Mixed</h3>
      <p>
        The members disagree: some are unrouted, or they sit on different sources. A group cannot get into that state
        from this page — it comes from the API, or from wiring a single speaker in the routing graph's
        <em>Show individual speakers</em> view. The pill expands to show who is on what; picking the group's source
        again puts everyone back on one stream.
      </p>
    </section>

    <section>
      <h3>And the graph below</h3>
      <p>
        The routing graph is this same routing one level lower: the per-speaker links a group-level source expands into.
        Dragging a wire onto a group there is the very same call as its <strong>Source</strong> dropdown here, so the two
        can't disagree. Only speakers added on the <strong>Outputs</strong> page appear on either surface.
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
</style>
