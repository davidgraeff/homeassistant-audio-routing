<script lang="ts">
  // "Explain outputs" — the document behind the help button in the Supported
  // outputs header. The page itself keeps one sentence; everything a first-time
  // reader needs (what each kind is, what the states mean, what add/ignore do)
  // lives here, the same split the Music groups and Announcements pages use.
  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

  // Focus the dialog so Page Down / arrows scroll it rather than the page behind.
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
    aria-labelledby="outputs-docs-title"
    tabindex="-1"
    bind:this={dialogEl}
    onclick={(e) => e.stopPropagation()}
  >
    <div class="card-head">
      <h2 id="outputs-docs-title">Outputs: what this router can play to</h2>
      <button class="ghost" type="button" onclick={onClose}>Close</button>
    </div>

    <h3>Three kinds of output</h3>
    <p class="card-sub">
      <strong>AirPlay 2</strong> receivers — AV receivers, HomePods, AirPlay speakers.
      <strong>Sendspin</strong> speakers — the open multi-room protocol used by ESPHome and Home
      Assistant Voice PE. And <strong>PipeWire hosts</strong>: an ordinary Linux machine running the
      receiver agent, which plays what you send it through its own speakers (see
      <em>Explain receiver hosts</em>).
    </p>
    <p class="card-sub">
      Route one source to several of them — any mix of kinds — and they play as one synchronized
      group.
    </p>

    <h3>Discovered is not the same as added</h3>
    <p class="card-sub">
      Compatible devices on your network are found automatically, but discovery only <em>offers</em>
      them: a device does nothing until you <strong>add</strong> it. Added devices are routable in the
      matrix, groupable, and — with the setting on — each becomes a Home Assistant
      <code>media_player</code>. Ignoring a device hides it from the list; adding it later restores any
      routing it used to have.
    </p>
    <p class="card-sub">
      Not sure which speaker a name belongs to? Expand its card and play a test tone — that works for
      a discovered device too, before you add it.
    </p>

    <h3>What the states mean</h3>
    <p class="card-sub">
      <span class="badge off">offline</span> — not on the network right now. Its routing is kept and
      reapplied when it comes back, so a speaker that is simply switched off does not lose its place in
      a group.
    </p>
    <p class="card-sub">
      <span class="badge caution">not connected</span> — a PipeWire target is reachable but has not
      connected to the session this router advertises, so anything routed to it is not being played
      yet. For an agent-backed host this usually means the agent is not running.
    </p>

    <h3>Removing</h3>
    <p class="card-sub">
      <strong>Remove</strong> puts a device back to undecided: it stops being routable, loses its Home
      Assistant <code>media_player</code>, and its routing and group membership are forgotten. If it is
      still on the network it reappears under Discovered.
    </p>
  </div>
</div>
