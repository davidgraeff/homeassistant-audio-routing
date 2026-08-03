<script lang="ts">
  // The routing graph's explanation, as a component so it can be rendered in two
  // places from one source: as a side card beside the graph on wide screens, and
  // inside the graph's help dialog. It used to be two paragraphs printed above and
  // below the graph, which is where nobody reads them.
  //
  // `tabbed` is for the side card only: read whole, the document is taller than
  // the graph it explains, so beside it we show one section at a time. The dialog
  // renders everything at once — it has the room, and it's where you go to read
  // rather than to glance.
  interface Props {
    /** Whether any node reports a latency estimate — gates the ~ms section. */
    anyLatency?: boolean;
    /** Whether any node reports xruns — gates the ⚠-count section. */
    anyXruns?: boolean;
    /** Show one section at a time behind a tab strip (side-card mode). */
    tabbed?: boolean;
  }
  let { anyLatency = false, anyXruns = false, tabbed = false }: Props = $props();

  type TabId = 'lines' | 'nodes' | 'badges';
  // The badge section only exists when there are badges to explain, so the tab
  // comes and goes with it.
  let showBadges = $derived(anyLatency || anyXruns);
  let tabs = $derived([
    { id: 'lines' as TabId, label: 'Lines' },
    { id: 'nodes' as TabId, label: 'Nodes' },
    ...(showBadges ? [{ id: 'badges' as TabId, label: 'Badges' }] : []),
  ]);
  let sel = $state<TabId>('lines');
  // Fall back if the selected tab has just disappeared (badges stopped arriving).
  let active = $derived(tabs.some((t) => t.id === sel) ? sel : 'lines');
</script>

{#snippet lines()}
  <p>
    <strong>Drag</strong> from a source on the left to a group on the right — or the other way — to make it play there;
    <strong>click a line</strong> to stop it. A group plays one source at a time, the same thing its <em>Source</em>
    dropdown does, so its speakers stay in sync.
  </p>
  <h3>What a line says</h3>
  <ul>
    <li><strong>Animated</strong> — audio is really being carried right now.</li>
    <li>
      <strong>Amber, still</strong> — routed and reachable, but no speaker on it has a session yet (<em
        >not connected</em
      >), so nothing is being heard.
    </li>
    <li><strong>Amber dashes</strong> — only some of a group's speakers are on that source: the ⚠ mixed state.</li>
    <li><strong>Grey dashes</strong> — offline. The routing is kept and reapplied when the device comes back.</li>
  </ul>
{/snippet}

{#snippet nodes()}
  <h3>Nodes</h3>
  <ul>
    <li>Speakers in no music group are nodes of their own, and may take more than one source — that mixes them.</li>
    <li>
      <strong>✕</strong> forgets an offline source's saved routing, or removes an offline speaker from your outputs.
    </li>
    <li>Only speakers you added on the <strong>Outputs</strong> page appear here. Everything is live from PipeWire.</li>
  </ul>
  <h3>Show individual speakers</h3>
  <p>
    The expert view: every speaker becomes its own node and you wire them one at a time. It's the only way to pull a
    single member of a group onto its own source — and the way a group ends up reporting <strong>⚠ Mixed</strong>. Pick
    that group's source again to reconcile it.
  </p>
{/snippet}

{#snippet badges()}
  {#if !tabbed}
    <h3>Badges</h3>
  {/if}
  {#if anyLatency}
    <p>
      The <strong>~ms</strong> figures estimate the buffering each node adds — the configured jitter/playout buffer, not
      a live measurement. A route's rough latency ≈ its <em>source</em> figure + its <em>speaker</em> figure, plus a
      little fixed graph overhead (sync anchor and one processing quantum, ~50–150&nbsp;ms) and the sender's own buffer
      (a phone's Bluetooth stack, say), which aren't shown. Tune the input jitter buffer and per-device alignment under
      <strong>Sources</strong>, the sendspin group lead under <strong>Settings</strong>.
    </p>
  {/if}
  {#if anyXruns}
    <p>
      A <strong class="xrun-inline">⚠ count</strong> is that node's dropped audio cycles (PipeWire xruns, like
      <code>pw-top</code>'s ERR). It turns red while climbing, marking where a dropout originates. Only collected while
      this page is open.
    </p>
  {/if}
{/snippet}

<div class="help">
  {#if tabbed}
    <div class="tabs" role="tablist">
      {#each tabs as t (t.id)}
        <button type="button" role="tab" class:on={active === t.id} aria-selected={active === t.id} onclick={() => (sel = t.id)}>
          {t.label}
        </button>
      {/each}
    </div>
    {#if active === 'lines'}
      {@render lines()}
    {:else if active === 'nodes'}
      {@render nodes()}
    {:else}
      {@render badges()}
    {/if}
  {:else}
    {@render lines()}
    {@render nodes()}
    {#if showBadges}
      {@render badges()}
    {/if}
  {/if}
</div>

<style>
  /* Document type: secondary text, same scale in the side card and the dialog. */
  .help p,
  .help li {
    font-size: 0.85rem;
    color: var(--secondary-text-color);
  }
  .help p {
    margin: 8px 0;
  }
  .help h3 {
    margin: 16px 0 6px;
    font-size: 0.9rem;
    font-weight: 500;
    color: var(--primary-text-color);
  }
  .help h3:first-child {
    margin-top: 0;
  }
  .help ul {
    margin: 6px 0;
    padding-left: 1.1rem;
  }
  .help li {
    margin-bottom: 5px;
  }
  .help strong,
  .help em {
    color: var(--primary-text-color);
  }
  .xrun-inline {
    color: var(--warning-color, #b26a00);
  }

  /* Section picker (side-card mode). Same underline idiom as the app's own tabs,
     one size down. */
  .tabs {
    display: flex;
    gap: 2px;
    margin-bottom: 10px;
    border-bottom: 1px solid var(--divider-color);
  }
  .tabs button {
    background: none;
    border: none;
    border-radius: 0;
    border-bottom: 2px solid transparent;
    padding: 4px 8px;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .tabs button:hover {
    box-shadow: none;
    color: var(--primary-text-color);
  }
  .tabs button.on {
    color: var(--primary-color);
    border-bottom-color: var(--primary-color);
    font-weight: 500;
  }
  /* The first heading of a pane sits right under the tab strip. */
  .tabs + p,
  .tabs + h3 {
    margin-top: 0;
  }
</style>
